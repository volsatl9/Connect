//! The viewer's half of the performance trace: a record per decoded frame (shown or not) and
//! per input event sent, kept in bounded rings; summarised into percentiles for the stats
//! overlay each second, and written as CSV on request or when the session ends.
//!
//! Two clocks appear here and are kept apart. Host times (`host_*`) come in each frame's header,
//! on the host's clock; viewer times (`viewer_*`) are this PC's
//! ([`connect_proto::micros_at`]). The only figures that span both, network and total, go
//! through the ping-estimated [`ClockOffset`] and are marked as estimates. See METRICS.md.

use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
    sync::Arc,
    sync::atomic::{AtomicU32, Ordering},
    time::Instant,
};

use connect_net::PathKind;
use connect_perf::{
    CsvRecord, CsvStream, Percentiles, SessionRow, Spool, Spread, TraceRing, cell, percentiles, spread,
};
use connect_proto::{
    micros_at,
    video::{Fate, FrameMeta},
};

use crate::{ClockOffset, FrameTiming};

/// One decoded frame.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct FrameRecord {
    /// The host's figures, on its clock.
    pub meta: FrameMeta,
    pub bytes: u32,
    /// Decoded on the graphics card.
    pub hardware: bool,
    // On this PC's clock, µs; 0 where unknown.
    pub first_seen_us: u64,
    pub completed_us: u64,
    pub released_us: u64,
    pub decode_start_us: u64,
    pub decoded_us: u64,
    /// When drawing it into the window was recorded; 0 if it never was (replaced first).
    pub painted_us: u64,
    /// The same moment on Windows' performance counter, for joining with PresentMon.
    pub painted_qpc: u64,
    pub rebuild_us: u32,
    pub data_chunks: u16,
    pub recovery_chunks: u16,
    pub rebuilt_chunks: u16,
    /// The clock estimate when the frame was recorded.
    pub clock: Option<ClockOffset>,
}

impl FrameRecord {
    pub fn new(timing: &FrameTiming, clock: Option<ClockOffset>) -> Self {
        let us = |instant: Option<Instant>| instant.map_or(0, micros_at);
        let arrival = timing.arrival;
        Self {
            meta: timing.meta,
            bytes: timing.bytes,
            hardware: timing.hardware,
            first_seen_us: us(arrival.map(|a| a.first_seen)),
            completed_us: us(arrival.map(|a| a.completed)),
            released_us: us(arrival.map(|a| a.released)),
            decode_start_us: us(timing.decode_started),
            decoded_us: us(timing.decoded),
            painted_us: us(timing.painted),
            painted_qpc: timing.painted_qpc,
            rebuild_us: arrival.map_or(0, |a| a.rebuild_us),
            data_chunks: arrival.map_or(0, |a| a.data_chunks),
            recovery_chunks: arrival.map_or(0, |a| a.recovery_chunks),
            rebuilt_chunks: arrival.map_or(0, |a| a.rebuilt_chunks),
            clock,
        }
    }

    pub fn shown(&self) -> bool {
        self.painted_us > 0
    }

    /// `to - from` on this clock, if both are known and in order.
    fn span(from: u64, to: u64) -> Option<i64> {
        (from > 0 && to >= from).then(|| (to - from) as i64)
    }

    /// First datagram to the whole frame being rebuilt.
    pub fn reassembly(&self) -> Option<i64> {
        Self::span(self.first_seen_us, self.completed_us)
    }

    /// Rebuilt to decoding starting: waiting for an earlier frame, then in the decoder's queue.
    pub fn decode_queue(&self) -> Option<i64> {
        Self::span(self.completed_us, self.decode_start_us)
    }

    pub fn decode(&self) -> Option<i64> {
        Self::span(self.decode_start_us, self.decoded_us)
    }

    /// Decoded to drawn into the window.
    pub fn render(&self) -> Option<i64> {
        Self::span(self.decoded_us, self.painted_us)
    }

