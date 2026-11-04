//! Follows every frame of a session through the host's and the viewer's traces, by frame number
//! (`seq`), and says what became of it. The traces are CSV files; put both PCs' in one folder.
//! See METRICS.md.

use std::{
    collections::HashMap,
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

/// A frame this far over the session's usual latency counts as late.
const LATE_US: i64 = 16_000;
/// Frames in trouble this close together, for the same reason, are one event.
const SAME_EVENT_US: u64 = 250_000;
/// A gap between captures of at least this is looked into.
const CAPTURE_GAP_US: u64 = 50_000;
/// The last frames of a session are expected not to arrive.
const SESSION_END_FRAMES: u32 = 480;
/// Events listed in the report; all of them go in the CSV.
const EVENTS_LISTED: usize = 40;

/// Where a frame went wrong. A frame gets one of these at most.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Cause {
    /// Its datagrams never came, or too few to rebuild it.
    NetworkLoss,
    /// Its datagrams came, after the viewer had to give up on it.
    NetworkLate,
    /// Arrived, but built on a lost frame.
    SkippedAfterLoss,
    /// Late on the way, or in the host's send queue.
    NetworkQueue,
    HostCapture,
    /// Held back for the network to catch up, or for its turn.
    HostHeld,
    Encoder,
    /// The decoder fell behind, or was late with it.
    ViewerDecode,
    /// Decoded, but a newer frame replaced it before it could be drawn.
    PresentationDrop,
    /// Late to draw.
    ViewerDraw,
    /// The host sent it; the viewer has no record of it.
    Unaccounted,
}

impl Cause {
    const ALL: [Cause; 11] = [
        Cause::NetworkLoss,
        Cause::NetworkLate,
        Cause::SkippedAfterLoss,
        Cause::NetworkQueue,
        Cause::HostCapture,
        Cause::HostHeld,
        Cause::Encoder,
        Cause::ViewerDecode,
        Cause::PresentationDrop,
        Cause::ViewerDraw,
        Cause::Unaccounted,
    ];

    fn name(self) -> &'static str {
        match self {
            Cause::NetworkLoss => "network_loss",
            Cause::NetworkLate => "network_late",
            Cause::SkippedAfterLoss => "skipped_after_loss",
            Cause::NetworkQueue => "network_queue",
            Cause::HostCapture => "host_capture",
            Cause::HostHeld => "host_held",
            Cause::Encoder => "encoder",
            Cause::ViewerDecode => "viewer_decode",
            Cause::PresentationDrop => "replaced_by_newer",
            Cause::ViewerDraw => "viewer_draw",
            Cause::Unaccounted => "unaccounted",
        }
    }
}

/// A CSV file, read by column name, so traces from other versions still work.
struct Table {
    columns: HashMap<String, usize>,
    rows: Vec<Vec<String>>,
}

impl Table {
    fn parse(text: &str) -> Self {
        let mut lines = text.lines();
        let columns = lines.next().unwrap_or("").split(',').enumerate().map(|(i, c)| (c.to_owned(), i)).collect();
        let rows = lines.map(|line| line.split(',').map(str::to_owned).collect()).collect();
        Self { columns, rows }
    }

    fn cell<'a>(&self, row: &'a [String], column: &str) -> Option<&'a str> {
        row.get(*self.columns.get(column)?).map(String::as_str).filter(|cell| !cell.is_empty())
    }

    fn number<T: std::str::FromStr>(&self, row: &[String], column: &str) -> Option<T> {
        self.cell(row, column)?.parse().ok()
    }
}

/// The session's traces, as text.
#[derive(Default)]
pub struct Traces {
    pub host_frames: String,
    pub host_captures: Option<String>,
    pub host_network: Option<String>,
    pub viewer_frames: String,
    pub viewer_losses: String,
}

/// What went wrong, in order of time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Event {
    cause: Cause,
    start_us: u64,
    end_us: u64,
    first_seq: u32,
    last_seq: u32,
    frames: u32,
    /// The largest amount over the usual, where that applies.
    worst_us: i64,
}

