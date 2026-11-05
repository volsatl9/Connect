//! The host's half of the performance trace: a record per frame sent, per capture, per colour
//! conversion timed on the graphics card and per remote input event, kept in bounded rings.
//! Summarised for the viewer about once a second, and streamed to CSV files as the session goes
//! if a trace directory is set. Every time here is on the host's clock
//! ([`connect_proto::now_micros`]). See METRICS.md.

use std::{
    fmt::Write as _,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use connect_perf::{CsvRecord, CsvStream, ProcessLoad, ProcessMonitor, Spool, TraceRing, cell, percentiles};
use connect_proto::{Codec, perf::HostPerf};

/// One frame, from capture to handing its datagrams to the network stack.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct FrameRecord {
    pub seq: u32,
    pub keyframe: bool,
    pub codec: Codec,
    /// A static picture encoded again, as a keyframe the viewer asked for: it wasn't held, just
    /// kept.
    pub resent: bool,
    /// Which capture the picture came from; matches [`ConvertRecord::capture`].
    pub capture: u64,
    pub bytes: u32,
    /// When the picture was staged for the encoder.
    pub capture_time_us: u64,
    /// From Windows putting the image on screen to capture picking it up; 0 where unknown.
    pub present_to_capture_us: u32,
    /// From acquiring the image to it being staged (copy, and submitting the conversion).
    pub capture_us: u32,
    /// Staged until the encoder started on it.
    pub held_us: u32,
    pub encode_us: u32,
    /// Of `encode_us`, the wait for the encoder to take the picture.
    pub encoder_input_wait_us: u32,
    /// From the encoder finishing to the session starting to send it.
    pub queue_us: u32,
    pub packetize_us: u32,
    /// The call handing the datagrams to the network stack.
    pub send_us: u32,
    /// From `capture_time_us` until packetizing began: the figure the frame's header carries.
    pub sent_after_us: u32,
    pub datagrams: u16,
    pub recovery: u16,
    /// Bytes already waiting in the connection's datagram buffer when this frame was sent.
    pub queued_bytes: u32,
    pub bitrate_kbps: u32,
    /// The path's datagrams got smaller, and the frame was dropped.
    pub dropped: bool,
}

/// A session's stamp for its files: the time, moved on if another took that second.
fn unique_stamp() -> u64 {
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = connect_perf::session_stamp();
    LAST.fetch_max(now, Ordering::Relaxed);
    LAST.fetch_add(1, Ordering::Relaxed) + 1
}

impl CsvRecord for FrameRecord {
    const HEADER: &'static str = "seq,keyframe,codec,resent,capture_index,bytes,capture_time_us,present_to_capture_us,\
capture_us,held_us,encode_us,queue_us,packetize_us,send_us,sent_after_us,datagrams,recovery_datagrams,\
queued_bytes,bitrate_kbps,dropped,encoder_input_wait_us";

    fn write_row(&self, out: &mut String) {
        let _ = write!(
            out,
            "{},{},{},{},{},{},{},",
            self.seq,
            self.keyframe as u8,
            self.codec,
            self.resent as u8,
            self.capture,
            self.bytes,
            self.capture_time_us
        );
        cell(out, (self.present_to_capture_us > 0).then_some(self.present_to_capture_us));
        let _ = write!(
            out,
            "{},{},{},{},{},{},{},{},{},{},{},{},{}",
            self.capture_us,
            self.held_us,
            self.encode_us,
            self.queue_us,
            self.packetize_us,
            self.send_us,
            self.sent_after_us,
            self.datagrams,
            self.recovery,
            self.queued_bytes,
            self.bitrate_kbps,
            self.dropped as u8,
            self.encoder_input_wait_us
        );
    }
}

/// Why a picture was replaced by a newer one before it could be encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Replaced {
    /// The picture before this one went, or was never held back.
    #[default]
    No,
    /// It was waiting for its turn: at most one picture goes per refresh of the host's screen.
    Turn,
    /// It was held while the network caught up.
    Backlog,
}

