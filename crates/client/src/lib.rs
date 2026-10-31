//! The connecting side of a session.
//!
//! [`connect`] dials a host by User-ID, waits while the host user is asked to allow it, then
//! receives and decodes the host's screen and sends input with [`ClientHandle::send_input`].
//! Progress arrives as [`ClientEvent`]s; the newest decoded frame and live [`Stats`] are read
//! from the [`ClientHandle`].

mod audio;
mod decoder;
pub mod diagnostics;
mod gpu_decode;
mod latency;
mod pacing;
mod video;

pub use connect_gpu_load::GpuLoad;
pub use decoder::{GpuDecodeSupport, gpu_decode_support};
pub use gpu_decode::SharedGpu;
pub use latency::{FrameTiming, Latency};

use std::{
    fmt,
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Result, bail};
use connect_net::{self as net, PathKind};
use connect_proto::{
    CLOSE_NORMAL, CLOSE_REJECTED, Codec, DeviceClass, PROTOCOL_VERSION, Permissions,
    control::{ClientMsg, HostMsg},
    cursor::{CursorPosition, CursorShape},
    input::InputEvent,
    now_micros,
    video::FrameMeta,
};
use iroh::{
    Endpoint, EndpointAddr,
    endpoint::{Connection, ConnectionError, RecvStream, SendStream},
};
use tokio::sync::{Notify, mpsc};
use tracing::debug;

/// Whether new sessions try to decode video on the graphics card; see [`set_hardware_decoding`].
static HARDWARE_DECODING: AtomicBool = AtomicBool::new(true);

/// Turns decoding on the graphics card on or off for sessions started from now on. On by
/// default; sessions fall back to the processor anyway where the graphics card can't do it.
pub fn set_hardware_decoding(enabled: bool) {
    HARDWARE_DECODING.store(enabled, Ordering::Relaxed);
}

/// Whether H.265 may be offered to hosts; see [`set_allow_h265`].
static ALLOW_H265: AtomicBool = AtomicBool::new(true);

/// Offers H.265 to hosts where the graphics card decodes it (the default), or (`false`) keeps
/// sessions on H.264. Applies from the next connection.
pub fn set_allow_h265(allow: bool) {
    ALLOW_H265.store(allow, Ordering::Relaxed);
}

/// Whether frames are shown evenly paced rather than as soon as they're decoded; see
/// [`set_smooth_pacing`].
static SMOOTH_PACING: AtomicBool = AtomicBool::new(false);

/// Shows frames a steady delay after capture, smoothing out uneven network and decode times at
/// the cost of that delay, or (`false`, the default) each one as soon as it's decoded. Takes
/// effect at once.
pub fn set_smooth_pacing(smooth: bool) {
    SMOOTH_PACING.store(smooth, Ordering::Relaxed);
}

const PING_INTERVAL: Duration = Duration::from_secs(1);
/// Lower bound between keyframe requests; each one costs the host a large frame. Never less
/// than a round trip, so a second request doesn't go out while the first keyframe is on its way.
const KEYFRAME_REQUEST_INTERVAL: Duration = Duration::from_millis(80);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Looking up the User-ID and reaching the host.
    Connecting,
    /// Reached the host; its user is being asked to allow the connection.
    WaitingForApproval,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ClientEvent {
    Stage(Stage),
    Accepted {
        host_name: String,
        width: u32,
        height: u32,
        permissions: Permissions,
    },
    PermissionsChanged(Permissions),
    VideoFormat {
        width: u32,
        height: u32,
    },
    /// The host can't use this kind of input even though it's allowed; the text says why.
    InputUnavailable {
        device: DeviceClass,
        reason: String,
    },
    VideoEncoder {
        name: String,
        codec: Codec,
    },
    AudioStatus {
        active: bool,
        reason: Option<String>,
    },
    /// Always the last event.
    Ended(EndReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndReason {
    /// The host turned the connection down; the text says why.
    Rejected(String),
    /// Couldn't reach the host, or the connection broke during setup.
    Failed(String),
    /// The host ended the session.
    HostEnded(String),
    /// [`ClientHandle::disconnect`] was called.
    Disconnected,
    /// The network path to the host died.
    ConnectionLost,
}

impl fmt::Display for EndReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EndReason::Rejected(reason) | EndReason::HostEnded(reason) => f.write_str(reason),
            EndReason::Failed(error) => write!(f, "Couldn't connect: {error}"),
            EndReason::Disconnected => f.write_str("Disconnected."),
            EndReason::ConnectionLost => f.write_str("Lost the connection to the host."),
        }
    }
}

