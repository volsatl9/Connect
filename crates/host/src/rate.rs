//! Fits the video to the connection. The bitrate drops as soon as datagrams back up, the viewer
//! loses frames, or loss turns heavy, and climbs back towards the host's cap once the path keeps
//! up. Lost packets are what the recovery data sent along is for, and its share follows the
//! measured loss; stray losses (common on Wi-Fi and the internet) don't mean the path is full.
//!
//! Each time the path proves too slow, the controller remembers roughly what it could carry (a
//! *ceiling*, from the congestion controller's bandwidth estimate where there is one). It climbs
//! back quickly to just under that, then only creeps: going over again would overflow the
//! bottleneck's queue once more, and a full queue drops packets in runs, too many at once for
//! the recovery data, so whole frames go. The ceiling is forgotten after a long calm, as paths
//! get faster too.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};

use connect_net as net;
use iroh::endpoint::Connection;

/// How often [`RateControl::update`] runs.
pub(crate) const TICK: Duration = Duration::from_millis(250);
/// Never below this, however bad the path: a smeary picture beats none.
const MIN_BPS: u32 = 2_000_000;
/// Video waiting to be sent beyond this many seconds' worth means the path can't keep up.
const BACKLOG_HIGH: f64 = 0.060;
const BACKLOG_SOME: f64 = 0.025;
/// How long the path has to keep up before the bitrate climbs again.
const CALM_BEFORE_RAISE: Duration = Duration::from_secs(2);
/// Loss beyond these is more than stray: the path is likely overloaded.
const LOSS_HEAVY: f32 = 0.20;
const LOSS_SOME: f32 = 0.10;
/// Fewer packets than this in a tick say too little about loss.
const MIN_SAMPLE_PACKETS: u64 = 20;
/// While more video than this waits to be sent, capturing more only adds delay: a few frames'
/// worth, in time, so a slow path doesn't let a long queue build (128 KB was 100 ms at 10 Mb/s),
/// with a floor so one keyframe alone doesn't stall the next frame.
const HOLD_BACKLOG: f64 = 0.030;
const HOLD_MIN_BYTES: usize = 64 * 1024;
/// The share of the path's estimated bandwidth the video may aim for: what's left carries the
/// recovery data (8% and up), packet headers, and the congestion controller's probing.
const VIDEO_SHARE: f64 = 0.80;
/// Below this share of the ceiling, the bitrate climbs at the usual pace; above it, it creeps:
/// about 1% a second.
const FAST_UNTIL: f64 = 0.90;
const CREEP: f64 = 1.0025;
/// A ceiling not run into again for this long is forgotten.
const FORGET_CEILING: Duration = Duration::from_secs(20);
/// Recovery requests this soon after the start are the viewer's first keyframe, not loss.
const SETTLING: Duration = Duration::from_secs(2);

/// What the session knows about the path at a tick.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct PathSample {
    /// Running totals.
    pub sent_packets: u64,
    pub lost_packets: u64,
    /// Bytes of datagrams waiting to be sent.
    pub queued_bytes: usize,
    /// The congestion controller's estimate of the path's bandwidth, bits per second.
    pub bandwidth_estimate: Option<u64>,
    /// Running total of the viewer's requests for a keyframe: each means it lost video.
    pub recovery_requests: u64,
}

/// Why the bitrate changed at a tick, for the trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Reason {
    #[default]
    Steady,
    Backlog,
    Loss,
    /// The viewer asked for a keyframe: it lost frames.
    ViewerLoss,
    Climb,
    /// Climbing slowly, near a ceiling the path hit before.
    Creep,
}

impl Reason {
    pub fn name(self) -> &'static str {
        match self {
            Reason::Steady => "steady",
            Reason::Backlog => "backlog",
            Reason::Loss => "loss",
            Reason::ViewerLoss => "viewer_loss",
            Reason::Climb => "climb",
            Reason::Creep => "creep",
        }
    }
}

pub(crate) struct RateControl {
    max_bps: u32,
    bps: Arc<AtomicU32>,
    /// Smoothed share of packets lost on the path.
    loss: f32,
    sent: u64,
    lost: u64,
    recovery_requests: u64,
    started: Instant,
    calm_since: Instant,
    /// Roughly what the path carried when it last proved too slow, and when.
    ceiling: Option<(u32, Instant)>,
    /// Whether to keep a ceiling and count the viewer's losses: off with `CONNECT_RATE_CEILING=0`,
    /// to compare with 0.9.0's controller.
    learn: bool,
    reason: Reason,
}

