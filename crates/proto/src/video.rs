//! Encoded video frames split across QUIC datagrams, with forward error correction.
//!
//! Every datagram carries a fixed [`HEADER_LEN`]-byte header and one chunk of one encoded
//! frame (an Annex-B access unit, in the codec named in its flags). A frame is cut into equal-sized data chunks, followed
//! by a few Reed-Solomon recovery chunks: any `data_count` of the frame's chunks rebuild it, so
//! a lost datagram or two costs nothing. Datagrams are never retransmitted; a frame missing more
//! than its recovery chunks can rebuild is lost, and the [`FrameAssembler`] then withholds what
//! follows until a keyframe (decoding against a missing reference shows garbage) and asks for one.

use std::{
    collections::{BTreeMap, VecDeque},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use bytes::{BufMut, Bytes, BytesMut};

use crate::Codec;
use reed_solomon_simd::{ReedSolomonDecoder, ReedSolomonEncoder};

/// First byte of every video datagram.
const KIND_VIDEO: u8 = 0x56;
const FLAG_KEYFRAME: u8 = 1 << 0;
/// Two bits naming the frame's codec ([`Codec::wire_bits`]).
const CODEC_SHIFT: u8 = 1;
const CODEC_MASK: u8 = 0b11 << CODEC_SHIFT;

/// kind u8 | flags u8 | index u16 | data_count u16 | recovery_count u16 | frame_seq u32 |
/// frame_len u32 | capture_time_us u64 | encode_time_us u32 | present_to_capture_us u32 |
/// capture_us u32 | sent_after_us u32
pub const HEADER_LEN: usize = 40;

/// Frames still being reassembled at once; older partial frames are abandoned beyond this.
const MAX_PARTIAL_FRAMES: usize = 16;
/// How long a complete frame waits for an earlier one still missing chunks (datagrams can
/// arrive out of order) before the earlier one counts as lost.
const REORDER_WAIT: Duration = Duration::from_millis(12);
/// Complete frames held back behind a missing one before it counts as lost regardless.
const REORDER_FRAMES: usize = 4;
/// Recovery chunks a frame gets on a path losing nothing, as a share of its data chunks.
const BASE_RECOVERY: f32 = 0.08;
/// Recovery never exceeds this share of the data, however lossy the path.
const MAX_RECOVERY: f32 = 0.6;

/// One frame's identity and where its time went on the host. Every stage is measured on the
/// host's clock, so the viewer can line them up with its own through the clock offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FrameMeta {
    /// Increments by one per encoded frame.
    pub seq: u32,
    pub keyframe: bool,
    /// What the frame is encoded with. It only changes at a keyframe.
    pub codec: Codec,
    /// Host wall clock when the captured image was ready for the encoder.
    pub capture_time_us: u64,
    /// Time the host spent encoding this frame.
    pub encode_time_us: u32,
    /// From Windows putting the image on the host's screen to capture picking it up; 0 where
    /// unknown.
    pub present_to_capture_us: u32,
    /// Taking the image from the screen and getting it ready for the encoder (copy, colour
    /// conversion).
    pub capture_us: u32,
    /// From [`Self::capture_time_us`] to the frame going out: waiting for the network, encoding,
    /// packetizing.
    pub sent_after_us: u32,
}

#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub meta: FrameMeta,
    pub data: Bytes,
    /// On the viewer: when the frame's first datagram arrived.
    pub first_seen: Option<Instant>,
}

/// Recovery chunks for a frame of `data_count` chunks on a path losing `loss` (0..1) of its
/// packets: enough that losing a few, or a short burst, still rebuilds the frame. Keyframes get
/// more, as losing one costs a whole new keyframe.
pub fn recovery_count(data_count: usize, loss: f32, keyframe: bool) -> usize {
    let share = (BASE_RECOVERY + 3.0 * loss.clamp(0.0, 1.0)).min(MAX_RECOVERY);
    let share = if keyframe { (share * 1.5).min(MAX_RECOVERY) } else { share };
    // At least one, and two once a frame spans a few datagrams, since losses come in pairs.
    let floor = if data_count >= 4 { 2 } else { 1 };
    // Less a hair, so float error can't round an exact product up.
    ((data_count as f32 * share - 1e-3).ceil() as usize).max(floor)
}