/// One decoded picture in I420: the full-size Y plane, then the quarter-size U and V planes, each
/// tightly packed. Give it back with [`ClientHandle::recycle`] once shown, to save allocating
/// the next one.
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    /// The picture, unless it's in `texture`.
    pub yuv: Vec<u8>,
    /// The picture as an NV12 texture, when hardware decoding shares the window's graphics card
    /// ([`SharedGpu`]); `yuv` is then empty.
    pub texture: Option<wgpu::Texture>,
    pub meta: FrameMeta,
    /// Where the frame's time went; call [`FrameTiming::mark_painted`] when it's drawn.
    pub timing: FrameTiming,
}

impl VideoFrame {
    /// Width and height of the U and V planes.
    pub fn chroma_size(&self) -> (usize, usize) {
        ((self.width as usize).div_ceil(2), (self.height as usize).div_ceil(2))
    }

    pub fn planes(&self) -> (&[u8], &[u8], &[u8]) {
        let luma = self.width as usize * self.height as usize;
        let (cw, ch) = self.chroma_size();
        let (y, chroma) = self.yuv.split_at(luma);
        let (u, v) = chroma.split_at(cw * ch);
        (y, u, v)
    }

    /// The picture as RGBA, for when the GPU can't convert it: BT.601 limited range, like the
    /// host's encoder.
    pub fn write_rgba(&self, rgba: &mut Vec<u8>) {
        let (w, h) = (self.width as usize, self.height as usize);
        let (cw, _) = self.chroma_size();
        let (y, u, v) = self.planes();
        rgba.resize(w * h * 4, 255);
        for (row, line) in rgba.chunks_exact_mut(w * 4).enumerate() {
            let (u, v) = (&u[row / 2 * cw..], &v[row / 2 * cw..]);
            for (x, px) in line.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let c = 298 * (y[row * w + x] as i32 - 16);
                let (d, e) = (u[x / 2] as i32 - 128, v[x / 2] as i32 - 128);
                let clamp = |n: i32| ((n + 128) >> 8).clamp(0, 255) as u8;
                px[..3].copy_from_slice(&[clamp(c + 409 * e), clamp(c - 100 * d - 208 * e), clamp(c + 516 * d)]);
            }
        }
    }
}

#[derive(Clone)]
pub struct RemoteCursor {
    pub position: CursorPosition,
    pub shape: Option<Arc<CursorShape>>,
}

/// Figures for the stats overlay, refreshed about once a second.
#[derive(Debug, Clone, Copy)]
pub struct Stats {
    pub path: PathKind,
    pub rtt: Option<Duration>,
    pub fps: f32,
    pub mbps: f32,
    pub frames_lost: u64,
    /// Frames that arrived damaged and were rebuilt from the recovery data sent along.
    pub frames_recovered: u64,
    /// Frames decoded but replaced by a newer one before they could be shown.
    pub frames_superseded: u64,
    /// The bitrate the host is aiming for, fitted to the connection; at most its cap.
    pub host_mbps: Option<f32>,
    /// Share of packets the host sees going missing, 0..1.
    pub loss: Option<f32>,
    pub encode_ms: f32,
    pub decode_ms: f32,
    /// Whether the graphics card is decoding the video, rather than the processor.
    pub gpu_decode: bool,
    /// The host's screen → drawn here, on average, via the estimated clock offset between the
    /// two PCs: [`Latency::total`].
    pub latency_ms: Option<f32>,
    /// Where that time goes, stage by stage.
    pub latency: Option<Latency>,
    /// How busy each PC's graphics card is, where it can tell.
    pub host_gpu: Option<GpuLoad>,
    pub gpu: Option<GpuLoad>,
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            path: PathKind::Unknown,
            rtt: None,
            fps: 0.0,
            mbps: 0.0,
            frames_lost: 0,
            frames_recovered: 0,
            frames_superseded: 0,
            host_mbps: None,
            loss: None,
            encode_ms: 0.0,
            decode_ms: 0.0,
            gpu_decode: false,
            latency_ms: None,
            latency: None,
            host_gpu: None,
            gpu: None,
        }
    }
}