#[derive(Debug, Default)]
struct Analysis {
    host_frames: u32,
    delivered: u32,
    /// Frames each cause accounts for.
    frames: HashMap<&'static str, u32>,
    events: Vec<Event>,
    usual_latency_us: i64,
    latency_us: Vec<i64>,
    bitrate_kbps: Vec<i64>,
    frame_bytes: Vec<i64>,
    /// Gaps between captures with images missed, and with nothing new to capture.
    capture_stalls: Vec<(u64, u64, u32)>,
    quiet_gaps: u32,
    replaced_turn: u32,
    replaced_backlog: u32,
    packets_lost: u64,
    packet_bursts: Vec<(u64, u64)>,
    host_priority_drops: u64,
    /// In ticks with host priority drops: the host's mouse moves, pixels, tagged events, keys.
    local_input: [u64; 4],
    viewer_without_host: u32,
}

fn median(values: &mut [i64]) -> i64 {
    values.sort_unstable();
    values.get(values.len() / 2).copied().unwrap_or(0)
}

fn percentile(sorted: &[i64], share: f64) -> i64 {
    sorted.get(((sorted.len().saturating_sub(1)) as f64 * share) as usize).copied().unwrap_or(0)
}

/// The stage of a late frame that took the most over its usual time.
fn blame(stages: &[(Cause, i64)], usual: &[i64]) -> (Cause, i64) {
    stages
        .iter()
        .zip(usual)
        .map(|(&(cause, us), usual)| (cause, us - usual))
        .max_by_key(|&(_, over)| over)
        .unwrap_or((Cause::Unaccounted, 0))
}

