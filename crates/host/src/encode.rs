//! The capture → encode thread behind a session.
//!
//! Encoders sit behind [`VideoEncoder`], one per codec and backend, so a session can change
//! codec (or fall back from the graphics card to the processor) without the rest knowing. On
//! Windows the picture can stay on the graphics card from capture to encoder: the captured image
//! is converted to NV12 there and handed to the hardware encoder as a texture. Where any part
//! of that isn't available, or fails, pictures go through memory as before.

#[cfg(windows)]
mod mf;
mod software;

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use bytes::Bytes;
use connect_proto::{
    Codec, CodecSupport, now_micros,
    video::{EncodedFrame, FrameMeta},
};
use tokio::sync::mpsc;

use crate::{
    capture::{Capturer, VideoSource},
    yuv::{Layout, Picture},
};

/// Used when the capture source can't tell its refresh rate. Capture only yields a frame when
/// the screen changes, so the stream runs at whatever rate the host's monitor and content do.
const DEFAULT_FPS: u32 = 144;
/// A keyframe at least this often, in case a keyframe request is lost along with the video.
const INTRA_PERIOD_FRAMES: u32 = 600;
/// How long one capture wait may block, so a stop request is noticed promptly.
const POLL: Duration = Duration::from_millis(100);
/// How soon to look again while a captured picture waits for the network to catch up.
const HELD_POLL: Duration = Duration::from_millis(2);

pub(crate) enum VideoEvent {
    /// The encoded size; always the first event, and sent again whenever the screen resizes.
    Format { width: u32, height: u32 },
    Frame(EncodedFrame),
    Encoder { name: String, codec: Codec },
}

/// True while the network is too backed up to take another frame.
pub(crate) type Backlogged = Box<dyn Fn() -> bool + Send + Sync>;

/// What the session tells the video thread.
struct Control {
    keyframe: AtomicBool,
    bitrate: AtomicU32,
    codec: Mutex<Codec>,
    stop: AtomicBool,
    backlogged: Backlogged,
}

/// Runs capture and encode on a dedicated thread, which stops when this is dropped.
pub(crate) struct VideoStream {
    events: mpsc::Receiver<Result<VideoEvent>>,
    control: Arc<Control>,
}

impl VideoStream {
    pub fn start(source: VideoSource, codec: Codec, bitrate_bps: u32, backlogged: Backlogged) -> Result<Self> {
        // Small, so a slow network pushes back on the encoder instead of queueing stale frames.
        let (tx, events) = mpsc::channel(2);
        let control = Arc::new(Control {
            keyframe: AtomicBool::new(false),
            bitrate: AtomicU32::new(bitrate_bps),
            codec: Mutex::new(codec),
            stop: AtomicBool::new(false),
            backlogged,
        });
        let thread = control.clone();
        std::thread::Builder::new()
            .name("connect-video".into())
            .spawn(move || {
                if let Err(e) = run(&source, &tx, &thread) {
                    let _ = tx.blocking_send(Err(e));
                }
            })
            .context("starting video thread")?;
        Ok(Self { events, control })
    }

    /// The next event; `None` once the thread has stopped.
    pub async fn next(&mut self) -> Option<Result<VideoEvent>> {
        self.events.recv().await
    }

    /// Makes the next encoded frame a keyframe, even if the screen hasn't changed.
    pub fn request_keyframe(&self) {
        self.control.keyframe.store(true, Ordering::Relaxed);
    }

    /// Changes the target bitrate from the next frame on.
    pub fn set_bitrate(&self, bps: u32) {
        self.control.bitrate.store(bps, Ordering::Relaxed);
    }

    /// Switches to `codec` from the next frame on, starting with a keyframe.
    pub fn set_codec(&self, codec: Codec) {
        *self.control.codec.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = codec;
    }
}

impl Drop for VideoStream {
    fn drop(&mut self) {
        self.control.stop.store(true, Ordering::Relaxed);
    }
}

