//! The capture → encode thread behind a session.
//!
//! Encoders sit behind [`VideoEncoder`], one per codec and backend, so a session can change
//! codec (or fall back from the graphics card to the processor) without the rest knowing. On
//! Windows the picture can stay on the graphics card from capture to encoder: the captured image
//! is converted to NV12 there and handed to the hardware encoder as a texture. Where any part
//! of that isn't available, or fails, pictures go through memory as before.

mod hevc;
#[cfg(windows)]
pub(crate) mod mf;
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
    trace::{CaptureRecord, HostTrace, Replaced},
    yuv::{Layout, Picture},
};

/// Used when the capture source can't tell its refresh rate. Capture only yields a frame when
/// the screen changes, so the stream runs at whatever rate the host's monitor and content do.
const DEFAULT_FPS: u32 = 144;
/// OpenH264's own keyframe interval, in frames.
const INTRA_PERIOD_FRAMES: u32 = 600;
/// Without intra refresh, a keyframe at least this often, in case the picture went wrong in a
/// way the viewer couldn't notice. Not by frame count: at 240 fps, 600 frames was a keyframe
/// burst every 2.5 s. With intra refresh, keyframes only go out when the viewer asks.
const KEYFRAME_PERIOD: Duration = Duration::from_secs(30);
/// How long one capture wait may block, so a stop request is noticed promptly.
const POLL: Duration = Duration::from_millis(100);
/// Why a session can't start: the graphics card has no encoding session left for it.
pub(crate) const NO_ENCODER: &str = "no encoder available";
/// How soon to look again while a captured picture waits for the network to catch up.
const HELD_POLL: Duration = Duration::from_millis(2);

pub(crate) enum VideoEvent {
    /// The encoded size; always the first event, and sent again whenever the screen resizes.
    Format {
        width: u32,
        height: u32,
        fps: u32,
    },
    Frame(EncodedFrame, FrameStamp),
    Encoder {
        name: String,
        codec: Codec,
    },
}

/// Timings that go with a frame from the video thread to the session, for the trace; not sent.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FrameStamp {
    /// Which capture the picture came from.
    pub capture: u64,
    /// A static picture encoded again as a keyframe, rather than a new one.
    pub resent: bool,
    /// From the picture being staged to the encoder starting on it.
    pub held_us: u32,
    /// Of the encode, the wait for the encoder to take the picture.
    pub input_wait_us: u32,
    /// When the encoder handed the frame over.
    pub encoded: Instant,
}

/// True while the network is too backed up to take another frame.
pub(crate) type Backlogged = Box<dyn Fn() -> bool + Send + Sync>;

/// What the session tells the video thread.
struct Control {
    keyframe: AtomicBool,
    bitrate: AtomicU32,
    /// The most the session may ask for, which the encoder is opened for.
    peak: u32,
    /// Whether a graphics card is doing the encoding.
    hardware: AtomicBool,
    codec: Mutex<Codec>,
    stop: AtomicBool,
    frame_rate_limit: u32,
    backlogged: Backlogged,
}

/// One session's video, from [`VideoHub`]. It stops when this is dropped.
pub(crate) struct VideoStream {
    events: mpsc::Receiver<Result<VideoEvent>>,
    control: Arc<Control>,
    busy: Arc<AtomicU32>,
}

impl VideoStream {
    /// Joins `hub`, starting at `bitrate_bps` and never asking for more than `peak_bps`.
    pub fn start(
        hub: &VideoHub,
        codec: Codec,
        bitrate_bps: u32,
        peak_bps: u32,
        frame_rate_limit: u32,
        backlogged: Backlogged,
        trace: Arc<HostTrace>,
    ) -> Result<Self> {
        // Small, so a slow network pushes back on the encoder instead of queueing stale frames.
        let (tx, events) = mpsc::channel(4);
        let control = Arc::new(Control {
            keyframe: AtomicBool::new(false),
            bitrate: AtomicU32::new(bitrate_bps),
            peak: peak_bps.max(bitrate_bps),
            hardware: AtomicBool::new(false),
            codec: Mutex::new(codec),
            stop: AtomicBool::new(false),
            frame_rate_limit,
            backlogged,
        });
        hub.join(Joiner { tx, control: control.clone(), trace })?;
        Ok(Self { events, control, busy: hub.busy.clone() })
    }

