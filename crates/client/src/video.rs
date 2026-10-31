//! Datagrams → whole frames (async task) → decoded I420 (decoder thread, on the graphics card
//! where it can, otherwise on the processor).

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc::{self as std_mpsc, TrySendError},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use connect_proto::{
    Codec,
    video::{EncodedFrame, FrameAssembler},
};
use iroh::endpoint::Connection;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, info, warn};

use crate::{
    FrameTiming, Shared, VideoFrame,
    decoder::{self, VideoDecoder},
    lock,
};

/// Complete frames waiting for the decoder. Every frame has to be decoded, as the next one
/// builds on it, but a decoder that is briefly behind only shows the newest; past this many it
/// has fallen too far behind, and skipping to a fresh keyframe beats showing ever-older video.
const DECODE_QUEUE: usize = 24;
/// Longest the picture goes without an update while the decoder catches up.
const MAX_DISPLAY_GAP: Duration = Duration::from_millis(20);

/// Hands complete frames to the decoder thread.
pub(crate) struct DecoderQueue {
    frames: std_mpsc::SyncSender<EncodedFrame>,
    /// Frames sent but not yet picked up by the decoder.
    queued: Arc<AtomicUsize>,
}

impl DecoderQueue {
    /// Queues `frame`; false if the decoder is too far behind to take it.
    fn send(&self, frame: EncodedFrame) -> Result<bool, ()> {
        self.queued.fetch_add(1, Ordering::AcqRel);
        match self.frames.try_send(frame) {
            Ok(()) => Ok(true),
            Err(TrySendError::Full(_)) => {
                self.queued.fetch_sub(1, Ordering::AcqRel);
                Ok(false)
            }
            Err(TrySendError::Disconnected(_)) => Err(()),
        }
    }
}

/// Starts the decoder thread. A codec it finds it can't decode goes to `failed`, for the host
/// to switch away from.
pub(crate) fn start_decoder(shared: Arc<Shared>, failed: UnboundedSender<Codec>) -> Result<DecoderQueue> {
    let (tx, rx) = std_mpsc::sync_channel::<EncodedFrame>(DECODE_QUEUE);
    let queued = Arc::new(AtomicUsize::new(0));
    let pending = queued.clone();
    std::thread::Builder::new()
        .name("connect-decode".into())
        .spawn(move || {
            // Opened ahead of the first frame, as the graphics card takes a moment: H.264, the
            // codec every session can use. Another codec gets its own decoder when it arrives.
            let mut decoder = match decoder::open(Codec::H264, hardware_allowed()) {
                Ok(decoder) => install(&shared, decoder),
                Err(e) => return warn!("no video decoder: {e:#}"),
            };
            let mut shown_at = Instant::now();
            while let Ok(frame) = rx.recv() {
                let behind = pending.fetch_sub(1, Ordering::AcqRel) > 1;
                // Behind: decode this one only to build on it, and show a newer one.
                let show = !behind || shown_at.elapsed() >= MAX_DISPLAY_GAP;
                if decode(&mut decoder, &frame, &shared, &failed, show) && show {
                    shown_at = Instant::now();
                }
            }
        })
        .context("starting decoder thread")?;
    Ok(DecoderQueue { frames: tx, queued })
}

fn hardware_allowed() -> bool {
    crate::HARDWARE_DECODING.load(Ordering::Relaxed)
}

/// Makes `decoder` the session's, and says so.
fn install(shared: &Shared, decoder: Box<dyn VideoDecoder>) -> Box<dyn VideoDecoder> {
    info!(codec = %decoder.codec(), "{}", decoder.label());
    *lock(&shared.decoder) = decoder.label();
    shared.gpu_decoding.store(decoder.hardware(), Ordering::Relaxed);
    decoder
}