/// The codecs this host can encode from `source`, for choosing one with the viewer.
pub(crate) fn supported(source: &VideoSource) -> Vec<CodecSupport> {
    let mut list = Vec::new();
    #[cfg(windows)]
    if *source == VideoSource::Screen {
        list.extend(mf::hardware_codecs().into_iter().map(CodecSupport::hardware));
    }
    #[cfg(not(windows))]
    let _ = source;
    list.push(CodecSupport::software(Codec::H264));
    list
}

/// One encoded frame.
pub(crate) struct Encoded {
    pub data: Bytes,
    pub keyframe: bool,
    pub capture_time_us: u64,
}

/// A picture for an encoder to read.
pub(crate) enum Input<'a> {
    /// In memory.
    Picture(&'a Picture),
    /// An NV12 texture on the graphics card, on the device the encoder was opened with.
    #[cfg(windows)]
    Texture(&'a windows::Win32::Graphics::Direct3D11::ID3D11Texture2D),
}

/// What an encoder reads its pictures from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputKind {
    Picture(Layout),
    #[cfg_attr(not(windows), allow(dead_code))]
    Texture,
}

/// One codec on one backend.
pub(crate) trait VideoEncoder {
    fn codec(&self) -> Codec;

    /// Shown to the viewer, e.g. "GPU: NVIDIA H.264 Encoder MFT".
    fn name(&self) -> &str;

    fn hardware(&self) -> bool;

    fn input(&self) -> InputKind;

    /// Changes the target bitrate for the frames that follow, where the encoder allows it.
    fn set_bitrate(&mut self, from: u32, to: u32);

    fn encode(&mut self, input: Input<'_>, capture_time_us: u64, force_keyframe: bool) -> Result<Vec<Encoded>>;
}

/// Where the latest captured picture waits for the encoder.
enum Stage {
    Memory(Picture),
    /// Converted to NV12 on the graphics card, never leaving it.
    #[cfg(windows)]
    Gpu(crate::gpu::Nv12Converter),
}

impl Stage {
    fn size(&self) -> (u32, u32) {
        match self {
            Stage::Memory(picture) => {
                let (w, h) = picture.size();
                (w as u32, h as u32)
            }
            #[cfg(windows)]
            Stage::Gpu(converter) => converter.size(),
        }
    }

    fn input(&self) -> Input<'_> {
        match self {
            Stage::Memory(picture) => Input::Picture(picture),
            #[cfg(windows)]
            Stage::Gpu(converter) => Input::Texture(converter.texture()),
        }
    }

    /// The picture in memory, laid out as `layout`, reading it back from the graphics card if
    /// that's where it is.
    fn into_memory(self, layout: Layout) -> Result<Picture> {
        let picture = match self {
            Stage::Memory(picture) => picture,
            #[cfg(windows)]
            Stage::Gpu(converter) => converter.read_back().context("reading the picture back from the GPU")?,
        };
        Ok(if picture.layout() == layout { picture } else { picture.to_layout(layout) })
    }
}

/// Opens the best encoder for `codec` that reads from memory: the graphics card's, then
/// OpenH264 for H.264.
fn open_memory_encoder(
    source: &VideoSource,
    codec: Codec,
    width: u32,
    height: u32,
    bitrate: u32,
    fps: u32,
) -> Result<Box<dyn VideoEncoder>> {
    #[cfg(windows)]
    if *source == VideoSource::Screen {
        match mf::HardwareEncoder::new(codec, width, height, bitrate, fps, None) {
            Ok(encoder) => return Ok(Box::new(encoder)),
            Err(error) => tracing::warn!("GPU {codec} encoder unavailable: {error:#}"),
        }
    }
    #[cfg(not(windows))]
    let _ = (source, width, height);
    open_software_encoder(codec, bitrate, fps)
}

fn open_software_encoder(codec: Codec, bitrate: u32, fps: u32) -> Result<Box<dyn VideoEncoder>> {
    match codec {
        Codec::H264 => Ok(Box::new(software::OpenH264::new(bitrate, fps)?)),
        Codec::H265 | Codec::Av1 => bail!("no software {codec} encoder"),
    }
}