    /// The next event; `None` once the thread has stopped.
    pub async fn next(&mut self) -> Option<Result<VideoEvent>> {
        self.events.recv().await
    }

    /// Makes the next encoded frame a keyframe, even if the screen hasn't changed.
    pub fn request_keyframe(&self) {
        self.control.keyframe.store(true, Ordering::Relaxed);
    }

    /// How hard the encoding is working, for the rate controller.
    pub fn encoder_load(&self) -> crate::rate::EncoderLoad {
        crate::rate::EncoderLoad {
            busy: f64::from(self.busy.load(Ordering::Relaxed)) / 1000.0,
            hardware: self.control.hardware.load(Ordering::Relaxed),
        }
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

/// Whose encoder it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderKind {
    Nvenc,
    Amf,
    QuickSync,
    OtherHardware,
    Software,
}

impl EncoderKind {
    /// From a driver's name for its encoder.
    #[cfg_attr(not(windows), allow(dead_code))]
    fn of(name: &str) -> Self {
        let name = name.to_lowercase();
        if name.contains("nvidia") {
            Self::Nvenc
        } else if name.contains("amd") {
            Self::Amf
        } else if name.contains("intel") || name.contains("quick sync") {
            Self::QuickSync
        } else {
            Self::OtherHardware
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Nvenc => "NVIDIA NVENC",
            Self::Amf => "AMD AMF",
            Self::QuickSync => "Intel Quick Sync",
            Self::OtherHardware => "Graphics card",
            Self::Software => "Processor",
        }
    }
}

/// An encoder on this PC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderInfo {
    pub kind: EncoderKind,
    pub codec: Codec,
    pub name: String,
    /// Whether Connect can use it: AV1 encoders are listed, not driven yet.
    pub usable: bool,
}

/// What can encode on this PC, separately from anything about the network.
#[cfg_attr(not(windows), allow(clippy::vec_init_then_push))]
pub fn encoder_capabilities() -> Vec<EncoderInfo> {
    let mut list = Vec::new();
    #[cfg(windows)]
    for codec in [Codec::H265, Codec::H264, Codec::Av1] {
        for name in mf::encoder_names(codec).unwrap_or_default() {
            list.push(EncoderInfo { kind: EncoderKind::of(&name), codec, name, usable: codec != Codec::Av1 });
        }
    }
    list.push(EncoderInfo { kind: EncoderKind::Software, codec: Codec::H264, name: "OpenH264".into(), usable: true });
    list
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

    /// How long the last frame waited for the encoder to take it, where that's known.
    fn input_wait_us(&self) -> u32 {
        0
    }

    /// Whether the encoder renews the picture band by band (intra refresh), so it never needs a
    /// keyframe unless the viewer asks for one.
    fn intra_refresh(&self) -> bool {
        false
    }

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
        // Only one way in off Windows.
        #[cfg_attr(not(windows), expect(clippy::infallible_destructuring_match))]
        let picture = match self {
            Stage::Memory(picture) => picture,
            #[cfg(windows)]
            Stage::Gpu(mut converter) => converter.read_back().context("reading the picture back from the GPU")?,
        };
        Ok(if picture.layout() == layout { picture } else { picture.to_layout(layout) })
    }
}

/// Opens `codec` with `open`, or H.264 if that fails: an encoder listed for a codec can still
/// refuse the settings a stream needs, and every hardware encoder does H.264.
#[cfg(windows)]
fn or_h264<T>(codec: Codec, mut open: impl FnMut(Codec) -> Result<T>) -> Result<T> {
    open(codec).or_else(|error| {
        if codec == Codec::H264 {
            return Err(error);
        }
        tracing::warn!("GPU {codec} encoder unavailable, trying H.264: {error:#}");
        open(Codec::H264)
    })
}

/// Opens the best encoder for `codec` that reads from memory: the graphics card's, then its
/// H.264 one, then OpenH264.
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
        match or_h264(codec, |codec| mf::HardwareEncoder::new(codec, width, height, bitrate, fps, None)) {
            Ok(encoder) => return Ok(Box::new(encoder)),
            Err(error) => tracing::warn!("GPU encoder unavailable, encoding on the processor: {error:#}"),
        }
    }
    #[cfg(not(windows))]
    let _ = (source, codec, width, height);
    open_software_encoder(Codec::H264, bitrate, fps)
}