impl RateControl {
    pub fn new(max_bps: u32) -> Self {
        let max_bps = max_bps.max(MIN_BPS);
        let now = Instant::now();
        Self {
            max_bps,
            bps: Arc::new(AtomicU32::new(max_bps)),
            loss: 0.0,
            sent: 0,
            lost: 0,
            recovery_requests: 0,
            started: now,
            calm_since: now,
            ceiling: None,
            learn: std::env::var("CONNECT_RATE_CEILING").map_or(true, |v| v.trim() != "0"),
            reason: Reason::Steady,
        }
    }

    pub fn bitrate(&self) -> u32 {
        self.bps.load(Ordering::Relaxed)
    }

    /// Smoothed share of packets lost, 0..1.
    pub fn loss(&self) -> f32 {
        self.loss
    }

    /// The ceiling the controller is keeping under, if any.
    pub fn ceiling(&self) -> Option<u32> {
        self.ceiling.map(|(bps, _)| bps)
    }

    /// Why the last update did what it did.
    pub fn reason(&self) -> Reason {
        self.reason
    }

    /// A check for the encoder thread: true while so much video waits to be sent that encoding
    /// another frame would only queue it behind the rest.
    pub fn backlog_check(&self, conn: Connection) -> impl Fn() -> bool + Send + 'static {
        let bps = self.bps.clone();
        move || backlogged(net::queued_datagram_bytes(&conn), bps.load(Ordering::Relaxed))
    }

    /// Runs every [`TICK`] with what the session knows about the path. The new bitrate, if it
    /// changed.
    pub fn update(&mut self, now: Instant, path: PathSample) -> Option<u32> {
        let sent = path.sent_packets.saturating_sub(self.sent);
        let lost = path.lost_packets.saturating_sub(self.lost);
        (self.sent, self.lost) = (path.sent_packets, path.lost_packets);
        let sample = if sent >= MIN_SAMPLE_PACKETS { (lost as f32 / sent as f32).min(1.0) } else { self.loss };
        self.loss = self.loss * 0.7 + sample * 0.3;
        let viewer_lost = self.learn
            && path.recovery_requests > self.recovery_requests
            && now.duration_since(self.started) >= SETTLING;
        self.recovery_requests = path.recovery_requests;

        let before = self.bitrate();
        let backlog = path.queued_bytes as f64 * 8.0 / before as f64;
        let clamp = |bps: f64| (bps as u32).clamp(MIN_BPS, self.max_bps);
        let congested = if backlog > BACKLOG_HIGH {
            Some((0.7, Reason::Backlog))
        } else if sample > LOSS_HEAVY {
            Some((0.7, Reason::Loss))
        } else if backlog > BACKLOG_SOME {
            Some((0.85, Reason::Backlog))
        } else if sample > LOSS_SOME {
            Some((0.85, Reason::Loss))
        } else if viewer_lost {
            Some((0.85, Reason::ViewerLoss))
        } else {
            None
        };
        if self.ceiling.is_some_and(|(_, at)| now.duration_since(at) >= FORGET_CEILING) {
            self.ceiling = None;
        }
        let (after, reason) = if let Some((factor, reason)) = congested {
            self.calm_since = now;
            // Where the path gave out: the congestion controller's estimate of what it carries,
            // less room for the rest of the traffic, or failing that, a step under where we were.
            let ceiling = match path.bandwidth_estimate {
                Some(bps) => clamp(bps as f64 * VIDEO_SHARE).min(before),
                None => clamp(f64::from(before) * 0.85),
            };
            if !self.learn {
                return self.settle(before, clamp(f64::from(before) * factor), reason);
            }
            self.ceiling = Some((ceiling, now));
            (clamp(f64::from(before) * factor).min(ceiling), reason)
        } else if now.duration_since(self.calm_since) >= CALM_BEFORE_RAISE && before < self.max_bps {
            match self.ceiling {
                Some((ceiling, _)) if f64::from(before) >= f64::from(ceiling) * FAST_UNTIL => {
                    (clamp(f64::from(before) * CREEP), Reason::Creep)
                }
                Some((ceiling, _)) => {
                    let up = clamp(f64::from(before) * 1.05).max(before.saturating_add(250_000).min(self.max_bps));
                    (up.min(clamp(f64::from(ceiling) * FAST_UNTIL)).max(before), Reason::Climb)
                }
                // Up gently: about doubling in four seconds.
                None => (
                    clamp(f64::from(before) * 1.05).max(before.saturating_add(250_000).min(self.max_bps)),
                    Reason::Climb,
                ),
            }
        } else {
            (before, Reason::Steady)
        };
        self.settle(before, after, reason)
    }

    fn settle(&mut self, before: u32, after: u32, reason: Reason) -> Option<u32> {
        self.reason = reason;
        self.bps.store(after, Ordering::Relaxed);
        (after != before).then_some(after)
    }
}

