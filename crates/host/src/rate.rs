//! Fits the video to the connection. The bitrate drops as soon as datagrams back up, the viewer
//! loses frames, or loss turns heavy, and climbs back towards the host's cap once the path keeps
//! up. Lost packets are what the recovery data sent along is for, and its share follows the
//! measured loss; stray losses (common on Wi-Fi and the internet) don't mean the path is full.
//!
//! Each time the path proves too slow, the controller remembers roughly what it could carry (a
//! *ceiling*, from the congestion controller's bandwidth estimate where there is one). It climbs
//! back quickly to just under that, then only creeps: going over again would overflow the
//! bottleneck's queue once more, and a full queue drops packets in runs, too many at once for
//! the recovery data, so whole frames go. The ceiling rises when the path measures faster, and is forgotten
//! after a long calm. BBR's estimate only sees a little past what's being sent, so once things
//! are calm the ceiling follows it: otherwise the two wait for each other, and a low bitrate
//! stays low on a path that came back long ago.
//!
//! A game's video comes in bursts, so what counts as the path giving out needs care. One large
//! frame queues for a moment on any path, so only a backlog lasting from one tick to the next
//! does. And one burst of lost packets while the video uses a fraction of the path says something
//! went wrong on the way, not that it was full: sending less wouldn't have helped. Heavy loss,
//! or loss going on from one tick to the next, counts whatever the bandwidth estimate says, as
//! that lags when the path really slows down.
//!
//! Frames lost to a low share of loss get one step down, which shows whether sending less helps.
//! If the loss carries on as before, the path is losing packets whatever it's sent: more cuts
//! would only cost picture, so the bitrate holds and the recovery data takes it from there.

use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};

use connect_net as net;
use connect_proto::control::ViewerFeedback;
use iroh::endpoint::Connection;

/// How often [`RateControl::update`] runs.
pub(crate) const TICK: Duration = Duration::from_millis(250);
/// Never below this, however bad the path: a smeary picture beats none.
const MIN_BPS: u32 = 2_000_000;
/// Where a stream starts, under its maximum: high enough to look good from the first frames, low
/// enough not to flood most uploads. It climbs from there.
const START_BPS: u32 = 30_000_000;
/// Until the path first pushes back, the bitrate climbs this much a tick after this long.
const FIRST_CLIMB: f64 = 1.10;
const FIRST_CALM: Duration = Duration::from_millis(500);
/// What's worth sending for each pixel of each frame, in bits, in automatic mode.
const AUTO_BITS_PER_PIXEL: f64 = 0.25;
/// A round trip this far past its best, for two ticks, means a queue past this PC, in a modem
/// say, that the send queue can't show.
const DELAYED: f64 = 1.5;
const DELAYED_BY: Duration = Duration::from_millis(15);
const VERY_DELAYED_BY: Duration = Duration::from_millis(50);
/// Only with the video this close to the estimate is the video the likely cause. Wi-Fi scans
/// spiked the round trip with 34 of 120 Mb/s going, and a full upload showed 35 of 47.
const DELAY_FILLS: f64 = 0.6;
/// How far under what was going a delay leaves the ceiling: it was already too much.
const DELAY_CEILING: f64 = 0.9;
/// The best round trip seen stops counting after this long, in case the route changed.
const BEST_RTT_LASTS: Duration = Duration::from_secs(120);
/// The viewer getting this much less than was sent, for two reports, means video is lost or
/// queued on the way.
const RECEIVED_SHARE: f64 = 0.75;
/// Reports older than this say nothing about now.
const FEEDBACK_LASTS: Duration = Duration::from_millis(2500);
/// A report covers about this many ticks.
const REPORT_TICKS: usize = 4;
/// Time for a cut for the round trip to show in it before the next.
const BETWEEN_DELAY_CUTS: Duration = Duration::from_secs(1);
/// Busier than this, an encoder or a decoder is the limit, not the path.
const SATURATED: f64 = 0.95;
const RELAXED: f64 = 0.8;
/// Frames waiting at the viewer's decoder that say it's fallen behind.
const DECODER_BEHIND: u32 = 8;
/// How long the encoder has to stay saturated before it counts, and quiet before it's let off.
const ENCODER_SATURATED_FOR: Duration = Duration::from_secs(2);
const ENCODER_CALM_FOR: Duration = Duration::from_secs(10);
/// How long the path has to stay beyond help at the minimum before going under it.
const UNUSABLE_AFTER: Duration = Duration::from_secs(2);
/// Video waiting to be sent beyond this many seconds' worth means the path can't keep up.
const BACKLOG_HIGH: f64 = 0.060;
const BACKLOG_SOME: f64 = 0.025;
/// Less than this is no backlog at any bitrate, whatever it comes to in time: a large frame or
/// two on its way.
const BACKLOG_MIN_BYTES: usize = 32 * 1024;
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
/// The congestion controller's estimate only says what the path carries once video has tested
/// it: this share of the bitrate, for this long. After a loading screen it says how little was
/// sent, and taking that for the path cut 80 Mb/s to the floor at a backlog with nothing lost.
const TESTED_SHARE: f64 = 0.5;
const TESTED_FOR: Duration = Duration::from_secs(2);
/// So has video that stays near the estimate for as long, however far under the bitrate: on an
/// upload slower than half the bitrate, the video never reaches that share.
const STEADY_TICKS: usize = 8;
/// How long to stay under the ceiling after raising the bitrate into trouble, doubling each time
/// up to the most: trying is how a better path is found, and on an upload already full each try
/// is a burst of delay.
const MOST_RETRY_WAIT: Duration = Duration::from_secs(120);
/// How far over its value at the ceiling the estimate has to be for the path to count as better.
const BETTER_PATH: f64 = 1.25;
/// After this long without trouble, the ceiling follows the estimate however little it moved.
const FOLLOW_ESTIMATE_AFTER: Duration = Duration::from_secs(3);
/// Loss still over this share of what it was at a cut means the cut didn't help.
const NO_BETTER: f32 = 0.7;
/// Less loss than this, smoothed, isn't what cost the viewer its frames.
const LOSS_NOTICED: f32 = 0.005;
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
    /// Running total of video bytes handed to the network.
    pub video_bytes: u64,
    /// The smoothed round trip. Goes stale when nothing comes back.
    pub rtt: Option<Duration>,
    /// How hard the host's encoder is working, kept apart from everything about the path.
    pub encoder: Option<EncoderLoad>,
}

/// How hard the encoding is working.
#[derive(Debug, Clone, Copy, Default)]
pub struct EncoderLoad {
    /// Share of the time spent encoding, for everyone connected, 0..1.
    pub busy: f64,
    /// Whether a graphics card does the work, rather than the processor.
    pub hardware: bool,
}