    /// The host handing the frame to the network (approximately: the header's `sent_after`
    /// is taken just before packetizing) to its first datagram arriving here. An estimate:
    /// it goes through the clock offset, and carries its uncertainty.
    pub fn network_estimate(&self) -> Option<i64> {
        let clock = self.clock?;
        let sent = self.meta.capture_time_us as i64 + i64::from(self.meta.sent_after_us);
        (self.first_seen_us > 0).then(|| self.first_seen_us as i64 - clock.offset_us - sent)
    }

    /// The image reaching the host's screen (or, where the host can't tell, capture starting)
    /// to it being drawn here. An estimate, like [`Self::network_estimate`].
    pub fn total_estimate(&self) -> Option<i64> {
        let clock = self.clock?;
        let before_staged = i64::from(self.meta.present_to_capture_us) + i64::from(self.meta.capture_us);
        self.shown()
            .then(|| self.painted_us as i64 - clock.offset_us - self.meta.capture_time_us as i64 + before_staged)
    }
}

impl CsvRecord for FrameRecord {
    const HEADER: &'static str = "seq,keyframe,codec,bytes,status,decoder,\
host_capture_time_us,host_present_to_capture_us,host_capture_us,host_encode_us,host_sent_after_us,\
viewer_first_seen_us,viewer_completed_us,viewer_released_us,viewer_decode_start_us,viewer_decoded_us,\
viewer_painted_us,viewer_painted_qpc,data_chunks,recovery_chunks,rebuilt_chunks,rebuild_us,\
reassembly_us,decode_queue_us,decode_us,render_us,clock_offset_us,clock_uncertainty_us,\
network_est_us,total_est_us";

    fn write_row(&self, out: &mut String) {
        let m = &self.meta;
        let _ = write!(
            out,
            "{},{},{},{},{},{},{},",
            m.seq,
            m.keyframe as u8,
            m.codec,
            self.bytes,
            if self.shown() { "shown" } else { "superseded" },
            if self.hardware { "gpu" } else { "cpu" },
            m.capture_time_us
        );
        cell(out, (m.present_to_capture_us > 0).then_some(m.present_to_capture_us));
        let _ = write!(out, "{},{},{},", m.capture_us, m.encode_time_us, m.sent_after_us);
        let known = |us: u64| (us > 0).then_some(us);
        for us in [self.first_seen_us, self.completed_us, self.released_us, self.decode_start_us, self.decoded_us] {
            cell(out, known(us));
        }
        cell(out, known(self.painted_us));
        cell(out, known(self.painted_qpc));
        let _ =
            write!(out, "{},{},{},{},", self.data_chunks, self.recovery_chunks, self.rebuilt_chunks, self.rebuild_us);
        for span in [self.reassembly(), self.decode_queue(), self.decode(), self.render()] {
            cell(out, span);
        }
        cell(out, self.clock.map(|c| c.offset_us));
        cell(out, self.clock.map(|c| c.uncertainty_us));
        cell(out, self.network_estimate());
        if let Some(total) = self.total_estimate() {
            let _ = write!(out, "{total}");
        }
    }
}

/// A frame that never reached the decoder, and why: lost on the way (and whether its datagrams
/// were merely late), or skipped here.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LossRecord {
    pub fate: Fate,
}

impl CsvRecord for LossRecord {
    const HEADER: &'static str = "seq,keyframe,reason,lost,chunks_received,chunks_needed,viewer_first_seen_us,\
viewer_last_seen_us,viewer_given_up_us,late_datagrams,viewer_last_late_us";

    fn write_row(&self, out: &mut String) {
        let f = &self.fate;
        let _ = write!(out, "{},{},{},{},{},", f.seq, f.keyframe as u8, f.gone.name(), f.gone.lost() as u8, f.chunks);
        cell(out, (f.needed > 0).then_some(f.needed));
        cell(out, f.first_seen.map(micros_at));
        cell(out, f.last_seen.map(micros_at));
        let _ = write!(out, "{},{},", micros_at(f.given_up), f.late);
        if let Some(late) = f.last_late {
            let _ = write!(out, "{}", micros_at(late));
        }
    }
}