fn open_software_encoder(codec: Codec, bitrate: u32, fps: u32) -> Result<Box<dyn VideoEncoder>> {
    match codec {
        Codec::H264 => Ok(Box::new(software::OpenH264::new(bitrate, fps)?)),
        Codec::H265 | Codec::Av1 => bail!("no software {codec} encoder"),
    }
}

/// Whether to try keeping pictures on the graphics card. On by default for the real screen;
/// `CONNECT_GPU_PIPELINE=0` turns it off, e.g. to compare.
fn gpu_pipeline_wanted(source: &VideoSource) -> bool {
    cfg!(windows)
        && *source == VideoSource::Screen
        && std::env::var("CONNECT_GPU_PIPELINE").map_or(true, |value| value != "0")
}

fn micros(duration: Duration) -> u32 {
    duration.as_micros().min(u32::MAX as u128) as u32
}

/// Hardware encoders budget a frame as the bitrate over the frame rate they were told, which is
/// the screen's: a game drawing fewer frames sends that much less. 80 Mb/s came to 33 at 103 fps
/// on a 240 Hz screen. So the encoder is told more, by what's missing of the frame rate.
struct FrameRateScale {
    since: Instant,
    frames: u32,
    scale: f64,
}

/// How often the frame rate is measured.
const FRAME_RATE_PERIOD: Duration = Duration::from_millis(500);
/// Fewer frames than this a second is a still screen, not the game's frame rate.
const LEAST_MEASURED_FPS: f64 = 30.0;
/// At most this much over the bitrate: enough for 80 fps on a 240 Hz screen.
const MOST_SCALE: f64 = 3.0;
/// A smaller change isn't worth telling the encoder about.
const SCALE_STEP: f64 = 0.05;

impl FrameRateScale {
    fn new() -> Self {
        Self { since: Instant::now(), frames: 0, scale: 1.0 }
    }

    /// Counts a frame encoded at `now`, for an encoder told `fps`.
    fn frame(&mut self, now: Instant, fps: u32) {
        self.frames += 1;
        let elapsed = now.saturating_duration_since(self.since);
        if elapsed < FRAME_RATE_PERIOD {
            return;
        }
        let measured = f64::from(self.frames) / elapsed.as_secs_f64();
        (self.since, self.frames) = (now, 0);
        if measured >= LEAST_MEASURED_FPS {
            let scale = (f64::from(fps) / measured).clamp(1.0, MOST_SCALE);
            if (scale - self.scale).abs() >= self.scale * SCALE_STEP {
                self.scale = scale;
            }
        }
    }

    /// What to tell an encoder to get `bitrate`.
    fn apply(&self, bitrate: u32) -> u32 {
        (f64::from(bitrate) * self.scale).min(f64::from(u32::MAX)) as u32
    }
}

/// The pictures a second to aim for: the screen's refresh, or the limit if there's one below it.
fn capped_fps(refresh: u32, limit: u32) -> u32 {
    if limit == 0 { refresh } else { refresh.min(limit) }
}

/// One capture for everyone connected, as Windows lets a process duplicate a screen only once.
/// Each session has its own encoder and bitrate on the one video thread, working through the
/// picture in turn: a graphics card has one encoding engine to share anyway, and the converted
/// picture stays where every encoder can read it.
pub(crate) struct VideoHub {
    source: VideoSource,
    state: Arc<Mutex<HubState>>,
    /// Share of the time spent encoding, in thousandths.
    busy: Arc<AtomicU32>,
}

#[derive(Default)]
struct HubState {
    joining: Vec<Joiner>,
    running: bool,
}

struct Joiner {
    tx: mpsc::Sender<Result<VideoEvent>>,
    control: Arc<Control>,
    trace: Arc<HostTrace>,
}

impl VideoHub {
    pub fn new(source: VideoSource) -> Self {
        Self { source, state: Arc::default(), busy: Arc::default() }
    }

    fn join(&self, joiner: Joiner) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.joining.push(joiner);
        if state.running {
            return Ok(());
        }
        let (source, shared, busy) = (self.source.clone(), self.state.clone(), self.busy.clone());
        std::thread::Builder::new()
            .name("connect-video".into())
            .spawn(move || run_until_empty(&source, &shared, &busy))
            .context("starting video thread")?;
        state.running = true;
        Ok(())
    }
}