/// What the host's settings allow a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BitrateLimits {
    /// Fit the bitrate to the picture too, not only to the path.
    pub automatic: bool,
    pub max_bps: u32,
    pub min_bps: u32,
}

impl BitrateLimits {
    fn bounded(self) -> Self {
        let max_bps = self.max_bps.max(MIN_BPS);
        Self { max_bps, min_bps: self.min_bps.clamp(MIN_BPS, max_bps), ..self }
    }
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
    /// The viewer lost frames, but the last cut did nothing for the loss: holding.
    LossHeld,
    /// The round trip stayed well past its best, with the video near what the path carries.
    Delay,
    /// The viewer reported getting well under what was sent.
    ViewerRate,
    /// The viewer's decoder is at its limit.
    ViewerDecode,
    /// Over the settings, this viewer's share of the upload, or what the encoder keeps up with.
    Limit,
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
            Reason::LossHeld => "loss_held",
            Reason::Delay => "delay",
            Reason::ViewerRate => "viewer_rate",
            Reason::ViewerDecode => "viewer_decode",
            Reason::Limit => "limit",
        }
    }
}

pub(crate) struct RateControl {
    bps: Arc<AtomicU32>,
    /// Smoothed share of packets lost on the path.
    loss: f32,
    sent: u64,
    lost: u64,
    recovery_requests: u64,
    video_bytes: u64,
    /// Whether video was backing up, and packets were being lost, at the last tick.
    backing_up: bool,
    losing: bool,
    started: Instant,
    calm_since: Instant,
    /// When the video last used less than [`TESTED_SHARE`] of the bitrate.
    quiet_at: Option<Instant>,
    /// The video's rate over the last [`STEADY_TICKS`] ticks.
    recent_video: VecDeque<f64>,
    /// Roughly what the path carried when it last proved too slow, and when.
    ceiling: Option<(u32, Instant)>,
    /// The bandwidth estimate then.
    estimate_at_ceiling: u64,
    /// The smoothed loss at the last cut made for it, until the loss falls.
    loss_at_cut: Option<f32>,
    /// How long to keep under the ceiling before trying over it again.
    retry_wait: Duration,
    /// The most and least the settings allow, and the least if the path is beyond it.
    limits: BitrateLimits,
    /// The screen's pixels a second, in automatic mode, once known.
    pixels_per_second: Option<f64>,
    /// This viewer's even share of the host's upload, with others connected.
    share: Option<u32>,
    /// The most the encoder has kept up with, for encoders whose work grows with the bitrate.
    encoder_cap: Option<u32>,
    encoder_saturated_at: Option<Instant>,
    encoder_calm_at: Option<Instant>,
    encoder_limited: bool,
    /// When the path started being beyond help at the minimum.
    unusable_since: Option<Instant>,
    /// Whether the path has pushed back yet.
    proved: bool,
    /// The best round trip seen and when, and whether the last tick's was well past it.
    best_rtt: Option<(Duration, Instant)>,
    delayed: bool,
    /// Whether the ceiling came from the round trip, which the estimate mustn't lift: it reads
    /// high on a path whose queue is out of sight.
    delay_ceiling: bool,
    /// The last report from the viewer, and when it came.
    feedback: Option<(ViewerFeedback, Instant)>,
    /// Whether that report hasn't been looked at yet: each counts once.
    new_report: bool,
    receiving_short: bool,
    /// The last cut for the round trip: time for it to show before the next.
    delay_cut_at: Option<Instant>,
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
            bps: Arc::new(AtomicU32::new(max_bps)),
            loss: 0.0,
            sent: 0,
            lost: 0,
            recovery_requests: 0,
            video_bytes: 0,
            backing_up: false,
            losing: false,
            started: now,
            calm_since: now,
            quiet_at: None,
            recent_video: VecDeque::with_capacity(STEADY_TICKS + 1),
            ceiling: None,
            estimate_at_ceiling: 0,
            loss_at_cut: None,
            retry_wait: CALM_BEFORE_RAISE,
            limits: BitrateLimits { automatic: false, max_bps, min_bps: MIN_BPS },
            pixels_per_second: None,
            share: None,
            encoder_cap: None,
            encoder_saturated_at: None,
            encoder_calm_at: None,
            encoder_limited: false,
            unusable_since: None,
            proved: true,
            best_rtt: None,
            delayed: false,
            delay_ceiling: false,
            feedback: None,
            new_report: false,
            receiving_short: false,
            delay_cut_at: None,
            learn: std::env::var("CONNECT_RATE_CEILING").map_or(true, |v| v.trim() != "0"),
            reason: Reason::Steady,
        }
    }

    /// Starts under the maximum and climbs, within `limits`.
    pub fn with_limits(limits: BitrateLimits) -> Self {
        let limits = limits.bounded();
        let mut rate = Self::new(limits.max_bps);
        rate.limits = limits;
        rate.bps.store(START_BPS.clamp(limits.min_bps, limits.max_bps), Ordering::Relaxed);
        rate.proved = false;
        rate
    }

    /// New settings, effective at the next tick.
    pub fn set_limits(&mut self, limits: BitrateLimits) {
        self.limits = limits.bounded();
    }

    /// The most the settings ask for: the maximum the encoder is opened for.
    pub fn most(&self) -> u32 {
        self.limits.max_bps
    }

    /// The screen's size and rate, for what's worth sending in automatic mode.
    pub fn set_format(&mut self, width: u32, height: u32, fps: u32) {
        self.pixels_per_second = Some(f64::from(width) * f64::from(height) * f64::from(fps.max(30)));
    }

    /// This viewer's share of the upload, or `None` for all of it.
    pub fn set_share(&mut self, share: Option<u32>) {
        self.share = share;
    }

    /// A report from the viewer, bounded: it can only lower the bitrate.
    pub fn feedback(&mut self, now: Instant, report: ViewerFeedback) {
        self.feedback = Some((report.bounded(), now));
        self.new_report = true;
    }

    /// The most the bitrate may be now, whatever the path would take.
    pub fn cap(&self) -> u32 {
        let automatic = self
            .pixels_per_second
            .filter(|_| self.limits.automatic)
            .map(|pixels| ((pixels * AUTO_BITS_PER_PIXEL).min(f64::from(u32::MAX)) as u32).max(START_BPS));
        [Some(self.limits.max_bps), automatic, self.share, self.encoder_cap]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(self.limits.max_bps)
            .max(self.limits.min_bps)
    }

    /// The most this viewer's own connection and the settings allow, whatever others take.
    pub fn own_limit(&self) -> u32 {
        self.ceiling.map_or(self.limits.max_bps, |(ceiling, _)| ceiling.min(self.limits.max_bps))
    }

    /// Video going now, b/s, over the last couple of seconds.
    pub fn sending(&self) -> f64 {
        self.recent_video.iter().sum::<f64>() / self.recent_video.len().max(1) as f64
    }

    /// The last report from the viewer.
    pub fn report(&self) -> Option<ViewerFeedback> {
        self.feedback.map(|(report, _)| report)
    }

    /// Whether the last tick cut the bitrate because of the path.
    pub fn cut_for_the_path(&self) -> bool {
        matches!(self.reason, Reason::Backlog | Reason::Loss | Reason::ViewerLoss | Reason::Delay | Reason::ViewerRate)
    }

    /// Whether the encoder, not the path, is what's holding the video back.
    pub fn encoder_limited(&self) -> bool {
        self.encoder_limited
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
        // Almost nothing going out with video waiting: nothing is coming back to send on. Connect
        // starting up, or a dropout. Sending less wouldn't help, and the picture would suffer for
        // a long while after, so this says nothing about the bitrate.
        let silent = self.learn && sent < MIN_SAMPLE_PACKETS;
        self.watch_encoder(now, path.encoder, before);
        let backing_up = path.queued_bytes >= BACKLOG_MIN_BYTES && backlog > BACKLOG_SOME && !silent;
        let lasting = !self.learn || (backing_up && self.backing_up);
        self.backing_up = backing_up;
        // Loss says the path is full if it lasts, or if the video was using all of it.
        let video_bps = path.video_bytes.saturating_sub(self.video_bytes) as f64 * 8.0 / TICK.as_secs_f64();
        self.video_bytes = path.video_bytes;
        if path.video_bytes > 0 && video_bps < f64::from(before) * TESTED_SHARE {
            self.quiet_at = Some(now);
        }
        if path.video_bytes > 0 {
            self.recent_video.push_back(video_bps);
            if self.recent_video.len() > STEADY_TICKS {
                self.recent_video.pop_front();
            }
        }
        let steady = self.recent_video.len() == STEADY_TICKS
            && self.recent_video.iter().all(|&bps| bps >= video_bps * TESTED_SHARE)
            && path.bandwidth_estimate.is_some_and(|estimate| video_bps >= estimate as f64 * TESTED_SHARE);
        let tested = steady || self.quiet_at.is_none_or(|at| now.duration_since(at) >= TESTED_FOR);
        let losing = sent >= MIN_SAMPLE_PACKETS && lost > 0 && sample > LOSS_SOME;
        let fresh = self.feedback.filter(|(_, at)| now.duration_since(*at) <= FEEDBACK_LASTS).map(|(report, _)| report);
        let (over, delay_cut) = self.watch_round_trip(now, path, video_bps, silent);
        let last_second = self.recent_video.iter().rev().take(REPORT_TICKS);
        let sending = last_second.clone().sum::<f64>() / last_second.count().max(1) as f64;
        let (received_cut, decoder_cut) = self.watch_viewer(fresh, sending, silent);
        let viewer_loss = fresh.is_some_and(|report| f32::from(report.loss_permille) / 1000.0 > LOSS_SOME);
        let filling = !self.learn
            || (losing && self.losing)
            || path.bandwidth_estimate.is_none_or(|estimate| video_bps >= estimate as f64 * VIDEO_SHARE);
        self.losing = losing;
        let cap = self.cap();
        let floor = self.floor(now, before, backlog > BACKLOG_HIGH && lasting);
        let clamp = |bps: f64| (bps as u32).clamp(floor, cap.max(floor));
        self.loss_at_cut = self.loss_at_cut.filter(|at_cut| self.loss >= at_cut * NO_BETTER);
        let holding = filling && viewer_lost && self.loss_at_cut.is_some();
        let congested = if lasting && backlog > BACKLOG_HIGH {
            Some((0.7, Reason::Backlog))
        } else if sample > LOSS_HEAVY {
            Some((0.7, Reason::Loss))
        } else if lasting && backlog > BACKLOG_SOME {
            Some((0.85, Reason::Backlog))
        } else if filling && (sample > LOSS_SOME || viewer_loss) {
            Some((0.85, Reason::Loss))
        } else if filling && viewer_lost && !holding {
            Some((0.85, Reason::ViewerLoss))
        } else if delay_cut {
            let very = over.is_some_and(|(rtt, best)| rtt > best + VERY_DELAYED_BY);
            Some((if very { 0.7 } else { 0.85 }, Reason::Delay))
        } else if received_cut {
            Some((0.85, Reason::ViewerRate))
        } else if decoder_cut {
            Some((0.85, Reason::ViewerDecode))
        } else {
            None
        };
        // A ceiling hit again stays: over it, only creeping, as a climb would overshoot.
        if self.retry_wait == CALM_BEFORE_RAISE
            && !self.delay_ceiling
            && self.ceiling.is_some_and(|(_, at)| now.duration_since(at) >= FORGET_CEILING)
        {
            self.ceiling = None;
        }
        // The path measures faster than when it last gave out: it has got better.
        if let (None, false, Some((ceiling, at)), Some(estimate)) =
            (congested, self.delay_ceiling, self.ceiling, path.bandwidth_estimate)
            && (estimate as f64 > self.estimate_at_ceiling as f64 * BETTER_PATH
                || now.duration_since(self.calm_since) >= FOLLOW_ESTIMATE_AFTER)
        {
            if estimate as f64 > self.estimate_at_ceiling as f64 * BETTER_PATH {
                self.retry_wait = CALM_BEFORE_RAISE;
            }
            self.ceiling = Some((ceiling.max(clamp(estimate as f64 * VIDEO_SHARE)), at));
        }
        if before > cap.max(floor) {
            return self.settle(before, cap.max(floor), Reason::Limit);
        }
        let (after, reason) = if let Some((factor, reason)) = congested {
            self.calm_since = now;
            self.proved = true;
            self.retry_after_failure();
            // What was going was too much, whatever the estimate says.
            if matches!(reason, Reason::Delay | Reason::ViewerRate | Reason::ViewerDecode) {
                let going = f64::from(before).min(if sending > 0.0 { sending } else { video_bps });
                let going = if reason == Reason::ViewerRate {
                    fresh.map_or(going, |report| going.min(f64::from(report.receive_kbps) * 1000.0))
                } else {
                    going
                };
                self.ceiling = Some((clamp(going * DELAY_CEILING), now));
                self.delay_ceiling = true;
                self.estimate_at_ceiling = path.bandwidth_estimate.unwrap_or(u64::MAX);
                return self.settle(before, clamp(going * factor), reason);
            }
            // Nothing says the path is slow, only that a burst is queued: a step, not the estimate.
            if self.learn && reason == Reason::Backlog && !tested && self.loss < LOSS_NOTICED {
                return self.settle(before, clamp(f64::from(before) * factor), reason);
            }
            // Where the path gave out: the congestion controller's estimate of what it carries,
            // less room for the rest of the traffic, or failing that, a step under where we were.
            let ceiling = match path.bandwidth_estimate {
                Some(bps) => clamp(bps as f64 * VIDEO_SHARE).min(before),
                None => clamp(f64::from(before) * 0.85),
            };
            if !self.learn {
                return self.settle(before, clamp(f64::from(before) * factor), reason);
            }
            self.delay_ceiling = false;
            self.ceiling = Some((ceiling, now));
            self.estimate_at_ceiling = path.bandwidth_estimate.unwrap_or(u64::MAX);
            if reason == Reason::ViewerLoss && self.loss >= LOSS_NOTICED {
                self.loss_at_cut = Some(self.loss);
            }
            (clamp(f64::from(before) * factor).min(ceiling), reason)
        } else if holding {
            self.calm_since = now;
            (before, Reason::LossHeld)
        } else if !self.proved && now.duration_since(self.calm_since) >= FIRST_CALM && before < cap {
            (clamp(f64::from(before) * FIRST_CLIMB), Reason::Climb)
        } else if now.duration_since(self.calm_since) >= CALM_BEFORE_RAISE && before < cap {
            match self.ceiling.map(|(ceiling, _)| clamp(f64::from(ceiling) * FAST_UNTIL)) {
                Some(creep_from) if before >= creep_from => {
                    let waited = now.duration_since(self.calm_since) >= self.retry_wait;
                    if waited { (clamp(f64::from(before) * CREEP), Reason::Creep) } else { (before, Reason::Steady) }
                }
                Some(creep_from) => {
                    let up = clamp(f64::from(before) * 1.05).max(before.saturating_add(250_000).min(cap));
                    (up.min(creep_from).max(before), Reason::Climb)
                }
                // Up gently: about doubling in four seconds.
                None => (clamp(f64::from(before) * 1.05).max(before.saturating_add(250_000).min(cap)), Reason::Climb),
            }
        } else {
            (before, Reason::Steady)
        };
        self.settle(before, after, reason)
    }

    /// A cut after raising the bitrate means the path's limit was found again: try it less often.
    fn retry_after_failure(&mut self) {
        if matches!(self.reason, Reason::Climb | Reason::Creep) {
            self.retry_wait = (self.retry_wait * 2).min(MOST_RETRY_WAIT);
        }
    }

    /// The round trip: how far past its best if it is, and whether that calls for a cut.
    fn watch_round_trip(
        &mut self,
        now: Instant,
        path: PathSample,
        video_bps: f64,
        silent: bool,
    ) -> (Option<(Duration, Duration)>, bool) {
        let rtt = path.rtt.filter(|rtt| !rtt.is_zero());
        if let Some(rtt) = rtt
            && self.best_rtt.is_none_or(|(best, at)| rtt <= best || now.duration_since(at) >= BEST_RTT_LASTS)
        {
            self.best_rtt = Some((rtt, now));
        }
        let best = self.best_rtt.map(|(best, _)| best);
        let over = rtt.zip(best).filter(|(rtt, best)| !silent && *rtt > best.mul_f64(DELAYED).max(*best + DELAYED_BY));
        let fills = path.bandwidth_estimate.is_some_and(|estimate| video_bps >= estimate as f64 * DELAY_FILLS);
        let due = self.delay_cut_at.is_none_or(|at| now.duration_since(at) >= BETWEEN_DELAY_CUTS);
        let cut = self.learn && self.delayed && over.is_some() && fills && due;
        if cut {
            self.delay_cut_at = Some(now);
        }
        self.delayed = over.is_some();
        (over, cut)
    }

    /// A new report from the viewer, if there is one: whether what it gets falls well short of
    /// what was sent, for the second report running, and whether its decoder is at its limit.
    fn watch_viewer(&mut self, report: Option<ViewerFeedback>, sending: f64, silent: bool) -> (bool, bool) {
        let Some(report) = report else {
            self.receiving_short = false;
            return (false, false);
        };
        if !std::mem::take(&mut self.new_report) {
            return (false, false);
        }
        let short = !silent && sending > 0.0 && f64::from(report.receive_kbps) * 1000.0 < sending * RECEIVED_SHARE;
        let was_short = std::mem::replace(&mut self.receiving_short, short);
        let busy = f64::from(report.decode_us) * f64::from(report.frames_decoded) / 1e6;
        let behind = report.frames_queued >= DECODER_BEHIND;
        let saturated = behind || (busy > SATURATED && report.frames_dropped > 0);
        (self.learn && short && was_short, self.learn && saturated)
    }

    /// Keeps the encoder's own limit apart from the path's. A hardware encoder's time hardly
    /// changes with the bitrate, so that only says so; a processor's does, so the bitrate gives way.
    fn watch_encoder(&mut self, now: Instant, encoder: Option<EncoderLoad>, before: u32) {
        let Some(encoder) = encoder else { return };
        let saturated = encoder.busy > SATURATED;
        if !saturated {
            self.encoder_saturated_at = None;
        } else if self.encoder_saturated_at.is_none() {
            self.encoder_saturated_at = Some(now);
        }
        if encoder.busy >= RELAXED {
            self.encoder_calm_at = None;
        } else if self.encoder_calm_at.is_none() {
            self.encoder_calm_at = Some(now);
        }
        let lasting = self.encoder_saturated_at.is_some_and(|at| now.duration_since(at) >= ENCODER_SATURATED_FOR);
        if lasting {
            self.encoder_limited = true;
            if !encoder.hardware {
                self.encoder_cap = Some((f64::from(before) * 0.85) as u32);
                self.encoder_saturated_at = Some(now);
            }
        } else if self.encoder_calm_at.is_some_and(|at| now.duration_since(at) >= ENCODER_CALM_FOR) {
            self.encoder_limited = false;
            self.encoder_cap = None;
        }
    }

    /// The least the bitrate may be: the setting, until the path has been beyond it for a while.
    fn floor(&mut self, now: Instant, before: u32, overwhelmed: bool) -> u32 {
        if !overwhelmed || before > self.limits.min_bps {
            if !overwhelmed {
                self.unusable_since = None;
            }
            return self.limits.min_bps.min(before.max(MIN_BPS));
        }
        let since = *self.unusable_since.get_or_insert(now);
        if now.duration_since(since) >= UNUSABLE_AFTER { MIN_BPS } else { self.limits.min_bps }
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

    /// As at the end of a loading screen on Wi-Fi: 80 Mb/s allowed, a trickle of video, an estimate
    /// that has only seen the trickle, and a burst queued for three ticks with nothing lost.
    #[test]
    fn a_burst_after_a_quiet_stretch_costs_a_step_not_the_stream() {
        let mut rate = RateControl::new(80_000_000);
        let start = rate.started;
        let at = |tick: u64| start + TICK * tick as u32;
        let mut video = 0;
        let mut quiet = |rate: &mut RateControl, tick: u64, queued, bytes, estimate| {
            video += bytes;
            let path = PathSample {
                sent_packets: tick * 100,
                queued_bytes: queued,
                video_bytes: video,
                bandwidth_estimate: Some(estimate),
                ..Default::default()
            };
            rate.update(at(tick), path)
        };
        for tick in 1..=40 {
            quiet(&mut rate, tick, 0, 2_000, 400_000);
        }
        for tick in 41..=43 {
            quiet(&mut rate, tick, 310_000, 2_000, 400_000);
        }
        assert!(rate.bitrate() >= 39_000_000, "only ever a step: {}", rate.bitrate());
        assert_eq!(rate.ceiling(), None, "no limit learned from an estimate of nothing");
        // Back to the full 80 Mb/s soon after it clears.
        for tick in 44..=68 {
            quiet(&mut rate, tick, 0, 2_000, 36_000_000);
        }
        assert_eq!(rate.bitrate(), 80_000_000);
    }

    /// As from the Ethernet upload of a PC hosting: it carries 38 Mb/s and the game would send 60,
    /// so video is steady at what the upload takes, under half of the 80 Mb/s allowed.
    #[test]
    fn steady_video_finds_a_slow_upload() {
        let mut rate = RateControl::new(80_000_000);
        let start = rate.started;
        let (capacity, game, seconds) = (38e6, 60e6, TICK.as_secs_f64());
        let (mut queued_bits, mut video, mut sent) = (0.0f64, 0u64, 0u64);
        let (mut cuts, mut worst_queue, mut bitrate_sum) = (0, 0.0f64, 0.0);
        for tick in 1..=1200u32 {
            let made = f64::from(rate.bitrate()).min(game);
            let going = (queued_bits + made * seconds).min(capacity * seconds);
            queued_bits = (queued_bits + made * seconds - going).max(0.0);
            video += (going / 8.0) as u64;
            sent += (going / 8.0 / 1200.0) as u64;
            let path = PathSample {
                sent_packets: sent,
                video_bytes: video,
                queued_bytes: (queued_bits / 8.0) as usize,
                bandwidth_estimate: Some(40_000_000),
                ..Default::default()
            };
            rate.update(start + TICK * tick, path);
            if tick > 80 {
                cuts += usize::from(rate.reason() == Reason::Backlog);
                worst_queue = worst_queue.max(queued_bits / capacity * 1000.0);
                bitrate_sum += f64::from(rate.bitrate());
            }
        }
        let mbps = bitrate_sum / 1120.0 / 1e6;
        println!("{mbps:.0} Mb/s, {cuts} cuts after the first 20 s, queue up to {worst_queue:.0} ms");
        assert!((28.0..38.0).contains(&mbps), "{mbps:.0} Mb/s");
        assert!(worst_queue < 60.0 && cuts <= 8, "{cuts} cuts, {worst_queue:.0} ms");
    }

    fn limits(max: u32, min: u32) -> BitrateLimits {
        BitrateLimits { automatic: true, max_bps: max * 1_000_000, min_bps: min * 1_000_000 }
    }

    /// A healthy path, a tick's worth at a time, for `rate` to climb on.
    fn healthy(rate: &mut RateControl, ticks: u32) {
        let start = rate.started;
        for tick in 1..=ticks {
            let (sent, video) = (u64::from(tick) * 1000, u64::from(tick) * 1_000_000);
            let path = PathSample {
                sent_packets: sent,
                video_bytes: video,
                bandwidth_estimate: Some(500_000_000),
                rtt: Some(Duration::from_millis(20)),
                ..Default::default()
            };
            rate.update(start + TICK * tick, path);
        }
    }

    /// As from a PC whose upload holds 32 Mb/s behind a modem with a deep buffer: the host's own
    /// queue stays empty, the estimate reads 47, and only the round trip shows the queue.
    #[test]
    fn a_queue_out_of_sight_is_found_by_the_round_trip() {
        let mut rate = RateControl::with_limits(limits(100, 3));
        let start = rate.started;
        let (capacity, seconds) = (32e6, TICK.as_secs_f64());
        let (mut queued_bits, mut video, mut sent) = (0.0f64, 0u64, 0u64);
        let (mut late_ticks, mut bitrate_sum) = (0, 0.0);
        for tick in 1..=1200u32 {
            let made = f64::from(rate.bitrate()).min(60e6);
            queued_bits = (queued_bits + (made - capacity) * seconds).max(0.0);
            let rtt_ms = 44.0 + queued_bits / capacity * 1000.0;
            video += (made * seconds / 8.0) as u64;
            sent += (made * seconds / 8.0 / 1200.0) as u64;
            let path = PathSample {
                sent_packets: sent,
                video_bytes: video,
                bandwidth_estimate: Some(47_000_000),
                rtt: Some(Duration::from_secs_f64(rtt_ms / 1000.0)),
                ..Default::default()
            };
            rate.update(start + TICK * tick, path);
            if tick > 120 {
                late_ticks += u32::from(rtt_ms > 70.0);
                bitrate_sum += f64::from(rate.bitrate());
            }
        }
        let mbps = bitrate_sum / 1080.0 / 1e6;
        println!("{mbps:.0} Mb/s, {late_ticks} of 1080 ticks over 70 ms");
        assert!((24.0..33.0).contains(&mbps), "{mbps:.0} Mb/s");
        assert!(late_ticks < 54, "{late_ticks} ticks over 70 ms");
    }

    /// Wi-Fi scans: the round trip spikes while the video is far under what the path carries.
    #[test]
    fn a_spike_with_room_to_spare_costs_nothing() {
        let mut rate = RateControl::new(80_000_000);
        let start = rate.started;
        for tick in 1..40u32 {
            let path = PathSample {
                sent_packets: u64::from(tick) * 1000,
                video_bytes: u64::from(tick) * 1_062_500,
                bandwidth_estimate: Some(120_000_000),
                rtt: Some(Duration::from_millis(if tick == 1 { 3 } else { 100 })),
                ..Default::default()
            };
            assert_eq!(rate.update(start + TICK * tick, path), None);
        }
    }

    #[test]
    fn starts_low_and_climbs_to_the_maximum_and_no_further() {
        let mut rate = RateControl::with_limits(limits(50, 3));
        assert_eq!(rate.bitrate(), 30_000_000);
        healthy(&mut rate, 120);
        assert_eq!(rate.bitrate(), 50_000_000);
        let rate = RateControl::with_limits(limits(5, 3));
        assert_eq!(rate.bitrate(), 5_000_000);
    }

    #[test]
    fn the_maximum_applies_as_soon_as_its_changed() {
        let mut rate = RateControl::with_limits(limits(50, 3));
        healthy(&mut rate, 60);
        rate.set_limits(limits(10, 3));
        rate.update(rate.started + TICK * 61, PathSample::default());
        assert_eq!((rate.bitrate(), rate.reason()), (10_000_000, Reason::Limit));
    }

    #[test]
    fn the_minimum_holds_until_the_path_is_beyond_it() {
        let mut rate = RateControl::with_limits(limits(10, 8));
        let start = rate.started;
        let mut sent = 0;
        let mut update = |rate: &mut RateControl, tick: u32| {
            sent += 1000;
            rate.update(
                start + TICK * tick,
                PathSample { sent_packets: sent, queued_bytes: 900_000, ..Default::default() },
            );
        };
        for tick in 1..=7 {
            update(&mut rate, tick);
            assert!(rate.bitrate() >= 8_000_000, "tick {tick}: {}", rate.bitrate());
        }
        for tick in 8..=30 {
            update(&mut rate, tick);
        }
        assert!(rate.bitrate() < 8_000_000, "{}", rate.bitrate());
    }

    #[test]
    fn in_automatic_mode_the_picture_sets_what_is_worth_sending() {
        let mut rate = RateControl::with_limits(limits(300, 3));
        rate.set_format(1920, 1080, 60);
        assert_eq!(rate.cap() / 1_000_000, 31);
        rate.set_format(3840, 2160, 120);
        assert_eq!(rate.cap() / 1_000_000, 248);
        let mut manual = RateControl::with_limits(BitrateLimits { automatic: false, ..limits(300, 3) });
        manual.set_format(1920, 1080, 60);
        assert_eq!(manual.cap() / 1_000_000, 300);
    }

    #[test]
    fn a_viewer_getting_less_than_was_sent_lowers_the_bitrate() {
        let mut rate = RateControl::new(80_000_000);
        let start = rate.started;
        let report = ViewerFeedback { receive_kbps: 20_000, ..Default::default() };
        let mut cut = None;
        for tick in 1..=12u32 {
            let path = PathSample {
                sent_packets: u64::from(tick) * 1000,
                video_bytes: u64::from(tick) * 6_250_000,
                ..Default::default()
            };
            if tick.is_multiple_of(4) {
                rate.feedback(start + TICK * tick, report);
            }
            if let Some(bps) = rate.update(start + TICK * tick, path) {
                cut = cut.or(Some((bps, rate.reason())));
            }
        }
        assert_eq!(cut, Some((17_000_000, Reason::ViewerRate)));
    }

    #[test]
    fn a_report_counts_once() {
        let mut rate = RateControl::new(80_000_000);
        let start = rate.started;
        rate.feedback(start, ViewerFeedback { receive_kbps: 20_000, ..Default::default() });
        for tick in 1..=4u32 {
            let path = PathSample {
                sent_packets: u64::from(tick) * 1000,
                video_bytes: u64::from(tick) * 6_250_000,
                ..Default::default()
            };
            assert_eq!(rate.update(start + TICK * tick, path), None, "tick {tick}");
        }
    }

    #[test]
    fn a_report_cant_raise_the_bitrate() {
        let mut rate = RateControl::with_limits(limits(20, 3));
        let start = rate.started;
        let report = ViewerFeedback { receive_kbps: u32::MAX, ..Default::default() };
        for tick in 1..40u32 {
            rate.feedback(start + TICK * tick, report);
            rate.update(start + TICK * tick, PathSample { sent_packets: u64::from(tick) * 1000, ..Default::default() });
            assert!(rate.bitrate() <= 20_000_000);
        }
        assert_eq!(report.bounded().receive_kbps, 10_000_000);
    }

    #[test]
    fn a_saturated_decoder_lowers_the_bitrate() {
        let mut rate = RateControl::new(80_000_000);
        let start = rate.started;
        let report = ViewerFeedback { decode_us: 10_000, frames_decoded: 100, frames_dropped: 5, ..Default::default() };
        rate.feedback(start + TICK, report);
        let path = PathSample { sent_packets: 1000, video_bytes: 5_000_000, ..Default::default() };
        rate.update(start + TICK, path);
        assert_eq!(rate.reason(), Reason::ViewerDecode);
    }

    #[test]
    fn a_processor_encoder_gives_way_and_a_graphics_card_only_says_so() {
        let saturated = |hardware| PathSample {
            sent_packets: 0,
            encoder: Some(EncoderLoad { busy: 1.0, hardware }),
            ..Default::default()
        };
        for (hardware, expected_cap) in [(false, Some(68_000_000)), (true, None)] {
            let mut rate = RateControl::new(80_000_000);
            let start = rate.started;
            for tick in 1..=9u32 {
                rate.update(
                    start + TICK * tick,
                    PathSample { sent_packets: u64::from(tick) * 1000, ..saturated(hardware) },
                );
            }
            assert!(rate.encoder_limited());
            assert_eq!(rate.encoder_cap, expected_cap, "hardware: {hardware}");
        }
    }

    #[test]
    fn a_share_of_the_upload_is_kept_to_and_given_back() {
        let mut rate = RateControl::with_limits(limits(80, 3));
        healthy(&mut rate, 60);
        rate.set_share(Some(25_000_000));
        rate.update(rate.started + TICK * 61, PathSample::default());
        assert_eq!((rate.bitrate(), rate.reason()), (25_000_000, Reason::Limit));
        rate.set_share(None);
        assert_eq!(rate.cap(), 80_000_000);
    }

    /// As in a session on Wi-Fi: video queued, nothing going out, for three seconds.
    #[test]
    fn a_dropout_doesnt_cost_the_bitrate() {
        let mut rate = RateControl::new(80_000_000);
        let start = rate.started;
        let mut sent = 0;
        for tick in 1..=14u32 {
            let going = if tick <= 2 { 1000 } else { 1 };
            sent += going;
            let path = PathSample {
                sent_packets: sent,
                queued_bytes: if tick <= 2 { 0 } else { 300_000 },
                video_bytes: 1_000_000,
                bandwidth_estimate: Some(47_000_000),
                ..Default::default()
            };
            rate.update(start + TICK * tick, path);
        }
        assert_eq!(rate.bitrate(), 80_000_000);
    }

    #[test]
    fn backs_off_on_backlog_or_loss_and_recovers_slowly() {
        let mut rate = RateControl::new(20_000_000);
        let start = rate.started;
        let at = |ms: u64| start + Duration::from_millis(ms);
        // A clean path leaves the cap alone.
        assert_eq!(rate.update(at(250), sample(1000, 0, 0)), None);
        // 200 ms of video queued, still there a tick later: cut hard, and remember a ceiling under
        // where it broke.
        assert_eq!(rate.update(at(375), sample(1500, 0, 500_000)), None);
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
        let path = |sent, queued, estimate| PathSample {
            queued_bytes: queued,
            bandwidth_estimate: Some(estimate),
            sent_packets: sent,
            ..Default::default()
        };
        // At 80 Mb/s on a 40 Mb/s path: straight down to the video's share of it.
        assert_eq!(rate.update(at(2750), path(1000, 400_000, 40_000_000)), None);
        assert_eq!(rate.update(at(3000), path(2000, 400_000, 40_000_000)), Some(32_000_000));
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

    /// Cuts that Kellu's first long session took for congestion, on a path that had room.
    #[test]
    fn a_burst_isnt_congestion() {
        let mut rate = RateControl::new(80_000_000);
        let start = rate.started;
        let at = |ms: u64| start + Duration::from_millis(ms);
        let path = |sent, queued| PathSample {
            sent_packets: sent,
            queued_bytes: queued,
            bandwidth_estimate: Some(76_000_000),
            ..Default::default()
        };
        // 330 KB for one tick, a scene change, then gone.
        assert_eq!(rate.update(at(250), path(1000, 330_000)), None);
        assert_eq!(rate.update(at(500), path(2000, 32)), None);
        assert_eq!(rate.ceiling(), None);

        // At a low bitrate, a single large frame is a lot of "time" but no backlog.
        let mut rate = RateControl::new(3_000_000);
        let start = rate.started;
        for tick in 1..8u32 {
            rate.update(start + TICK * tick, sample(u64::from(tick) * 1000, 0, 20_000));
        }
        assert_eq!(rate.bitrate(), 3_000_000);
    }

    #[test]
    fn loss_on_a_path_with_room_keeps_the_bitrate() {
        let mut rate = RateControl::new(40_000_000);
        let start = rate.started;
        let at = |ms: u64| start + Duration::from_millis(ms);
        // 20 Mb/s on a path good for 73: ten packets gone at once, and the viewer lost a frame.
        let tick_of = |seconds: f64| (20e6 / 8.0 * seconds) as u64;
        let sent = |n: u64, lost, requests, video_bytes| PathSample {
            sent_packets: n,
            lost_packets: lost,
            recovery_requests: requests,
            video_bytes,
            bandwidth_estimate: Some(73_000_000),
            ..Default::default()
        };
        assert_eq!(rate.update(at(4750), sent(500, 0, 0, tick_of(4.75))), None);
        assert_eq!(rate.update(at(5000), sent(1050, 60, 1, tick_of(5.0))), None);
        assert_eq!(rate.ceiling(), None);
        // The same loss with the video filling the path is what a ceiling is for.
        let filled = |n: u64, lost, requests, video_bytes| PathSample {
            bandwidth_estimate: Some(21_000_000),
            ..sent(n, lost, requests, video_bytes)
        };
        assert_eq!(rate.update(at(5250), filled(1600, 60, 2, tick_of(5.25))), Some(16_800_000));
        assert_eq!(rate.reason(), Reason::ViewerLoss);
    }

    #[test]
    fn a_ceiling_rises_with_the_estimate() {
        let mut rate = RateControl::new(80_000_000);
        let start = rate.started;
        let at = |ms: u64| start + Duration::from_millis(ms);
        let path = |sent: u64, queued, estimate| PathSample {
            sent_packets: sent,
            queued_bytes: queued,
            bandwidth_estimate: Some(estimate),
            ..Default::default()
        };
        rate.update(at(2750), path(1000, 400_000, 10_000_000));
        rate.update(at(3000), path(2000, 400_000, 10_000_000));
        assert_eq!(rate.ceiling(), Some(8_000_000));
        // The path is back: no need to wait for the ceiling to be forgotten.
        rate.update(at(5250), path(3000, 0, 40_000_000));
        assert_eq!(rate.ceiling(), Some(32_000_000));
        for tick in 0..40 {
            rate.update(at(5500 + tick * 250), path(4000 + tick * 1000, 0, 40_000_000));
        }
        assert!(rate.bitrate() >= 28_000_000, "back near the path in ten seconds: {}", rate.bitrate());
    }

    /// A path that falls to 10 Mb/s and comes back, with an encoder that follows its target (as
    /// the hardware ones do) and an estimate that can only see a little past what's being sent.
    #[test]
    fn comes_back_when_the_path_does() {
        let mut rate = RateControl::new(40_000_000);
        let start = rate.started;
        let (mut queued, mut estimate, mut sent) = (0f64, 40e6f64, 0u64);
        let (mut lowest, mut recovered_after) = (u32::MAX, None);
        for tick in 1..=320u32 {
            let seconds = f64::from(tick) / 4.0;
            let capacity = if (20.0..35.0).contains(&seconds) { 10e6 } else { 40e6 };
            let offered = f64::from(rate.bitrate()) * 1.12;
            queued = (queued + (offered - capacity) * 0.25 / 8.0).max(0.0);
            estimate = (offered.min(capacity) * 1.25).min(capacity).max(estimate.min(capacity) * 0.98);
            sent += 1000;
            rate.update(
                start + TICK * tick,
                PathSample {
                    sent_packets: sent,
                    queued_bytes: queued as usize,
                    bandwidth_estimate: Some(estimate as u64),
                    ..Default::default()
                },
            );
            lowest = lowest.min(rate.bitrate());
            if seconds >= 35.0 && recovered_after.is_none() && rate.bitrate() >= 32_000_000 {
                recovered_after = Some(seconds - 35.0);
            }
        }
        assert!(lowest < 10_000_000, "it did go under the slow path: {lowest}");
        let recovered_after = recovered_after.expect("back to 80% of the cap");
        println!("back to 80% of the cap {recovered_after} s after the path");
        assert!(recovered_after <= 30.0, "{recovered_after} s");
    }

    /// The path really slowed down, and the estimate hasn't caught up.
    #[test]
    fn lasting_loss_counts_however_much_room_the_estimate_shows() {
        let mut rate = RateControl::new(40_000_000);
        let start = rate.started;
        let at = |ms: u64| start + Duration::from_millis(ms);
        let lossy = |n: u64| PathSample {
            sent_packets: n * 100,
            lost_packets: n * 15,
            bandwidth_estimate: Some(400_000_000),
            ..Default::default()
        };
        assert_eq!(rate.update(at(4750), lossy(1)), None);
        assert_eq!(rate.update(at(5000), lossy(2)), Some(34_000_000));
        assert_eq!(rate.reason(), Reason::Loss);
        // Heavy loss doesn't wait for the next tick.
        let mut rate = RateControl::new(40_000_000);
        let heavy = PathSample { lost_packets: 30, ..lossy(1) };
        assert_eq!(rate.update(rate.started + Duration::from_secs(5), heavy), Some(28_000_000));
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
        // Creeping does go over in the end: that's how it finds out the path has more room.
        assert!(new.excess_mb * 2.5 <= old.excess_mb, "{:.1} vs {:.1} MB over", new.excess_mb, old.excess_mb);
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

    /// What the 0.9.3 session got stuck in after a loss: the bitrate at 90% of the ceiling, BBR's
    /// estimate about 1.6 times the bitrate as it can't see further than what's sent, and the
    /// path back at 100 Mb/s all along.
    #[test]
    fn the_estimate_lifts_the_ceiling_once_the_path_is_calm() {
        let mut rate = RateControl::new(80_000_000);
        let start = rate.started;
        let at = |ms: u64| start + Duration::from_millis(ms);
        let path = |sent: u64, queued, estimate: f64| PathSample {
            sent_packets: sent,
            queued_bytes: queued,
            bandwidth_estimate: Some(estimate as u64),
            ..Default::default()
        };
        rate.bps.store(15_000_000, Ordering::Relaxed);
        rate.update(at(2750), path(1000, 80_000, 21.4e6));
        rate.update(at(3000), path(2000, 80_000, 21.4e6));
        assert!(rate.ceiling().is_some());
        let mut seconds_back = None;
        for tick in 0..120u64 {
            let estimate = (f64::from(rate.bitrate()) * 1.6).min(100e6);
            rate.update(at(3250 + tick * 250), path(3000 + tick * 1000, 0, estimate));
            if rate.bitrate() >= 72_000_000 {
                seconds_back = Some(tick as f64 / 4.0);
                break;
            }
        }
        let seconds_back = seconds_back.expect("back near the cap");
        assert!(seconds_back <= 15.0, "{seconds_back} s");
    }

    /// `ceiling * FAST_UNTIL` is rarely a whole number, and a bitrate is.
    #[test]
    fn climbing_reaches_the_ceiling_and_creeps() {
        let mut rate = RateControl::new(80_000_000);
        let start = rate.started;
        rate.bps.store(5_000_000, Ordering::Relaxed);
        rate.ceiling = Some((13_782_123, start + Duration::from_secs(10)));
        rate.estimate_at_ceiling = u64::MAX;
        for tick in 0..60u64 {
            rate.update(start + Duration::from_millis(10_000 + tick * 250), sample(1000 * (tick + 1), 0, 0));
        }
        assert!(rate.bitrate() > 12_403_911, "stopped at 90% of the ceiling: {}", rate.bitrate());
        assert_eq!(rate.reason(), Reason::Creep);
    }

    /// The 0.9.3 session: 2-3% lost whatever was sent. One step down tries whether sending less
    /// helps; when it doesn't, more of them only cost picture.
    #[test]
    fn stops_cutting_when_cutting_doesnt_help_the_loss() {
        let mut rate = RateControl::new(80_000_000);
        let start = rate.started;
        let at = |ms: u64| start + Duration::from_millis(ms);
        let path = |tick: u64, requests: u64, estimate: u64| PathSample {
            sent_packets: tick * 2000,
            lost_packets: tick * 50,
            recovery_requests: requests,
            video_bytes: tick * 2_500_000,
            bandwidth_estimate: Some(estimate),
            ..Default::default()
        };
        for tick in 1..=8 {
            rate.update(at(4000 + tick * 250), path(tick, 0, 100_000_000));
        }
        assert_eq!(rate.update(at(6250), path(9, 1, 100_000_000)), Some(68_000_000));
        assert_eq!(rate.reason(), Reason::ViewerLoss);
        for tick in 10..=20 {
            rate.update(at(4000 + tick * 250), path(tick, tick, 100_000_000));
            assert_eq!(rate.bitrate(), 68_000_000, "the loss is the same as before the cut");
            assert_eq!(rate.reason(), Reason::LossHeld);
        }
        // The loss ends and the bitrate climbs back.
        for tick in 21..=60 {
            let clean = PathSample { lost_packets: 1000, ..path(tick, 20, 100_000_000) };
            rate.update(at(4000 + tick * 250), clean);
        }
        assert!(rate.bitrate() > 72_000_000, "{}", rate.bitrate());
    }

    /// Loss that falls when the bitrate does is the path being full: keep going down.
    #[test]
    fn keeps_cutting_while_cutting_helps_the_loss() {
        let mut rate = RateControl::new(80_000_000);
        let start = rate.started;
        let at = |ms: u64| start + Duration::from_millis(ms);
        let path = |sent: u64, lost: u64, requests: u64| PathSample {
            sent_packets: sent,
            lost_packets: lost,
            recovery_requests: requests,
            video_bytes: sent * 1250,
            bandwidth_estimate: Some(80_000_000),
            ..Default::default()
        };
        let (mut sent, mut lost) = (0, 0);
        for tick in 1..=16u64 {
            (sent, lost) = (sent + 2000, lost + 120);
            rate.update(at(4000 + tick * 250), path(sent, lost, 0));
        }
        (sent, lost) = (sent + 2000, lost + 120);
        assert_eq!(rate.update(at(8250), path(sent, lost, 1)), Some(64_000_000));
        for tick in 0..16u64 {
            (sent, lost) = (sent + 2000, lost + 10);
            rate.update(at(8500 + tick * 250), path(sent, lost, 1));
        }
        (sent, lost) = (sent + 2000, lost + 10);
        assert!(rate.update(at(12_500), path(sent, lost, 2)).is_some(), "a new loss after the last one faded");
        assert_eq!(rate.reason(), Reason::ViewerLoss);
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