/// The connection as this side saw it, once a stats window (about a second).
#[derive(Debug, Clone, Copy)]
pub(crate) struct NetworkRecord {
    pub time_us: u64,
    pub path: PathKind,
    pub rtt_us: Option<u32>,
    pub mbps: f32,
    pub fps: f32,
    /// Running totals.
    pub frames_lost: u64,
    pub frames_skipped: u64,
    pub frames_recovered: u64,
    pub frames_superseded: u64,
    pub host_kbps: Option<u32>,
    pub loss_permille: Option<u32>,
    pub clock: Option<ClockOffset>,
    pub gpu: Option<connect_gpu_load::GpuLoad>,
}

impl CsvRecord for NetworkRecord {
    const HEADER: &'static str = "viewer_time_us,path,rtt_us,mbps,fps,frames_lost,frames_skipped,frames_repaired,\
frames_superseded,host_target_kbps,host_loss_permille,clock_offset_us,clock_uncertainty_us,gpu_graphics_percent,\
gpu_decode_percent";

    fn write_row(&self, out: &mut String) {
        let path = match self.path {
            PathKind::Direct => "direct",
            PathKind::Relay => "relay",
            PathKind::Unknown => "unknown",
        };
        let _ = write!(out, "{},{path},", self.time_us);
        cell(out, self.rtt_us);
        let _ = write!(
            out,
            "{:.2},{:.1},{},{},{},{},",
            self.mbps, self.fps, self.frames_lost, self.frames_skipped, self.frames_recovered, self.frames_superseded
        );
        cell(out, self.host_kbps);
        cell(out, self.loss_permille);
        cell(out, self.clock.map(|c| c.offset_us));
        cell(out, self.clock.map(|c| c.uncertainty_us));
        cell(out, self.gpu.map(|g| g.graphics));
        if let Some(decode) = self.gpu.and_then(|g| g.decode) {
            let _ = write!(out, "{decode}");
        }
    }
}

/// One input event sent to the host.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct InputRecord {
    pub seq: u32,
    pub kind: &'static str,
    /// When it was handed to the session, on this clock.
    pub sent_us: u64,
    pub clock: Option<ClockOffset>,
}

impl CsvRecord for InputRecord {
    const HEADER: &'static str = "seq,kind,viewer_sent_us,clock_offset_us,clock_uncertainty_us";

    fn write_row(&self, out: &mut String) {
        let _ = write!(out, "{},{},{},", self.seq, self.kind, self.sent_us);
        cell(out, self.clock.map(|c| c.offset_us));
        if let Some(clock) = self.clock {
            let _ = write!(out, "{}", clock.uncertainty_us);
        }
    }
}

/// A session's trace.
pub(crate) struct ViewerTrace {
    pub frames: TraceRing<FrameRecord>,
    pub inputs: TraceRing<InputRecord>,
    pub losses: TraceRing<LossRecord>,
    pub network: TraceRing<NetworkRecord>,
    /// When the session started, in Unix seconds, to name its files.
    session: u64,
    /// Saves made during the session, which number their files.
    saves: AtomicU32,
}

impl ViewerTrace {
    pub fn new() -> Self {
        let capacity = connect_perf::ring_capacity();
        Self {
            frames: TraceRing::new(capacity),
            inputs: TraceRing::new(capacity),
            losses: TraceRing::new(capacity),
            network: TraceRing::new(capacity),
            session: connect_perf::session_stamp(),
            saves: AtomicU32::new(0),
        }
    }

    pub fn dropped(&self) -> u64 {
        self.frames.dropped_busy()
            + self.inputs.dropped_busy()
            + self.losses.dropped_busy()
            + self.network.dropped_busy()
    }