/// One session's side of the video thread.
struct Output {
    tx: mpsc::Sender<Result<VideoEvent>>,
    control: Arc<Control>,
    trace: Arc<HostTrace>,
    encoder: Option<Box<dyn VideoEncoder>>,
    /// Encode on the processor from now on: the graphics card failed this session.
    software: bool,
    /// Codecs that failed here; the stream stays off them even if the session asks again.
    failed: Vec<Codec>,
    /// What the encoder was last told, and what makes that come to the target.
    bitrate: u32,
    frame_rate: FrameRateScale,
    fps: u32,
    frame_interval: Duration,
    /// The size the session was last told.
    announced: Option<(u32, u32)>,
    /// Whether the staged picture is newer than the last one sent. The last picture is kept so a
    /// keyframe can be sent while the screen is static, and so a picture held back while the
    /// network catches up can be sent once it has.
    unsent: bool,
    /// Why the staged picture hasn't gone yet, for when a newer one replaces it.
    waiting: Replaced,
    sent_time_us: u64,
    last_sent: Option<Instant>,
    last_keyframe: Instant,
    seq: u32,
    /// The session has gone, or its encoder can't be opened.
    gone: bool,
}

impl Output {
    fn new(joiner: Joiner, refresh: u32) -> Self {
        let fps = capped_fps(refresh, joiner.control.frame_rate_limit);
        Self {
            bitrate: joiner.control.peak,
            tx: joiner.tx,
            control: joiner.control,
            trace: joiner.trace,
            encoder: None,
            software: false,
            failed: Vec::new(),
            frame_rate: FrameRateScale::new(),
            fps,
            // At most one picture per refresh of the host's screen or the limit (with a little
            // slack for jitter): a game presenting faster would spend bitrate on pictures nobody
            // sees.
            frame_interval: Duration::from_secs(1) * 19 / (20 * fps),
            announced: None,
            unsent: true,
            waiting: Replaced::No,
            sent_time_us: 0,
            last_sent: None,
            last_keyframe: Instant::now(),
            seq: 0,
            gone: false,
        }
    }

    fn send(&mut self, event: VideoEvent) {
        match self.tx.try_send(Ok(event)) {
            Ok(()) => {}
            // No room is a frame lost to a session that isn't keeping up, never a wait: the
            // others are depending on this thread.
            Err(mpsc::error::TrySendError::Full(_)) => self.control.keyframe.store(true, Ordering::Relaxed),
            Err(mpsc::error::TrySendError::Closed(_)) => self.gone = true,
        }
    }

    fn fail(&mut self, error: anyhow::Error) {
        let _ = self.tx.try_send(Err(error));
        self.gone = true;
    }

    /// How soon this session's next picture may go, given whether one is waiting.
    fn wait(&self) -> Duration {
        let slot = self.last_sent.map_or(Duration::ZERO, |sent| self.frame_interval.saturating_sub(sent.elapsed()));
        match (self.unsent, slot.is_zero()) {
            (false, _) => POLL,
            (true, true) => HELD_POLL,
            (true, false) => slot.min(HELD_POLL),
        }
    }

    fn wanted_codec(&self) -> Codec {
        let codec = *self.control.codec.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.failed.contains(&codec) { Codec::H264 } else { codec }
    }
}

/// The picture being sent.
#[derive(Clone, Copy)]
struct Staged {
    index: u64,
    at: Instant,
    time_us: u64,
    /// How long it took to capture, and how long it was on screen before that.
    capture_us: u32,
    present_us: u32,
}

/// Runs the video thread until nobody's left, and starts over for anyone who joined as it ended.
fn run_until_empty(source: &VideoSource, state: &Mutex<HubState>, busy: &AtomicU32) {
    let lock = || state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    loop {
        let mut outputs = Vec::new();
        if let Err(error) = run(source, state, busy, &mut outputs) {
            for output in &mut outputs {
                output.fail(anyhow::anyhow!("{error:#}"));
            }
            for joiner in lock().joining.drain(..) {
                let _ = joiner.tx.try_send(Err(anyhow::anyhow!("{error:#}")));
            }
        }
        let mut state = lock();
        if state.joining.is_empty() {
            state.running = false;
            return;
        }
    }
}