/// The size of every chunk of a `frame_len`-byte frame cut into `data_count` chunks: even, as
/// Reed-Solomon needs, and as small as fits.
fn chunk_size(frame_len: usize, data_count: usize) -> usize {
    frame_len.div_ceil(data_count).next_multiple_of(2)
}

/// Splits one encoded frame into datagrams of at most `max_datagram` bytes: its data chunks,
/// then `recovery` recovery chunks (see [`recovery_count`]).
pub fn packetize(meta: FrameMeta, data: &[u8], max_datagram: usize, recovery: usize) -> Result<Vec<Bytes>> {
    // Chunks have an even size, so the room for one is rounded down to even.
    let room = max_datagram.checked_sub(HEADER_LEN).context("datagram size too small")? & !1;
    ensure!(room > 0, "datagram size {max_datagram} too small");
    let data_count = data.len().div_ceil(room).max(1);
    let size = chunk_size(data.len(), data_count);
    // Nothing to protect in an empty frame.
    let recovery = if data.is_empty() { 0 } else { recovery };
    ensure!(
        data_count <= u16::MAX as usize && ReedSolomonEncoder::supports(data_count, recovery.max(1)),
        "frame too large: {} bytes",
        data.len()
    );
    ensure!(u32::try_from(data.len()).is_ok(), "frame too large: {} bytes", data.len());

    let header = |index: usize, body: &[u8]| {
        let mut buf = BytesMut::with_capacity(HEADER_LEN + body.len());
        buf.put_u8(KIND_VIDEO);
        buf.put_u8(if meta.keyframe { FLAG_KEYFRAME } else { 0 } | meta.codec.wire_bits() << CODEC_SHIFT);
        buf.put_u16_le(index as u16);
        buf.put_u16_le(data_count as u16);
        buf.put_u16_le(recovery as u16);
        buf.put_u32_le(meta.seq);
        buf.put_u32_le(data.len() as u32);
        buf.put_u64_le(meta.capture_time_us);
        buf.put_u32_le(meta.encode_time_us);
        buf.put_u32_le(meta.present_to_capture_us);
        buf.put_u32_le(meta.capture_us);
        buf.put_u32_le(meta.sent_after_us);
        buf.put_slice(body);
        buf.freeze()
    };
    let mut datagrams = Vec::with_capacity(data_count + recovery);
    // The last data chunk goes out unpadded; the receiver pads it back for recovery.
    for (index, body) in data.chunks(size.max(1)).enumerate() {
        datagrams.push(header(index, body));
    }
    if data.is_empty() {
        datagrams.push(header(0, &[]));
    }
    if recovery > 0 {
        let mut encoder = ReedSolomonEncoder::new(data_count, recovery, size)?;
        let mut last = vec![0; size];
        for body in data.chunks(size) {
            if body.len() == size {
                encoder.add_original_shard(body)?;
            } else {
                last[..body.len()].copy_from_slice(body);
                encoder.add_original_shard(&last)?;
            }
        }
        for (index, body) in encoder.encode()?.recovery_iter().enumerate() {
            datagrams.push(header(data_count + index, body));
        }
    }
    Ok(datagrams)
}

struct Chunk {
    meta: FrameMeta,
    index: usize,
    data_count: usize,
    recovery_count: usize,
    frame_len: usize,
    body: Bytes,
}

