//! Datagrams → whole frames (async task) → decoded I420 (decoder thread).

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
    now_micros,
    video::{EncodedFrame, FrameAssembler},
};
use iroh::endpoint::Connection;
use openh264::{decoder::Decoder, formats::YUVSource};
use tokio::sync::mpsc::UnboundedSender;
use tracing::debug;

use crate::{Shared, VideoFrame, lock};

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

pub(crate) fn start_decoder(shared: Arc<Shared>) -> Result<DecoderQueue> {
    let (tx, rx) = std_mpsc::sync_channel::<EncodedFrame>(DECODE_QUEUE);
    let queued = Arc::new(AtomicUsize::new(0));
    let mut decoder = Decoder::new().context("starting H.264 decoder")?;
    let pending = queued.clone();
    std::thread::Builder::new()
        .name("connect-decode".into())
        .spawn(move || {
            let mut shown_at = Instant::now();
            while let Ok(frame) = rx.recv() {
                let behind = pending.fetch_sub(1, Ordering::AcqRel) > 1;
                // Behind: decode this one only to build on it, and show a newer one.
                let show = !behind || shown_at.elapsed() >= MAX_DISPLAY_GAP;
                if decode(&mut decoder, &frame, &shared, show) && show {
                    shown_at = Instant::now();
                }
            }
        })
        .context("starting decoder thread")?;
    Ok(DecoderQueue { frames: tx, queued })
}

/// Decodes `frame`, publishing the picture if `show`. False if it didn't decode.
fn decode(decoder: &mut Decoder, frame: &EncodedFrame, shared: &Shared, show: bool) -> bool {
    let started = Instant::now();
    let yuv = match decoder.decode(&frame.data) {
        Ok(Some(yuv)) => yuv,
        Ok(None) => return false,
        Err(e) => {
            debug!(seq = frame.meta.seq, "decode failed: {e}");
            shared.decoder_failed.store(true, Ordering::Relaxed);
            return false;
        }
    };
    let c = &shared.counters;
    if c.frames_decoded.fetch_add(1, Ordering::Relaxed) == 0 {
        debug!(seq = frame.meta.seq, bytes = frame.data.len(), "first frame decoded");
    }
    if !show {
        c.frames_superseded.fetch_add(1, Ordering::Relaxed);
        return true;
    }

    // Tightly packed planes, in a buffer the GUI handed back after showing an earlier frame.
    let (w, h) = yuv.dimensions();
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let mut buf = lock(&shared.spare_buffers).pop().unwrap_or_default();
    buf.resize(w * h + 2 * cw * ch, 0);
    let (y, chroma) = buf.split_at_mut(w * h);
    let (u, v) = chroma.split_at_mut(cw * ch);
    let (ys, us, vs) = yuv.strides();
    for (dst, src, stride, width) in [(y, yuv.y(), ys, w), (u, yuv.u(), us, cw), (v, yuv.v(), vs, cw)] {
        for (row, line) in dst.chunks_exact_mut(width).enumerate() {
            line.copy_from_slice(&src[row * stride..][..width]);
        }
    }

    c.decode_us.store(started.elapsed().as_micros() as u64, Ordering::Relaxed);
    c.encode_us.store(frame.meta.encode_time_us as u64, Ordering::Relaxed);
    let offset = c.clock_offset_us.load(Ordering::Relaxed);
    if offset != i64::MIN {
        let latency = now_micros() as i64 - (frame.meta.capture_time_us as i64 + offset);
        c.latency_us.store(latency, Ordering::Relaxed);
    }
    let frame = VideoFrame { width: w as u32, height: h as u32, yuv: buf, meta: frame.meta };
    // A frame the GUI never got to is simply replaced; its buffer is reused.
    if let Some(unseen) = lock(&shared.frame).replace(frame) {
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
