//! Where each frame's time goes, from the host's screen to this one, averaged for the stats.
//!
//! The host stamps its stages into every frame on its own clock; this side adds when the frame
//! arrived, was decoded and was drawn, and lines the two clocks up through the offset the pings
//! measure. "Glass to glass" here ends when the frame is drawn into the window: the time until
//! the display shows it (at most one refresh) isn't measurable from inside the app.

use std::time::Instant;

use connect_proto::{now_micros, video::FrameMeta};

/// One frame's timeline. Travels with the decoded picture to the window and back.
#[derive(Debug, Clone, Copy, Default)]
pub struct FrameTiming {
    pub(crate) meta: FrameMeta,
    pub(crate) first_seen: Option<Instant>,
    pub(crate) decode_started: Option<Instant>,
    pub(crate) decoded: Option<Instant>,
    pub(crate) painted: Option<Instant>,
}

impl FrameTiming {
    /// Call when the picture is drawn into the window.
    pub fn mark_painted(&mut self) {
        self.painted.get_or_insert_with(Instant::now);
    }
}

/// Average milliseconds per stage over the last stats window.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Latency {
    /// On the host: from the image reaching its screen to capture picking it up. `None` where
    /// the host can't tell.
    pub present_to_capture: Option<f32>,
    /// On the host: copying the image and converting its colours.
    pub capture: f32,
    pub encode: f32,
    /// On the host: waiting for the network to take the frame, and packetizing.
    pub host_wait: f32,
    /// Host to here. `None` until the clocks are lined up.
    pub network: Option<f32>,
    /// From the first packet arriving to decoding starting: the rest of the packets, rebuilding
    /// lost ones, waiting for earlier frames, the decoder's queue.
    pub reassembly: f32,
    pub decode: f32,
    /// From decoded to drawn into the window.
    pub render: f32,
    /// The host's screen to this window, on average; `None` until the clocks are lined up.
    pub total: Option<f32>,
    /// The slowest frame's total in the window.
    pub worst: Option<f32>,
}

/// Sums frames' timings until the stats take an average.
#[derive(Debug, Default)]
pub(crate) struct LatencyWindow {
    frames: u32,
    present: (f64, u32),
    capture: f64,
    encode: f64,
    host_wait: f64,
    network: (f64, u32),
    reassembly: f64,
    decode: f64,
    render: f64,
    total: (f64, u32),
    worst: Option<f64>,
}

impl LatencyWindow {
    /// Adds a frame that has been drawn. `offset_us` is this clock minus the host's, once known.
    pub fn add(&mut self, timing: &FrameTiming, offset_us: Option<i64>) {
        let (Some(first_seen), Some(started), Some(decoded), Some(painted)) =
            (timing.first_seen, timing.decode_started, timing.decoded, timing.painted)
        else {
            return;
        };
        let ms = |us: u32| f64::from(us) / 1000.0;
        let meta = &timing.meta;
        self.frames += 1;
        if meta.present_to_capture_us > 0 {
            self.present.0 += ms(meta.present_to_capture_us);
            self.present.1 += 1;
        }
        self.capture += ms(meta.capture_us);
        self.encode += ms(meta.encode_time_us);
        self.host_wait += ms(meta.sent_after_us.saturating_sub(meta.encode_time_us));
        self.reassembly += started.saturating_duration_since(first_seen).as_secs_f64() * 1000.0;
        self.decode += decoded.saturating_duration_since(started).as_secs_f64() * 1000.0;
        let render = painted.saturating_duration_since(decoded).as_secs_f64() * 1000.0;
        self.render += render;
        if let Some(offset) = offset_us {
            // The first packet's arrival on this clock, then on the host's.
            let arrived = now_micros() as i64 - first_seen.elapsed().as_micros() as i64;
            let sent = (meta.capture_time_us + u64::from(meta.sent_after_us)) as i64;
            let network = ((arrived - offset - sent).max(0) as f64) / 1000.0;
            self.network.0 += network;
            self.network.1 += 1;
            let total = ms(meta.present_to_capture_us)
                + ms(meta.capture_us)
                + ms(meta.sent_after_us)
                + network
                + painted.saturating_duration_since(first_seen).as_secs_f64() * 1000.0;
            self.total.0 += total;
            self.total.1 += 1;
            self.worst = Some(self.worst.map_or(total, |worst| worst.max(total)));
        }
    }

    /// The averages since the last call, and starts afresh. `None` if no frame was drawn.
    pub fn take(&mut self) -> Option<Latency> {
        let window = std::mem::take(self);
        if window.frames == 0 {
            return None;
        }
        let n = f64::from(window.frames);
        let average = |(sum, count): (f64, u32)| (count > 0).then(|| (sum / f64::from(count)) as f32);
        Some(Latency {
            present_to_capture: average(window.present),
            capture: (window.capture / n) as f32,
            encode: (window.encode / n) as f32,
            host_wait: (window.host_wait / n) as f32,
            network: average(window.network),
            reassembly: (window.reassembly / n) as f32,
            decode: (window.decode / n) as f32,
            render: (window.render / n) as f32,
            total: average(window.total),
            worst: window.worst.map(|worst| worst as f32),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn stages_add_up_to_the_total() {
        let now = Instant::now();
        let at = |ms: u64| now - Duration::from_millis(ms);
        // Arrived 20 ms ago, decoding 18 ms ago, decoded 16, drawn 10.
        let mut timing = FrameTiming {
            meta: FrameMeta {
                present_to_capture_us: 3_000,
                capture_us: 1_000,
                encode_time_us: 2_000,
                sent_after_us: 2_500,
                capture_time_us: now_micros() - 30_000,
                ..Default::default()
            },
            first_seen: Some(at(20)),
            decode_started: Some(at(18)),
            decoded: Some(at(16)),
            painted: Some(at(10)),
        };
        let mut window = LatencyWindow::default();
        window.add(&timing, Some(0));
        let latency = window.take().unwrap();
        assert_eq!(latency.present_to_capture, Some(3.0));
        assert_eq!((latency.capture, latency.encode, latency.host_wait), (1.0, 2.0, 0.5));
        let close = |a: f32, b: f32| (a - b).abs() < 1.0;
        // Captured 30 ms ago, sent 2.5 ms later, arrived 20 ms ago: 7.5 ms on the network.
        assert!(close(latency.network.unwrap(), 7.5), "{latency:?}");
        assert!(close(latency.reassembly, 2.0) && close(latency.decode, 2.0) && close(latency.render, 6.0));
        // 3 + 1 + 2.5 + 7.5 + 10 (arrival to drawn) = 24.
        assert!(close(latency.total.unwrap(), 24.0), "{latency:?}");
        assert!(window.take().is_none(), "starts afresh");
        // Without the clock offset, nothing that spans both clocks.
        timing.mark_painted();
        window.add(&timing, None);
        let latency = window.take().unwrap();
        assert_eq!((latency.network, latency.total), (None, None));
    }
}