    /// Writes what the rings hold to `dir` as CSV; returns the files written. Blocks on file
    /// I/O. These saves are numbered (`viewer-<session>-save1-frames.csv`), apart from the files
    /// streamed as the session goes, which [`Self::spool`] writes.
    pub fn save(&self, dir: &Path) -> std::io::Result<Vec<PathBuf>> {
        let save = self.saves.fetch_add(1, Ordering::Relaxed) + 1;
        let path = |what: &str| connect_perf::trace_path(dir, "viewer", self.session, &format!("save{save}-{what}"));
        let files = [path("frames"), path("input"), path("loss"), path("network"), path("session")];
        connect_perf::write_csv(&files[0], &self.frames.snapshot())?;
        connect_perf::write_csv(&files[1], &self.inputs.snapshot())?;
        connect_perf::write_csv(&files[2], &self.losses.snapshot())?;
        connect_perf::write_csv(&files[3], &self.network.snapshot())?;
        connect_perf::write_csv(&files[4], &self.summary(0))?;
        Ok(files.into())
    }

    fn summary(&self, overrun: u64) -> [SessionRow; 6] {
        [
            SessionRow("version", env!("CARGO_PKG_VERSION").into()),
            SessionRow("session_unix_s", self.session.to_string()),
            SessionRow("qpc_frequency_hz", connect_perf::qpc_frequency().to_string()),
            SessionRow("frames_recorded", self.frames.written().to_string()),
            SessionRow("records_overrun", overrun.to_string()),
            SessionRow("records_dropped_busy", self.dropped().to_string()),
        ]
    }

    /// Starts writing the trace to CSV files in `dir` as it goes.
    pub fn spool(self: &Arc<Self>, dir: &Path) -> std::io::Result<Spool> {
        let path = |what| connect_perf::trace_path(dir, "viewer", self.session, what);
        let session_file = path("session");
        let (mut frames, mut inputs) = (CsvStream::new(path("frames")), CsvStream::new(path("input")));
        let (mut losses, mut network) = (CsvStream::new(path("loss")), CsvStream::new(path("network")));
        let trace = self.clone();
        Spool::start(move |last| {
            let written = frames
                .drain(&trace.frames)
                .and_then(|()| inputs.drain(&trace.inputs))
                .and_then(|()| losses.drain(&trace.losses))
                .and_then(|()| network.drain(&trace.network))
                .and_then(|()| {
                    if last {
                        connect_perf::write_csv(&session_file, &trace.summary(frames.overrun() + losses.overrun()))
                    } else {
                        Ok(())
                    }
                });
            if let Err(error) = written {
                tracing::warn!("couldn't write the performance trace: {error}");
            }
        })
    }
}

/// Percentiles (µs) of where frames' time went over one stats window, and how smoothly they were
/// shown. Stages on one PC are exact; `network` and `total` span both and are estimates.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct FrameStats {
    /// The host's screen to drawn here, estimated. Frames drawn only.
    pub total: Option<Percentiles>,
    pub present_to_capture: Option<Percentiles>,
    pub capture: Option<Percentiles>,
    pub encode: Option<Percentiles>,
    /// On the host from encoded to packetizing (the header's `sent_after` less encoding).
    pub host_wait: Option<Percentiles>,
    /// Host to here, estimated.
    pub network: Option<Percentiles>,
    /// First datagram to the frame being whole again.
    pub reassembly: Option<Percentiles>,
    /// Rebuilding the frame from its chunks (Reed-Solomon recovery included).
    pub rebuild: Option<Percentiles>,
    /// Whole to decoding: waiting for an earlier frame, and in the decoder's queue.
    pub decode_queue: Option<Percentiles>,
    pub decode: Option<Percentiles>,
    /// Decoded to drawn.
    pub render: Option<Percentiles>,
    /// Time between frames drawn, ms.
    pub frame_time: Option<Spread>,
    /// The same, as percentiles in µs.
    pub frame_interval: Option<Percentiles>,
    /// Encoded frame sizes, bytes.
    pub frame_bytes: Option<Percentiles>,
    pub shown: u32,
    /// Decoded but replaced by a newer frame before being drawn.
    pub superseded: u32,
    /// Estimated times a picture stayed up a frame longer than the stream's cadence called for
    /// (see [`repeats`]).
    pub repeated: u32,
    pub keyframes: u32,
}

