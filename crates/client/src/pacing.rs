//! When each decoded frame goes to the window.
//!
//! Lowest latency shows a frame as soon as it's decoded: the least delay, but uneven network
//! and decode times turn into uneven motion. Smooth shows each frame a steady delay after the
//! host captured it, the delay most recent frames needed to get here (90th percentile), so
//! frames come out spaced as they were captured, at the cost of that extra delay.

use std::{collections::VecDeque, time::Duration};

use connect_proto::now_micros;

use crate::VideoFrame;

/// Recent frames' transit times kept for the delay estimate: about a second at 120 fps.
const SAMPLES: usize = 120;
/// Frames held at most; past this the oldest is dropped.
const MAX_QUEUED: usize = 8;
/// Never wait longer than this for smoothness.
const MAX_DELAY_US: i64 = 60_000;
/// Margin above the 90th percentile, so the usual frame isn't just barely on time.
const MARGIN_US: i64 = 1_000;

#[derive(Default)]
pub(crate) struct Pacer {
    /// Frames waiting, with when to show them (this PC's clock, µs).
    queue: VecDeque<(VideoFrame, i64)>,
    /// Host capture → decoded here, µs, for recent frames.
    transit: VecDeque<i64>,
}

impl Pacer {
    /// Queues a decoded frame. `offset_us` is this clock minus the host's, once known; without
    /// it, or without `smooth`, the frame is due at once. Returns frames it replaced unshown.
    pub fn push(&mut self, frame: VideoFrame, offset_us: Option<i64>, smooth: bool) -> Vec<VideoFrame> {
        let now = now_micros() as i64;
        let due = match offset_us {
            Some(offset) => {
                let captured_here = frame.meta.capture_time_us as i64 + offset;
                if self.transit.len() == SAMPLES {
                    self.transit.pop_front();
                }
                self.transit.push_back(now - captured_here);
                if smooth { (captured_here + self.delay()).max(now) } else { now }
            }
            None => now,
        };
        let mut replaced = Vec::new();
        if due <= now {
            // Due now: anything still waiting is older and would never be seen.
            replaced.extend(self.queue.drain(..).map(|(frame, _)| frame));
        }
        self.queue.push_back((frame, due));
        while self.queue.len() > MAX_QUEUED {
            replaced.extend(self.queue.pop_front().map(|(frame, _)| frame));
        }
        replaced
    }

    /// The newest frame now due, and the due ones before it, which it replaces unshown.
    pub fn take(&mut self) -> (Option<VideoFrame>, Vec<VideoFrame>) {
        let now = now_micros() as i64;
        let mut replaced = Vec::new();
        let mut newest = None;
        while self.queue.front().is_some_and(|(_, due)| *due <= now) {
            let (frame, _) = self.queue.pop_front().expect("checked above");
            replaced.extend(newest.replace(frame));
        }
        (newest, replaced)
    }

    /// How long until the next waiting frame is due.
    pub fn next_due(&self) -> Option<Duration> {
        let (_, due) = self.queue.front()?;
        Some(Duration::from_micros((*due - now_micros() as i64).max(0) as u64))
    }

    /// The delay that would have got 90% of recent frames here in time.
    fn delay(&self) -> i64 {
        let mut sorted: Vec<i64> = self.transit.iter().copied().collect();
        sorted.sort_unstable();
        let p90 = sorted.get(sorted.len() * 9 / 10).copied().unwrap_or(0);
        (p90 + MARGIN_US).clamp(0, MAX_DELAY_US)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use connect_proto::video::FrameMeta;

    fn frame(seq: u32, captured_ago_us: u64) -> VideoFrame {
        VideoFrame {
            width: 2,
            height: 2,
            yuv: Vec::new(),
            texture: None,
            meta: FrameMeta { seq, capture_time_us: now_micros() - captured_ago_us, ..Default::default() },
            timing: Default::default(),
        }
    }

    #[test]
    fn lowest_latency_shows_the_newest_frame_at_once() {
        let mut pacer = Pacer::default();
        assert!(pacer.push(frame(0, 5_000), Some(0), false).is_empty());
        let replaced = pacer.push(frame(1, 5_000), Some(0), false);
        assert_eq!(replaced.iter().map(|f| f.meta.seq).collect::<Vec<_>>(), [0]);
        let (shown, _) = pacer.take();
        assert_eq!(shown.unwrap().meta.seq, 1);
        assert!(pacer.next_due().is_none());
    }

    #[test]
    fn smooth_holds_a_quick_frame_back_to_the_usual_delay() {
        let mut pacer = Pacer::default();
        // Most frames take 20 ms to get here, and are shown as they arrive.
        // (plus the margin).
        for seq in 0..20 {
            pacer.push(frame(seq, 20_000), Some(0), true);
            let wait = pacer.next_due().unwrap();
            assert!(wait <= Duration::from_millis(2), "{wait:?}");
            std::thread::sleep(wait);
            assert!(pacer.take().0.is_some());
        }
        // One that made it in 5 ms waits until about 21 ms after capture.
        pacer.push(frame(20, 5_000), Some(0), true);
        assert!(pacer.take().0.is_none(), "held back");
        let wait = pacer.next_due().unwrap();
        assert!(wait > Duration::from_millis(14) && wait < Duration::from_millis(18), "{wait:?}");
        // A slow one is shown at once, replacing nothing newer.
        let replaced = pacer.push(frame(21, 40_000), Some(0), true);
        assert_eq!(replaced.len(), 1, "the held frame is older and would never be seen");
        assert_eq!(pacer.take().0.unwrap().meta.seq, 21);
    }

    #[test]
    fn without_the_clock_offset_frames_are_due_at_once() {
        let mut pacer = Pacer::default();
        pacer.push(frame(0, 5_000), None, true);
        assert_eq!(pacer.take().0.unwrap().meta.seq, 0);
    }
}