fn run(source: &VideoSource, state: &Mutex<HubState>, busy: &AtomicU32, outputs: &mut Vec<Output>) -> Result<()> {
    let mut capturer = source.open()?;
    let refresh = capturer.refresh_hz().unwrap_or(DEFAULT_FPS).clamp(30, 240);
    // Turned off for the rest of the stream once anything on the graphics-card path fails.
    let mut gpu = gpu_pipeline_wanted(source);
    let mut stage: Option<Stage> = None;
    // What a picture in memory is laid out as: what the encoders read.
    let mut layout = Layout::I420;
    let mut captures = 0u64;
    let mut picture: Option<Staged> = None;
    let clock_start = now_micros();
    let stream_start = Instant::now();
    let (mut busy_since, mut busy_time) = (Instant::now(), Duration::ZERO);

    loop {
        {
            let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            outputs.extend(state.joining.drain(..).map(|joiner| Output::new(joiner, refresh)));
        }
        outputs.retain(|output| !output.gone && !output.control.stop.load(Ordering::Relaxed));
        if outputs.is_empty() {
            return Ok(());
        }
        let timeout = outputs.iter().map(Output::wait).min().unwrap_or(POLL);
        let mut resized = false;
        let fresh = capture(&mut *capturer, &mut stage, &mut gpu, layout, timeout, captures, &mut resized)?;
        if resized {
            for output in outputs.iter_mut() {
                output.encoder = None;
            }
        }
        let staged = Instant::now();
        #[cfg(windows)]
        if let Some(Stage::Gpu(converter)) = &mut stage {
            converter.poll_timings(|capture, gpu_us| {
                for output in outputs.iter() {
                    output.trace.conversions.push(crate::trace::ConvertRecord { capture, gpu_us });
                }
            });
        }
        if fresh {
            let timing = capturer.timing();
            let staged_picture = Staged {
                index: captures,
                at: staged,
                time_us: clock_start + stream_start.elapsed().as_micros() as u64,
                capture_us: micros(staged.saturating_duration_since(timing.acquired)),
                present_us: timing.present_to_capture.map_or(0, micros),
            };
            picture = Some(staged_picture);
            captures += 1;
            for output in outputs.iter_mut() {
                output.trace.captures.push(CaptureRecord {
                    capture: staged_picture.index,
                    time_us: staged_picture.time_us,
                    capture_us: staged_picture.capture_us,
                    present_to_capture_us: staged_picture.present_us,
                    accumulated: timing.accumulated,
                    replaced: if output.unsent { output.waiting } else { Replaced::No },
                });
                output.unsent = true;
            }
            // Nobody stays last in line.
            outputs.rotate_left(1);
        }
        let (Some(picture), true) = (picture, stage.is_some()) else { continue };
        for index in 0..outputs.len() {
            let others_running = outputs.iter().enumerate().any(|(i, o)| i != index && o.encoder.is_some());
            let Some(current) = stage.as_mut() else { break };
            let step = serve(&mut outputs[index], current, source, picture, others_running, &mut layout);
            busy_time += step.encoding;
            if step.leave_gpu {
                // Everyone carries on from memory.
                gpu = false;
                let memory = stage.take().context("no picture staged")?.into_memory(Layout::I420)?;
                layout = Layout::I420;
                stage = Some(Stage::Memory(memory));
                for output in outputs.iter_mut() {
                    output.encoder = None;
                }
                break;
            }
        }
        let since = busy_since.elapsed();
        if since >= FRAME_RATE_PERIOD {
            let share = busy_time.as_secs_f64() / since.as_secs_f64();
            busy.store((share * 1000.0).min(1000.0) as u32, Ordering::Relaxed);
            (busy_since, busy_time) = (Instant::now(), Duration::ZERO);
        }
    }
}

#[derive(Default)]
struct Step {
    encoding: Duration,
    /// The graphics-card path can't go on, for anyone.
    leave_gpu: bool,
}