impl Replaced {
    fn name(self) -> &'static str {
        match self {
            Replaced::No => "",
            Replaced::Turn => "turn",
            Replaced::Backlog => "backlog",
        }
    }
}

/// One image captured from the screen, whether or not it was encoded. A gap between these with
/// `accumulated` at 1 means nothing new reached the screen; above 1, images came and capture
/// missed them.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CaptureRecord {
    /// Matches [`FrameRecord::capture`], for the images that went on to be encoded.
    pub capture: u64,
    pub time_us: u64,
    pub capture_us: u32,
    pub present_to_capture_us: u32,
    pub accumulated: Option<u32>,
    /// What became of the image before this one, if it was never encoded.
    pub replaced: Replaced,
}

impl CsvRecord for CaptureRecord {
    const HEADER: &'static str = "capture_index,time_us,capture_us,present_to_capture_us,accumulated_frames,\
previous_replaced";

    fn write_row(&self, out: &mut String) {
        let _ = write!(out, "{},{},{},", self.capture, self.time_us, self.capture_us);
        cell(out, (self.present_to_capture_us > 0).then_some(self.present_to_capture_us));
        cell(out, self.accumulated);
        out.push_str(self.replaced.name());
    }
}

/// One colour conversion's run time on the graphics card.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ConvertRecord {
    pub capture: u64,
    pub gpu_us: u32,
}

impl CsvRecord for ConvertRecord {
    const HEADER: &'static str = "capture_index,convert_gpu_us";

    fn write_row(&self, out: &mut String) {
        let _ = write!(out, "{},{}", self.capture, self.gpu_us);
    }
}

/// What became of a remote input event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum InputOutcome {
    /// Handed to Windows (`SendInput`) or the gamepad driver.
    #[default]
    Injected,
    /// Windows refused it, e.g. an elevated window has the focus.
    #[cfg_attr(not(windows), allow(dead_code))]
    Refused,
    /// Dropped because the host is using that device themselves.
    #[cfg_attr(not(windows), allow(dead_code))]
    HostPriority,
    /// Dropped because the session doesn't allow that device.
    NotAllowed,
    /// Handed to a test channel instead of the system.
    Channel,
    /// Moved the connector's labelled pointer only: others share the mouse.
    PointerOnly,
}

impl InputOutcome {
    fn name(self) -> &'static str {
        match self {
            InputOutcome::Injected => "injected",
            InputOutcome::PointerOnly => "pointer_only",
            InputOutcome::Refused => "refused",
            InputOutcome::HostPriority => "host_priority",
            InputOutcome::NotAllowed => "not_allowed",
            InputOutcome::Channel => "channel",
        }
    }
}

/// One remote input event.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct InputRecord {
    pub seq: u32,
    pub kind: &'static str,
    /// When the viewer sent it, on the viewer's clock: not comparable with the others here
    /// without the clock offset.
    pub client_time_us: u64,
    /// When it came off the input stream.
    pub received_us: u64,
    /// When it was acted on (or dropped).
    pub done_us: u64,
    pub outcome: InputOutcome,
}

impl CsvRecord for InputRecord {
    const HEADER: &'static str = "seq,kind,client_time_us_viewer_clock,received_us,done_us,inject_us,outcome";

    fn write_row(&self, out: &mut String) {
        let _ = write!(
            out,
            "{},{},{},{},{},{},{}",
            self.seq,
            self.kind,
            self.client_time_us,
            self.received_us,
            self.done_us,
            self.done_us.saturating_sub(self.received_us),
            self.outcome.name()
        );
    }
}