fn backlogged(queued_bytes: usize, bps: u32) -> bool {
    queued_bytes > HOLD_MIN_BYTES && queued_bytes as f64 * 8.0 > bps as f64 * HOLD_BACKLOG
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(sent: u64, lost: u64, queued: usize) -> PathSample {
        PathSample { sent_packets: sent, lost_packets: lost, queued_bytes: queued, ..Default::default() }
    }

    #[test]
    fn backs_off_on_backlog_or_loss_and_recovers_slowly() {
        let mut rate = RateControl::new(20_000_000);
        let start = rate.started;
        let at = |ms: u64| start + Duration::from_millis(ms);
        // A clean path leaves the cap alone.
        assert_eq!(rate.update(at(250), sample(1000, 0, 0)), None);
        // 200 ms of video queued: cut hard, and remember a ceiling under where it broke.
        assert_eq!(rate.update(at(500), sample(2000, 0, 500_000)), Some(14_000_000));
        assert_eq!(rate.ceiling(), Some(17_000_000));
        // Stray loss is for the recovery data to fix, not a reason to send less.
        assert_eq!(rate.update(at(750), sample(3000, 50, 0)), None);
        assert!(rate.loss() > 0.01);
        // Loss well beyond stray is: the path is likely overloaded.
        assert_eq!(rate.update(at(875), sample(4000, 200, 0)), Some(11_900_000));
        assert_eq!((rate.reason(), rate.ceiling()), (Reason::Loss, Some(11_900_000)));
        // Calm, but not for long enough yet.
        assert_eq!(rate.update(at(1000), sample(5000, 200, 0)), None);
        // After two calm seconds it creeps, as it's at the ceiling it just hit.
        assert_eq!(rate.update(at(2875), sample(6000, 200, 0)), Some(11_929_750));
        assert_eq!(rate.reason(), Reason::Creep);
        // About 1% a second, until the ceiling is forgotten after 20 calm seconds; then up to the
        // cap at the usual pace.
        let mut bitrates = Vec::new();
        for tick in 0..240 {
            rate.update(at(3000 + tick * 250), sample(7000 + tick * 1000, 200, 0));
            bitrates.push(rate.bitrate());
        }
        assert!(bitrates[8] < 12_300_000, "creeping near the ceiling: {:?}", &bitrates[..12]);
        assert_eq!(rate.bitrate(), 20_000_000, "forgot the ceiling after a long calm");
        assert!(rate.loss() < 0.001);
    }

    #[test]
    fn the_bandwidth_estimate_sets_the_ceiling() {
        let mut rate = RateControl::new(80_000_000);
        let start = rate.started;
        let at = |ms: u64| start + Duration::from_millis(ms);
        let path = |queued, estimate| PathSample {
            queued_bytes: queued,
            bandwidth_estimate: Some(estimate),
            sent_packets: 1000,
            ..Default::default()
        };
        // At 80 Mb/s on a 40 Mb/s path: straight down to the video's share of it.
        assert_eq!(rate.update(at(3000), path(400_000, 40_000_000)), Some(32_000_000));
        assert_eq!(rate.ceiling(), Some(32_000_000));
    }

    #[test]
    fn a_viewer_losing_frames_counts_as_congestion() {
        let mut rate = RateControl::new(40_000_000);
        let start = rate.started;
        let at = |ms: u64| start + Duration::from_millis(ms);
        let requests = |n| PathSample { recovery_requests: n, sent_packets: 1000, ..Default::default() };
        // The first keyframe the viewer asks for at the start isn't loss.
        assert_eq!(rate.update(at(500), requests(1)), None);
        // Later ones are.
        assert_eq!(rate.update(at(5000), requests(2)), Some(34_000_000));
        assert_eq!(rate.reason(), Reason::ViewerLoss);
        assert_eq!(rate.update(at(5250), requests(2)), None, "only new requests count");
    }

    /// The controller against a model of Kellu's 0.9.0 session: a path that carries 80 Mb/s,
    /// then drops to 40. The encoder follows the target exactly (as the hardware encoder did).
    /// Whatever goes over the path's rate queues (BBR paces to the path), and that excess is
    /// what overflows the bottleneck and drops runs of packets: bursts of lost frames. The old
    /// controller (no ceiling) ran into it again and again, much further over each time.
    #[test]
    fn keeps_under_a_path_that_slowed_down() {
        struct Run {
            cuts: u32,
            /// Megabytes sent beyond what the path could carry.
            excess_mb: f64,
            /// Quarter seconds with over 100 KB queued.
            deep_queue_ticks: u32,
            mbps: f64,
        }
        fn run(remember: bool) -> Run {
            let mut rate = RateControl::new(80_000_000);
            let start = rate.started;
            let (mut queued, mut sent) = (0f64, 0u64);
            let mut result = Run { cuts: 0, excess_mb: 0.0, deep_queue_ticks: 0, mbps: 0.0 };
            for tick in 1..=480u64 {
                let now = start + TICK * tick as u32;
                let capacity = if tick < 120 { 80e6 } else { 40e6 };
                let offered = f64::from(rate.bitrate()) * 1.12; // video plus recovery and headers
                let seconds = TICK.as_secs_f64();
                result.excess_mb += (offered - capacity).max(0.0) * seconds / 8e6;
                queued = (queued + (offered - capacity) * seconds / 8.0).max(0.0);
                result.deep_queue_ticks += u32::from(queued > 100_000.0);
                result.mbps += offered.min(capacity) * seconds / 120.0 / 1e6;
                sent += 1000;
                let before = rate.bitrate();
                rate.update(
                    now,
                    PathSample {
                        sent_packets: sent,
                        queued_bytes: queued as usize,
                        bandwidth_estimate: Some(capacity as u64),
                        ..Default::default()
                    },
                );
                if !remember {
                    rate.ceiling = None;
                }
                result.cuts += u32::from(rate.bitrate() < before);
            }
            result
        }
        let (old, new) = (run(false), run(true));
        for (name, r) in [("old", &old), ("new", &new)] {
            println!(
                "{name}: {} cuts, {:.1} MB over the path, {} ticks with a deep queue, {:.1} Mb/s delivered",
                r.cuts, r.excess_mb, r.deep_queue_ticks, r.mbps
            );
        }
        assert!(new.cuts * 3 <= old.cuts, "{} vs {} cuts", new.cuts, old.cuts);
        assert!(new.excess_mb * 3.0 <= old.excess_mb, "{:.1} vs {:.1} MB over", new.excess_mb, old.excess_mb);
        // What's left of the deep queue is each creep finding the ceiling: the backlog that says
        // so (25 ms of video) is itself about 100 KB at 40 Mb/s.
        assert!(
            new.deep_queue_ticks * 2 <= old.deep_queue_ticks,
            "{} vs {}",
            new.deep_queue_ticks,
            old.deep_queue_ticks
        );
        assert!(new.mbps > old.mbps * 0.9, "without giving up much throughput: {:.1} vs {:.1}", new.mbps, old.mbps);
    }

    #[test]
    fn never_below_the_floor() {
        let mut rate = RateControl::new(3_000_000);
        for tick in 1..20 {
            rate.update(Instant::now(), sample(tick * 100, tick * 50, 10_000_000));
        }
        assert_eq!(rate.bitrate(), MIN_BPS);
    }

    #[test]
    fn holds_capture_only_for_a_real_backlog() {
        assert!(!backlogged(60_000, 2_000_000)); // under one large frame, however slow the stream
        assert!(!backlogged(200_000, 80_000_000)); // 20 ms at 80 Mb/s
        assert!(backlogged(400_000, 80_000_000)); // 40 ms at 80 Mb/s
        assert!(backlogged(100_000, 10_000_000)); // 80 ms at 10 Mb/s: held, where 0.9.0 let it build
    }
}