/// Opens the graphics-card-only path for `codec` at this size: NV12 conversion on the device
/// the capture uses, and a hardware encoder reading its textures.
#[cfg(windows)]
fn open_gpu_path(
    device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
    codec: Codec,
    width: u32,
    height: u32,
    bitrate: u32,
    fps: u32,
) -> Result<(Stage, Box<dyn VideoEncoder>)> {
    let converter = crate::gpu::Nv12Converter::new(device, width, height).context("GPU colour conversion")?;
    let encoder = mf::HardwareEncoder::new(codec, width, height, bitrate, fps, Some(device))
        .context("GPU encoder taking textures")?;
    Ok((Stage::Gpu(converter), Box::new(encoder)))
}

/// Whether to try keeping pictures on the graphics card. On by default for the real screen;
/// `CONNECT_GPU_PIPELINE=0` turns it off, e.g. to compare.
fn gpu_pipeline_wanted(source: &VideoSource) -> bool {
    cfg!(windows)
        && *source == VideoSource::Screen
        && std::env::var("CONNECT_GPU_PIPELINE").map_or(true, |value| value != "0")
}

/// The stream's encoder and the picture it reads, rebuilt when the size or codec changes.
struct Pipeline {
    stage: Stage,
    encoder: Box<dyn VideoEncoder>,
}

fn run(source: &VideoSource, tx: &mpsc::Sender<Result<VideoEvent>>, control: &Control) -> Result<()> {
    let mut capturer = source.open()?;
    let fps = capturer.refresh_hz().unwrap_or(DEFAULT_FPS).clamp(30, 240);
    // Turned off for the rest of the stream once anything on the graphics-card path fails.
    let mut gpu = gpu_pipeline_wanted(source);
    // Codecs that failed here; the stream stays off them even if the session asks again.
    let mut failed: Vec<Codec> = Vec::new();
    let mut pipeline: Option<Pipeline> = None;
    let mut bitrate = control.bitrate.load(Ordering::Relaxed);
    // Whether the staged picture is newer than the last one sent. The last picture is kept
    // so a keyframe can be sent while the screen is static, and so a picture held back while
    // the network catches up can be sent once it has.
    let mut unsent = false;
    let mut picture_time_us = 0;
    let mut sent_time_us = 0;
    let mut seq = 0u32;
    let clock_start = now_micros();
    let stream_start = Instant::now();
    let send = |event| tx.blocking_send(Ok(event)).is_ok();
    let wanted_codec = |failed: &[Codec]| {
        let codec = *control.codec.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if failed.contains(&codec) { Codec::H264 } else { codec }
    };

    while !control.stop.load(Ordering::Relaxed) {
        let timeout = if unsent { HELD_POLL } else { POLL };
        let codec = wanted_codec(&failed);
        let fresh = capture(
            &mut *capturer,
            &mut pipeline,
            &mut gpu,
            source,
            codec,
            control.bitrate.load(Ordering::Relaxed),
            fps,
            timeout,
            &send,
        )?;
        let Some(fresh) = fresh else { return Ok(()) };
        let now_us = clock_start + stream_start.elapsed().as_micros() as u64;
        if fresh {
            picture_time_us = now_us;
            unsent = true;
        }
        let Some(active) = &mut pipeline else { continue };

        let mut force_keyframe = control.keyframe.load(Ordering::Relaxed);
        if active.encoder.codec() != codec {
            // The session switched codec: a new encoder, which starts with a keyframe.
            let (w, h) = active.stage.size();
            let stage = std::mem::replace(&mut active.stage, Stage::Memory(Picture::new(Layout::I420, 2, 2)));
            let rebuilt = rebuild(stage, source, codec, w, h, bitrate, fps)?;
            *active = rebuilt;
            if !send(VideoEvent::Encoder { name: active.encoder.name().to_owned(), codec }) {
                return Ok(());
            }
            force_keyframe = true;
        }
        // Hold the picture while the network catches up: sent now, it would only queue behind
        // the rest. A requested keyframe goes regardless, as the viewer is stuck without one.
        if !(unsent || force_keyframe) || ((control.backlogged)() && !force_keyframe) {
            continue;
        }
        control.keyframe.store(false, Ordering::Relaxed);
        unsent = false;
        // Stamped when captured; a static picture sent again as a keyframe is stamped anew, as
        // hardware encoders need every timestamp later than the last.
        let capture_time_us = picture_time_us.max(sent_time_us + 1);
        sent_time_us = capture_time_us;

        let wanted = control.bitrate.load(Ordering::Relaxed);
        if wanted != bitrate {
            active.encoder.set_bitrate(bitrate, wanted);
            bitrate = wanted;
        }
        let started = Instant::now();
        let encoded = match active.encoder.encode(active.stage.input(), capture_time_us, force_keyframe) {
            Ok(encoded) => encoded,
            Err(error) if active.encoder.hardware() || !matches!(active.stage, Stage::Memory(_)) => {
                // Carry on on the processor: OpenH264, reading the picture from memory. A
                // codec only the graphics card can do is given up for H.264.
                let codec = active.encoder.codec();
                tracing::warn!("{} failed; recovering on the processor: {error:#}", active.encoder.name());
                gpu = false;
                if codec != Codec::H264 {
                    failed.push(codec);
                }
                let stage = std::mem::replace(&mut active.stage, Stage::Memory(Picture::new(Layout::I420, 2, 2)));
                let encoder = open_software_encoder(Codec::H264, bitrate, fps)?;
                active.stage = Stage::Memory(stage.into_memory(Layout::I420)?);
                active.encoder = encoder;
                if !send(VideoEvent::Encoder { name: active.encoder.name().to_owned(), codec: Codec::H264 }) {
                    return Ok(());
                }
                active.encoder.encode(active.stage.input(), capture_time_us, true)?
            }
            Err(error) => return Err(error.context("encoding frame")),
        };
        let encode_time_us = started.elapsed().as_micros().min(u32::MAX as u128) as u32;
        let codec = active.encoder.codec();
        for picture in encoded {
            let meta = FrameMeta {
                seq,
                keyframe: picture.keyframe,
                codec,
                capture_time_us: picture.capture_time_us,
                encode_time_us,
            };
            seq = seq.wrapping_add(1);
            if !send(VideoEvent::Frame(EncodedFrame { meta, data: picture.data })) {
                return Ok(());
            }
        }
    }
    Ok(())
}