/// Controls a connection attempt or session. Cheap to clone.
#[derive(Clone)]
pub struct ClientHandle(Arc<Shared>);

struct Shared {
    wake: Box<dyn Fn() + Send + Sync>,
    disconnect: Notify,
    disconnected: AtomicBool,
    conn: Mutex<Option<Connection>>,
    /// Feeds the input stream while a session runs.
    input: Mutex<Option<mpsc::UnboundedSender<InputEvent>>>,
    frame: Mutex<pacing::Pacer>,
    /// Picture buffers the GUI is done with, for the decoder to reuse.
    spare_buffers: Mutex<Vec<Vec<u8>>>,
    /// Set by the decoder when a frame didn't decode: video needs a fresh keyframe.
    decoder_failed: AtomicBool,
    /// What is decoding the video, for people to read.
    decoder: Mutex<String>,
    gpu_decoding: AtomicBool,
    stats: Mutex<Stats>,
    counters: Counters,
    latency: Mutex<latency::LatencyWindow>,
    host_gpu: Mutex<Option<GpuLoad>>,
    cursor: Mutex<(CursorPosition, Option<Arc<CursorShape>>, Instant)>,
    cursor_sequence: AtomicU64,
    playback: Mutex<Option<Arc<audio::Playback>>>,
    audio_allowed: AtomicBool,
    audio_muted: AtomicBool,
    audio_generation: AtomicU64,
    audio_failure: Mutex<Option<String>>,
}

/// Written by the receive and decode loops, summarised into [`Stats`] by the control loop.
#[derive(Default)]
struct Counters {
    bytes: AtomicU64,
    frames_decoded: AtomicU64,
    frames_lost: AtomicU64,
    frames_recovered: AtomicU64,
    frames_superseded: AtomicU64,
    /// From the host's last report; `u32::MAX` until one arrives.
    host_kbps: AtomicU32,
    loss_permille: AtomicU32,
    encode_us: AtomicU64,
    decode_us: AtomicU64,
    /// Client clock minus host clock, µs; `i64::MIN` until measured.
    clock_offset_us: AtomicI64,
}

/// Starts connecting to `host`. `wake` is called (from any thread) whenever there's a new event
/// or frame, e.g. to request a repaint.
pub fn connect(
    endpoint: Endpoint,
    host: impl Into<EndpointAddr>,
    client_name: impl Into<String>,
    wake: impl Fn() + Send + Sync + 'static,
) -> (ClientHandle, mpsc::Receiver<ClientEvent>) {
    let shared = Arc::new(Shared {
        wake: Box::new(wake),
        disconnect: Notify::new(),
        disconnected: AtomicBool::new(false),
        conn: Mutex::new(None),
        input: Mutex::new(None),
        frame: Mutex::new(pacing::Pacer::default()),
        spare_buffers: Mutex::new(Vec::new()),
        decoder_failed: AtomicBool::new(false),
        decoder: Mutex::new(String::new()),
        gpu_decoding: AtomicBool::new(false),
        stats: Mutex::new(Stats::default()),
        latency: Mutex::new(latency::LatencyWindow::default()),
        host_gpu: Mutex::new(None),
        cursor: Mutex::new((CursorPosition::default(), None, Instant::now())),
        cursor_sequence: AtomicU64::new(0),
        playback: Mutex::new(None),
        audio_allowed: AtomicBool::new(false),
        audio_muted: AtomicBool::new(false),
        audio_generation: AtomicU64::new(0),
        audio_failure: Mutex::new(None),
        counters: Counters {
            host_kbps: AtomicU32::new(u32::MAX),
            clock_offset_us: AtomicI64::new(i64::MIN),
            ..Default::default()
        },
    });
    let (events_tx, events_rx) = mpsc::channel(32);
    let events = Events { tx: events_tx, shared: shared.clone() };
    let (host, client_name) = (host.into(), client_name.into());
    let task_shared = shared.clone();
    tokio::spawn(async move {
        let reason = run(&endpoint, host, &client_name, &task_shared, &events).await;
        debug!("session ended: {reason}");
        events.send(ClientEvent::Ended(reason)).await;
    });
    (ClientHandle(shared), events_rx)
}

impl ClientHandle {
    /// The newest decoded frame, if one arrived since the last call.
    pub fn take_frame(&self) -> Option<VideoFrame> {
        let (frame, replaced) = lock(&self.0.frame).take();
        for unseen in replaced {
            self.0.counters.frames_superseded.fetch_add(1, Ordering::Relaxed);
            recycle(&self.0, unseen.yuv);
        }
        frame
    }