fn analyse(traces: &Traces) -> Result<Analysis> {
    let host = Table::parse(&traces.host_frames);
    let viewer = Table::parse(&traces.viewer_frames);
    let losses = Table::parse(&traces.viewer_losses);
    if host.rows.is_empty() || viewer.rows.is_empty() {
        bail!("no frames in the traces");
    }
    let mut result = Analysis::default();

    let viewer_by_seq: HashMap<u32, usize> =
        viewer.rows.iter().enumerate().filter_map(|(i, row)| Some((viewer.number(row, "seq")?, i))).collect();
    let loss_by_seq: HashMap<u32, usize> =
        losses.rows.iter().enumerate().filter_map(|(i, row)| Some((losses.number(row, "seq")?, i))).collect();
    let host_seqs: std::collections::HashSet<u32> =
        host.rows.iter().filter_map(|row| host.number(row, "seq")).collect();
    result.viewer_without_host = viewer_by_seq.keys().filter(|seq| !host_seqs.contains(seq)).count() as u32;

    // A stage of one frame, 0 where it wasn't measured.
    let host_us = |row: &[String], columns: &[&str]| -> i64 {
        columns.iter().filter_map(|column| host.number::<i64>(row, column)).sum()
    };
    let viewer_us = |row: &[String], columns: &[&str]| -> i64 {
        columns.iter().filter_map(|column| viewer.number::<i64>(row, column)).sum()
    };
    const CAPTURE: &[&str] = &["present_to_capture_us", "capture_us"];
    const HELD: &[&str] = &["held_us"];
    const ENCODE: &[&str] = &["encode_us"];
    const SEND: &[&str] = &["queue_us", "packetize_us", "send_us"];
    const NETWORK: &[&str] = &["network_est_us", "reassembly_us"];
    const DECODE: &[&str] = &["decode_queue_us", "decode_us"];
    const DRAW: &[&str] = &["render_us"];

    let stages = |row: &[String], seen: &[String]| {
        [
            (Cause::HostCapture, host_us(row, CAPTURE)),
            (Cause::HostHeld, host_us(row, HELD)),
            (Cause::Encoder, host_us(row, ENCODE)),
            (Cause::NetworkQueue, host_us(row, SEND) + viewer_us(seen, NETWORK)),
            (Cause::ViewerDecode, viewer_us(seen, DECODE)),
            (Cause::ViewerDraw, viewer_us(seen, DRAW)),
        ]
    };

    // What's usual for each stage and for the whole trip.
    let mut samples: [Vec<i64>; 6] = Default::default();
    for row in &host.rows {
        let Some(seen) = host.number(row, "seq").and_then(|seq: u32| viewer_by_seq.get(&seq)) else { continue };
        for (sample, (_, us)) in samples.iter_mut().zip(stages(row, &viewer.rows[*seen])) {
            sample.push(us);
        }
    }
    let usual: Vec<i64> = samples.iter_mut().map(|sample| median(sample)).collect();
    result.latency_us = viewer.rows.iter().filter_map(|row| viewer.number(row, "total_est_us")).collect();
    result.latency_us.sort_unstable();
    result.usual_latency_us = percentile(&result.latency_us, 0.5);

    let first_time: u64 = host.number(&host.rows[0], "capture_time_us").unwrap_or(0);
    let last_seq: u32 = host.rows.iter().filter_map(|row| host.number(row, "seq")).max().unwrap_or(0);
    let mut trouble: Vec<(Cause, u64, u32, i64)> = Vec::new();
    for row in &host.rows {
        let Some(seq) = host.number::<u32>(row, "seq") else { continue };
        let time = host.number::<u64>(row, "capture_time_us").unwrap_or(first_time).saturating_sub(first_time);
        result.host_frames += 1;
        result.bitrate_kbps.extend(host.number::<i64>(row, "bitrate_kbps"));
        result.frame_bytes.extend(host.number::<i64>(row, "bytes"));
        let found = if let Some(&loss) = loss_by_seq.get(&seq) {
            let row = &losses.rows[loss];
            let late = losses.number::<u32>(row, "late_datagrams").unwrap_or(0);
            let cause = match losses.cell(row, "reason") {
                Some("skipped_after_loss") => Cause::SkippedAfterLoss,
                Some("skipped_decoder_behind") => Cause::ViewerDecode,
                _ if late > 0 => Cause::NetworkLate,
                _ => Cause::NetworkLoss,
            };
            Some((cause, 0))
        } else if let Some(&seen) = viewer_by_seq.get(&seq) {
            let seen = &viewer.rows[seen];
            if viewer.cell(seen, "status") == Some("superseded") {
                Some((Cause::PresentationDrop, 0))
            } else {
                viewer
                    .number::<i64>(seen, "total_est_us")
                    .filter(|&total| total > result.usual_latency_us + LATE_US)
                    .map(|_| blame(&stages(row, seen), &usual))
            }
        } else if last_seq.saturating_sub(seq) >= SESSION_END_FRAMES {
            Some((Cause::Unaccounted, 0))
        } else {
            None
        };
        match found {
            Some((cause, over)) => {
                *result.frames.entry(cause.name()).or_default() += 1;
                trouble.push((cause, time, seq, over));
            }
            None => result.delivered += 1,
        }
    }

    // Merged, for each cause, into events.
    trouble.sort_by_key(|&(cause, time, ..)| (cause, time));
    for (cause, time, seq, over) in trouble {
        match result.events.last_mut() {
            Some(event) if event.cause == cause && time - event.end_us <= SAME_EVENT_US => {
                event.end_us = time;
                event.last_seq = seq;
                event.frames += 1;
                event.worst_us = event.worst_us.max(over);
            }
            _ => result.events.push(Event {
                cause,
                start_us: time,
                end_us: time,
                first_seq: seq,
                last_seq: seq,
                frames: 1,
                worst_us: over,
            }),
        }
    }
    result.events.sort_by_key(|event| event.start_us);

    if let Some(text) = &traces.host_captures {
        let captures = Table::parse(text);
        let mut last: Option<u64> = None;
        for row in &captures.rows {
            match captures.cell(row, "previous_replaced") {
                Some("turn") => result.replaced_turn += 1,
                Some("backlog") => result.replaced_backlog += 1,
                _ => {}
            }
            let Some(time) = captures.number::<u64>(row, "time_us") else { continue };
            if let Some(gap) = last.map(|last| time - last).filter(|&gap| gap >= CAPTURE_GAP_US) {
                match captures.number::<u32>(row, "accumulated_frames") {
                    Some(missed) if missed > 1 => {
                        result.capture_stalls.push((time.saturating_sub(first_time), gap, missed));
                    }
                    _ => result.quiet_gaps += 1,
                }
            }
            last = Some(time);
        }
    }

    if let Some(text) = &traces.host_network {
        let network = Table::parse(text);
        for row in &network.rows {
            let lost: u64 = network.number(row, "lost_packets").unwrap_or(0);
            result.packets_lost += lost;
            if lost > 0 {
                let time = network.number::<u64>(row, "time_us").unwrap_or(first_time);
                result.packet_bursts.push((time.saturating_sub(first_time), lost));
            }
            let drops: u64 = network.number(row, "host_priority_drops").unwrap_or(0);
            if drops > 0 {
                result.host_priority_drops += drops;
                for (total, column) in result.local_input.iter_mut().zip([
                    "local_mouse_moves",
                    "local_mouse_pixels",
                    "local_mouse_tagged",
                    "local_keys",
                ]) {
                    *total += network.number::<u64>(row, column).unwrap_or(0);
                }
            }
        }
    }
    Ok(result)
}