fn parse(datagram: Bytes) -> Result<Chunk> {
    ensure!(datagram.len() >= HEADER_LEN, "short video datagram");
    if datagram[0] != KIND_VIDEO {
        bail!("unknown datagram kind {:#x}", datagram[0]);
    }
    let u16_at = |i: usize| u16::from_le_bytes([datagram[i], datagram[i + 1]]) as usize;
    let u32_at = |i: usize| u32::from_le_bytes(datagram[i..i + 4].try_into().unwrap());
    let u64_at = |i: usize| u64::from_le_bytes(datagram[i..i + 8].try_into().unwrap());
    ensure!(datagram[1] & !(FLAG_KEYFRAME | CODEC_MASK) == 0, "unknown video flags");
    let codec = Codec::from_wire_bits((datagram[1] & CODEC_MASK) >> CODEC_SHIFT).context("unknown video codec")?;
    let chunk = Chunk {
        meta: FrameMeta {
            keyframe: datagram[1] & FLAG_KEYFRAME != 0,
            codec,
            seq: u32_at(8),
            capture_time_us: u64_at(16),
            encode_time_us: u32_at(24),
            present_to_capture_us: u32_at(28),
            capture_us: u32_at(32),
            sent_after_us: u32_at(36),
        },
        index: u16_at(2),
        data_count: u16_at(4),
        recovery_count: u16_at(6),
        frame_len: u32_at(12) as usize,
        body: datagram.slice(HEADER_LEN..),
    };
    ensure!(chunk.data_count > 0 && chunk.index < chunk.data_count + chunk.recovery_count, "bad chunk index");
    ensure!(chunk.frame_len.div_ceil(chunk.data_count.max(1)) <= u16::MAX as usize, "bad frame length");
    ensure!(chunk.frame_len > 0 || (chunk.data_count == 1 && chunk.recovery_count == 0), "bad empty frame");
    // Every chunk but the last data chunk is full size.
    let size = chunk_size(chunk.frame_len, chunk.data_count);
    let expected = if chunk.index + 1 == chunk.data_count {
        chunk.frame_len.checked_sub(size * (chunk.data_count - 1)).context("bad frame length")?
    } else {
        size
    };
    ensure!(chunk.body.len() == expected, "chunk size {} where {expected} was expected", chunk.body.len());
    Ok(chunk)
}

struct Partial {
    meta: FrameMeta,
    first_seen: Instant,
    frame_len: usize,
    data_count: usize,
    chunks: Vec<Option<Bytes>>,
    received: usize,
}

impl Partial {
    fn new(chunk: &Chunk) -> Self {
        Self {
            meta: chunk.meta,
            first_seen: Instant::now(),
            frame_len: chunk.frame_len,
            data_count: chunk.data_count,
            chunks: vec![None; chunk.data_count + chunk.recovery_count],
            received: 0,
        }
    }

    fn matches(&self, chunk: &Chunk) -> bool {
        self.meta == chunk.meta
            && self.frame_len == chunk.frame_len
            && self.data_count == chunk.data_count
            && self.chunks.len() == chunk.data_count + chunk.recovery_count
    }

    /// The whole frame, rebuilding missing data chunks from recovery chunks if need be. Only
    /// called once `data_count` chunks are in.
    fn rebuild(self) -> Result<(EncodedFrame, bool)> {
        let (data, recovery) = self.chunks.split_at(self.data_count);
        let missing = data.iter().filter(|c| c.is_none()).count();
        let mut frame = BytesMut::with_capacity(self.frame_len);
        if missing == 0 {
            for body in data.iter().flatten() {
                frame.put_slice(body);
            }
        } else {
            let size = chunk_size(self.frame_len, self.data_count);
            let mut decoder = ReedSolomonDecoder::new(self.data_count, recovery.len(), size)?;
            let mut last = vec![0; size];
            for (index, body) in data.iter().enumerate() {
                let Some(body) = body else { continue };
                if body.len() == size {
                    decoder.add_original_shard(index, body)?;
                } else {
                    last[..body.len()].copy_from_slice(body);
                    decoder.add_original_shard(index, &last)?;
                }
            }
            for (index, body) in recovery.iter().enumerate() {
                if let Some(body) = body {
                    decoder.add_recovery_shard(index, body)?;
                }
            }
            let restored = decoder.decode()?;
            for (index, body) in data.iter().enumerate() {
                let body = match body {
                    Some(body) => &body[..],
                    None => restored.restored_original(index).context("chunk not restored")?,
                };
                frame.put_slice(body);
            }
            frame.truncate(self.frame_len);
        }
        Ok((EncodedFrame { meta: self.meta, data: frame.freeze(), first_seen: Some(self.first_seen) }, missing > 0))
    }
}

/// Counters for the stats overlay.
#[derive(Debug, Default, Clone, Copy)]
pub struct AssemblerStats {
    pub frames_completed: u64,
    /// Frames that arrived with chunks missing and were rebuilt from recovery chunks.
    pub frames_recovered: u64,
    /// Frames that never completed (more chunks lost than could be rebuilt, or too late).
    pub frames_lost: u64,
    /// Complete frames thrown away because they depended on a lost frame.
    pub frames_skipped: u64,
}