/// The path and the bitrate controller at one rate tick (every 250 ms).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct NetworkRecord {
    pub time_us: u64,
    /// Packets sent and lost on the path since the last tick.
    pub sent_packets: u32,
    pub lost_packets: u32,
    /// Bytes of datagrams waiting to be sent.
    pub queued_bytes: u32,
    /// The congestion controller's estimate of the path's bandwidth, its window and round trip.
    pub bandwidth_estimate_bps: Option<u64>,
    pub cwnd_bytes: u64,
    pub rtt_us: u32,
    /// Times the congestion controller has reacted to loss, since the last tick.
    pub congestion_events: u32,
    pub relay: bool,
    /// The viewer's keyframe requests since the last tick: each means it lost video.
    pub recovery_requests: u32,
    /// The encoder's target after this tick, the controller's ceiling, and why.
    pub bitrate_kbps: u32,
    pub ceiling_kbps: Option<u32>,
    pub reason: &'static str,
    /// Video bytes handed to the network since the last tick.
    pub video_bytes: u32,
    /// What the host's own mouse and keyboard did since the last tick.
    pub local: crate::input::LocalActivity,
    /// The remote events host priority dropped since the last tick.
    pub host_priority: u32,
}

impl CsvRecord for NetworkRecord {
    const HEADER: &'static str = "time_us,sent_packets,lost_packets,queued_bytes,bandwidth_estimate_bps,cwnd_bytes,\
rtt_us,congestion_events,path,recovery_requests,bitrate_kbps,ceiling_kbps,reason,video_bytes,\
local_mouse_moves,local_mouse_pixels,local_mouse_tagged,local_mouse_buttons,local_keys,host_priority_drops";

    fn write_row(&self, out: &mut String) {
        let _ = write!(out, "{},{},{},{},", self.time_us, self.sent_packets, self.lost_packets, self.queued_bytes);
        cell(out, self.bandwidth_estimate_bps);
        let _ = write!(
            out,
            "{},{},{},{},{},{},",
            self.cwnd_bytes,
            self.rtt_us,
            self.congestion_events,
            if self.relay { "relay" } else { "direct" },
            self.recovery_requests,
            self.bitrate_kbps
        );
        cell(out, self.ceiling_kbps);
        let l = &self.local;
        let _ = write!(
            out,
            "{},{},{},{},{},{},{},{}",
            self.reason,
            self.video_bytes,
            l.mouse_moves,
            l.mouse_pixels,
            l.mouse_tagged,
            l.mouse_buttons,
            l.keys,
            self.host_priority
        );
    }
}

/// How busy the host was, about once a second: a stall that lines up with a full graphics card
/// or processor came from the load, not from Connect.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct LoadRecord {
    pub time_us: u64,
    pub process: Option<ProcessLoad>,
    pub gpu: Option<connect_gpu_load::GpuLoad>,
}

impl CsvRecord for LoadRecord {
    const HEADER: &'static str = "time_us,connect_cpu_percent,gpu_graphics_percent,gpu_encode_percent";

    fn write_row(&self, out: &mut String) {
        let _ = write!(out, "{},", self.time_us);
        cell(out, self.process.map(|p| format!("{:.1}", p.cpu_percent)));
        cell(out, self.gpu.map(|g| g.graphics));
        if let Some(encode) = self.gpu.and_then(|g| g.encode) {
            let _ = write!(out, "{encode}");
        }
    }
}

/// When a remote input event was sent and received, carried with it to where it's acted on.
#[derive(Debug, Clone, Copy)]
pub(crate) struct InputStamp {
    pub seq: u32,
    pub client_time_us: u64,
    pub received_us: u64,
}

impl InputStamp {
    pub fn record(self, kind: &'static str, outcome: InputOutcome) -> InputRecord {
        InputRecord {
            seq: self.seq,
            kind,
            client_time_us: self.client_time_us,
            received_us: self.received_us,
            done_us: connect_proto::now_micros(),
            outcome,
        }
    }
}