fn seconds(us: u64) -> String {
    format!("{:.1} s", us as f64 / 1e6)
}

fn format_report(a: &Analysis) -> String {
    let mut out = String::new();
    let share = |frames: u32| 100.0 * f64::from(frames) / f64::from(a.host_frames.max(1));
    let ms = |us: i64| us as f64 / 1000.0;
    let replaced = a.frames.get(Cause::PresentationDrop.name()).copied().unwrap_or(0);
    let on_time = a.delivered + replaced;
    let _ = writeln!(out, "{} frames sent. {on_time} ({:.3}%) arrived on time.", a.host_frames, share(on_time));
    let _ = writeln!(out, "{replaced} of those were replaced by a newer frame before the screen refreshed.");
    let _ = writeln!(out, "\nWhere the other frames went (one reason each):");
    for cause in Cause::ALL {
        let frames = a.frames.get(cause.name()).copied().unwrap_or(0);
        let events = a.events.iter().filter(|event| event.cause == cause).count();
        let _ = writeln!(out, "  {:<20} {frames:>8} frames {:>7.3}%  in {events} events", cause.name(), share(frames));
    }
    let l = &a.latency_us;
    let _ = writeln!(
        out,
        "\nLatency (estimated, frames drawn): p50 {:.1}  p95 {:.1}  p99 {:.1}  p99.9 {:.1}  max {:.1} ms",
        ms(percentile(l, 0.5)),
        ms(percentile(l, 0.95)),
        ms(percentile(l, 0.99)),
        ms(percentile(l, 0.999)),
        ms(percentile(l, 1.0))
    );
    let mut bitrate = a.bitrate_kbps.clone();
    bitrate.sort_unstable();
    let mut bytes = a.frame_bytes.clone();
    bytes.sort_unstable();
    let _ = writeln!(
        out,
        "Bitrate target, by frames: p1 {:.1}  p50 {:.1}  p99 {:.1} Mb/s. Frame size p50 {:.1}  p99 {:.1} KB.",
        ms(percentile(&bitrate, 0.01)),
        ms(percentile(&bitrate, 0.5)),
        ms(percentile(&bitrate, 0.99)),
        ms(percentile(&bytes, 0.5)),
        ms(percentile(&bytes, 0.99))
    );
    let _ = writeln!(
        out,
        "\nNetwork: {} packets lost in {} quarter-seconds with loss.",
        a.packets_lost,
        a.packet_bursts.len()
    );
    for &(time, lost) in a.packet_bursts.iter().take(EVENTS_LISTED) {
        let _ = writeln!(out, "  {:>10}  {lost} packets", seconds(time));
    }
    let _ = writeln!(
        out,
        "\nCapture: {} gaps of {} ms or more with images missed (capture stalled); {} with nothing new on \
         screen (the game).",
        a.capture_stalls.len(),
        CAPTURE_GAP_US / 1000,
        a.quiet_gaps
    );
    for &(time, gap, missed) in a.capture_stalls.iter().take(EVENTS_LISTED) {
        let _ =
            writeln!(out, "  {:>10}  {:.0} ms, {missed} images came in that time", seconds(time), gap as f64 / 1000.0);
    }
    let _ = writeln!(
        out,
        "Images replaced before encoding: {} waiting their turn, {} held for the network.",
        a.replaced_turn, a.replaced_backlog
    );
    let [moves, pixels, tagged, keys] = a.local_input;
    let _ = writeln!(
        out,
        "\nHost priority dropped {} remote events. In the quarter-seconds it did, the host's own input: {moves} \
         mouse moves over {pixels} pixels ({tagged} tagged by software), {keys} key presses.",
        a.host_priority_drops
    );
    if a.viewer_without_host > 0 {
        let _ = writeln!(
            out,
            "\n{} frames in the viewer's trace aren't in the host's: wrong pair of files?",
            a.viewer_without_host
        );
    }
    let _ =
        writeln!(out, "\nEvents ({} in all; the longest {EVENTS_LISTED} here, every one in the CSV):", a.events.len());
    let mut longest = a.events.clone();
    longest.sort_by_key(|event| std::cmp::Reverse(event.frames));
    longest.truncate(EVENTS_LISTED);
    longest.sort_by_key(|event| event.start_us);
    for event in longest {
        let _ = writeln!(
            out,
            "  {:>10}  {:<20} frames {}–{} ({}), worst {:.0} ms over",
            seconds(event.start_us),
            event.cause.name(),
            event.first_seq,
            event.last_seq,
            event.frames,
            ms(event.worst_us)
        );
    }
    out
}