/// A pipeline for `codec` around the picture already staged.
fn rebuild(
    stage: Stage,
    source: &VideoSource,
    codec: Codec,
    width: u32,
    height: u32,
    bitrate: u32,
    fps: u32,
) -> Result<Pipeline> {
    #[cfg(windows)]
    if let Stage::Gpu(converter) = &stage {
        match mf::HardwareEncoder::new(codec, width, height, bitrate, fps, Some(converter.device())) {
            Ok(encoder) => return Ok(Pipeline { stage, encoder: Box::new(encoder) }),
            Err(error) => tracing::warn!("GPU {codec} encoder taking textures unavailable: {error:#}"),
        }
    }
    let encoder = open_memory_encoder(source, codec, width, height, bitrate, fps)
        .or_else(|_| open_software_encoder(Codec::H264, bitrate, fps))?;
    let InputKind::Picture(layout) = encoder.input() else { bail!("memory encoder wants textures") };
    Ok(Pipeline { stage: Stage::Memory(stage.into_memory(layout)?), encoder })
}

/// Waits up to `timeout` for a new screen image and stages it for the encoder, (re)building
/// the pipeline when the size changes. `Some(true)` with a new picture staged, `Some(false)`
/// without, `None` once the session has gone.
#[expect(clippy::too_many_arguments)]
fn capture(
    capturer: &mut dyn Capturer,
    pipeline: &mut Option<Pipeline>,
    gpu: &mut bool,
    source: &VideoSource,
    codec: Codec,
    bitrate: u32,
    fps: u32,
    timeout: Duration,
    send: &dyn Fn(VideoEvent) -> bool,
) -> Result<Option<bool>> {
    // Announces a new size and encoder; false if the session has gone.
    let announce = |pipeline: &Pipeline| {
        let (width, height) = pipeline.stage.size();
        send(VideoEvent::Format { width, height })
            && send(VideoEvent::Encoder { name: pipeline.encoder.name().to_owned(), codec: pipeline.encoder.codec() })
    };
    // The screen in memory: from the capture itself, or, after the GPU path just failed, read
    // back from the image it captured.
    #[cfg_attr(not(windows), expect(unused_mut))]
    let mut read_back_latest = false;
    #[cfg(not(windows))]
    let _ = gpu;

    #[cfg(windows)]
    if *gpu && let Some(dxgi) = capturer.as_dxgi() {
        let Some((w, h)) = dxgi.next_texture(timeout)? else { return Ok(Some(false)) };
        // 4:2:0 chroma needs even dimensions; drop an odd last row/column.
        let (w, h) = (w & !1, h & !1);
        ensure!(w > 0 && h > 0, "captured an empty image");
        let current = pipeline.as_ref().filter(|p| matches!(p.stage, Stage::Gpu(_))).map(|p| p.stage.size());
        if current != Some((w, h)) {
            match open_gpu_path(dxgi.device(), codec, w, h, bitrate, fps) {
                Ok((stage, encoder)) => {
                    tracing::info!(%codec, "keeping pictures on the GPU from capture to encoder");
                    let built = Pipeline { stage, encoder };
                    if !announce(&built) {
                        return Ok(None);
                    }
                    *pipeline = Some(built);
                }
                Err(error) => {
                    tracing::warn!("GPU-only pipeline unavailable, going through memory: {error:#}");
                    *gpu = false;
                    read_back_latest = true;
                }
            }
        }
        if *gpu {
            let Some(Pipeline { stage: Stage::Gpu(converter), .. }) = pipeline else { unreachable!("built above") };
            match converter.convert(dxgi.latest()) {
                Ok(()) => return Ok(Some(true)),
                Err(error) => {
                    tracing::warn!("GPU colour conversion failed, going through memory: {error:#}");
                    *gpu = false;
                    *pipeline = None;
                    read_back_latest = true;
                }
            }
        }
    }

    let frame = if read_back_latest {
        #[cfg(windows)]
        {
            capturer.as_dxgi().context("read back without DXGI")?.read_latest()?
        }
        #[cfg(not(windows))]
        unreachable!("only the Windows GPU path reads back")
    } else {
        capturer.next_frame(timeout)?
    };
    let Some(frame) = frame else { return Ok(Some(false)) };
    let (w, h) = (frame.width & !1, frame.height & !1);
    ensure!(w > 0 && h > 0, "captured an empty image");
    let current = pipeline.as_ref().filter(|p| matches!(p.stage, Stage::Memory(_))).map(|p| p.stage.size());
    if current != Some((w, h)) {
        let encoder = open_memory_encoder(source, codec, w, h, bitrate, fps)
            .or_else(|_| open_software_encoder(Codec::H264, bitrate, fps))
            .context("starting video encoder")?;
        let InputKind::Picture(layout) = encoder.input() else { bail!("memory encoder wants textures") };
        let built = Pipeline { stage: Stage::Memory(Picture::new(layout, w as usize, h as usize)), encoder };
        if !announce(&built) {
            return Ok(None);
        }
        *pipeline = Some(built);
    }
    let Some(Pipeline { stage: Stage::Memory(picture), .. }) = pipeline else { unreachable!("built above") };
    picture.convert_from_bgra(frame.bgra, frame.stride);
    Ok(Some(true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use openh264::{
        OpenH264API,
        decoder::Decoder,
        encoder::Encoder,
        formats::YUVSource as _,
    };

    fn moving_image(width: usize, height: usize, frame: usize) -> Picture {
        let mut pixels = vec![0; width * height * 4];
        for (i, pixel) in pixels.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let (x, y) = (i % width + frame * 7, i / width + frame * 3);
            pixel.copy_from_slice(&[(x ^ y) as u8, (y * 3) as u8, (x * 5) as u8, 255]);
        }
        let mut picture = Picture::new(Layout::I420, width, height);
        picture.convert_from_bgra(&pixels, width * 4);
        picture
    }

    /// Gentle gradients drifting sideways, more like a game than `moving_image`'s noise (which
    /// OpenH264 codes as a fresh keyframe every time).
    fn smooth_image(frame: usize) -> Picture {
        let (w, h) = (640, 480);
        let mut pixels = vec![0u8; w * h * 4];
        for (i, p) in pixels.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let (x, y) = ((i % w + frame * 3) as f32, (i / w) as f32);
            let v = ((x * 0.05).sin() * 50.0 + (y * 0.04).cos() * 50.0 + (x * y * 0.0007).sin() * 25.0 + 128.0) as u8;
            *p = [v, v / 2, 255 - v, 255];
        }
        let mut picture = Picture::new(Layout::I420, w, h);
        picture.convert_from_bgra(&pixels, w * 4);
        picture
    }

    fn video_slices(data: &[u8]) -> usize {
        openh264::nal_units(data)
            .filter(|nal| {
                let header = if nal.starts_with(&[0, 0, 0, 1]) { 4 } else { 3 };
                matches!(nal[header] & 0x1f, 1 | 5)
            })
            .count()
    }

    #[test]
    fn parallel_slices_decode_and_recover_after_resize() {
        // Two workers explicitly, so this also exercises the slice setup on single-core CI.
        let config = software::config(20_000_000, 60).num_threads(2);
        let mut encoder = Encoder::with_api_config(OpenH264API::from_source(), config).unwrap();
        let mut decoder = Decoder::new().unwrap();
        for (width, height) in [(640, 480), (640, 480), (800, 600)] {
            let image = moving_image(width, height, 0);
            let encoded = encoder.encode(&image).unwrap();
            let data = encoded.to_vec();
            assert!(video_slices(&data) >= 2, "a single slice serializes the encoder");
            let decoded = decoder.decode(&data).unwrap().expect("frame decodes without buffering");
            assert_eq!(decoded.dimensions(), (width, height));
        }
        encoder.force_intra_frame();
        let data = encoder.encode(&moving_image(800, 600, 1)).unwrap().to_vec();
        let mut fresh_decoder = Decoder::new().unwrap();
        assert_eq!(fresh_decoder.decode(&data).unwrap().unwrap().dimensions(), (800, 600));
    }

    #[test]
    fn tiny_frames_still_encode() {
        let mut encoder = Encoder::with_api_config(OpenH264API::from_source(), software::config(20_000_000, 60)).unwrap();
        let data = encoder.encode(&moving_image(16, 16, 0)).unwrap().to_vec();
        let mut decoder = Decoder::new().unwrap();
        assert_eq!(decoder.decode(&data).unwrap().unwrap().dimensions(), (16, 16));
    }

    /// The rate controller retunes the encoder mid-stream: no new keyframe, still decodable,
    /// and smaller frames right after the bitrate drops. (Without frame skipping OpenH264 can't
    /// hold a bitrate far below what the picture needs; the session's backlog check covers that.)
    #[test]
    fn bitrate_changes_in_place() {
        let mut encoder = software::OpenH264::new(20_000_000, 60).unwrap();
        let mut decoder = Decoder::new().unwrap();
        let mut sizes = Vec::new();
        for frame in 0..60 {
            if frame == 30 {
                encoder.set_bitrate(20_000_000, 300_000);
            }
            let encoded = encoder.encode(Input::Picture(&smooth_image(frame)), frame as u64 * 16_667, false).unwrap();
            let encoded = encoded.into_iter().next().expect("no frame skipped");
            assert_eq!(encoded.keyframe, frame == 0, "frame {frame}");
            assert!(decoder.decode(&encoded.data).unwrap().is_some());
            sizes.push(encoded.data.len());
        }
        let (before, after): (usize, usize) = (sizes[27..30].iter().sum(), sizes[30..33].iter().sum());
        assert!(after * 4 < before * 3, "{before} bytes before, {after} after");
    }
}
