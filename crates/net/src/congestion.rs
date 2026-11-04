//! BBR, remembering what the path has carried. A game's video isn't steady: a menu or a loading
//! screen sends almost nothing for a minute. BBR only keeps its bandwidth samples for a few
//! seconds, so a quiet stretch reads as a slow path (a 76 Mb/s path was estimated at 1.6 Mb/s,
//! with no loss at all, and a 30 KB window), and the next large frame then trickles out over a
//! quarter of a second. A remembered rate keeps the pacing and the window where the path has
//! shown it can be, until the path says otherwise.
//!
//! It only steps in once BBR's estimate has collapsed, at under half of what's remembered. BBR
//! also shrinks its window on purpose, to measure the path's round trip: it can't finish that,
//! or ever probe for more bandwidth, until what's in flight falls to the shrunken window. A
//! window held up all the time would freeze it, and the path along with it.
//!
//! It also keeps stray loss from BBR, which takes nearly any loss as the path being full and
//! backs off a step each round trip. A session lost 2-3% for 16 seconds whatever it sent, from
//! 90 Mb/s down to 7, with the round trip barely moved: sending less did nothing but take BBR
//! from 101 Mb/s to 8. Loss counts as stray while its share is low and no queue is building,
//! and BBR is told it was spurious. The recovery data carries the video through. The cost is
//! that BBR won't give way to other traffic at the first lost packet, only once a queue builds
//! or the loss climbs.

use std::{
    any::Any,
    sync::Arc,
    time::{Duration, Instant},
};

use noq_proto::{
    RttEstimator,
    congestion::{Bbr3Config, Controller, ControllerFactory, ControllerMetrics},
};

/// A rate not seen again for this long is forgotten: longer than a loading screen.
const REMEMBER: Duration = Duration::from_secs(120);
/// A round trip this far over the path's best means a queue is building somewhere.
const RTT_INFLATED: f64 = 1.5;
/// Over this share of what was sent, loss is more than stray.
const STRAY_LOSS: f64 = 0.05;
/// Loss is counted over two of these.
const LOSS_WINDOW: Duration = Duration::from_millis(250);
/// With less than this counted, there's no telling what share of it was lost.
const MIN_COUNTED_BYTES: u64 = 32 * 1024;

/// Builds [`Remembering`] controllers, one per path.
#[derive(Debug, Default)]
pub(crate) struct PathMemory(pub Arc<Bbr3Config>);

impl ControllerFactory for PathMemory {
    fn build(self: Arc<Self>, now: Instant, current_mtu: u16) -> Box<dyn Controller> {
        Box::new(Remembering::new(self.0.clone().build(now, current_mtu)))
    }
}

/// The best rate seen, in bytes per second, and when.
#[derive(Debug, Clone, Copy)]
struct Best {
    rate: u64,
    at: Instant,
}

/// Bytes acknowledged and lost in a [`LOSS_WINDOW`].
#[derive(Debug, Clone, Copy, Default)]
struct Delivery {
    acked: u64,
    lost: u64,
}

#[derive(Debug)]
struct Remembering {
    inner: Box<dyn Controller>,
    best: Option<Best>,
    rtt: Duration,
    min_rtt: Duration,
    /// When the remembered rate was last halved: at most once a round trip.
    backed_off: Option<Instant>,
    /// The current loss window and when it began, and the one before.
    delivery: Delivery,
    delivery_since: Option<Instant>,
    last_delivery: Delivery,
}

impl Remembering {
    fn new(inner: Box<dyn Controller>) -> Self {
        Self {
            inner,
            best: None,
            rtt: Duration::ZERO,
            min_rtt: Duration::ZERO,
            backed_off: None,
            delivery: Delivery::default(),
            delivery_since: None,
            last_delivery: Delivery::default(),
        }
    }

    /// Starts a new loss window if the current one is over.
    fn roll_delivery(&mut self, now: Instant) {
        let since = *self.delivery_since.get_or_insert(now);
        if now.duration_since(since) >= LOSS_WINDOW {
            self.last_delivery = std::mem::take(&mut self.delivery);
            self.delivery_since = Some(now);
        }
    }

    fn acked(&mut self, now: Instant, bytes: u64) {
        self.roll_delivery(now);
        self.delivery.acked += bytes;
    }

    fn rtt_inflated(&self) -> bool {
        self.rtt.as_secs_f64() > self.min_rtt.as_secs_f64() * RTT_INFLATED
    }

