//! The capture → H.264 encode thread behind a session.

#[cfg(windows)]
mod mf;

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use bytes::Bytes;
use connect_proto::{
    now_micros,
    video::{EncodedFrame, FrameMeta},
};
use openh264::{
    OpenH264API,
    encoder::{BitRate, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, RateControlMode, UsageType},
};
use tokio::sync::mpsc;

use crate::{
    capture::VideoSource,
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
    Format {
        width: u32,
        height: u32,
    },
    Frame(EncodedFrame),
    Encoder {
        name: String,
    },
}

/// True while the network is too backed up to take another frame.
pub(crate) type Backlogged = Box<dyn Fn() -> bool + Send>;

/// Runs capture and encode on a dedicated thread, which stops when this is dropped.
pub(crate) struct VideoStream {
    events: mpsc::Receiver<Result<VideoEvent>>,
    keyframe: Arc<AtomicBool>,
    bitrate: Arc<AtomicU32>,
    stop: Arc<AtomicBool>,
}

impl VideoStream {
    pub fn start(source: VideoSource, bitrate_bps: u32, backlogged: Backlogged) -> Result<Self> {
        // Small, so a slow network pushes back on the encoder instead of queueing stale frames.
        let (tx, events) = mpsc::channel(2);
        let keyframe = Arc::new(AtomicBool::new(false));
        let bitrate = Arc::new(AtomicU32::new(bitrate_bps));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = Control { keyframe: keyframe.clone(), bitrate: bitrate.clone(), stop: stop.clone(), backlogged };
        std::thread::Builder::new()
            .name("connect-video".into())
            .spawn(move || {
                if let Err(e) = run(&source, &tx, &thread) {
                    let _ = tx.blocking_send(Err(e));
                }
            })
            .context("starting video thread")?;
        Ok(Self { events, keyframe, bitrate, stop })
    }

    /// The next event; `None` once the thread has stopped.
    pub async fn next(&mut self) -> Option<Result<VideoEvent>> {
        self.events.recv().await
    }

    /// Makes the next encoded frame a keyframe, even if the screen hasn't changed.
    pub fn request_keyframe(&self) {
        self.keyframe.store(true, Ordering::Relaxed);
    }

    /// Changes the target bitrate from the next frame on.
    pub fn set_bitrate(&self, bps: u32) {
        self.bitrate.store(bps, Ordering::Relaxed);
    }
}

impl Drop for VideoStream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// What the session tells the video thread.
struct Control {
    keyframe: Arc<AtomicBool>,
    bitrate: Arc<AtomicU32>,
    stop: Arc<AtomicBool>,
    backlogged: Backlogged,
}

fn encoder_config(bitrate_bps: u32, fps: u32) -> EncoderConfig {
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get().min(4)) as u16;
    EncoderConfig::new()
        .usage_type(UsageType::ScreenContentRealTime)
        .rate_control_mode(RateControlMode::Bitrate)
        .bitrate(BitRate::from_bps(bitrate_bps))
        .max_frame_rate(FrameRate::from_hz(fps as f32))
        // A single slice forces one worker even with automatic threading. Bound CPU use for games.
        .max_slice_len(16 * 1024)
        .num_threads(threads)
        // Never drop a frame to save bits: in fast motion that stutters. The session holds
        // frames back instead when the network, rather than the encoder's budget, can't keep up.
        .skip_frames(false)
        // Not supported for screen content; openh264 warns and turns them off otherwise.
        .adaptive_quantization(false)
        .background_detection(false)
        .intra_frame_period(IntraFramePeriod::from_num_frames(INTRA_PERIOD_FRAMES))
}

enum VideoEncoder {
    Software(Box<Encoder>),
    #[cfg(windows)]
    Hardware(mf::HardwareEncoder),
}

struct Encoded {
    data: Bytes,
    keyframe: bool,
    capture_time_us: u64,
}

impl VideoEncoder {
    fn software(bitrate_bps: u32, fps: u32) -> Result<Self> {
        let encoder = Encoder::with_api_config(OpenH264API::from_source(), encoder_config(bitrate_bps, fps))?;
        Ok(Self::Software(Box::new(encoder)))
    }