/// Opens this session's encoder if it needs one.
fn prepare(output: &mut Output, stage: &Stage, source: &VideoSource, others_running: bool) -> Step {
    let (width, height) = stage.size();
    let wanted = output.wanted_codec();
    if output.encoder.as_ref().is_some_and(|encoder| encoder.codec() == wanted) {
        return Step::default();
    }
    if output.encoder.as_ref().is_some_and(|encoder| encoder.codec() == Codec::H264 && !encoder.hardware()) {
        return Step::default(); // nothing faster to go back to
    }
    let peak = output.control.peak;
    let opened = if output.software {
        open_software_encoder(Codec::H264, peak, output.fps)
    } else {
        open_encoder(stage, source, wanted, width, height, peak, output.fps)
    };
    match opened {
        Ok(encoder) => {
            if encoder.codec() != wanted {
                output.failed.push(wanted);
            }
            output.control.hardware.store(encoder.hardware(), Ordering::Relaxed);
            output.bitrate = peak;
            let (name, codec) = (encoder.name().to_owned(), encoder.codec());
            output.encoder = Some(encoder);
            output.control.keyframe.store(true, Ordering::Relaxed);
            if output.announced != Some((width, height)) {
                output.announced = Some((width, height));
                output.send(VideoEvent::Format { width, height, fps: output.fps });
            }
            output.send(VideoEvent::Encoder { name, codec });
            Step::default()
        }
        // With others encoding, the graphics card is fine and this is one encoder too many.
        Err(error) if !others_running && !matches!(stage, Stage::Memory(_)) => {
            tracing::warn!("GPU-only pipeline unavailable, going through memory: {error:#}");
            Step { leave_gpu: true, ..Step::default() }
        }
        Err(error) => {
            output.fail(error.context(NO_ENCODER));
            Step::default()
        }
    }
}

/// Gives this session the staged picture, if it's due one.
fn serve(
    output: &mut Output,
    stage: &mut Stage,
    source: &VideoSource,
    picture: Staged,
    others_running: bool,
    layout: &mut Layout,
) -> Step {
    let step = prepare(output, stage, source, others_running);
    if step.leave_gpu || output.gone {
        return step;
    }
    let Some(encoder) = &mut output.encoder else { return step };
    // The first encoder through decides how pictures in memory are laid out.
    if let (Stage::Memory(staged), InputKind::Picture(wanted), false) = (&mut *stage, encoder.input(), others_running)
        && staged.layout() != wanted
    {
        *staged = staged.to_layout(wanted);
        *layout = wanted;
    }

    let mut force_keyframe = output.control.keyframe.load(Ordering::Relaxed);
    // Hold the picture while the network catches up: sent now, it would only queue behind the
    // rest. Likewise until its turn comes, a newer picture replacing it meanwhile. A requested
    // keyframe goes regardless, as the viewer is stuck without one.
    let too_soon = output.last_sent.is_some_and(|sent| sent.elapsed() < output.frame_interval);
    let backlogged = (output.unsent || force_keyframe) && (output.control.backlogged)();
    let no_room = output.tx.capacity() == 0;
    if no_room || !(output.unsent || force_keyframe) || ((backlogged || too_soon) && !force_keyframe) {
        output.waiting = if backlogged { Replaced::Backlog } else { Replaced::Turn };
        return step;
    }
    output.last_sent = Some(Instant::now());
    output.control.keyframe.store(false, Ordering::Relaxed);
    let resent = !output.unsent;
    output.unsent = false;
    // Stamped when captured; a static picture sent again as a keyframe is stamped anew, as
    // hardware encoders need every timestamp later than the last.
    let capture_time_us = picture.time_us.max(output.sent_time_us + 1);
    output.sent_time_us = capture_time_us;

    let wanted = output.control.bitrate.load(Ordering::Relaxed);
    let wanted = if encoder.hardware() { output.frame_rate.apply(wanted) } else { wanted };
    if wanted != output.bitrate {
        encoder.set_bitrate(output.bitrate, wanted);
        output.bitrate = wanted;
    }
    // The periodic keyframe never jumps the queue as a requested one does.
    let periodic = !encoder.intra_refresh() && output.last_keyframe.elapsed() >= KEYFRAME_PERIOD;
    force_keyframe |= periodic;
    let converted = match (&*stage, encoder.input()) {
        (Stage::Memory(staged), InputKind::Picture(wanted)) if staged.layout() != wanted => {
            Some(staged.to_layout(wanted))
        }
        _ => None,
    };
    let input = converted.as_ref().map_or_else(|| stage.input(), Input::Picture);
    let started = Instant::now();
    let result = encoder.encode(input, capture_time_us, force_keyframe);
    let encoded_at = Instant::now();
    let encoding = encoded_at.duration_since(started);
    let encoded = match result {
        Ok(encoded) => encoded,
        Err(error) if encoder.codec() != Codec::H264 => {
            // A codec only the graphics card does: H.264 instead, on the graphics card if it
            // will, where the picture already is.
            tracing::warn!("{} failed; switching to H.264: {error:#}", encoder.name());
            let codec = encoder.codec();
            output.failed.push(codec);
            output.encoder = None;
            output.unsent = true;
            return Step { encoding, ..step };
        }
        Err(error) if encoder.hardware() || !matches!(stage, Stage::Memory(_)) => {
            // Carry on on the processor: OpenH264, reading the picture from memory.
            tracing::warn!("{} failed; recovering on the processor: {error:#}", encoder.name());
            output.software = true;
            output.encoder = None;
            output.unsent = true;
            return Step { encoding, leave_gpu: !matches!(stage, Stage::Memory(_)) };
        }
        Err(error) => {
            output.fail(error.context("encoding frame"));
            return Step { encoding, ..step };
        }
    };
    output.frame_rate.frame(encoded_at, output.fps);
    if encoded.iter().any(|picture| picture.keyframe) {
        output.last_keyframe = encoded_at;
    }
    let encode_time_us = micros(encoding);
    let stamp = FrameStamp {
        capture: picture.index,
        resent,
        input_wait_us: encoder.input_wait_us(),
        held_us: if resent { 0 } else { micros(started.saturating_duration_since(picture.at)) },
        encoded: encoded_at,
    };
    let codec = encoder.codec();
    for encoded_picture in encoded {
        let meta = FrameMeta {
            seq: output.seq,
            keyframe: encoded_picture.keyframe,
            codec,
            capture_time_us: encoded_picture.capture_time_us,
            encode_time_us,
            present_to_capture_us: picture.present_us,
            capture_us: picture.capture_us,
            // Filled in as the frame goes out.
            sent_after_us: 0,
        };
        output.seq = output.seq.wrapping_add(1);
        output.send(VideoEvent::Frame(EncodedFrame { meta, data: encoded_picture.data, arrival: None }, stamp));
    }
    Step { encoding, ..step }
}