    /// Whether the loss of late is a share too low to mean the path is full.
    fn loss_is_stray(&self) -> bool {
        let lost = self.delivery.lost + self.last_delivery.lost;
        let counted = self.delivery.acked + self.last_delivery.acked + lost;
        counted >= MIN_COUNTED_BYTES && lost as f64 <= counted as f64 * STRAY_LOSS && !self.rtt_inflated()
    }

    /// Takes in the estimate after a batch of acknowledgements.
    fn observe(&mut self, now: Instant) {
        if self.best.is_some_and(|best| now.duration_since(best.at) >= REMEMBER) {
            self.best = None;
        }
        if let Some(rate) = self.inner.metrics().bandwidth_estimate.map(|bits| bits / 8)
            && self.best.is_none_or(|best| rate >= best.rate)
        {
            self.best = Some(Best { rate, at: now });
        }
        if self.rtt_inflated() {
            self.back_off(now);
        }
    }

    /// The path pushed back: it may no longer carry what it did. Halved, not dropped, as one
    /// burst of stray loss doesn't mean it got slower, and a real slowdown soon halves it again.
    fn back_off(&mut self, now: Instant) {
        if self.backed_off.is_some_and(|at| now.duration_since(at) < self.rtt) {
            return;
        }
        self.backed_off = Some(now);
        if let Some(best) = &mut self.best {
            best.rate /= 2;
        }
    }

    /// The rate to hold to, if BBR has lost sight of the path; 0 otherwise.
    fn remembered_rate(&self) -> u64 {
        let estimate = self.inner.metrics().bandwidth_estimate.map_or(0, |bits| bits / 8);
        self.best.map_or(0, |best| if best.rate > estimate * 2 { best.rate } else { 0 })
    }
}

impl Controller for Remembering {
    fn on_sent(&mut self, now: Instant, bytes: u64, largest_pn: u64) {
        self.inner.on_sent(now, bytes, largest_pn);
    }

    fn on_packet_sent(&mut self, now: Instant, bytes: u16, pn: u64) {
        self.inner.on_packet_sent(now, bytes, pn);
    }

    fn on_cwnd_limited(&mut self) {
        self.inner.on_cwnd_limited();
    }

    fn on_ack(&mut self, now: Instant, sent: Instant, bytes: u64, pn: u64, app_limited: bool, rtt: &RttEstimator) {
        (self.rtt, self.min_rtt) = (rtt.get(), rtt.min());
        self.acked(now, bytes);
        self.inner.on_ack(now, sent, bytes, pn, app_limited, rtt);
    }

    fn on_end_acks(&mut self, now: Instant, in_flight: u64, app_limited: bool, largest_packet_num_acked: Option<u64>) {
        self.inner.on_end_acks(now, in_flight, app_limited, largest_packet_num_acked);
        self.observe(now);
    }

    fn on_congestion_event(
        &mut self,
        now: Instant,
        sent: Instant,
        is_persistent_congestion: bool,
        is_ecn: bool,
        lost_bytes: u64,
        largest_lost_pn: u64,
    ) {
        self.inner.on_congestion_event(now, sent, is_persistent_congestion, is_ecn, lost_bytes, largest_lost_pn);
        if is_persistent_congestion || is_ecn || !self.loss_is_stray() {
            self.back_off(now);
        }
    }

    fn on_packet_lost(&mut self, lost_bytes: u16, pn: u64, now: Instant) {
        self.inner.on_packet_lost(lost_bytes, pn, now);
        self.roll_delivery(now);
        self.delivery.lost += u64::from(lost_bytes);
        if self.loss_is_stray() {
            self.inner.on_spurious_congestion_event();
        } else {
            self.back_off(now);
        }
    }

    fn on_spurious_congestion_event(&mut self) {
        self.inner.on_spurious_congestion_event();
    }

    fn on_mtu_update(&mut self, new_mtu: u16) {
        self.inner.on_mtu_update(new_mtu);
    }

    fn on_ack_frequency_update(&mut self, ack_eliciting_threshold: u64, requested_max_ack_delay: Duration) {
        self.inner.on_ack_frequency_update(ack_eliciting_threshold, requested_max_ack_delay);
    }

    /// At least one round trip at the remembered rate.
    fn window(&self) -> u64 {
        let remembered = self.remembered_rate() as f64 * self.min_rtt.as_secs_f64();
        self.inner.window().max(remembered as u64)
    }