/// The last frame drawn, carried from one window to the next so the gap across the boundary
/// counts: when it was drawn (this clock) and captured (the host's).
pub(crate) type LastShown = Option<(u64, u64)>;

/// Summarises the records of one window, oldest first.
pub(crate) fn summarize(records: &[FrameRecord], last: &mut LastShown) -> FrameStats {
    let collect = |pick: &dyn Fn(&FrameRecord) -> Option<i64>| {
        let mut values: Vec<i64> = records.iter().filter_map(pick).collect();
        percentiles(&mut values)
    };
    let shown = |r: &FrameRecord| r.shown();
    let mut stats = FrameStats {
        total: collect(&FrameRecord::total_estimate),
        present_to_capture: collect(&|r| {
            (r.meta.present_to_capture_us > 0).then_some(r.meta.present_to_capture_us.into())
        }),
        capture: collect(&|r| Some(r.meta.capture_us.into())),
        encode: collect(&|r| Some(r.meta.encode_time_us.into())),
        host_wait: collect(&|r| Some(r.meta.sent_after_us.saturating_sub(r.meta.encode_time_us).into())),
        network: collect(&FrameRecord::network_estimate),
        reassembly: collect(&FrameRecord::reassembly),
        rebuild: collect(&|r| (r.first_seen_us > 0).then_some(r.rebuild_us.into())),
        decode_queue: collect(&FrameRecord::decode_queue),
        decode: collect(&FrameRecord::decode),
        render: collect(&FrameRecord::render),
        frame_bytes: collect(&|r| Some(r.bytes.into())),
        shown: records.iter().filter(|r| shown(r)).count() as u32,
        superseded: records.iter().filter(|r| !shown(r)).count() as u32,
        keyframes: records.iter().filter(|r| r.meta.keyframe).count() as u32,
        ..Default::default()
    };

    // Shown frames in the order they were drawn, with the last one from before this window.
    let mut drawn: Vec<(u64, u64)> = last.iter().copied().collect();
    drawn.extend(records.iter().filter(|r| shown(r)).map(|r| (r.painted_us, r.meta.capture_time_us)));
    drawn.sort_by_key(|(painted, _)| *painted);
    let intervals: Vec<i64> = drawn.windows(2).map(|w| w[1].0 as i64 - w[0].0 as i64).collect();
    stats.frame_time = spread(&intervals.iter().map(|&us| us as f64 / 1000.0).collect::<Vec<_>>());
    stats.frame_interval = percentiles(&mut intervals.clone());
    // The host's usual time between captures, from consecutively numbered frames.
    let mut by_seq: Vec<&FrameRecord> = records.iter().collect();
    by_seq.sort_by_key(|r| r.meta.seq);
    let mut host_intervals: Vec<i64> = by_seq
        .windows(2)
        .filter(|w| w[1].meta.seq == w[0].meta.seq.wrapping_add(1))
        .map(|w| w[1].meta.capture_time_us as i64 - w[0].meta.capture_time_us as i64)
        .collect();
    let host_cadence = percentiles(&mut host_intervals).map(|p| p.p50);
    let draw_cadence = percentiles(&mut intervals.clone()).map(|p| p.p50);
    if let Some(cadence) = host_cadence.max(draw_cadence) {
        stats.repeated = drawn.windows(2).map(|w| repeats(w[0], w[1], cadence)).sum();
    }
    *last = drawn.last().copied().or(*last);
    stats
}