/// A session's trace.
pub(crate) struct HostTrace {
    pub frames: TraceRing<FrameRecord>,
    pub captures: TraceRing<CaptureRecord>,
    pub conversions: TraceRing<ConvertRecord>,
    pub inputs: TraceRing<InputRecord>,
    pub network: TraceRing<NetworkRecord>,
    pub load: TraceRing<LoadRecord>,
    /// Remote events host priority has dropped so far.
    pub host_priority_drops: AtomicU64,
    /// When the session started, in Unix seconds, to name its files.
    pub session: u64,
}

impl HostTrace {
    pub fn new() -> Self {
        let capacity = connect_perf::ring_capacity();
        Self {
            frames: TraceRing::new(capacity),
            captures: TraceRing::new(capacity),
            conversions: TraceRing::new(capacity),
            inputs: TraceRing::new(capacity),
            network: TraceRing::new(capacity),
            load: TraceRing::new(capacity),
            host_priority_drops: AtomicU64::new(0),
            session: unique_stamp(),
        }
    }

    fn dropped(&self) -> u64 {
        self.frames.dropped_busy()
            + self.captures.dropped_busy()
            + self.conversions.dropped_busy()
            + self.inputs.dropped_busy()
            + self.network.dropped_busy()
            + self.load.dropped_busy()
    }

    /// Starts writing the trace to CSV files in `dir` as it goes.
    pub fn spool(self: &Arc<Self>, dir: &Path) -> std::io::Result<Spool> {
        let path = |what| connect_perf::trace_path(dir, "host", self.session, what);
        let session_file = path("session");
        let (mut frames, mut captures, mut conversions) =
            (CsvStream::new(path("frames")), CsvStream::new(path("capture")), CsvStream::new(path("convert")));
        let (mut inputs, mut network, mut load) =
            (CsvStream::new(path("input")), CsvStream::new(path("network")), CsvStream::new(path("load")));
        let trace = self.clone();
        Spool::start(move |last| {
            let written = frames
                .drain(&trace.frames)
                .and_then(|()| captures.drain(&trace.captures))
                .and_then(|()| conversions.drain(&trace.conversions))
                .and_then(|()| inputs.drain(&trace.inputs))
                .and_then(|()| network.drain(&trace.network))
                .and_then(|()| load.drain(&trace.load));
            if let Err(error) = written {
                tracing::warn!("couldn't write the performance trace: {error}");
            }
            if last {
                let overrun = frames.overrun() + captures.overrun() + conversions.overrun();
                let session = [
                    connect_perf::SessionRow("version", env!("CARGO_PKG_VERSION").into()),
                    connect_perf::SessionRow("session_unix_s", trace.session.to_string()),
                    connect_perf::SessionRow("frames_recorded", trace.frames.written().to_string()),
                    connect_perf::SessionRow("records_overrun", overrun.to_string()),
                    connect_perf::SessionRow("records_dropped_busy", trace.dropped().to_string()),
                    connect_perf::SessionRow("displays", crate::capture::displays()),
                ];
                if let Err(error) = connect_perf::write_csv(&session_file, &session) {
                    tracing::warn!("couldn't write the performance trace: {error}");
                }
            }
        })
    }
}

/// Turns the trace into [`HostPerf`] reports, each covering the records since the last.
pub(crate) struct Reporter {
    marks: [u64; 3],
    process: ProcessMonitor,
}

impl Reporter {
    pub fn new() -> Self {
        Self { marks: [0; 3], process: ProcessMonitor::new() }
    }