fn events_csv(events: &[Event]) -> String {
    let mut out = String::from("start_s,end_s,cause,first_seq,last_seq,frames,worst_over_usual_ms\n");
    for event in events {
        let _ = writeln!(
            out,
            "{:.3},{:.3},{},{},{},{},{:.1}",
            event.start_us as f64 / 1e6,
            event.end_us as f64 / 1e6,
            event.cause.name(),
            event.first_seq,
            event.last_seq,
            event.frames,
            event.worst_us as f64 / 1000.0
        );
    }
    out
}

/// The sessions with `suffix` files in `dir`, newest first: their stamps and paths.
fn sessions(dir: &Path, role: &str, suffix: &str) -> Result<Vec<(u64, PathBuf)>> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        let name = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
        let stamp = name.strip_prefix(role).and_then(|rest| rest.strip_suffix(suffix));
        if let Some(stamp) = stamp.and_then(|stamp| stamp.parse().ok()) {
            found.push((stamp, path));
        }
    }
    found.sort_by_key(|&(stamp, _)| std::cmp::Reverse(stamp));
    Ok(found)
}

/// Reports on the newest session in `dir`: the text, and the path it's saved to.
pub fn write(dir: &Path) -> Result<(String, PathBuf)> {
    let hosts = sessions(dir, "host-", "-frames.csv")?;
    let viewers = sessions(dir, "viewer-", "-frames.csv")?;
    let (Some((host, host_path)), false) = (hosts.first(), viewers.is_empty()) else {
        bail!("{} needs host-…-frames.csv and viewer-…-frames.csv from one session", dir.display());
    };
    // The PCs start a session within seconds of each other.
    let (viewer, viewer_path) = viewers.iter().min_by_key(|(stamp, _)| stamp.abs_diff(*host)).expect("not empty");
    let read = |path: &Path| fs::read_to_string(path).with_context(|| format!("reading {}", path.display()));
    let sibling = |what: &str| dir.join(format!("host-{host}-{what}.csv"));
    let traces = Traces {
        host_frames: read(host_path)?,
        host_captures: fs::read_to_string(sibling("capture")).ok(),
        host_network: fs::read_to_string(sibling("network")).ok(),
        viewer_frames: read(viewer_path)?,
        viewer_losses: read(&dir.join(format!("viewer-{viewer}-loss.csv"))).unwrap_or_default(),
    };
    let analysis = analyse(&traces)?;
    let report = format!("Session {host} (host), {viewer} (viewer)\n\n{}", format_report(&analysis));
    let path = dir.join(format!("report-{host}.txt"));
    fs::write(&path, &report).with_context(|| format!("saving {}", path.display()))?;
    fs::write(dir.join(format!("report-{host}-events.csv")), events_csv(&analysis.events))
        .context("saving the events")?;
    Ok((report, path))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST_HEADER: &str = "seq,capture_time_us,bytes,bitrate_kbps,capture_us,present_to_capture_us,held_us,\
encode_us,queue_us,packetize_us,send_us";
    const VIEWER_HEADER: &str =
        "seq,status,network_est_us,reassembly_us,decode_queue_us,decode_us,render_us,total_est_us";

    /// A session of 1000 frames, 5 ms apart, all fine but for what `change` does: to the host's
    /// row, and to the viewer's, or `None` to leave the frame out of the viewer's trace.
    fn session(change: impl Fn(u32, &mut [i64; 11], &mut [String; 8]) -> bool) -> Traces {
        let (mut host, mut viewer) = (HOST_HEADER.to_owned(), VIEWER_HEADER.to_owned());
        for seq in 0..1000u32 {
            let mut h = [i64::from(seq), i64::from(seq) * 5000, 10_000, 40_000, 100, 100, 0, 2_500, 30, 15, 2];
            let mut v = ["", "shown", "21000", "500", "10", "1500", "300", "27000"].map(str::to_owned);
            v[0] = seq.to_string();
            let seen = change(seq, &mut h, &mut v);
            host += &format!("\n{}", h.map(|n| n.to_string()).join(","));
            if seen {
                viewer += &format!("\n{}", v.join(","));
            }
        }
        Traces { host_frames: host, viewer_frames: viewer, ..Default::default() }
    }

    fn frames(a: &Analysis, cause: Cause) -> u32 {
        a.frames.get(cause.name()).copied().unwrap_or(0)
    }

    #[test]
    fn a_clean_session_has_nothing_to_report() {
        let a = analyse(&session(|_, _, _| true)).unwrap();
        assert_eq!((a.host_frames, a.delivered), (1000, 1000));
        assert!(a.events.is_empty());
    }

    #[test]
    fn says_which_stage_a_late_frame_lost_its_time_in() {
        let a = analyse(&session(|seq, h, v| {
            match seq {
                100 => (h[7], v[7]) = (60_000, "85000".into()),         // the encoder
                200 => (v[2], v[7]) = ("70000".into(), "76000".into()), // the way there
                300 => (h[5], v[7]) = (40_000, "67000".into()),         // capture
                400 => (v[5], v[7]) = ("50000".into(), "76000".into()), // the decoder
                500 => (v[6], v[7]) = ("40000".into(), "67000".into()), // drawing
                _ => {}
            }
            true
        }))
        .unwrap();
        for cause in [Cause::Encoder, Cause::NetworkQueue, Cause::HostCapture, Cause::ViewerDecode, Cause::ViewerDraw] {
            assert_eq!(frames(&a, cause), 1, "{}", cause.name());
        }
        assert_eq!(a.delivered, 995);
    }

    #[test]
    fn accounts_for_frames_that_never_got_drawn() {
        let mut traces = session(|seq, _, v| {
            if seq == 300 {
                v[1] = "superseded".into();
            }
            !(100..103).contains(&seq) && seq != 400 && seq < 990
        });
        traces.viewer_losses =
            "seq,reason,late_datagrams\n100,never_seen,0\n101,incomplete,3\n102,skipped_after_loss,0".to_owned();
        let a = analyse(&traces).unwrap();
        assert_eq!(frames(&a, Cause::NetworkLoss), 1);
        assert_eq!(frames(&a, Cause::NetworkLate), 1);
        assert_eq!(frames(&a, Cause::SkippedAfterLoss), 1);
        assert_eq!(frames(&a, Cause::PresentationDrop), 1);
        assert_eq!(frames(&a, Cause::Unaccounted), 1, "400 has no record; the session's last frames aren't missed");
        let event = a.events.iter().find(|event| event.cause == Cause::Unaccounted).unwrap();
        assert_eq!((event.first_seq, event.last_seq), (400, 400));
    }

    #[test]
    fn merges_a_run_of_trouble_into_one_event() {
        let a = analyse(&session(|seq, h, v| {
            if (100..120).contains(&seq) || (500..505).contains(&seq) {
                (h[7], v[7]) = (60_000, "85000".into());
            }
            true
        }))
        .unwrap();
        let spans: Vec<_> = a.events.iter().map(|e| (e.first_seq, e.last_seq, e.frames)).collect();
        assert_eq!(spans, [(100, 119, 20), (500, 504, 5)]);
        assert_eq!(a.events[0].worst_us, 57_500);
    }

    #[test]
    fn tells_a_capture_stall_from_a_game_with_nothing_to_show() {
        let mut traces = session(|_, _, _| true);
        traces.host_captures = Some(
            "capture_index,time_us,accumulated_frames,previous_replaced\n0,0,1,\n1,5000,1,turn\n\
             2,105000,1,\n3,110000,1,backlog\n4,210000,24,\n"
                .to_owned(),
        );
        let a = analyse(&traces).unwrap();
        assert_eq!(a.capture_stalls, [(210_000, 100_000, 24)]);
        assert_eq!((a.quiet_gaps, a.replaced_turn, a.replaced_backlog), (1, 1, 1));
    }

    #[test]
    fn traces_from_0_9_1_still_work() {
        let a = analyse(&session(|_, _, _| true)).unwrap();
        assert!(a.capture_stalls.is_empty());
        assert!(format_report(&a).contains("1000 frames sent"));
    }
}