    fn new(source: &VideoSource, width: u32, height: u32, bitrate_bps: u32, fps: u32) -> Result<Self> {
        #[cfg(windows)]
        if *source == VideoSource::Screen {
            match mf::HardwareEncoder::new(width, height, bitrate_bps, fps) {
                Ok(encoder) => return Ok(Self::Hardware(encoder)),
                Err(error) => tracing::warn!("GPU encoder unavailable; using OpenH264: {error:#}"),
            }
        }
        #[cfg(not(windows))]
        let _ = (source, width, height);
        Self::software(bitrate_bps, fps)
    }

    fn name(&self) -> &str {
        match self {
            Self::Software(_) => "CPU: OpenH264",
            #[cfg(windows)]
            Self::Hardware(encoder) => encoder.name(),
        }
    }

    /// The picture layout this encoder takes.
    fn layout(&self) -> Layout {
        match self {
            Self::Software(_) => Layout::I420,
            #[cfg(windows)]
            Self::Hardware(_) => Layout::Nv12,
        }
    }

    fn set_bitrate(&mut self, from: u32, to: u32) {
        match self {
            Self::Software(encoder) => set_software_bitrate(encoder, from, to),
            #[cfg(windows)]
            Self::Hardware(encoder) => encoder.set_bitrate(to),
        }
    }

    fn encode(&mut self, picture: &Picture, capture_time_us: u64, force_keyframe: bool) -> Result<Vec<Encoded>> {
        match self {
            Self::Software(encoder) => {
                if force_keyframe {
                    encoder.force_intra_frame();
                }
                let encoded = encoder.encode_at(picture, openh264::Timestamp::from_millis(capture_time_us / 1000))?;
                if encoded.frame_type() == FrameType::Skip {
                    return Ok(Vec::new());
                }
                let data = encoded.to_vec();
                if data.is_empty() {
                    return Ok(Vec::new());
                }
                Ok(vec![Encoded {
                    data: data.into(),
                    keyframe: encoded.frame_type() == FrameType::IDR,
                    capture_time_us,
                }])
            }
            #[cfg(windows)]
            Self::Hardware(encoder) => Ok(encoder
                .encode(picture, capture_time_us, force_keyframe)?
                .into_iter()
                .map(|frame| Encoded {
                    data: frame.data.into(),
                    keyframe: frame.keyframe,
                    capture_time_us: frame.capture_time_us,
                })
                .collect()),
        }
    }
}

/// Changes OpenH264's target (and ceiling) bitrate in place, without a new keyframe.
fn set_software_bitrate(encoder: &mut Encoder, from: u32, to: u32) {
    use openh264_sys2::{ENCODER_OPTION_BITRATE, ENCODER_OPTION_MAX_BITRATE, SBitrateInfo, SPATIAL_LAYER_ALL};
    let mut info = SBitrateInfo { iLayer: SPATIAL_LAYER_ALL, iBitrate: to.min(i32::MAX as u32) as i32 };
    // The ceiling goes first when rising and last when falling, so it never sits below the target.
    let options = if to > from {
        [ENCODER_OPTION_MAX_BITRATE, ENCODER_OPTION_BITRATE]
    } else {
        [ENCODER_OPTION_BITRATE, ENCODER_OPTION_MAX_BITRATE]
    };
    for option in options {
        // SAFETY: the encoder is initialised (it has encoded a frame), and both options read an
        // SBitrateInfo that outlives the call.
        if unsafe { encoder.raw_api().set_option(option, (&raw mut info).cast()) } != 0 {
            tracing::debug!(bps = to, "OpenH264 refused a bitrate change");
        }
    }
}