    /// How long until a frame held back for smooth pacing is due; ask for a repaint then.
    pub fn next_frame_due(&self) -> Option<Duration> {
        lock(&self.0.frame).next_due()
    }

    /// Returns a shown frame's buffer for the decoder to reuse.
    pub fn recycle(&self, frame: VideoFrame) {
        let offset = self.0.counters.clock_offset_us.load(Ordering::Relaxed);
        lock(&self.0.latency).add(&frame.timing, (offset != i64::MIN).then_some(offset));
        recycle(&self.0, frame.yuv);
    }

    /// What is decoding the video, e.g. "Decoder: GPU · AMD Radeon RX 7600"; empty until
    /// the session's decoder starts.
    pub fn decoder(&self) -> String {
        lock(&self.0.decoder).clone()
    }

    pub fn stats(&self) -> Stats {
        *lock(&self.0.stats)
    }

    pub fn cursor(&self) -> RemoteCursor {
        let cursor = lock(&self.0.cursor);
        let mut position = cursor.0;
        if cursor.2.elapsed() > Duration::from_secs(1) {
            position.visible = false;
        }
        RemoteCursor { position, shape: cursor.1.clone() }
    }

    /// Visual pointer telemetry; this never grants or injects mouse input.
    pub fn send_cursor(&self, x: u16, y: u16, visible: bool) {
        let position =
            CursorPosition { seq: self.0.cursor_sequence.fetch_add(1, Ordering::Relaxed) as u32, x, y, visible };
        if let Some(conn) = &*lock(&self.0.conn) {
            let _ = conn.send_datagram(position.encode());
        }
    }

    pub fn set_audio_muted(&self, muted: bool) {
        self.0.audio_muted.store(muted, Ordering::Relaxed);
        if let Some(playback) = &*lock(&self.0.playback) {
            playback.set_muted(muted);
        }
    }

    /// Sends input to the host, in order. Dropped unless a session is running; the host also
    /// drops anything its permissions don't allow.
    pub fn send_input(&self, event: InputEvent) {
        if let Some(tx) = &*lock(&self.0.input) {
            let _ = tx.send(event);
        }
    }

    /// Hangs up, or stops connecting. [`ClientEvent::Ended`] follows.
    pub fn disconnect(&self) {
        self.0.disconnected.store(true, Ordering::Relaxed);
        self.0.disconnect.notify_one();
        if let Some(conn) = &*lock(&self.0.conn) {
            conn.close(CLOSE_NORMAL.into(), b"disconnected");
        }
    }
}

#[derive(Clone)]
struct Events {
    tx: mpsc::Sender<ClientEvent>,
    shared: Arc<Shared>,
}

impl Events {
    async fn send(&self, event: ClientEvent) {
        let _ = self.tx.send(event).await;
        (self.shared.wake)();
    }
}

async fn run(
    endpoint: &Endpoint,
    host: EndpointAddr,
    client_name: &str,
    shared: &Arc<Shared>,
    events: &Events,
) -> EndReason {
    events.send(ClientEvent::Stage(Stage::Connecting)).await;
    let conn = tokio::select! {
        conn = net::connect(endpoint, host) => match conn {
            Ok(conn) => conn,
            Err(e) => return EndReason::Failed(format!("{e:#}")),
        },
        _ = shared.disconnect.notified() => return EndReason::Disconnected,
    };
    *lock(&shared.conn) = Some(conn.clone());
    if shared.disconnected.load(Ordering::Relaxed) {
        conn.close(CLOSE_NORMAL.into(), b"disconnected");
        return EndReason::Disconnected;
    }

    let result = session(&conn, client_name, shared, events).await;
    stop_audio(shared);
    *lock(&shared.input) = None;
    let reason = match (conn.close_reason(), result) {
        (_, Ok(Some(reason))) => reason,
        (Some(closed), _) => describe_close(&closed),
        (None, Err(e)) => EndReason::Failed(format!("{e:#}")),
        (None, Ok(None)) => EndReason::HostEnded("The host ended the session.".into()),
    };
    conn.close(CLOSE_NORMAL.into(), b"bye");
    *lock(&shared.conn) = None;
    reason
}