/// Reassembles datagrams into frames, handing them out in order with nothing missing between.
pub struct FrameAssembler {
    partial: BTreeMap<u32, Partial>,
    /// Complete frames waiting for an earlier, still incomplete one; and since when.
    held: BTreeMap<u32, EncodedFrame>,
    held_since: Option<Instant>,
    /// Frames ready to decode, in order.
    ready: VecDeque<EncodedFrame>,
    /// The last frame handed out or given up on; anything at or below it is stale.
    last_seq: Option<u32>,
    /// The newest frame passed over while waiting for a keyframe.
    last_skipped: Option<u32>,
    awaiting_keyframe: bool,
    /// Set when a frame is lost, until [`Self::take_keyframe_request`] collects it.
    keyframe_wanted: bool,
    pub stats: AssemblerStats,
}

impl Default for FrameAssembler {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameAssembler {
    pub fn new() -> Self {
        Self {
            partial: BTreeMap::new(),
            held: BTreeMap::new(),
            held_since: None,
            ready: VecDeque::new(),
            last_seq: None,
            last_skipped: None,
            // The decoder needs SPS/PPS + IDR before anything else.
            awaiting_keyframe: true,
            keyframe_wanted: false,
            stats: AssemblerStats::default(),
        }
    }

    /// True while frames are being withheld until the next keyframe.
    pub fn needs_keyframe(&self) -> bool {
        self.awaiting_keyframe
    }

    /// Withholds frames until the next keyframe, e.g. after the decoder dropped or failed on one.
    pub fn request_keyframe(&mut self) {
        self.awaiting_keyframe = true;
        self.keyframe_wanted = true;
        self.stats.frames_skipped += (self.held.len() + self.ready.len()) as u64;
        self.held.clear();
        self.ready.clear();
        self.held_since = None;
    }

    /// True once after video was lost: the host should be asked for a keyframe.
    pub fn take_keyframe_request(&mut self) -> bool {
        std::mem::take(&mut self.keyframe_wanted)
    }

    /// The next frame safe to decode, in order.
    pub fn next_frame(&mut self) -> Option<EncodedFrame> {
        self.ready.pop_front()
    }

    /// Feeds one datagram; complete frames then come out of [`Self::next_frame`].
    pub fn push(&mut self, datagram: Bytes) -> Result<()> {
        let result = self.push_chunk(datagram);
        self.expire_held(Instant::now());
        result
    }

    fn push_chunk(&mut self, datagram: Bytes) -> Result<()> {
        let chunk = parse(datagram)?;
        let seq = chunk.meta.seq;
        if self.last_seq.is_some_and(|last| seq <= last) || self.held.contains_key(&seq) {
            return Ok(()); // a frame already handed out, given up on, or complete
        }
        if self.awaiting_keyframe && !chunk.meta.keyframe {
            // Undecodable until the next keyframe anyway. Frames still coming without one
            // mean the keyframe went missing too: ask (again; the caller paces requests).
            if self.last_skipped.is_none_or(|last| seq > last) {
                self.last_skipped = Some(seq);
                self.stats.frames_skipped += 1;
                self.keyframe_wanted = true;
            }
            return Ok(());
        }

        let partial = self.partial.entry(seq).or_insert_with(|| Partial::new(&chunk));
        ensure!(partial.matches(&chunk), "inconsistent chunks for frame {seq}");
        let slot = &mut partial.chunks[chunk.index];
        if slot.is_none() {
            *slot = Some(chunk.body);
            partial.received += 1;
        }
        if partial.received < partial.data_count {
            if self.partial.len() > MAX_PARTIAL_FRAMES {
                self.partial.pop_first();
            }
            return Ok(());
        }

        let (frame, recovered) = self.partial.remove(&seq).expect("frame present").rebuild()?;
        self.stats.frames_recovered += recovered as u64;
        self.complete(frame);
        Ok(())
    }

