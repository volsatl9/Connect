//! Fits the video to the connection. The bitrate drops as soon as datagrams back up, and climbs
//! back towards the host's cap once the path keeps up. Lost packets are what the recovery data
//! sent along is for, and its share follows the measured loss; only heavy loss also cuts the
//! bitrate, since stray losses (common on Wi-Fi and the internet) don't mean the path is full.

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
/// While more video than this waits to be sent, capturing more only adds delay.
const HOLD_BACKLOG: f64 = 0.050;
const HOLD_MIN_BYTES: usize = 128 * 1024;

pub(crate) struct RateControl {
    max_bps: u32,
    bps: Arc<AtomicU32>,
    /// Smoothed share of packets lost on the path.
    loss: f32,
    sent: u64,
    lost: u64,
    calm_since: Instant,
}

impl RateControl {
    pub fn new(max_bps: u32) -> Self {
        let max_bps = max_bps.max(MIN_BPS);
        Self {
            max_bps,
            bps: Arc::new(AtomicU32::new(max_bps)),
            loss: 0.0,
            sent: 0,
            lost: 0,
            calm_since: Instant::now(),
        }
    }

    pub fn bitrate(&self) -> u32 {
        self.bps.load(Ordering::Relaxed)
    }

    /// Smoothed share of packets lost, 0..1.
    pub fn loss(&self) -> f32 {
        self.loss
    }

    /// A check for the encoder thread: true while so much video waits to be sent that encoding
    /// another frame would only queue it behind the rest.
    pub fn backlog_check(&self, conn: Connection) -> impl Fn() -> bool + Send + 'static {
        let bps = self.bps.clone();
        move || backlogged(net::queued_datagram_bytes(&conn), bps.load(Ordering::Relaxed))
    }

    /// Runs every [`TICK`] with the connection's running packet counts and the bytes waiting
    /// to be sent. The new bitrate, if it changed.
    pub fn update(&mut self, now: Instant, sent_packets: u64, lost_packets: u64, queued_bytes: usize) -> Option<u32> {
        let sent = sent_packets.saturating_sub(self.sent);
        let lost = lost_packets.saturating_sub(self.lost);
        (self.sent, self.lost) = (sent_packets, lost_packets);
        let sample = if sent >= MIN_SAMPLE_PACKETS { (lost as f32 / sent as f32).min(1.0) } else { self.loss };
        self.loss = self.loss * 0.7 + sample * 0.3;

        let before = self.bitrate();
        let backlog = queued_bytes as f64 * 8.0 / before as f64;
        let scaled = |factor: f64| ((before as f64 * factor) as u32).clamp(MIN_BPS, self.max_bps);
        let after = if backlog > BACKLOG_HIGH || sample > LOSS_HEAVY {
            self.calm_since = now;
            scaled(0.7)
        } else if backlog > BACKLOG_SOME || sample > LOSS_SOME {
            self.calm_since = now;
            scaled(0.85)
        } else if now.duration_since(self.calm_since) >= CALM_BEFORE_RAISE {
            // Up gently: about doubling in four seconds.
            scaled(1.05).max(before.saturating_add(250_000).min(self.max_bps))
        } else {
            before
        };
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

    #[test]
    fn backs_off_on_backlog_or_loss_and_recovers_slowly() {
        let mut rate = RateControl::new(20_000_000);
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        // A clean path leaves the cap alone.
        assert_eq!(rate.update(at(250), 1000, 0, 0), None);
        // 200 ms of video queued: cut hard.
        assert_eq!(rate.update(at(500), 2000, 0, 500_000), Some(14_000_000));
        // Stray loss is for the recovery data to fix, not a reason to send less.
        assert_eq!(rate.update(at(750), 3000, 50, 0), None);
        assert!(rate.loss() > 0.01);
        // Heavy loss is: the path is likely overloaded.
        assert_eq!(rate.update(at(875), 4000, 200, 0), Some(11_900_000));
        // Calm, but not for long enough yet.
        assert_eq!(rate.update(at(1000), 5000, 200, 0), None);
        // After two calm seconds it climbs, by at least a quarter megabit a step, up to the cap.
        assert_eq!(rate.update(at(2875), 6000, 200, 0), Some(12_495_000));
        let mut last = 0;
        for tick in 0..100 {
            last = rate.update(at(3000 + tick * 250), 7000 + tick * 1000, 200, 0).unwrap_or(last);
        }
        assert_eq!(rate.bitrate(), 20_000_000);
        assert!(rate.loss() < 0.001);
    }

    #[test]
    fn never_below_the_floor() {
        let mut rate = RateControl::new(3_000_000);
        for tick in 1..20 {
            rate.update(Instant::now(), tick * 100, tick * 50, 10_000_000);
        }
        assert_eq!(rate.bitrate(), MIN_BPS);
    }

    #[test]
    fn holds_capture_only_for_a_real_backlog() {
        assert!(!backlogged(100_000, 2_000_000)); // small, however slow the stream
        assert!(!backlogged(200_000, 80_000_000)); // 20 ms at 80 Mb/s
        assert!(backlogged(400_000, 20_000_000)); // 160 ms at 20 Mb/s
    }
}