/// Runs the handshake and, once allowed, the session. `Ok(Some(reason))` when the host said
/// why it ended; otherwise the reason comes from how the connection closed.
async fn session(
    conn: &Connection,
    client_name: &str,
    shared: &Arc<Shared>,
    events: &Events,
) -> Result<Option<EndReason>> {
    // Opening the graphics card's decoder can take a moment; do it while the host user decides.
    let (failed_tx, failed_codecs) = mpsc::unbounded_channel();
    let decoder = video::start_decoder(shared.clone(), failed_tx)?;
    let decoders = tokio::task::spawn_blocking(decoder::supported).await?;
    debug!(?decoders, "decoders offered");
    let (mut send, mut recv) = conn.open_bi().await?;
    let hello = ClientMsg::Hello { version: PROTOCOL_VERSION, client_name: client_name.to_owned() };
    net::write_msg(&mut send, &hello).await?;
    net::write_msg(&mut send, &ClientMsg::Decoders(decoders)).await?;
    events.send(ClientEvent::Stage(Stage::WaitingForApproval)).await;

    let reply = tokio::select! {
        reply = net::read_msg::<HostMsg>(&mut recv) => reply,
        _ = shared.disconnect.notified() => return Ok(Some(EndReason::Disconnected)),
    };
    match reply {
        Ok(Some(HostMsg::Welcome { host_name, width, height, permissions })) => {
            // Ready before `Accepted`, so input sent in response to it isn't dropped.
            let input = conn.open_uni().await?;
            let (input_tx, input_rx) = mpsc::unbounded_channel();
            tokio::spawn(write_input(input, input_rx));
            *lock(&shared.input) = Some(input_tx);
            events.send(ClientEvent::Accepted { host_name, width, height, permissions }).await;
        }
        Ok(Some(HostMsg::Rejected { reason })) => return Ok(Some(EndReason::Rejected(reason))),
        Ok(Some(other)) => bail!("unexpected reply from host: {other:?}"),
        Ok(None) => bail!("the host closed the connection"),
        // If the host closed before its reason arrived, `run` takes the reason from the close.
        Err(e) => return Err(e),
    }

    stream(conn, send, recv, decoder, failed_codecs, shared, events).await
}