    fn complete(&mut self, frame: EncodedFrame) {
        let seq = frame.meta.seq;
        let next = self.last_seq.map_or(seq, |last| last + 1);
        if frame.meta.keyframe {
            // A keyframe needs nothing before it: anything still missing is lost, and complete
            // frames held for it are now useless.
            if seq > next {
                let held = self.held.range(..seq).count() as u64;
                self.stats.frames_lost += (seq - next) as u64 - held;
                self.stats.frames_skipped += held;
                self.held = self.held.split_off(&seq);
            }
            self.awaiting_keyframe = false;
            self.hand_out(frame);
        } else if self.awaiting_keyframe {
            self.stats.frames_skipped += 1;
            self.last_seq = Some(seq);
        } else if seq == next {
            self.hand_out(frame);
        } else {
            // An earlier frame is still arriving; wait for it a little.
            self.held.insert(seq, frame);
            self.held_since.get_or_insert_with(Instant::now);
            return;
        }
        self.held_since = (!self.held.is_empty()).then(Instant::now);
        self.drop_stale_partials();
    }

    fn drop_stale_partials(&mut self) {
        if let Some(last) = self.last_seq {
            self.partial = self.partial.split_off(&(last + 1));
        }
    }

    /// Hands out `frame` and any held frames that follow on from it.
    fn hand_out(&mut self, frame: EncodedFrame) {
        let mut seq = frame.meta.seq;
        self.stats.frames_completed += 1;
        self.ready.push_back(frame);
        while let Some(frame) = self.held.remove(&(seq + 1)) {
            seq += 1;
            self.stats.frames_completed += 1;
            self.ready.push_back(frame);
        }
        self.last_seq = Some(seq);
    }

    /// Gives up on a frame that complete later frames have waited on for too long.
    fn expire_held(&mut self, now: Instant) {
        let Some(since) = self.held_since else { return };
        if self.held.len() < REORDER_FRAMES && now.duration_since(since) < REORDER_WAIT {
            return;
        }
        let first = *self.held.keys().next().expect("held frames exist while waiting");
        let next = self.last_seq.map_or(first, |last| last + 1);
        self.stats.frames_lost += (first - next) as u64;
        let newest = *self.held.keys().next_back().expect("non-empty");
        self.stats.frames_skipped += self.held.len() as u64;
        self.held.clear();
        self.held_since = None;
        self.last_seq = Some(newest);
        self.drop_stale_partials();
        self.awaiting_keyframe = true;
        self.keyframe_wanted = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(seq: u32, keyframe: bool, len: usize) -> (FrameMeta, Vec<u8>) {
        let meta =
            FrameMeta { seq, keyframe, capture_time_us: 1_000 + seq as u64, encode_time_us: 7, ..Default::default() };
        (meta, (0..len).map(|i| (i * 7 + seq as usize) as u8).collect())
    }

    fn chunks(seq: u32, keyframe: bool, len: usize, recovery: usize) -> Vec<Bytes> {
        let (meta, data) = frame(seq, keyframe, len);
        packetize(meta, &data, 1200, recovery).unwrap()
    }

    fn feed(asm: &mut FrameAssembler, chunks: impl IntoIterator<Item = Bytes>) -> Vec<EncodedFrame> {
        for c in chunks {
            asm.push(c).unwrap();
        }
        std::iter::from_fn(|| asm.next_frame()).collect()
    }

    fn seqs(frames: &[EncodedFrame]) -> Vec<u32> {
        frames.iter().map(|f| f.meta.seq).collect()
    }

    #[test]
    fn reassembles_out_of_order_chunks() {
        let (meta, data) = frame(0, true, 5000);
        let mut c = packetize(meta, &data, 1200, 0).unwrap();
        assert_eq!(c.len(), 5000usize.div_ceil(1200 - HEADER_LEN));
        assert!(c.iter().all(|c| c.len() <= 1200));
        c.reverse();

        let mut asm = FrameAssembler::new();
        let out = feed(&mut asm, c);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].meta, meta);
        assert_eq!(&out[0].data[..], &data[..]);
        assert!(!asm.needs_keyframe());
    }

    #[test]
    fn recovery_chunks_rebuild_any_lost_chunks() {
        for (len, lose) in [(5000, vec![0]), (5000, vec![4]), (5000, vec![1, 3]), (100, vec![0]), (1173, vec![1])] {
            let (meta, data) = frame(0, true, len);
            let all = packetize(meta, &data, 1200, 2).unwrap();
            let mut asm = FrameAssembler::new();
            let kept = all.iter().enumerate().filter(|(i, _)| !lose.contains(i)).map(|(_, c)| c.clone());
            let out = feed(&mut asm, kept);
            assert_eq!(out.len(), 1, "{len} bytes, losing {lose:?}");
            assert_eq!(&out[0].data[..], &data[..]);
            assert_eq!(asm.stats.frames_recovered, 1);
        }
    }