/// An encoder for `codec` that reads the staged picture, wherever it is.
fn open_encoder(
    stage: &Stage,
    source: &VideoSource,
    codec: Codec,
    width: u32,
    height: u32,
    bitrate: u32,
    fps: u32,
) -> Result<Box<dyn VideoEncoder>> {
    #[cfg(windows)]
    if let Stage::Gpu(converter) = stage {
        let encoder = or_h264(codec, |codec| {
            mf::HardwareEncoder::new(codec, width, height, bitrate, fps, Some(converter.device()))
        })
        .context("GPU encoder taking textures")?;
        return Ok(Box::new(encoder));
    }
    #[cfg(not(windows))]
    let _ = stage;
    open_memory_encoder(source, codec, width, height, bitrate, fps)
}

/// Waits up to `timeout` for a new screen image and stages it, replacing the stage (and setting
/// `resized`) when the size changes. Whether a new picture was staged.
fn capture(
    capturer: &mut dyn Capturer,
    stage: &mut Option<Stage>,
    gpu: &mut bool,
    layout: Layout,
    timeout: Duration,
    capture_index: u64,
    resized: &mut bool,
) -> Result<bool> {
    // The screen in memory: from the capture itself, or, after the GPU path just failed, read
    // back from the image it captured.
    #[cfg_attr(not(windows), expect(unused_mut))]
    let mut read_back_latest = false;
    #[cfg(not(windows))]
    let _ = (&gpu, capture_index);

    #[cfg(windows)]
    if *gpu && let Some(dxgi) = capturer.as_dxgi() {
        let Some((w, h)) = dxgi.next_texture(timeout)? else { return Ok(false) };
        // 4:2:0 chroma needs even dimensions; drop an odd last row/column.
        let (w, h) = (w & !1, h & !1);
        ensure!(w > 0 && h > 0, "captured an empty image");
        let current = stage.as_ref().filter(|stage| matches!(stage, Stage::Gpu(_))).map(Stage::size);
        if current != Some((w, h)) {
            match crate::gpu::Nv12Converter::new(dxgi.device(), w, h).context("GPU colour conversion") {
                Ok(converter) => {
                    tracing::info!(method = %converter.method(), "GPU colour conversion");
                    tracing::info!("keeping pictures on the GPU from capture to encoder");
                    *stage = Some(Stage::Gpu(converter));
                    *resized = true;
                }
                Err(error) => {
                    tracing::warn!("GPU-only pipeline unavailable, going through memory: {error:#}");
                    *gpu = false;
                    read_back_latest = true;
                }
            }
        }
        if *gpu {
            let Some(Stage::Gpu(converter)) = stage else { unreachable!("built above") };
            match converter.convert(dxgi.latest(), Some(capture_index)) {
                Ok(()) => return Ok(true),
                Err(error) => {
                    tracing::warn!("GPU colour conversion failed, going through memory: {error:#}");
                    *gpu = false;
                    *stage = None;
                    *resized = true;
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
    let Some(frame) = frame else { return Ok(false) };
    let (w, h) = (frame.width & !1, frame.height & !1);
    ensure!(w > 0 && h > 0, "captured an empty image");
    let current = stage.as_ref().filter(|stage| matches!(stage, Stage::Memory(_))).map(Stage::size);
    if current != Some((w, h)) {
        *stage = Some(Stage::Memory(Picture::new(layout, w as usize, h as usize)));
        *resized = true;
    }
    let Some(Stage::Memory(picture)) = stage else { unreachable!("built above") };
    picture.convert_from_bgra(frame.bgra, frame.stride);
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoders_are_told_apart_by_name() {
        assert_eq!(EncoderKind::of("NVIDIA HEVC Encoder MFT"), EncoderKind::Nvenc);
        assert_eq!(EncoderKind::of("AMDh264Encoder"), EncoderKind::Amf);
        assert_eq!(EncoderKind::of("Intel Quick Sync Video H.264 Encoder MFT"), EncoderKind::QuickSync);
        assert_eq!(EncoderKind::of("Something New"), EncoderKind::OtherHardware);
    }

    #[test]
    fn a_frame_rate_under_the_screens_is_made_up_for() {
        let mut scale = FrameRateScale::new();
        let start = scale.since;
        let at = |ms: u64| start + Duration::from_millis(ms);
        // 103 fps on a 240 Hz screen: 80 Mb/s has to be asked for as 186 to come to 80.
        for frame in 1..=52u64 {
            scale.frame(at(frame * 1000 / 103), 240);
        }
        assert_eq!(scale.apply(80_000_000) / 1_000_000, 186);
        // A small wander isn't passed on.
        (scale.since, scale.frames) = (at(1000), 0);
        for frame in 1..=50u64 {
            scale.frame(at(1000 + frame * 1000 / 100), 240);
        }
        assert_eq!(scale.apply(80_000_000) / 1_000_000, 186);
    }

    #[test]
    fn a_still_screen_says_nothing_and_the_scale_has_a_limit() {
        let mut scale = FrameRateScale::new();
        let start = scale.since;
        scale.frame(start + Duration::from_secs(5), 240);
        assert_eq!(scale.apply(80_000_000), 80_000_000);
        (scale.since, scale.frames) = (start, 0);
        for frame in 1..=20u64 {
            scale.frame(start + Duration::from_millis(frame * 25), 240);
        }
        assert_eq!(scale.apply(80_000_000), 240_000_000);
        (scale.since, scale.frames) = (start, 0);
        for frame in 1..=200u64 {
            scale.frame(start + Duration::from_micros(frame * 2500), 240);
        }
        assert_eq!(scale.apply(80_000_000), 80_000_000);
    }

    #[test]
    fn a_limit_only_ever_lowers_the_frame_rate() {
        assert_eq!(capped_fps(240, 0), 240);
        assert_eq!(capped_fps(240, 120), 120);
        assert_eq!(capped_fps(60, 120), 60);
    }
    use openh264::{OpenH264API, decoder::Decoder, encoder::Encoder, formats::YUVSource as _};

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
        let mut encoder =
            Encoder::with_api_config(OpenH264API::from_source(), software::config(20_000_000, 60)).unwrap();
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