async fn stream(
    conn: &Connection,
    mut send: SendStream,
    recv: RecvStream,
    decoder: video::DecoderQueue,
    mut failed_codecs: mpsc::UnboundedReceiver<Codec>,
    shared: &Arc<Shared>,
    events: &Events,
) -> Result<Option<EndReason>> {
    let (keyframe_tx, mut keyframe_rx) = mpsc::unbounded_channel();
    tokio::spawn(video::receive(conn.clone(), decoder, keyframe_tx, shared.clone()));

    // Reading isn't cancel-safe (a half-read message would be lost), so it gets its own task.
    let (msg_tx, mut msgs) = mpsc::channel(16);
    tokio::spawn(read_loop(recv, msg_tx));

    let mut ping = tokio::time::interval(PING_INTERVAL);
    let mut last_keyframe_request: Option<Instant> = None;
    let mut window = StatsWindow::new(shared);
    let mut best_rtt_us = u64::MAX;

    loop {
        tokio::select! {
            msg = msgs.recv() => match msg {
                Some(Ok(HostMsg::Pong { client_time_us, host_time_us })) => {
                    let now = now_micros();
                    let rtt = now.saturating_sub(client_time_us);
                    // The fastest round trip gives the tightest bound on the clock offset.
                    if rtt <= best_rtt_us {
                        best_rtt_us = rtt;
                        let offset = (client_time_us + rtt / 2) as i64 - host_time_us as i64;
                        shared.counters.clock_offset_us.store(offset, Ordering::Relaxed);
                    }
                }
                Some(Ok(HostMsg::PermissionsChanged(p))) => {
                    if !p.allows(DeviceClass::Audio) { stop_audio(shared); }
                    events.send(ClientEvent::PermissionsChanged(p)).await;
                }
                Some(Ok(HostMsg::VideoEncoder { name, codec })) => {
                    events.send(ClientEvent::VideoEncoder { name, codec }).await
                }
                Some(Ok(HostMsg::VideoStats { bitrate_kbps, loss_permille, gpu })) => {
                    *lock(&shared.host_gpu) = gpu;
                    shared.counters.host_kbps.store(bitrate_kbps, Ordering::Relaxed);
                    shared.counters.loss_permille.store(loss_permille.into(), Ordering::Relaxed);
                }
                Some(Ok(HostMsg::CursorShape(shape))) => {
                    anyhow::ensure!(shape.is_valid(), "invalid host cursor image");
                    lock(&shared.cursor).1 = Some(Arc::new(shape));
                    (shared.wake)();
                }
                Some(Ok(HostMsg::AudioStatus { active, reason })) => {
                    if active { let _ = start_audio(shared.clone(), events.clone()); }
                    else { stop_audio(shared); events.send(ClientEvent::AudioStatus { active, reason }).await; }
                }
                Some(Ok(HostMsg::VideoFormat { width, height })) => {
                    events.send(ClientEvent::VideoFormat { width, height }).await;
                }
                Some(Ok(HostMsg::InputUnavailable { device, reason })) => {
                    events.send(ClientEvent::InputUnavailable { device, reason }).await;
                }
                Some(Ok(HostMsg::Rejected { reason })) => return Ok(Some(EndReason::HostEnded(reason))),
                Some(Ok(other @ HostMsg::Welcome { .. })) => bail!("unexpected message from host: {other:?}"),
                Some(Err(e)) => return Err(e),
                None => return Ok(None),
            },
            Some(()) = keyframe_rx.recv() => {
                let round_trip = Duration::from_micros(best_rtt_us.min(200_000)) + Duration::from_millis(30);
                if last_keyframe_request.is_none_or(|t| t.elapsed() >= KEYFRAME_REQUEST_INTERVAL.max(round_trip)) {
                    last_keyframe_request = Some(Instant::now());
                    debug!("requesting a keyframe");
                    net::write_msg(&mut send, &ClientMsg::RequestKeyframe).await?;
                }
            }
            Some(codec) = failed_codecs.recv() => {
                net::write_msg(&mut send, &ClientMsg::DecoderFailed { codec }).await?;
            }
            _ = ping.tick() => {
                let failure = lock(&shared.audio_failure).take();
                if let Some(reason) = failure {
                    stop_audio(shared);
                    events.send(ClientEvent::AudioStatus { active: false, reason: Some(reason) }).await;
                }
                net::write_msg(&mut send, &ClientMsg::Ping { client_time_us: now_micros() }).await?;
                window.update(conn, shared);
            }
            _ = conn.closed() => return Ok(None),
            _ = shared.disconnect.notified() => return Ok(Some(EndReason::Disconnected)),
        }
    }
}

fn stop_audio(shared: &Shared) {
    shared.audio_allowed.store(false, Ordering::Release);
    shared.audio_generation.fetch_add(1, Ordering::AcqRel);
    let playback = lock(&shared.playback).take();
    drop(playback);
}

fn start_audio(shared: Arc<Shared>, events: Events) -> Option<tokio::task::JoinHandle<()>> {
    // A host packetizer restart changes capture epoch; the playback buffer handles it in place.
    if shared.audio_allowed.load(Ordering::Acquire) {
        return None;
    }
    stop_audio(&shared);
    shared.audio_allowed.store(true, Ordering::Release);
    let generation = shared.audio_generation.load(Ordering::Acquire);
    Some(tokio::spawn(async move {
        let result = tokio::task::spawn_blocking(audio::Playback::start).await;
        match result {
            Ok(Ok(playback)) => {
                {
                    let mut slot = lock(&shared.playback);
                    if shared.audio_generation.load(Ordering::Acquire) != generation
                        || !shared.audio_allowed.load(Ordering::Acquire)
                    {
                        return;
                    }
                    playback.set_muted(shared.audio_muted.load(Ordering::Relaxed));
                    *slot = Some(Arc::new(playback));
                }
                events.send(ClientEvent::AudioStatus { active: true, reason: None }).await;
            }
            other => {
                if shared.audio_generation.load(Ordering::Acquire) != generation {
                    return;
                }
                shared.audio_allowed.store(false, Ordering::Release);
                let reason = match other {
                    Ok(Err(error)) => format!("Desktop audio playback unavailable: {error:#}"),
                    Err(error) => format!("Desktop audio playback stopped: {error}"),
                    _ => unreachable!(),
                };
                events.send(ClientEvent::AudioStatus { active: false, reason: Some(reason) }).await;
            }
        }
    }))
}