    #[test]
    fn too_many_lost_chunks_lose_the_frame() {
        let mut asm = FrameAssembler::new();
        assert_eq!(seqs(&feed(&mut asm, chunks(0, true, 3000, 1))), [0]);
        let lossy = chunks(1, false, 3000, 1).into_iter().skip(2);
        assert!(feed(&mut asm, lossy).is_empty());
        // Later frames wait a moment for it, then give up and want a keyframe.
        for seq in 2..6 {
            assert!(feed(&mut asm, chunks(seq, false, 3000, 1)).is_empty());
        }
        assert!(asm.needs_keyframe());
        assert!(asm.take_keyframe_request());
        assert!(!asm.take_keyframe_request());
        assert_eq!(asm.stats.frames_lost, 1);
        assert_eq!(asm.stats.frames_skipped, 4);
        assert_eq!(seqs(&feed(&mut asm, chunks(6, true, 3000, 1))), [6]);
        assert!(!asm.needs_keyframe());
        assert_eq!(seqs(&feed(&mut asm, chunks(7, false, 3000, 1))), [7]);
    }

    #[test]
    fn frames_completing_out_of_order_are_handed_out_in_order() {
        // Keyframe 0; the first chunk of 1; all of 2; the rest of 1; then 3. Nothing is lost.
        let mut asm = FrameAssembler::new();
        assert_eq!(seqs(&feed(&mut asm, chunks(0, true, 3000, 0))), [0]);
        let mut one = chunks(1, false, 3000, 0);
        let rest = one.split_off(1);
        assert!(feed(&mut asm, one).is_empty());
        assert!(feed(&mut asm, chunks(2, false, 3000, 0)).is_empty());
        assert_eq!(seqs(&feed(&mut asm, rest)), [1, 2]);
        assert_eq!(seqs(&feed(&mut asm, chunks(3, false, 3000, 0))), [3]);
        assert!(!asm.needs_keyframe());
        assert_eq!(asm.stats.frames_lost, 0);
    }

    #[test]
    fn a_held_frame_gives_up_after_a_short_wait() {
        let mut asm = FrameAssembler::new();
        feed(&mut asm, chunks(0, true, 100, 0));
        assert!(feed(&mut asm, chunks(2, false, 100, 0)).is_empty());
        std::thread::sleep(REORDER_WAIT + Duration::from_millis(2));
        assert!(feed(&mut asm, chunks(3, false, 100, 0)).is_empty());
        assert!(asm.needs_keyframe());
        assert_eq!(asm.stats.frames_lost, 1);
    }

    #[test]
    fn keyframe_skips_past_a_missing_frame() {
        let mut asm = FrameAssembler::new();
        feed(&mut asm, chunks(0, true, 100, 0));
        assert!(feed(&mut asm, chunks(2, false, 100, 0)).is_empty());
        assert_eq!(seqs(&feed(&mut asm, chunks(3, true, 100, 0))), [3]);
        assert_eq!((asm.stats.frames_lost, asm.stats.frames_skipped), (1, 1));
        assert!(!asm.needs_keyframe());
    }

    #[test]
    fn empty_frame_is_one_chunk() {
        let (meta, _) = frame(0, true, 0);
        let c = packetize(meta, &[], 1200, 3).unwrap();
        assert_eq!(c.len(), 1);
        let out = feed(&mut FrameAssembler::new(), c);
        assert!(out[0].data.is_empty());
    }

    #[test]
    fn waits_for_first_keyframe() {
        let mut asm = FrameAssembler::new();
        assert!(feed(&mut asm, chunks(0, false, 100, 1)).is_empty());
        assert!(asm.needs_keyframe());
        let got = feed(&mut asm, chunks(1, true, 100, 1));
        assert!(got[0].meta.keyframe);
    }

    #[test]
    fn wholly_missing_frame_is_a_gap() {
        let mut asm = FrameAssembler::new();
        feed(&mut asm, chunks(0, true, 10, 0));
        for seq in 2..2 + REORDER_FRAMES as u32 {
            feed(&mut asm, chunks(seq, false, 10, 0));
        }
        assert!(asm.needs_keyframe());
        assert_eq!(asm.stats.frames_lost, 1);
    }

