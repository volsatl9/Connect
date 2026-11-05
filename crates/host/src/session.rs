//! One incoming connection: handshake, approval, then streaming until either side hangs up.

use std::{
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use connect_net as net;
use connect_proto::{
    CLOSE_NORMAL, CLOSE_PROTOCOL, CLOSE_REJECTED, Codec, CodecSupport, CursorPosition, DeviceClass, PROTOCOL_VERSION,
    Permissions, codec,
    control::{ClientMsg, HostMsg},
    video::{self, EncodedFrame},
};
use iroh::endpoint::{Connection, ConnectionError, Incoming, RecvStream, SendDatagramError, SendStream};
use tokio::{
    sync::{mpsc, oneshot, watch},
    time::timeout,
};
use tracing::{debug, info, warn};

use crate::{
    ConnectionRequest, Decision, HostEvent, Shared, VideoSource,
    audio::AudioStream,
    cursor::CursorStream,
    encode::{FrameStamp, VideoEvent, VideoStream},
    input::{self, Unavailable},
    rate::{self, PathSample, RateControl},
    trace::{FrameRecord, HostTrace, LoadRecord, NetworkRecord, Reporter},
    viewer_files,
};

/// An unchanged pointer position is sent again this often, so the viewer knows it's current.
const CURSOR_REPEAT: Duration = Duration::from_millis(250);

/// How long a new connection may take to say hello.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// How long to wait for a turned-away client to read the reason and hang up itself.
const REJECT_LINGER: Duration = Duration::from_secs(3);
/// How long a finished session may take to release the remote user's input.
const INPUT_RELEASE: Duration = Duration::from_secs(2);

// Why a connection was turned away. The codes are listed in the README.
const DECLINED: &str = "The host declined the connection. (Code C-106)";
const NO_ANSWER: &str = "The host didn't answer in time. (Code C-107)";
const BUSY: &str = "The host is in another session right now. (Code C-105)";
const NOT_ACCEPTING: &str = "The host isn't accepting connections right now. (Code C-104)";
const NO_CAPTURE: &str = "The host couldn't capture its screen. (Code C-108)";
const NO_COMMON_CODEC: &str =
    "The two PCs have no video format in common. Update both to the same version of Connect. (Code C-102)";
const CANNOT_HOST: &str =
    "That PC can't be shared: hosting needs Windows. It can still connect to others. (Code C-103)";

pub(crate) async fn run(incoming: Incoming, shared: Arc<Shared>, events: mpsc::Sender<HostEvent>) {
    let conn = match incoming.accept() {
        Ok(accepting) => match accepting.await {
            Ok(conn) => conn,
            Err(e) => return debug!("incoming handshake failed: {e:#}"),
        },
        Err(e) => return debug!("incoming connection failed: {e:#}"),
    };
    if let Err(e) = serve(&conn, &shared, &events).await {
        warn!(peer = %conn.remote_id().fmt_short(), "connection failed: {e:#}");
        conn.close(CLOSE_PROTOCOL.into(), b"protocol error");
    }
}

async fn serve(conn: &Connection, shared: &Shared, events: &mpsc::Sender<HostEvent>) -> Result<()> {
    let peer = conn.remote_id();
    let (mut send, mut recv) = timeout(HELLO_TIMEOUT, conn.accept_bi()).await.context("no hello")??;
    let hello = timeout(HELLO_TIMEOUT, net::read_msg(&mut recv)).await.context("no hello")??;
    let Some(ClientMsg::Hello { version, client_name }) = hello else { bail!("expected hello") };
    let client_name = clean_name(&client_name);
    info!(peer = %peer.fmt_short(), %client_name, "connection request");

    if version != PROTOCOL_VERSION {
        let reason = format!(
            "The host runs a different version of Connect (protocol {PROTOCOL_VERSION}, yours is {version}). \
             Update both PCs to the same version. (Code C-101)"
        );
        return reject(conn, &mut send, &reason).await;
    }
    let decoders = match timeout(HELLO_TIMEOUT, net::read_msg(&mut recv)).await.context("no decoder list")?? {
        Some(ClientMsg::Decoders(decoders)) => decoders,
        other => bail!("expected the decoder list, got {other:?}"),
    };
    let source = shared.video.clone();
    let encoders = tokio::task::spawn_blocking(move || crate::encode::supported(&source)).await?;
    let Some(codec) = codec::choose(&encoders, &decoders, &[]) else {
        return reject(conn, &mut send, NO_COMMON_CODEC).await;
    };
    info!(?encoders, ?decoders, %codec, "video codec chosen");
    if shared.video == VideoSource::Screen && !VideoSource::SCREEN_SUPPORTED {
        return reject(conn, &mut send, CANNOT_HOST).await;
    }
    if !shared.accepting.load(std::sync::atomic::Ordering::Relaxed) {
        return reject(conn, &mut send, NOT_ACCEPTING).await;
    }
    let Some(_slot) = shared.reserve(conn) else {
        return reject(conn, &mut send, BUSY).await;
    };

    let granted = if let Some(permissions) = shared.trusted_permissions(&peer) {
        permissions
    } else {
        match ask(conn, shared, events, &client_name).await {
            Answer::Decided(Decision::Allow(permissions)) => permissions,
            Answer::Decided(Decision::Deny) => return reject(conn, &mut send, DECLINED).await,
            Answer::TimedOut => return reject(conn, &mut send, NO_ANSWER).await,
            Answer::Withdrawn => return Ok(()),
        }
    };

    let bitrate_bps = shared.bitrate_mbps.load(Ordering::Relaxed).saturating_mul(1_000_000);
    let rate = RateControl::new(bitrate_bps);
    let trace = Arc::new(HostTrace::new());
    let spool = start_trace(&trace);
    let backlog = Box::new(rate.backlog_check(conn.clone()));
    let limit = shared.frame_rate_limit.load(Ordering::Relaxed);
    let mut video = VideoStream::start(shared.video.clone(), codec, rate.bitrate(), limit, backlog, trace.clone())?;
    let (width, height) = match video.next().await {
        Some(Ok(VideoEvent::Format { width, height })) => (width, height),
        Some(Err(e)) => {
            warn!("screen capture failed to start: {e:#}");
            return reject(conn, &mut send, NO_CAPTURE).await;
        }
        _ => return reject(conn, &mut send, NO_CAPTURE).await,
    };

    shared.permissions.send_replace(granted);
    let permissions = shared.permissions.subscribe();
    let welcome = HostMsg::Welcome { host_name: shared.host_name(), width, height, permissions: granted };
    net::write_msg(&mut send, &welcome).await?;
    info!(peer = %peer.fmt_short(), %client_name, width, height, ?granted, "session started");
    let _ =
        events.send(HostEvent::SessionStarted { peer, client_name: client_name.clone(), permissions: granted }).await;

    let (unavailable_tx, unavailable) = mpsc::unbounded_channel();
    let (input_taken, taken) = oneshot::channel();
    let viewer_files = connect_perf::trace_dir()
        .filter(|_| connect_perf::enabled())
        .map(|dir| tokio::spawn(viewer_files::collect(conn.clone(), taken, dir, trace.session)));
    let input = tokio::spawn(input::serve(
        conn.clone(),
        shared.permissions.subscribe(),
        shared.input.clone(),
        unavailable_tx,
        trace.clone(),
        input_taken,
    ));

    let desktop = shared.video == VideoSource::Screen;
    let codecs = Codecs { encoders, decoders, failed: Vec::new() };
    let result = stream(conn, send, recv, video, codecs, rate, permissions, unavailable, desktop, &trace).await;
    let reason = end_reason(conn, &client_name, &result);
    info!(peer = %peer.fmt_short(), "session ended: {reason}");
    if conn.close_reason().is_none() {
        let code = if result.is_ok() { CLOSE_NORMAL } else { CLOSE_PROTOCOL };
        conn.close(code.into(), b"session ended");
    }
    // The input task sees the close and releases whatever was held; the session isn't over
    // (and the host can't shut down) until it has.
    if timeout(INPUT_RELEASE, input).await.is_err() {
        warn!("input took too long to release");
    }
    finish_trace(spool).await;
    if let Some(files) = viewer_files {
        let _ = timeout(INPUT_RELEASE, files).await;
    }
    let _ = events.send(HostEvent::SessionEnded { peer, reason }).await;
    Ok(())
}

enum Answer {
    Decided(Decision),
    TimedOut,
    /// The requester hung up (or the host user cancelled) before an answer.
    Withdrawn,
}

async fn ask(conn: &Connection, shared: &Shared, events: &mpsc::Sender<HostEvent>, client_name: &str) -> Answer {
    let (respond, answer) = oneshot::channel();
    let request = ConnectionRequest { peer: conn.remote_id(), client_name: client_name.to_owned(), respond };
    if events.send(HostEvent::Request(request)).await.is_err() {
        return Answer::Decided(Decision::Deny); // nobody is listening to ask
    }
    tokio::select! {
        answer = timeout(shared.approval_timeout, answer) => match answer {
            Ok(Ok(decision)) => Answer::Decided(decision),
            Ok(Err(_)) => Answer::Decided(Decision::Deny), // request dropped unanswered
            Err(_) => Answer::TimedOut,
        },
        _ = conn.closed() => Answer::Withdrawn,
    }
}

/// Tells the client why, then closes. Closing straight away could discard the reason in flight.
async fn reject(conn: &Connection, send: &mut SendStream, reason: &str) -> Result<()> {
    info!(peer = %conn.remote_id().fmt_short(), "turning away: {reason}");
    net::write_msg(send, &HostMsg::Rejected { reason: reason.to_owned() }).await?;
    send.finish()?;
    let _ = timeout(REJECT_LINGER, conn.closed()).await;
    conn.close(CLOSE_REJECTED.into(), reason.as_bytes());
    Ok(())
}

#[expect(clippy::too_many_arguments)]
async fn stream(
    conn: &Connection,
    mut send: SendStream,
    recv: RecvStream,
    mut video: VideoStream,
    mut codecs: Codecs,
    mut rate: RateControl,
    mut permissions: watch::Receiver<Permissions>,
    mut unavailable: mpsc::UnboundedReceiver<Unavailable>,
    desktop: bool,
    trace: &HostTrace,
) -> Result<()> {
    // Reading isn't cancel-safe (a half-read message would be lost), so it gets its own task.
    let (msg_tx, mut msgs) = mpsc::channel(16);
    tokio::spawn(read_loop(recv, msg_tx));
    let cursor = if desktop {
        match CursorStream::start() {
            Ok(cursor) => Some(cursor),
            Err(error) => {
                warn!("cursor sharing unavailable: {error:#}");
                None
            }
        }
    } else {
        None
    };
    let cursor_updates = cursor.as_ref().map(CursorStream::subscribe);
    let mut cursor_tick = tokio::time::interval(Duration::from_millis(8));
    cursor_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut shape_id = None;
    let mut cursor_seq = 0u32;
    // The last position sent, and when: unchanged, it's only repeated now and then.
    let mut cursor_sent: Option<(CursorPosition, Instant)> = None;
    let mut rate_tick = tokio::time::interval(rate::TICK);
    rate_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut rate_ticks = 0u32;
    let mut gpu_load = connect_gpu_load::Monitor::new();
    let mut reporter = Reporter::new();
    // For the rate controller and the trace: the viewer's keyframe requests (each means it lost
    // video), video bytes sent, and the path's counters at the last tick.
    let (mut recovery_requests, mut video_bytes) = (0u64, 0u64);
    let mut last_tick = (0, 0, 0, 0, 0, crate::input::LocalActivity::default(), 0);
    let mut remote_seq = None;
    let mut audio_enabled = desktop && permissions.borrow().allows(DeviceClass::Audio);
    let mut audio = start_audio(conn, &mut send, audio_enabled).await?;

    loop {
        tokio::select! {
            msg = msgs.recv() => match msg {
                Some(Ok(ClientMsg::Ping { client_time_us })) => {
                    let pong = HostMsg::Pong { client_time_us, host_time_us: connect_proto::now_micros() };
                    net::write_msg(&mut send, &pong).await?;
                }
                Some(Ok(ClientMsg::RequestKeyframe)) => {
                    debug!("client asked for a keyframe");
                    recovery_requests += 1;
                    video.request_keyframe();
                }
                Some(Ok(ClientMsg::DecoderFailed { codec })) => {
                    codecs.failed.push(codec);
                    let next = codec::choose(&codecs.encoders, &codecs.decoders, &codecs.failed)
                        .context("the viewer can't decode any codec this host can send")?;
                    warn!(%codec, %next, "the viewer couldn't decode the video; switching codec");
                    video.set_codec(next);
                }
                Some(Ok(ClientMsg::Hello { .. } | ClientMsg::Decoders(_))) => bail!("unexpected second hello"),
                Some(Err(e)) => return Err(e),
                None => return Ok(()), // the client finished its control stream: a clean hang-up
            },
            Ok(()) = permissions.changed() => {
                let p = *permissions.borrow_and_update();
                // Drop the capture callback before acknowledging a revoked permission.
                let enabled = desktop && p.allows(DeviceClass::Audio);
                if enabled != audio_enabled { audio = None; }
                net::write_msg(&mut send, &HostMsg::PermissionsChanged(p)).await?;
                if enabled != audio_enabled {
                    audio_enabled = enabled;
                    audio = start_audio(conn, &mut send, enabled).await?;
                }
            }
            packet = async {
                match &mut audio { Some(audio) => audio.next().await, None => std::future::pending().await }
            } => {
                if !permissions.borrow().allows(DeviceClass::Audio) { audio = None; continue; }
                match packet {
                    Some(Ok(data)) => {
                        match conn.send_datagram(data) {
                            Ok(()) => {},
                            Err(SendDatagramError::TooLarge) => {
                                // A smaller path MTU requires a fresh packetizer, not endless PCM loss.
                                drop(audio.take());
                                let enabled = permissions.borrow().allows(DeviceClass::Audio);
                                audio = start_audio(conn, &mut send, enabled).await?;
                            },
                            Err(error) => return Err(error.into()),
                        }
                    }
                    error => {
                        let reason = match error { Some(Err(error)) => format!("Desktop audio stopped: {error:#}"), _ => "Desktop audio stopped.".into() };
                        audio = None;
                        net::write_msg(&mut send, &HostMsg::AudioStatus { active: false, reason: Some(reason) }).await?;
                    }
                }
            }
            _ = cursor_tick.tick(), if cursor_updates.is_some() => {
                let state = cursor_updates.as_ref().expect("guarded").borrow().clone();
                if let Some(shape) = state.shape && shape_id != Some(shape.id) {
                        net::write_msg(&mut send, &HostMsg::CursorShape((*shape).clone())).await?;
                        shape_id = Some(shape.id);
                }
                let mut position = state.position;
                let unchanged = cursor_sent.is_some_and(|(sent, at)| {
                    (sent.x, sent.y, sent.visible) == (position.x, position.y, position.visible)
                        && at.elapsed() < CURSOR_REPEAT
                });
                if !unchanged {
                    position.seq = cursor_seq;
                    cursor_seq = cursor_seq.wrapping_add(1);
                    cursor_sent = Some((position, Instant::now()));
                    match conn.send_datagram(position.encode()) {
                        Ok(()) | Err(SendDatagramError::TooLarge) => {},
                        Err(error) => return Err(error.into()),
                    }
                }
            }
            _ = rate_tick.tick() => {
                let (sent, lost) = net::packet_counts(conn);
                let queued = net::queued_datagram_bytes(conn);
                let path = net::path_snapshot(conn);
                let sample = PathSample {
                    sent_packets: sent,
                    lost_packets: lost,
                    queued_bytes: queued,
                    bandwidth_estimate: path.and_then(|p| p.bandwidth_estimate),
                    recovery_requests,
                    video_bytes,
                };
                if let Some(bps) = rate.update(Instant::now(), sample) {
                    debug!(mbps = bps as f32 / 1e6, loss = rate.loss(), queued, reason = rate.reason().name(), "video bitrate changed");
                    video.set_bitrate(bps);
                }
                let congestion = path.map_or(0, |p| p.congestion_events);
                let local = crate::input::local_activity();
                let host_priority = trace.host_priority_drops.load(Ordering::Relaxed);
                let delta = |now: u64, then: u64| now.saturating_sub(then).min(u64::from(u32::MAX)) as u32;
                trace.network.push(NetworkRecord {
                    time_us: connect_proto::now_micros(),
                    sent_packets: delta(sent, last_tick.0),
                    lost_packets: delta(lost, last_tick.1),
                    queued_bytes: queued.min(u32::MAX as usize) as u32,
                    bandwidth_estimate_bps: path.and_then(|p| p.bandwidth_estimate),
                    cwnd_bytes: path.map_or(0, |p| p.cwnd),
                    rtt_us: path.map_or(0, |p| p.rtt.as_micros().min(u128::from(u32::MAX)) as u32),
                    congestion_events: delta(congestion, last_tick.2),
                    relay: path.is_some_and(|p| p.kind == net::PathKind::Relay),
                    recovery_requests: delta(recovery_requests, last_tick.3),
                    bitrate_kbps: rate.bitrate() / 1000,
                    ceiling_kbps: rate.ceiling().map(|bps| bps / 1000),
                    reason: rate.reason().name(),
                    video_bytes: delta(video_bytes, last_tick.4),
                    local: local.since(last_tick.5),
                    host_priority: delta(host_priority, last_tick.6),
                });
                last_tick = (sent, lost, congestion, recovery_requests, video_bytes, local, host_priority);
                rate_ticks += 1;
                if rate_ticks.is_multiple_of(4) {
                    let gpu = gpu_load.as_mut().and_then(connect_gpu_load::Monitor::sample);
                    let perf = reporter.report(trace).map(Box::new);
                    trace.load.push(LoadRecord {
                        time_us: connect_proto::now_micros(),
                        process: perf.as_ref().and_then(|p| p.process),
                        gpu,
                    });
                    let stats = HostMsg::VideoStats {
                        bitrate_kbps: rate.bitrate() / 1000,
                        loss_permille: (rate.loss() * 1000.0).round() as u16,
                        gpu,
                        perf,
                    };
                    net::write_msg(&mut send, &stats).await?;
                }
            }
            datagram = conn.read_datagram() => {
                let Ok(datagram) = datagram else { return Ok(()); };
                if let Ok(position) = CursorPosition::decode(&datagram) && remote_seq.is_none_or(|seq| position.is_newer_than(seq)) {
                        remote_seq = Some(position.seq);
                        if let Some(cursor) = &cursor { cursor.set_remote_pointer(position); }
                }
            }
            Some(Unavailable { device, reason }) = unavailable.recv() => {
                net::write_msg(&mut send, &HostMsg::InputUnavailable { device, reason }).await?;
            }
            event = video.next() => match event {
                Some(Ok(VideoEvent::Frame(frame, stamp))) => {
                    video_bytes += frame.data.len() as u64;
                    send_frame(conn, &frame, stamp, rate.loss(), rate.bitrate() / 1000, trace)?;
                }
                Some(Ok(VideoEvent::Encoder { name, codec })) => {
                    net::write_msg(&mut send, &HostMsg::VideoEncoder { name, codec }).await?;
                }
                Some(Ok(VideoEvent::Format { width, height })) => {
                    net::write_msg(&mut send, &HostMsg::VideoFormat { width, height }).await?;
                }
                Some(Err(e)) => return Err(e.context("screen capture failed")),
                None => bail!("video thread stopped"),
            },
            _ = conn.closed() => return Ok(()),
        }
    }
}

/// What each end can do with video, for switching codec when one fails.
struct Codecs {
    encoders: Vec<CodecSupport>,
    decoders: Vec<CodecSupport>,
    /// Codecs the viewer reported it couldn't decode after all.
    failed: Vec<Codec>,
}

async fn start_audio(conn: &Connection, send: &mut SendStream, enabled: bool) -> Result<Option<AudioStream>> {
    if !enabled {
        net::write_msg(send, &HostMsg::AudioStatus { active: false, reason: None }).await?;
        return Ok(None);
    }
    let max = conn.max_datagram_size().context("peer does not accept audio datagrams")?;
    match tokio::task::spawn_blocking(move || AudioStream::start(max)).await {
        Ok(Ok(audio)) => {
            net::write_msg(send, &HostMsg::AudioStatus { active: true, reason: None }).await?;
            Ok(Some(audio))
        }
        result => {
            let reason = match result {
                Ok(Err(error)) => format!("Desktop audio unavailable: {error:#}"),
                Err(error) => format!("Desktop audio capture stopped: {error}"),
                _ => unreachable!(),
            };
            warn!(%reason);
            net::write_msg(send, &HostMsg::AudioStatus { active: false, reason: Some(reason) }).await?;
            Ok(None)
        }
    }
}

async fn read_loop(mut recv: RecvStream, tx: mpsc::Sender<Result<ClientMsg>>) {
    loop {
        match net::read_msg::<ClientMsg>(&mut recv).await {
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

/// Sends one frame, with recovery data in proportion to how lossy the path has been.
fn send_frame(
    conn: &Connection,
    frame: &EncodedFrame,
    stamp: FrameStamp,
    loss: f32,
    bitrate_kbps: u32,
    trace: &HostTrace,
) -> Result<()> {
    let started = Instant::now();
    let max = conn.max_datagram_size().context("the client doesn't accept video datagrams")?;
    let data_chunks = frame.data.len().div_ceil(max.saturating_sub(video::HEADER_LEN).max(2) & !1);
    let recovery = video::recovery_count(data_chunks, loss, frame.meta.keyframe);
    let tracing = connect_perf::enabled();
    let queued_bytes = if tracing { net::queued_datagram_bytes(conn) } else { 0 };
    let sent_after = connect_proto::now_micros().saturating_sub(frame.meta.capture_time_us);
    let meta = video::FrameMeta { sent_after_us: sent_after.min(u64::from(u32::MAX)) as u32, ..frame.meta };
    let chunks = video::packetize(meta, &frame.data, max, recovery)?;
    if frame.meta.keyframe {
        debug!(seq = frame.meta.seq, bytes = frame.data.len(), datagrams = chunks.len(), max, "sending keyframe");
    }
    let packetized = Instant::now();
    let result = conn.send_many_datagrams(&chunks);
    if tracing {
        let micros = |d: Duration| d.as_micros().min(u128::from(u32::MAX)) as u32;
        trace.frames.push(FrameRecord {
            seq: meta.seq,
            keyframe: meta.keyframe,
            codec: meta.codec,
            resent: stamp.resent,
            capture: stamp.capture,
            bytes: frame.data.len().min(u32::MAX as usize) as u32,
            capture_time_us: meta.capture_time_us,
            present_to_capture_us: meta.present_to_capture_us,
            capture_us: meta.capture_us,
            held_us: stamp.held_us,
            encode_us: meta.encode_time_us,
            queue_us: micros(started.saturating_duration_since(stamp.encoded)),
            packetize_us: micros(packetized - started),
            send_us: micros(packetized.elapsed()),
            sent_after_us: meta.sent_after_us,
            datagrams: chunks.len().min(u16::MAX as usize) as u16,
            recovery: recovery.min(u16::MAX as usize) as u16,
            queued_bytes: queued_bytes.min(u32::MAX as usize) as u32,
            bitrate_kbps,
            dropped: matches!(result, Err(SendDatagramError::TooLarge)),
        });
    }
    match result {
        Ok(_) => Ok(()),
        // The path MTU shrank; the client will notice the gap and ask for a keyframe.
        Err(SendDatagramError::TooLarge) => {
            debug!(max, "datagram too large for the path; frame dropped");
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

/// Starts streaming the session's trace to CSV files, if a trace directory is set.
fn start_trace(trace: &Arc<HostTrace>) -> Option<connect_perf::Spool> {
    let dir = connect_perf::trace_dir().filter(|_| connect_perf::enabled())?;
    trace.spool(&dir).inspect_err(|error| warn!("couldn't start the performance trace: {error}")).ok()
}

async fn finish_trace(spool: Option<connect_perf::Spool>) {
    let Some(spool) = spool else { return };
    match tokio::task::spawn_blocking(move || spool.finish()).await {
        Ok(()) => info!("performance trace written"),
        Err(error) => warn!("writing the performance trace stopped: {error}"),
    }
}

fn end_reason(conn: &Connection, client_name: &str, result: &Result<()>) -> String {
    match conn.close_reason() {
        Some(ConnectionError::LocallyClosed) => "You ended the session.".into(),
        Some(ConnectionError::ApplicationClosed(_)) => format!("{client_name} disconnected."),
        Some(ConnectionError::TimedOut | ConnectionError::Reset) => format!("Lost the connection to {client_name}."),
        Some(other) => format!("Connection error: {other}"),
        None => match result {
            Ok(()) => format!("{client_name} disconnected."),
            Err(e) => format!("Session error: {e:#}"),
        },
    }
}

/// A peer-supplied name made safe to show: no control characters, bounded length.
fn clean_name(name: &str) -> String {
    let name: String = name.chars().filter(|c| !c.is_control()).take(48).collect();
    let name = name.trim();
    if name.is_empty() { "Unknown device".into() } else { name.to_owned() }
}

#[cfg(test)]
mod tests {
    use super::clean_name;

    #[test]
    fn names_are_sanitised() {
        assert_eq!(clean_name("  Gaming PC \n"), "Gaming PC");
        assert_eq!(clean_name("\u{7}\u{1b}[2J"), "[2J");
        assert_eq!(clean_name(""), "Unknown device");
        assert_eq!(clean_name(&"x".repeat(100)).len(), 48);
    }
}