    /// The figures since the last report; `None` while tracing is off.
    pub fn report(&mut self, trace: &HostTrace) -> Option<HostPerf> {
        if !connect_perf::enabled() {
            return None;
        }
        let (frames, mark) = trace.frames.since(self.marks[0]);
        self.marks[0] = mark;
        let (conversions, mark) = trace.conversions.since(self.marks[1]);
        self.marks[1] = mark;
        let (inputs, mark) = trace.inputs.since(self.marks[2]);
        self.marks[2] = mark;

        let of = |pick: &dyn Fn(&FrameRecord) -> Option<u32>| {
            percentiles(&mut frames.iter().filter_map(pick).map(i64::from).collect::<Vec<_>>())
        };
        let sent = |f: &FrameRecord| !f.dropped;
        let injected: Vec<&InputRecord> = inputs.iter().filter(|i| i.outcome == InputOutcome::Injected).collect();
        Some(HostPerf {
            process: self.process.sample(),
            capture: of(&|f| Some(f.capture_us)),
            convert_gpu: percentiles(&mut conversions.iter().map(|c| i64::from(c.gpu_us)).collect::<Vec<_>>()),
            held: of(&|f| (!f.resent).then_some(f.held_us)),
            encode: of(&|f| Some(f.encode_us)),
            queue: of(&|f| Some(f.queue_us)),
            packetize: of(&|f| sent(f).then_some(f.packetize_us)),
            send: of(&|f| sent(f).then_some(f.send_us)),
            input_inject: percentiles(
                &mut injected.iter().map(|i| i.done_us.saturating_sub(i.received_us) as i64).collect::<Vec<_>>(),
            ),
            input_transit_raw: percentiles(
                &mut inputs.iter().map(|i| i.received_us as i64 - i.client_time_us as i64).collect::<Vec<_>>(),
            ),
            trace_dropped: trace.dropped(),
        })
    }
}

/// What to do with one GPU timing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) enum TimerVerdict {
    Keep,
    /// Too short to be the conversion: the timestamps didn't bracket the work.
    Skip,
    /// Only such readings so far: this driver's timestamps don't cover the work at all.
    GiveUp,
}