    #[test]
    fn late_chunks_are_ignored() {
        let mut asm = FrameAssembler::new();
        let c = chunks(0, true, 10, 1);
        assert_eq!(feed(&mut asm, [c[0].clone()]).len(), 1);
        assert!(feed(&mut asm, c).is_empty());
    }

    #[test]
    fn recovery_grows_with_loss_and_size() {
        assert_eq!(recovery_count(1, 0.0, false), 1);
        assert_eq!(recovery_count(10, 0.0, false), 2);
        assert_eq!(recovery_count(100, 0.0, false), 8);
        assert_eq!(recovery_count(100, 0.05, false), 23);
        assert_eq!(recovery_count(100, 0.05, true), 35);
        assert_eq!(recovery_count(100, 1.0, false), 60);
    }

    /// Random loss and reordering: whatever comes out decodes (each frame follows the previous
    /// one or is a keyframe) and is intact, and a little loss costs no frames.
    #[test]
    fn lossy_reordering_path_never_breaks_the_chain() {
        let mut seed = 0x2545F4914F6CDD1Du64;
        let mut random = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for loss in [0.01, 0.05, 0.3] {
            let mut asm = FrameAssembler::new();
            let (mut out, mut in_flight) = (Vec::new(), Vec::new());
            for seq in 0..600u32 {
                let key = seq % 200 == 0 || asm.take_keyframe_request();
                let len = if key { 40_000 } else { 1_000 + (random() % 9_000) as usize };
                let (meta, data) = frame(seq, key, len);
                let datagrams = packetize(meta, &data, 1200, recovery_count(len.div_ceil(1172), loss, key)).unwrap();
                in_flight.extend(datagrams.into_iter().filter(|_| random() % 1000 >= (loss * 1000.0) as u64));
                // Deliver most of what's in flight, now and then swapping neighbours.
                for i in 1..in_flight.len() {
                    if random() % 50 == 0 {
                        in_flight.swap(i - 1, i);
                    }
                }
                let keep = in_flight.split_off(in_flight.len() * 4 / 5);
                out.extend(feed(&mut asm, std::mem::replace(&mut in_flight, keep)));
            }
            out.extend(feed(&mut asm, in_flight));
            for pair in out.windows(2) {
                assert!(pair[1].meta.keyframe || pair[1].meta.seq == pair[0].meta.seq + 1, "chain broken");
            }
            for f in &out {
                assert_eq!(f.data, frame(f.meta.seq, f.meta.keyframe, f.data.len()).1);
            }
            if loss <= 0.01 {
                assert!(out.len() >= 590, "{} of 600 frames at {loss} loss", out.len());
                assert!(asm.stats.frames_recovered > 0);
            }
        }
    }

    #[test]
    fn rejects_garbage() {
        let mut asm = FrameAssembler::new();
        assert!(asm.push(Bytes::from_static(b"nope")).is_err());
        assert!(asm.push(Bytes::from(vec![0u8; HEADER_LEN])).is_err());
        let mut bad = chunks(0, true, 3000, 1)[0].to_vec();
        bad.pop();
        assert!(asm.push(Bytes::from(bad)).is_err());
    }

    #[test]
    fn codec_travels_in_the_flags() {
        let meta =
            |codec| FrameMeta { keyframe: true, codec, capture_time_us: 5, encode_time_us: 1, ..Default::default() };
        for codec in Codec::BY_EFFICIENCY {
            let mut asm = FrameAssembler::new();
            for datagram in packetize(meta(codec), &[1, 2, 3, 4], 1200, 0).unwrap() {
                asm.push(datagram).unwrap();
            }
            assert_eq!(asm.next_frame().unwrap().meta.codec, codec);
        }
        let good = packetize(meta(Codec::H264), &[1, 2, 3, 4], 1200, 0).unwrap().remove(0);
        for flags in [FLAG_KEYFRAME | 3 << CODEC_SHIFT, FLAG_KEYFRAME | 1 << 4] {
            let mut bad = good.to_vec();
            bad[1] = flags;
            assert!(FrameAssembler::new().push(bad.into()).is_err(), "flags {flags:#b}");
        }
    }
}