fn run(source: &VideoSource, tx: &mpsc::Sender<Result<VideoEvent>>, control: &Control) -> Result<()> {
    let mut capturer = source.open()?;
    let fps = capturer.refresh_hz().unwrap_or(DEFAULT_FPS).clamp(30, 240);
    let mut encoder: Option<VideoEncoder> = None;
    let mut bitrate = control.bitrate.load(Ordering::Relaxed);
    // The last captured picture, kept so a keyframe can be sent while the screen is static, and
    // so a picture held back while the network catches up can be sent once it has.
    let mut picture: Option<Picture> = None;
    let mut unsent = false;
    let mut picture_time_us = 0;
    let mut sent_time_us = 0;
    let mut seq = 0u32;
    let clock_start = now_micros();
    let stream_start = Instant::now();
    let send = |event| tx.blocking_send(Ok(event)).is_ok();

    while !control.stop.load(Ordering::Relaxed) {
        let frame = capturer.next_frame(if unsent { HELD_POLL } else { POLL })?;
        let now_us = clock_start + stream_start.elapsed().as_micros() as u64;
        if let Some(frame) = frame {
            // 4:2:0 chroma needs even dimensions; drop an odd last row/column.
            let (w, h) = (frame.width & !1, frame.height & !1);
            ensure!(w > 0 && h > 0, "captured an empty image");
            if picture.as_ref().map(Picture::size) != Some((w as usize, h as usize)) {
                if !send(VideoEvent::Format { width: w, height: h }) {
                    return Ok(());
                }
                bitrate = control.bitrate.load(Ordering::Relaxed);
                let selected = VideoEncoder::new(source, w, h, bitrate, fps).context("starting H.264 encoder")?;
                if !send(VideoEvent::Encoder { name: selected.name().to_owned() }) {
                    return Ok(());
                }
                picture = Some(Picture::new(selected.layout(), w as usize, h as usize));
                encoder = Some(selected);
            }
            picture.as_mut().expect("set above").convert_from_bgra(frame.bgra, frame.stride);
            picture_time_us = now_us;
            unsent = true;
        }
        let Some(image) = &mut picture else { continue };
        let force_keyframe = control.keyframe.load(Ordering::Relaxed);
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

        let active = encoder.as_mut().expect("encoder initialized with the image");
        let wanted = control.bitrate.load(Ordering::Relaxed);
        if wanted != bitrate {
            active.set_bitrate(bitrate, wanted);
            bitrate = wanted;
        }
        let started = Instant::now();
        let encoded = match active.encode(image, capture_time_us, force_keyframe) {
            Ok(encoded) => encoded,
            Err(error) => {
                if matches!(active, VideoEncoder::Software(_)) {
                    return Err(error.context("encoding frame"));
                }
                tracing::warn!("GPU encoding failed; recovering on OpenH264: {error:#}");
                *active = VideoEncoder::software(bitrate, fps)?;
                if !send(VideoEvent::Encoder { name: active.name().to_owned() }) {
                    return Ok(());
                }
                *image = image.to_layout(active.layout());
                active.encode(image, capture_time_us, true)?
            }
        };
        let encode_time_us = started.elapsed().as_micros().min(u32::MAX as u128) as u32;
        for picture in encoded {
            let meta =
                FrameMeta { seq, keyframe: picture.keyframe, capture_time_us: picture.capture_time_us, encode_time_us };
            seq = seq.wrapping_add(1);
            if !send(VideoEvent::Frame(EncodedFrame { meta, data: picture.data })) {
                return Ok(());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use openh264::{decoder::Decoder, formats::YUVSource as _};

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
        let config = encoder_config(20_000_000, 60).num_threads(2);
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
        let mut encoder = Encoder::with_api_config(OpenH264API::from_source(), encoder_config(20_000_000, 60)).unwrap();
        let data = encoder.encode(&moving_image(16, 16, 0)).unwrap().to_vec();
        let mut decoder = Decoder::new().unwrap();
        assert_eq!(decoder.decode(&data).unwrap().unwrap().dimensions(), (16, 16));
    }

    /// The rate controller retunes the encoder mid-stream: no new keyframe, still decodable,
    /// and smaller frames right after the bitrate drops. (Without frame skipping OpenH264 can't
    /// hold a bitrate far below what the picture needs; the session's backlog check covers that.)
    #[test]
    fn bitrate_changes_in_place() {
        let mut encoder = VideoEncoder::software(20_000_000, 60).unwrap();
        let mut decoder = Decoder::new().unwrap();
        let mut sizes = Vec::new();
        for frame in 0..60 {
            if frame == 30 {
                encoder.set_bitrate(20_000_000, 300_000);
            }
            let encoded = encoder.encode(&smooth_image(frame), frame as u64 * 16_667, false).unwrap();
            let encoded = encoded.into_iter().next().expect("no frame skipped");
            assert_eq!(encoded.keyframe, frame == 0, "frame {frame}");
            assert!(decoder.decode(&encoded.data).unwrap().is_some());
            sizes.push(encoded.data.len());
        }
        let (before, after): (usize, usize) = (sizes[27..30].iter().sum(), sizes[30..33].iter().sum());
        assert!(after * 4 < before * 3, "{before} bytes before, {after} after");
    }
}