async fn read_loop(mut recv: RecvStream, tx: mpsc::Sender<Result<HostMsg>>) {
    loop {
        match net::read_msg::<HostMsg>(&mut recv).await {
            Ok(Some(msg)) => {
                if tx.send(Ok(msg)).await.is_err() {
                    return;
                }
            }
            Ok(None) => return,
            Err(e) => {
                let _ = tx.send(Err(e)).await;
                return;
            }
        }
    }
}

async fn write_input(mut stream: SendStream, mut events: mpsc::UnboundedReceiver<InputEvent>) {
    while let Some(event) = events.recv().await {
        if let Err(e) = net::write_msg(&mut stream, &event).await {
            return debug!("input stream closed: {e:#}");
        }
    }
    let _ = stream.finish();
}

fn describe_close(error: &ConnectionError) -> EndReason {
    match error {
        ConnectionError::LocallyClosed => EndReason::Disconnected,
        ConnectionError::ApplicationClosed(close) => {
            let reason = String::from_utf8_lossy(&close.reason).into_owned();
            if close.error_code == CLOSE_REJECTED.into() {
                EndReason::Rejected(reason)
            } else if close.error_code == CLOSE_NORMAL.into() && !reason.is_empty() {
                EndReason::HostEnded(reason)
            } else {
                EndReason::HostEnded("The host ended the session.".into())
            }
        }
        ConnectionError::TimedOut | ConnectionError::Reset => EndReason::ConnectionLost,
        other => EndReason::Failed(other.to_string()),
    }
}

/// Turns the running counters into per-second rates.
struct StatsWindow {
    at: Instant,
    bytes: u64,
    frames: u64,
    gpu: Option<connect_gpu_load::Monitor>,
}

impl StatsWindow {
    fn new(shared: &Shared) -> Self {
        let c = &shared.counters;
        Self {
            at: Instant::now(),
            bytes: c.bytes.load(Ordering::Relaxed),
            frames: c.frames_decoded.load(Ordering::Relaxed),
            gpu: connect_gpu_load::Monitor::new(),
        }
    }

    fn update(&mut self, conn: &Connection, shared: &Shared) {
        let c = &shared.counters;
        let secs = self.at.elapsed().as_secs_f32();
        if secs < 0.5 {
            return; // the first tick fires at once; rates need a real window
        }
        let (bytes, frames) = (c.bytes.load(Ordering::Relaxed), c.frames_decoded.load(Ordering::Relaxed));
        let (path, rtt) = net::selected_path(conn);
        let latency = lock(&shared.latency).take();
        *lock(&shared.stats) = Stats {
            path,
            rtt,
            fps: (frames - self.frames) as f32 / secs,
            mbps: (bytes - self.bytes) as f32 * 8.0 / secs / 1e6,
            frames_lost: c.frames_lost.load(Ordering::Relaxed),
            frames_recovered: c.frames_recovered.load(Ordering::Relaxed),
            frames_superseded: c.frames_superseded.load(Ordering::Relaxed),
            host_mbps: match c.host_kbps.load(Ordering::Relaxed) {
                u32::MAX => None,
                kbps => Some(kbps as f32 / 1000.0),
            },
            loss: (c.host_kbps.load(Ordering::Relaxed) != u32::MAX)
                .then(|| c.loss_permille.load(Ordering::Relaxed) as f32 / 1000.0),
            encode_ms: c.encode_us.load(Ordering::Relaxed) as f32 / 1000.0,
            decode_ms: c.decode_us.load(Ordering::Relaxed) as f32 / 1000.0,
            gpu_decode: shared.gpu_decoding.load(Ordering::Relaxed),
            latency_ms: latency.and_then(|latency| latency.total),
            latency,
            host_gpu: *lock(&shared.host_gpu),
            gpu: self.gpu.as_mut().and_then(connect_gpu_load::Monitor::sample),
        };
        (self.at, self.bytes, self.frames) = (Instant::now(), bytes, frames);
        (shared.wake)();
    }
}