    /// BBR's own, with the window and pacing it would have now the path is remembered. The
    /// bandwidth estimate stays BBR's: it's what was measured.
    fn metrics(&self) -> ControllerMetrics {
        let mut metrics = self.inner.metrics();
        metrics.congestion_window = self.window();
        metrics.pacing_rate = metrics.pacing_rate.map(|rate| rate.max(self.remembered_rate()));
        metrics
    }

    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(Self { inner: self.inner.clone_box(), ..*self })
    }

    fn initial_window(&self) -> u64 {
        self.inner.initial_window()
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Reports what it's told, as BBR would.
    #[derive(Debug, Clone)]
    struct Scripted {
        estimate_bits: u64,
        window: u64,
        pacing: u64,
        /// Losses taken back with [`Controller::on_spurious_congestion_event`].
        undone: Arc<AtomicU32>,
    }

    impl Scripted {
        fn new(estimate_bits: u64, window: u64, pacing: u64) -> Self {
            Self { estimate_bits, window, pacing, undone: Arc::default() }
        }
    }

    impl Controller for Scripted {
        fn on_congestion_event(&mut self, _: Instant, _: Instant, _: bool, _: bool, _: u64, _: u64) {}
        fn on_mtu_update(&mut self, _: u16) {}

        fn on_spurious_congestion_event(&mut self) {
            self.undone.fetch_add(1, Ordering::Relaxed);
        }

        fn window(&self) -> u64 {
            self.window
        }

        fn metrics(&self) -> ControllerMetrics {
            let mut metrics = ControllerMetrics::default();
            metrics.congestion_window = self.window;
            metrics.pacing_rate = Some(self.pacing);
            metrics.bandwidth_estimate = Some(self.estimate_bits);
            metrics
        }

        fn clone_box(&self) -> Box<dyn Controller> {
            Box::new(self.clone())
        }

        fn initial_window(&self) -> u64 {
            self.window
        }

        fn into_any(self: Box<Self>) -> Box<dyn Any> {
            self
        }
    }

    /// 76 Mb/s measured over a 42 ms path, then BBR's estimate collapses as it did at Kellu.
    fn busy_then_quiet(start: Instant) -> Remembering {
        let inner = Scripted::new(76_000_000, 880_000, 9_500_000);
        let mut controller = Remembering::new(Box::new(inner));
        (controller.rtt, controller.min_rtt) = (Duration::from_millis(43), Duration::from_millis(42));
        controller.observe(start);
        controller.inner = Box::new(Scripted::new(1_600_000, 30_000, 200_000));
        controller.observe(start + Duration::from_secs(20));
        controller
    }

    #[test]
    fn a_quiet_stretch_doesnt_shrink_the_path() {
        let controller = busy_then_quiet(Instant::now());
        assert_eq!(controller.metrics().pacing_rate, Some(9_500_000));
        assert_eq!(controller.window(), 399_000, "one round trip at 76 Mb/s");
        assert_eq!(controller.metrics().bandwidth_estimate, Some(1_600_000), "what was measured");
    }

    #[test]
    fn never_slows_bbr_down() {
        let start = Instant::now();
        let mut controller = busy_then_quiet(start);
        controller.inner = Box::new(Scripted::new(80_000_000, 900_000, 12_000_000));
        controller.observe(start + Duration::from_secs(21));
        assert_eq!(controller.metrics().pacing_rate, Some(12_000_000));
        assert_eq!(controller.window(), 900_000);
    }

    /// BBR shrinking its window to measure the round trip, while its estimate is fine.
    #[test]
    fn leaves_bbr_alone_while_it_knows_the_path() {
        let start = Instant::now();
        let inner = Scripted::new(76_000_000, 880_000, 9_500_000);
        let mut controller = Remembering::new(Box::new(inner));
        (controller.rtt, controller.min_rtt) = (Duration::from_millis(43), Duration::from_millis(42));
        controller.observe(start);
        controller.inner = Box::new(Scripted::new(76_000_000, 50_000, 9_500_000));
        controller.observe(start + Duration::from_secs(1));
        assert_eq!(controller.window(), 50_000);
    }

    /// A healthy controller carrying 2 000 packets a window, with `lost` of them gone.
    fn losing(start: Instant, lost: u32) -> (Remembering, Arc<AtomicU32>) {
        let inner = Scripted::new(100_000_000, 1_100_000, 12_500_000);
        let undone = inner.undone.clone();
        let mut controller = Remembering::new(Box::new(inner));
        (controller.rtt, controller.min_rtt) = (Duration::from_millis(44), Duration::from_millis(40));
        controller.observe(start);
        controller.acked(start, 2000 * 1200);
        for pn in 0..u64::from(lost) {
            controller.on_packet_lost(1200, pn, start);
        }
        (controller, undone)
    }

    /// 2-3% gone at any rate, the round trip about where it was: the 0.9.3 session.
    #[test]
    fn stray_loss_doesnt_reach_bbr() {
        let start = Instant::now();
        let (mut controller, undone) = losing(start, 50);
        assert_eq!(undone.load(Ordering::Relaxed), 50);
        controller.on_congestion_event(start, start, false, false, 1200, 1);
        assert_eq!(controller.best.unwrap().rate, 100_000_000 / 8, "the memory stays");
    }

    #[test]
    fn heavy_loss_reaches_bbr() {
        let start = Instant::now();
        let (controller, undone) = losing(start, 400);
        assert!(undone.load(Ordering::Relaxed) < 110, "from 5% up: {}", undone.load(Ordering::Relaxed));
        assert!(controller.best.unwrap().rate < 100_000_000 / 8);
    }

    /// Any share of loss counts with a queue building: it's the queue overflowing.
    #[test]
    fn loss_with_an_inflated_round_trip_reaches_bbr() {
        let start = Instant::now();
        let inner = Scripted::new(100_000_000, 1_100_000, 12_500_000);
        let undone = inner.undone.clone();
        let mut controller = Remembering::new(Box::new(inner));
        (controller.rtt, controller.min_rtt) = (Duration::from_millis(70), Duration::from_millis(40));
        controller.acked(start, 2000 * 1200);
        controller.on_packet_lost(1200, 1, start);
        assert_eq!(undone.load(Ordering::Relaxed), 0);
    }

    /// Too little sent yet to tell, as at the start of a connection.
    #[test]
    fn loss_isnt_stray_on_too_few_packets() {
        let start = Instant::now();
        let (controller, undone) = {
            let inner = Scripted::new(100_000_000, 1_100_000, 12_500_000);
            let undone = inner.undone.clone();
            let mut controller = Remembering::new(Box::new(inner));
            controller.on_packet_lost(1200, 1, start);
            (controller, undone)
        };
        assert_eq!(undone.load(Ordering::Relaxed), 0);
        drop(controller);
    }

    #[test]
    fn the_share_of_loss_follows_the_last_half_second() {
        let start = Instant::now();
        let (mut controller, _) = losing(start, 400);
        for step in 1..=3u64 {
            let at = start + LOSS_WINDOW * step as u32;
            controller.acked(at, 2000 * 1200);
        }
        assert!(controller.loss_is_stray(), "the burst has passed");
    }

    #[test]
    fn the_path_pushing_back_shrinks_the_memory() {
        let start = Instant::now();
        let mut controller = busy_then_quiet(start);
        let at = start + Duration::from_secs(21);
        controller.on_congestion_event(at, at, false, false, 1200, 1);
        assert_eq!(controller.metrics().pacing_rate, Some(4_750_000));
        // Once a round trip, however many events the same burst raises.
        controller.on_congestion_event(at + Duration::from_millis(1), at, false, false, 1200, 2);
        assert_eq!(controller.metrics().pacing_rate, Some(4_750_000));
        // A queue building somewhere, even with nothing lost, counts too.
        controller.rtt = Duration::from_millis(70);
        controller.observe(at + Duration::from_secs(1));
        assert_eq!(controller.metrics().pacing_rate, Some(2_375_000));
        // Down to what BBR reads anyway, there's nothing left to remember.
        for _ in 0..3 {
            controller.back_off(at + Duration::from_secs(5));
            controller.backed_off = None;
        }
        assert_eq!(controller.metrics().pacing_rate, Some(200_000));
    }

    #[test]
    fn forgets_a_rate_not_seen_for_minutes() {
        let start = Instant::now();
        let mut controller = busy_then_quiet(start);
        controller.observe(start + REMEMBER);
        assert_eq!(controller.metrics().pacing_rate, Some(200_000));
        assert_eq!(controller.window(), 30_000);
    }
}