/// How many extra frame times the picture `from` stayed up before `to` replaced it, beyond what
/// the host's own gap between capturing them explains, counted in `cadence` (µs): the slower of
/// the host's capture rate and this window's drawing rate. A static screen sends nothing, and
/// the long gap that follows isn't a repeat, as the host's gap is just as long; a frame replaced
/// unseen in between is "superseded", not a repeat either.
pub(crate) fn repeats(from: (u64, u64), to: (u64, u64), cadence: i64) -> u32 {
    if cadence <= 0 {
        return 0;
    }
    let slots = |us: i64| ((us as f64) / cadence as f64).round().max(0.0) as i64;
    let drawn_gap = slots(to.0 as i64 - from.0 as i64);
    let host_gap = slots(to.1 as i64 - from.1 as i64).max(1);
    (drawn_gap - host_gap).max(0) as u32
}

#[cfg(test)]
mod tests {
    use connect_proto::video::FrameMeta;

    use super::*;

    const OFFSET: ClockOffset = ClockOffset { offset_us: 5_000_000, uncertainty_us: 400 };

    /// A frame captured at `capture` (host clock) and drawn `painted` µs later on this clock, with
    /// stages in a fixed pattern.
    fn record(seq: u32, capture: u64, painted: Option<u64>) -> FrameRecord {
        let here = capture as i64 + OFFSET.offset_us;
        let at = |ms: i64| (here + ms * 1000) as u64;
        FrameRecord {
            meta: FrameMeta {
                seq,
                capture_time_us: capture,
                present_to_capture_us: 2_000,
                capture_us: 1_000,
                encode_time_us: 3_000,
                sent_after_us: 4_000,
                ..Default::default()
            },
            bytes: 10_000,
            first_seen_us: at(9),
            completed_us: at(10),
            released_us: at(10),
            decode_start_us: at(11),
            decoded_us: at(13),
            painted_us: painted.map_or(0, |p| (here + p as i64) as u64),
            rebuild_us: 50,
            clock: Some(OFFSET),
            ..Default::default()
        }
    }

    #[test]
    fn stages_and_estimates_from_one_record() {
        let r = record(0, 1_000_000, Some(16_000));
        assert_eq!(
            (r.reassembly(), r.decode_queue(), r.decode(), r.render()),
            (Some(1_000), Some(1_000), Some(2_000), Some(3_000))
        );
        // Sent 4 ms after capture, first datagram 9 ms after: 5 ms on the network.
        assert_eq!(r.network_estimate(), Some(5_000));
        // 2 ms on screen before capture + 1 ms capturing + 16 ms from staged to drawn.
        assert_eq!(r.total_estimate(), Some(19_000));
        // Without the clock offset, nothing that spans both PCs.
        let blind = FrameRecord { clock: None, ..r };
        assert_eq!((blind.network_estimate(), blind.total_estimate()), (None, None));
        assert_eq!(blind.decode(), Some(2_000), "single-clock stages don't need it");
        // Not drawn: no render time or total, still a network estimate.
        let unseen = record(1, 1_000_000, None);
        assert_eq!((unseen.render(), unseen.total_estimate(), unseen.shown()), (None, None, false));
    }

    #[test]
    fn a_window_of_steady_frames() {
        // 100 frames captured every 4 ms, each drawn 16 ms after capture.
        let records: Vec<_> = (0..100).map(|i| record(i, 1_000_000 + u64::from(i) * 4_000, Some(16_000))).collect();
        let mut last = None;
        let stats = summarize(&records, &mut last);
        assert_eq!(stats.shown, 100);
        assert_eq!((stats.superseded, stats.repeated, stats.keyframes), (0, 0, 0));
        assert_eq!(stats.total.unwrap().p99, 19_000);
        assert_eq!(stats.decode.unwrap().p50, 2_000);
        let frame_time = stats.frame_time.unwrap();
        assert_eq!((frame_time.count, frame_time.mean, frame_time.stddev), (99, 4.0, 0.0));
        assert_eq!(last, Some((records[99].painted_us, records[99].meta.capture_time_us)));
        // The next window's first gap is measured from this one's last frame.
        let next = [record(100, 1_000_000 + 100 * 4_000, Some(16_000))];
        assert_eq!(summarize(&next, &mut last).frame_interval.unwrap().p50, 4_000);
    }