/// Keeps a few picture buffers around for the decoder; more would only hold memory.
fn recycle(shared: &Shared, buffer: Vec<u8>) {
    let mut spare = lock(&shared.spare_buffers);
    if spare.len() < 3 {
        spare.push(buffer);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod media_tests {
    use super::*;
    use connect_proto::{
        CursorShape,
        audio::{self, AudioPacket},
        video::{self, FrameMeta},
    };
    use openh264::{
        OpenH264API,
        encoder::{Encoder, EncoderConfig, FrameRate},
        formats::YUVBuffer,
    };

    #[tokio::test(flavor = "multi_thread")]
    async fn revoked_or_disconnected_audio_startup_cannot_install_playback() {
        let host = net::bind_local_endpoint(iroh::SecretKey::generate()).await.unwrap();
        let endpoint = net::bind_local_endpoint(iroh::SecretKey::generate()).await.unwrap();
        let (client, _events) = connect(endpoint.clone(), net::local_addr(&host), "startup probe", || {});
        let (tx, _rx) = mpsc::channel(32);
        let events = Events { tx, shared: client.0.clone() };
        let worker = start_audio(client.0.clone(), events).unwrap();
        stop_audio(&client.0);
        client.disconnect();
        tokio::time::timeout(Duration::from_secs(7), worker).await.unwrap().unwrap();
        assert!(!client.0.audio_allowed.load(Ordering::Acquire));
        assert!(lock(&client.0.playback).is_none());
        endpoint.close().await;
        host.close().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn mixed_datagrams_keep_cursor_and_video_independent() {
        let host = net::bind_local_endpoint(iroh::SecretKey::generate()).await.unwrap();
        let client_endpoint = net::bind_local_endpoint(iroh::SecretKey::generate()).await.unwrap();
        let (client, mut events) = connect(client_endpoint.clone(), net::local_addr(&host), "media probe", || {});
        let server_endpoint = host.clone();
        let server = tokio::spawn(async move {
            let conn = server_endpoint.accept().await.unwrap().accept().unwrap().await.unwrap();
            let (mut send, mut recv) = conn.accept_bi().await.unwrap();
            assert!(matches!(
                net::read_msg::<ClientMsg>(&mut recv).await.unwrap(),
                Some(ClientMsg::Hello { version: PROTOCOL_VERSION, .. })
            ));
            net::write_msg(
                &mut send,
                &HostMsg::Welcome {
                    host_name: "probe".into(),
                    width: 16,
                    height: 16,
                    permissions: Permissions::VIEW_ONLY,
                },
            )
            .await
            .unwrap();
            let shape = CursorShape { id: 99, width: 1, height: 1, hotspot_x: 0, hotspot_y: 0, rgba: vec![255; 4] };
            net::write_msg(&mut send, &HostMsg::CursorShape(shape)).await.unwrap();
            let pcm = AudioPacket {
                stream_id: 1,
                sequence: 0,
                first_frame: 0,
                sample_rate: 48000,
                channels: 2,
                payload: audio::Payload::Pcm(vec![0.0; 120]),
            };
            // Audio is ungranted: it must not open playback or corrupt video assembly.
            conn.send_datagram(audio::encode(&pcm, 1200).unwrap()).unwrap();
            conn.send_datagram(CursorPosition { seq: 2, x: 123, y: 456, visible: true }.encode()).unwrap();
            let mut encoder = Encoder::with_api_config(
                OpenH264API::from_source(),
                EncoderConfig::new().max_frame_rate(FrameRate::from_hz(60.0)),
            )
            .unwrap();
            let image = YUVBuffer::new(16, 16);
            let encoded = encoder.encode(&image).unwrap().to_vec();
            let meta = FrameMeta { keyframe: true, capture_time_us: now_micros(), ..Default::default() };
            for chunk in video::packetize(meta, &encoded, 1200, 0).unwrap() {
                conn.send_datagram(chunk).unwrap();
            }
            conn.closed().await;
        });
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match events.recv().await.expect("client event stream remains open") {
                    ClientEvent::Accepted { .. } => break,
                    ClientEvent::Ended(reason) => panic!("media probe ended early: {reason}"),
                    _ => {}
                }
            }
            let mut saw_video = false;
            loop {
                if let Some(frame) = client.take_frame() {
                    assert_eq!((frame.width, frame.height), (16, 16));
                    saw_video = true;
                }
                let cursor = client.cursor();
                if saw_video
                    && cursor.position.x == 123
                    && cursor.position.visible
                    && cursor.shape.as_ref().is_some_and(|s| s.id == 99)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("mixed media delivers video and cursor independently");
        assert!(lock(&client.0.playback).is_none());
        client.disconnect();
        server.await.unwrap();
        client_endpoint.close().await;
        host.close().await;
    }
}