/// Decodes `frame`, publishing the picture if `show`. False if it didn't decode.
fn decode(
    decoder: &mut Box<dyn VideoDecoder>,
    frame: &EncodedFrame,
    shared: &Shared,
    failed: &UnboundedSender<Codec>,
    show: bool,
) -> bool {
    let codec = frame.meta.codec;
    let give_up = |why: String| {
        warn!("{why}; asking the host for another codec");
        let _ = failed.send(codec);
        shared.decoder_failed.store(true, Ordering::Relaxed);
        false
    };
    if decoder.codec() != codec {
        // The host changes codec only at a keyframe; anything before it can't be decoded.
        if !frame.meta.keyframe {
            shared.decoder_failed.store(true, Ordering::Relaxed);
            return false;
        }
        match decoder::open(codec, hardware_allowed()) {
            Ok(opened) => *decoder = install(shared, opened),
            Err(e) => return give_up(format!("can't decode {codec}: {e:#}")),
        }
    }
    let started = Instant::now();
    // Tightly packed planes, in a buffer the GUI handed back after showing an earlier frame.
    let mut buf = show.then(|| lock(&shared.spare_buffers).pop().unwrap_or_default());
    let mut result = decoder.decode(&frame.data, buf.as_mut());
    if let Err(e) = &result
        && decoder.hardware()
        && frame.meta.keyframe
    {
        // A keyframe needs nothing before it, so failing one means the graphics card can't
        // decode this stream at all: carry on on the processor, starting with this keyframe,
        // or with another codec if this one has no software decoder.
        warn!("hardware {codec} decoding failed on a keyframe: {e:#}");
        match decoder::software(codec) {
            Ok(software) => {
                *decoder = install(shared, software);
                result = decoder.decode(&frame.data, buf.as_mut());
            }
            Err(e) => {
                if let Some(buf) = buf {
                    crate::recycle(shared, buf);
                }
                return give_up(format!("{e:#}"));
            }
        }
    }
    let (w, h) = match result {
        Ok(Some(size)) => size,
        other => {
            if let Err(e) = other {
                debug!(seq = frame.meta.seq, "decode failed: {e:#}");
                shared.decoder_failed.store(true, Ordering::Relaxed);
            }
            if let Some(buf) = buf {
                crate::recycle(shared, buf);
            }
            return false;
        }
    };
    let c = &shared.counters;
    if c.frames_decoded.fetch_add(1, Ordering::Relaxed) == 0 {
        debug!(seq = frame.meta.seq, bytes = frame.data.len(), "first frame decoded");
    }
    let Some(buf) = buf else {
        c.frames_superseded.fetch_add(1, Ordering::Relaxed);
        return true;
    };

    c.decode_us.store(started.elapsed().as_micros() as u64, Ordering::Relaxed);
    c.encode_us.store(frame.meta.encode_time_us as u64, Ordering::Relaxed);
    let timing = FrameTiming {
        meta: frame.meta,
        first_seen: frame.first_seen,
        decode_started: Some(started),
        decoded: Some(Instant::now()),
        painted: None,
    };
    let frame = VideoFrame { width: w as u32, height: h as u32, yuv: buf, meta: frame.meta, timing };
    // Frames the GUI never got to are simply replaced; their buffers are reused.
    let offset = c.clock_offset_us.load(Ordering::Relaxed);
    let smooth = crate::SMOOTH_PACING.load(Ordering::Relaxed);
    let replaced = lock(&shared.frame).push(frame, (offset != i64::MIN).then_some(offset), smooth);
    for unseen in replaced {
        c.frames_superseded.fetch_add(1, Ordering::Relaxed);
        crate::recycle(shared, unseen.yuv);
    }
    (shared.wake)();
    true
}

/// Reassembles datagrams into frames for the decoder, asking for a keyframe after any loss.
pub(crate) async fn receive(
    conn: Connection,
    decoder: DecoderQueue,
    keyframes: UnboundedSender<()>,
    shared: Arc<Shared>,
) {
    let mut assembler = FrameAssembler::new();
    while let Ok(datagram) = conn.read_datagram().await {
        shared.counters.bytes.fetch_add(datagram.len() as u64, Ordering::Relaxed);
        match datagram.first().copied() {
            Some(connect_proto::audio::KIND_AUDIO) => {
                if shared.audio_allowed.load(Ordering::Acquire) {
                    let playback = lock(&shared.playback).clone();
                    if let Some(playback) = playback
                        && let Err(error) = playback.push_datagram(&datagram)
                    {
                        shared.audio_allowed.store(false, Ordering::Release);
                        *lock(&shared.audio_failure) = Some(format!("Desktop audio playback stopped: {error:#}"));
                    }
                }
                continue;
            }
            Some(connect_proto::cursor::KIND_CURSOR) => {
                if let Ok(position) = connect_proto::CursorPosition::decode(&datagram) {
                    let mut cursor = lock(&shared.cursor);
                    if position.is_newer_than(cursor.0.seq) || cursor.2.elapsed() > std::time::Duration::from_secs(1) {
                        let moved =
                            (position.x, position.y, position.visible) != (cursor.0.x, cursor.0.y, cursor.0.visible);
                        cursor.0 = position;
                        cursor.2 = Instant::now();
                        drop(cursor);
                        if moved {
                            (shared.wake)();
                        }
                    }
                }
                continue;
            }
            _ => {}
        }
        if let Err(e) = assembler.push(datagram) {
            debug!("bad video datagram: {e:#}");
        }
        if shared.decoder_failed.swap(false, Ordering::Relaxed) {
            assembler.request_keyframe();
        }
        while let Some(frame) = assembler.next_frame() {
            match decoder.send(frame) {
                Ok(true) => {}
                // The decoder is too far behind to catch up: skip to a fresh keyframe.
                Ok(false) => assembler.request_keyframe(),
                Err(()) => return,
            }
        }
        if assembler.take_keyframe_request() {
            let _ = keyframes.send(());
        }
        let stats = assembler.stats;
        let c = &shared.counters;
        c.frames_lost.store(stats.frames_lost, Ordering::Relaxed);
        c.frames_recovered.store(stats.frames_recovered, Ordering::Relaxed);
    }
}