    #[test]
    fn a_stall_counts_repeats_but_a_static_screen_does_not() {
        let mut records: Vec<_> = (0..50).map(|i| record(i, u64::from(i) * 4_000, Some(16_000))).collect();
        // Frame 50 is captured on time but drawn 12 ms late: three frame times without a new picture.
        records.push(record(50, 50 * 4_000, Some(28_000)));
        let stats = summarize(&records, &mut None);
        assert_eq!(stats.repeated, 3, "{stats:?}");
        // Variance shows the stall too.
        assert!(stats.frame_time.unwrap().stddev > 1.0 && stats.frame_interval.unwrap().max == 16_000);

        // A screen that stops changing for a second: the host sends nothing, so no repeats.
        let mut records: Vec<_> = (0..50).map(|i| record(i, u64::from(i) * 4_000, Some(16_000))).collect();
        records.push(record(50, 49 * 4_000 + 1_000_000, Some(16_000)));
        assert_eq!(summarize(&records, &mut None).repeated, 0);
    }

    #[test]
    fn superseded_frames_are_counted_not_timed_as_shown() {
        let records = [record(0, 0, Some(16_000)), record(1, 4_000, None), record(2, 8_000, Some(16_000))];
        let stats = summarize(&records, &mut None);
        assert_eq!((stats.shown, stats.superseded, stats.repeated), (2, 1, 0));
        assert_eq!(stats.total.unwrap().count, 2);
        assert_eq!(stats.network.unwrap().count, 3);
    }

    #[test]
    fn records_stay_small() {
        // The ring's memory is records × capacity: about 0.6 MB by default, 10 MB with a trace
        // directory.
        assert!(size_of::<FrameRecord>() <= 160, "{} bytes", size_of::<FrameRecord>());
        assert!(size_of::<InputRecord>() <= 64, "{} bytes", size_of::<InputRecord>());
    }

    #[test]
    fn csv_rows_match_the_header() {
        for record in [record(7, 1_000, Some(16_000)), FrameRecord { clock: None, ..record(8, 2_000, None) }] {
            let mut row = String::new();
            record.write_row(&mut row);
            assert_eq!(row.split(',').count(), FrameRecord::HEADER.split(',').count(), "{row}");
        }
        let mut row = String::new();
        InputRecord { seq: 1, kind: "key", sent_us: 5, clock: None }.write_row(&mut row);
        assert_eq!(row.split(',').count(), InputRecord::HEADER.split(',').count(), "{row}");
        let now = Instant::now();
        let fate = connect_proto::video::Fate {
            seq: 3,
            gone: connect_proto::video::Gone::NeverSeen,
            keyframe: false,
            chunks: 0,
            needed: 0,
            first_seen: None,
            last_seen: None,
            given_up: now,
            late: 0,
            last_late: None,
        };
        for fate in [
            fate,
            connect_proto::video::Fate {
                needed: 9,
                first_seen: Some(now),
                last_seen: Some(now),
                late: 2,
                last_late: Some(now),
                ..fate
            },
        ] {
            row.clear();
            LossRecord { fate }.write_row(&mut row);
            assert_eq!(row.split(',').count(), LossRecord::HEADER.split(',').count(), "{row}");
        }
        for clock in [None, Some(OFFSET)] {
            row.clear();
            let network = NetworkRecord {
                time_us: 1,
                path: PathKind::Direct,
                rtt_us: clock.map(|_| 42_000),
                mbps: 30.0,
                fps: 240.0,
                frames_lost: 1,
                frames_skipped: 2,
                frames_recovered: 3,
                frames_superseded: 4,
                host_kbps: clock.map(|_| 80_000),
                loss_permille: None,
                clock,
                gpu: None,
            };
            network.write_row(&mut row);
            assert_eq!(row.split(',').count(), NetworkRecord::HEADER.split(',').count(), "{row}");
        }
    }
}
