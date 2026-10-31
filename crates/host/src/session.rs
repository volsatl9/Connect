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
    encode::{VideoEvent, VideoStream},
    input::{self, Unavailable},
    rate::{self, RateControl},
};

/// An unchanged pointer position is sent again this often, so the viewer knows it's current.
const CURSOR_REPEAT: Duration = Duration::from_millis(250);

/// How long a new connection may take to say hello.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// How long to wait for a turned-away client to read the reason and hang up itself.
const REJECT_LINGER: Duration = Duration::from_secs(3);
/// How long a finished session may take to release the remote user's input.
const INPUT_RELEASE: Duration = Duration::from_secs(2);

const DECLINED: &str = "The host declined the connection.";
const NO_ANSWER: &str = "The host didn't answer in time.";
const BUSY: &str = "The host is in another session right now.";
const NOT_ACCEPTING: &str = "The host isn't accepting connections right now.";
const NO_CAPTURE: &str = "The host couldn't capture its screen.";
const NO_COMMON_CODEC: &str = "The two PCs have no video format in common. Update both to the same version of Connect.";
const CANNOT_HOST: &str = "That PC can't be shared: hosting needs Windows. It can still connect to others.";

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
             Update both PCs to the same version."
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

    let granted = if shared.is_trusted(&peer) {
        shared.default_permissions()
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
    let mut video =
        VideoStream::start(shared.video.clone(), codec, rate.bitrate(), Box::new(rate.backlog_check(conn.clone())))?;
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
    let input =
        tokio::spawn(input::serve(conn.clone(), shared.permissions.subscribe(), shared.input.clone(), unavailable_tx));

    let desktop = shared.video == VideoSource::Screen;
    let codecs = Codecs { encoders, decoders, failed: Vec::new() };
    let result = stream(conn, send, recv, video, codecs, rate, permissions, unavailable, desktop).await;
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
                if let Some(bps) = rate.update(Instant::now(), sent, lost, queued) {
                    debug!(mbps = bps as f32 / 1e6, loss = rate.loss(), queued, "video bitrate changed");
                    video.set_bitrate(bps);
                }
                rate_ticks += 1;
                if rate_ticks.is_multiple_of(4) {
                    let stats = HostMsg::VideoStats {
                        bitrate_kbps: rate.bitrate() / 1000,
                        loss_permille: (rate.loss() * 1000.0).round() as u16,
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
                Some(Ok(VideoEvent::Frame(frame))) => send_frame(conn, &frame, rate.loss())?,
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
fn send_frame(conn: &Connection, frame: &EncodedFrame, loss: f32) -> Result<()> {
    let max = conn.max_datagram_size().context("the client doesn't accept video datagrams")?;
    let data_chunks = frame.data.len().div_ceil(max.saturating_sub(video::HEADER_LEN).max(2) & !1);
    let recovery = video::recovery_count(data_chunks, loss, frame.meta.keyframe);
    let chunks = video::packetize(frame.meta, &frame.data, max, recovery)?;
    if frame.meta.keyframe {
        debug!(seq = frame.meta.seq, bytes = frame.data.len(), datagrams = chunks.len(), max, "sending keyframe");
    }
    match conn.send_many_datagrams(&chunks) {
        Ok(_) => Ok(()),
        // The path MTU shrank; the client will notice the gap and ask for a keyframe.
        Err(SendDatagramError::TooLarge) => {
            debug!(max, "datagram too large for the path; frame dropped");
            Ok(())
        }
        Err(e) => Err(e.into()),
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