/// Screens GPU timings for ones that can't be real. Some drivers run the video processor
/// outside the command stream that timestamp queries mark, so both timestamps land at nearly the
/// same point and every conversion reads under 1 µs (seen on an RTX 5060: 0 µs after rounding,
/// though not always 0 ticks). Readings under [`Self::MIN_US`] are dropped. A timer that
/// has produced nothing else after [`Self::GIVE_UP_AFTER`] readings is given up on, so the stage
/// shows as unmeasured rather than as 0.
#[derive(Debug, Default)]
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) struct TimerCheck {
    empty_in_a_row: u32,
    seen_real: bool,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl TimerCheck {
    pub const GIVE_UP_AFTER: u32 = 32;
    /// Shorter than this can't be a whole-screen conversion on a graphics card (the shaders take
    /// tens of µs at 1080p); it's timestamps that missed the work, as NVIDIA's video processor
    /// gives (under 1 µs).
    pub const MIN_US: u64 = 2;

    /// `ticks` is end minus start, at `frequency` ticks a second.
    pub fn check(&mut self, ticks: u64, frequency: u64) -> TimerVerdict {
        let us = u128::from(ticks) * 1_000_000 / u128::from(frequency.max(1));
        if us >= u128::from(Self::MIN_US) {
            (self.seen_real, self.empty_in_a_row) = (true, 0);
            return TimerVerdict::Keep;
        }
        self.empty_in_a_row += 1;
        if !self.seen_real && self.empty_in_a_row >= Self::GIVE_UP_AFTER {
            TimerVerdict::GiveUp
        } else {
            TimerVerdict::Skip
        }
    }
}

#[cfg(test)]
mod tests {
    use connect_perf::Percentiles;

    use super::*;

    fn p50(p: Option<Percentiles>) -> Option<i64> {
        p.map(|p| p.p50)
    }

    #[test]
    fn reports_cover_only_records_since_the_last() {
        let trace = HostTrace::new();
        let mut reporter = Reporter::new();
        for seq in 0..10 {
            trace.frames.push(FrameRecord { seq, capture_us: 1_000 + seq, encode_us: 3_000, ..Default::default() });
        }
        trace.conversions.push(ConvertRecord { capture: 1, gpu_us: 250 });
        let stamp = InputStamp { seq: 0, client_time_us: 5_000, received_us: 9_000 };
        trace.inputs.push(InputRecord { done_us: 9_300, ..stamp.record("key", InputOutcome::Injected) });
        trace.inputs.push(stamp.record("key", InputOutcome::HostPriority));

        let first = reporter.report(&trace).unwrap();
        assert_eq!(first.capture.unwrap().count, 10);
        assert_eq!(p50(first.capture), Some(1_004));
        assert_eq!(p50(first.encode), Some(3_000));
        assert_eq!(p50(first.convert_gpu), Some(250));
        // Only the injected event counts towards injection time; both towards transit.
        assert_eq!((first.input_inject.unwrap().count, p50(first.input_inject)), (1, Some(300)));
        assert_eq!((first.input_transit_raw.unwrap().count, p50(first.input_transit_raw)), (2, Some(4_000)));
        assert!(first.process.is_some());

        let second = reporter.report(&trace).unwrap();
        assert_eq!((second.capture, second.convert_gpu, second.input_inject), (None, None, None));
        trace.frames.push(FrameRecord { seq: 10, capture_us: 7, resent: true, ..Default::default() });
        let third = reporter.report(&trace).unwrap();
        assert_eq!(p50(third.capture), Some(7));
        assert_eq!(third.held, None, "a re-sent picture wasn't held");
    }

    const GHZ: u64 = 1_000_000_000;

    #[test]
    fn empty_gpu_timings_are_dropped_then_given_up_on() {
        let mut check = TimerCheck::default();
        for _ in 1..TimerCheck::GIVE_UP_AFTER {
            assert_eq!(check.check(0, GHZ), TimerVerdict::Skip);
        }
        assert_eq!(check.check(0, GHZ), TimerVerdict::GiveUp, "never a real reading");

        // A few hundred ticks of a 1 GHz counter is under a microsecond: also not real.
        let mut check = TimerCheck::default();
        for _ in 1..TimerCheck::GIVE_UP_AFTER {
            assert_eq!(check.check(400, GHZ), TimerVerdict::Skip);
        }
        assert_eq!(check.check(400, GHZ), TimerVerdict::GiveUp);

        // A timer that works but now and then reads 0 keeps going, minus those readings.
        let mut check = TimerCheck::default();
        assert_eq!(check.check(250_000, GHZ), TimerVerdict::Keep);
        assert_eq!(check.check(3_000, GHZ), TimerVerdict::Keep, "a small screen's quick conversion is real");
        for _ in 0..100 {
            assert_eq!(check.check(0, GHZ), TimerVerdict::Skip);
        }
        assert_eq!(check.check(200_000, GHZ), TimerVerdict::Keep);
    }

    #[test]
    fn records_stay_small() {
        assert!(size_of::<FrameRecord>() <= 96, "{} bytes", size_of::<FrameRecord>());
        assert!(size_of::<InputRecord>() <= 64, "{} bytes", size_of::<InputRecord>());
    }

    #[test]
    fn csv_rows_match_the_header() {
        let frame = FrameRecord { seq: 3, keyframe: true, codec: Codec::H265, bytes: 9_000, ..Default::default() };
        let mut row = String::new();
        frame.write_row(&mut row);
        assert_eq!(row.split(',').count(), FrameRecord::HEADER.split(',').count(), "{row}");
        assert!(row.starts_with("3,1,H.265,0,"), "{row}");
        for network in [
            NetworkRecord::default(),
            NetworkRecord {
                bandwidth_estimate_bps: Some(40_000_000),
                ceiling_kbps: Some(32_000),
                relay: true,
                ..Default::default()
            },
        ] {
            row.clear();
            network.write_row(&mut row);
            assert_eq!(row.split(',').count(), NetworkRecord::HEADER.split(',').count(), "{row}");
        }
        let input = InputStamp { seq: 1, client_time_us: 10, received_us: 20 }.record("key", InputOutcome::Refused);
        row.clear();
        input.write_row(&mut row);
        assert_eq!(row.split(',').count(), InputRecord::HEADER.split(',').count(), "{row}");
        assert!(row.ends_with(",refused"));
    }
}
