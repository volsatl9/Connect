use std::time::{Duration, Instant};

use anyhow::Result;

use super::{CaptureTiming, Capturer, Frame};

/// BGRA colour bars.
const BARS: [[u8; 4]; 8] = [
    [235, 235, 235, 255],
    [16, 235, 235, 255],
    [235, 235, 16, 255],
    [16, 235, 16, 255],
    [235, 16, 235, 255],
    [16, 16, 235, 255],
    [235, 16, 16, 255],
    [16, 16, 16, 255],
];

/// Colour bars scrolling sideways and a bouncing white square, so motion and dropped frames show.
pub(crate) struct TestPattern {
    width: u32,
    height: u32,
    interval: Duration,
    next_at: Instant,
    frame: u64,
    buf: Vec<u8>,
    acquired: Instant,
}

impl TestPattern {
    pub fn new(width: u32, height: u32, fps: u32) -> Self {
        Self {
            width: width.max(16),
            height: height.max(16),
            interval: Duration::from_secs(1) / fps.max(1),
            next_at: Instant::now(),
            frame: 0,
            buf: vec![0; width.max(16) as usize * height.max(16) as usize * 4],
            acquired: Instant::now(),
        }
    }

    fn draw(&mut self) {
        let (w, h) = (self.width as usize, self.height as usize);
        let stride = w * 4;
        let bar_width = w.div_ceil(BARS.len());
        let shift = (self.frame as usize * 4) % w;

        let (first_row, rest) = self.buf.split_at_mut(stride);
        for (x, px) in first_row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            *px = BARS[((x + shift) % w) / bar_width];
        }
        for row in rest.chunks_exact_mut(stride) {
            row.copy_from_slice(first_row);
        }

        let size = (h / 6).max(8);
        let bounce = |pos: usize, span: usize| {
            let span = span.saturating_sub(size).max(1);
            let p = pos % (2 * span);
            if p < span { p } else { 2 * span - p }
        };
        let (sx, sy) = (bounce(self.frame as usize * 7, w), bounce(self.frame as usize * 5, h));
        for row in self.buf.chunks_exact_mut(stride).skip(sy).take(size) {
            row[sx * 4..(sx + size).min(w) * 4].fill(255);
        }
    }
}

impl Capturer for TestPattern {
    fn timing(&self) -> CaptureTiming {
        CaptureTiming { acquired: self.acquired, present_to_capture: None }
    }

    fn refresh_hz(&self) -> Option<u32> {
        Some((Duration::from_secs(1).as_secs_f64() / self.interval.as_secs_f64()).round() as u32)
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame<'_>>> {
        let now = Instant::now();
        if self.next_at > now {
            let wait = self.next_at - now;
            if wait > timeout {
                std::thread::sleep(timeout);
                return Ok(None);
            }
            std::thread::sleep(wait);
        }
        // Don't try to catch up after a stall; just carry on from now.
        self.next_at = (self.next_at + self.interval).max(Instant::now());

        self.acquired = Instant::now();
        self.draw();
        self.frame += 1;
        Ok(Some(Frame { width: self.width, height: self.height, stride: self.width as usize * 4, bgra: &self.buf }))
    }
}
