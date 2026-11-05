//! This PC's upload, shared by everyone connected. Each session has its own congestion control and
//! keeps to its own viewer's connection, so a slow viewer only slows itself. Sessions cutting back
//! together mean the upload itself is full: then it's split evenly, and what someone can't use
//! goes to the others.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

/// Cuts this close together, in more than one session, are the upload's doing.
const TOGETHER: Duration = Duration::from_secs(2);
/// Under what was going when it filled up: that was already too much.
const HEADROOM: f64 = 0.95;
/// Without trouble the upload is taken to carry this much more, each second after the wait: it may
/// have been something else's traffic.
const GROWTH: f64 = 1.01;
const GROW_AFTER: Duration = Duration::from_secs(5);
/// A viewer using less than this of its target doesn't need all of a share.
const USING: f64 = 0.7;
/// What such a viewer is left, over what it uses, to grow into.
const ROOM: f64 = 1.3;

/// One session's tick, as it matters to the others.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Report {
    /// Video going now, b/s.
    pub sending: f64,
    /// What it's aiming for.
    pub target: f64,
    /// The most its own connection and the settings allow.
    pub limit: f64,
    /// It cut the bitrate this tick because of the path.
    pub congested: bool,
}

#[derive(Debug, Clone, Copy)]
struct Viewer {
    report: Report,
    congested_at: Option<Instant>,
}

#[derive(Debug, Default)]
pub(crate) struct Uplink {
    viewers: HashMap<u64, Viewer>,
    /// What the upload carries, once it's been seen to fill up.
    capacity: Option<f64>,
    changed_at: Option<Instant>,
}

impl Uplink {
    /// Takes a session's tick. Its share of the upload, or `None` for all it can use.
    pub fn report(&mut self, id: u64, now: Instant, report: Report) -> Option<u32> {
        let viewer = self.viewers.entry(id).or_insert(Viewer { report, congested_at: None });
        viewer.report = report;
        if report.congested {
            viewer.congested_at = Some(now);
        }
        if self.viewers.len() < 2 {
            self.capacity = None;
            return None;
        }
        let recently = |at: Option<Instant>, within: Duration| at.is_some_and(|at| now.duration_since(at) < within);
        let cutting = self.viewers.values().filter(|viewer| recently(viewer.congested_at, TOGETHER)).count();
        if cutting >= 2 && !recently(self.changed_at, TOGETHER) {
            let total: f64 = self.viewers.values().map(|viewer| viewer.report.sending).sum();
            self.capacity = Some(total * HEADROOM);
            self.changed_at = Some(now);
            tracing::info!(mbps = total * HEADROOM / 1e6, "the upload is full; sharing it evenly");
        } else if let Some(capacity) = self.capacity
            && !recently(self.changed_at, GROW_AFTER)
        {
            self.capacity = Some(capacity * GROWTH);
            self.changed_at = Some(now - GROW_AFTER + Duration::from_secs(1));
        }
        self.share(id)
    }

    pub fn remove(&mut self, id: u64) {
        self.viewers.remove(&id);
    }

    /// Everyone's even share, handing what anyone can't use to the rest.
    fn share(&self, id: u64) -> Option<u32> {
        let mut left = self.capacity?;
        let mut wants: Vec<(u64, f64)> = self
            .viewers
            .iter()
            .map(|(&viewer_id, viewer)| {
                let Report { sending, target, limit, .. } = viewer.report;
                (viewer_id, if sending < target * USING { limit.min(sending * ROOM) } else { limit })
            })
            .collect();
        wants.sort_unstable_by(|a, b| a.1.total_cmp(&b.1));
        for (index, &(viewer_id, want)) in wants.iter().enumerate() {
            let even = left / (wants.len() - index) as f64;
            let share = want.min(even);
            if viewer_id == id {
                // The last in line gets everything left, so nothing's stranded by rounding.
                return Some(if want > even { even as u32 } else { share as u32 });
            }
            left -= share;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(sending: f64, congested: bool) -> Report {
        Report { sending, target: sending, limit: 200e6, congested }
    }

    #[test]
    fn one_viewer_has_the_whole_upload() {
        let mut uplink = Uplink::default();
        assert_eq!(uplink.report(1, Instant::now(), report(80e6, true)), None);
    }

    #[test]
    fn one_slow_viewer_doesnt_limit_the_others() {
        let mut uplink = Uplink::default();
        let now = Instant::now();
        uplink.report(1, now, report(80e6, false));
        assert_eq!(uplink.report(2, now, report(10e6, true)), None);
        assert_eq!(uplink.report(1, now, report(80e6, false)), None);
        assert_eq!(uplink.report(2, now + Duration::from_secs(5), report(8e6, true)), None);
    }

    #[test]
    fn viewers_cutting_together_share_the_upload_evenly() {
        let mut uplink = Uplink::default();
        let now = Instant::now();
        uplink.report(1, now, report(60e6, true));
        let two = uplink.report(2, now, report(40e6, true));
        assert_eq!(two, Some(47_500_000));
        assert_eq!(uplink.report(1, now, report(60e6, false)), Some(47_500_000));
    }

    #[test]
    fn what_someone_cant_use_goes_to_the_others() {
        let mut uplink = Uplink::default();
        let now = Instant::now();
        for id in [1, 2, 3] {
            uplink.report(id, now, report(40e6, false));
        }
        for id in [1, 2, 3] {
            uplink.report(id, now, report(40e6, true));
        }
        let limited = Report { sending: 10e6, target: 40e6, limit: 12e6, congested: false };
        assert_eq!(uplink.report(3, now, limited), Some(12_000_000));
        let share = uplink.report(1, now, report(40e6, false)).unwrap();
        assert_eq!(share / 1_000_000, 51);
    }

    #[test]
    fn the_upload_is_tried_again_and_forgotten_with_one_viewer_left() {
        let mut uplink = Uplink::default();
        let now = Instant::now();
        uplink.report(1, now, report(50e6, true));
        let first = uplink.report(2, now, report(50e6, true)).unwrap();
        let later = uplink.report(2, now + Duration::from_secs(6), report(47e6, false)).unwrap();
        assert!(later > first, "{later} over {first}");
        uplink.remove(2);
        assert_eq!(uplink.report(1, now + Duration::from_secs(7), report(47e6, false)), None);
    }
}
