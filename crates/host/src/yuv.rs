//! Captured BGRA → 4:2:0 YUV for the encoders: BT.601 limited range (the same coefficients as
//! openh264's own converter and the client's decoder), in integers, split across threads by
//! bands of rows, and read straight from the captured image wherever its rows are.
//!
//! OpenH264 takes I420 (separate U and V planes); hardware encoders take NV12 (U and V
//! interleaved in one plane).

use openh264::formats::YUVSource;

/// Frames smaller than this many pixels aren't worth splitting across threads.
const PARALLEL_FROM_PIXELS: usize = 640 * 480;
/// More threads than this stop paying off; memory bandwidth is the limit.
const MAX_THREADS: usize = 8;

/// How the chroma samples are laid out after the full-size Y plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Layout {
    /// A quarter-size U plane, then a quarter-size V plane.
    I420,
    /// One half-size plane of U, V pairs.
    #[cfg_attr(not(windows), allow(dead_code))]
    Nv12,
}

/// One 4:2:0 picture: a full-size Y plane, then the chroma.
pub(crate) struct Picture {
    layout: Layout,
    width: usize,
    height: usize,
    data: Vec<u8>,
}

impl Picture {
    /// `width` and `height` must be even.
    pub fn new(layout: Layout, width: usize, height: usize) -> Self {
        debug_assert!(width.is_multiple_of(2) && height.is_multiple_of(2), "4:2:0 needs even dimensions");
        Self { layout, width, height, data: vec![0; width * height * 3 / 2] }
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn layout(&self) -> Layout {
        self.layout
    }

    pub fn size(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    /// The whole picture, planes back to back.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// The same picture in `layout`.
    pub fn to_layout(&self, layout: Layout) -> Picture {
        let mut out = Picture::new(layout, self.width, self.height);
        let luma = self.width * self.height;
        out.data[..luma].copy_from_slice(&self.data[..luma]);
        let (from, to) = (&self.data[luma..], &mut out.data[luma..]);
        let quarter = luma / 4;
        match (self.layout, layout) {
            (Layout::I420, Layout::Nv12) => {
                for (i, pair) in to.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                    *pair = [from[i], from[quarter + i]];
                }
            }
            (Layout::Nv12, Layout::I420) => {
                let (u, v) = to.split_at_mut(quarter);
                for (i, [cb, cr]) in from.as_chunks::<2>().0.iter().enumerate() {
                    (u[i], v[i]) = (*cb, *cr);
                }
            }
            _ => to.copy_from_slice(from),
        }
        out
    }

    /// Fills this picture from the top-left `width`×`height` of `bgra`, whose rows are `stride`
    /// bytes apart.
    pub fn convert_from_bgra(&mut self, bgra: &[u8], stride: usize) {
        let (w, h) = (self.width, self.height);
        assert!(stride >= w * 4 && bgra.len() >= stride * (h - 1) + w * 4, "image smaller than {w}×{h}");
        let threads = if w * h < PARALLEL_FROM_PIXELS {
            1
        } else {
            std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(1, MAX_THREADS)
        };
        // Bands of whole row pairs, since each chroma row comes from two image rows.
        let pairs_per_band = (h / 2).div_ceil(threads);
        let rows = 2 * pairs_per_band;
        let (y_plane, chroma) = self.data.split_at_mut(w * h);
        let y_bands = y_plane.chunks_mut(rows * w);
        let sources = (0..h.div_ceil(rows)).map(|band| &bgra[band * rows * stride..]);
        match self.layout {
            Layout::I420 => {
                let (u_plane, v_plane) = chroma.split_at_mut(w * h / 4);
                let chroma_bands =
                    u_plane.chunks_mut(pairs_per_band * w / 2).zip(v_plane.chunks_mut(pairs_per_band * w / 2));
                let work = sources.zip(y_bands).zip(chroma_bands);
                run(threads, work, |((src, y), (u, v))| {
                    convert_band(src, stride, w, y, |pair, x, cb, cr| {
                        u[pair * w / 2 + x] = cb;
                        v[pair * w / 2 + x] = cr;
                    })
                });
            }
            Layout::Nv12 => {
                let work = sources.zip(y_bands).zip(chroma.chunks_mut(pairs_per_band * w));
                run(threads, work, |((src, y), uv)| {
                    convert_band(src, stride, w, y, |pair, x, cb, cr| {
                        uv[pair * w + 2 * x] = cb;
                        uv[pair * w + 2 * x + 1] = cr;
                    })
                });
            }
        }
    }
}

/// Runs `f` on each item, on scoped threads unless `threads` is 1.
fn run<T: Send>(threads: usize, work: impl Iterator<Item = T>, f: impl Fn(T) + Sync) {
    if threads == 1 {
        work.for_each(f);
        return;
    }
    let f = &f;
    std::thread::scope(|scope| {
        for item in work {
            scope.spawn(move || f(item));
        }
    });
}

/// Converts the rows behind `y` (two image rows per chroma row) starting at `src`, handing each
/// chroma sample to `chroma(row pair, column, U, V)`.
#[inline(always)]
fn convert_band(src: &[u8], stride: usize, w: usize, y: &mut [u8], mut chroma: impl FnMut(usize, usize, u8, u8)) {
    for (pair, y_pair) in y.chunks_exact_mut(2 * w).enumerate() {
        let top = &src[2 * pair * stride..][..w * 4];
        let bottom = &src[(2 * pair + 1) * stride..][..w * 4];
        let (y_top, y_bottom) = y_pair.split_at_mut(w);
        for (x, ((a, b), (yt, yb))) in top
            .as_chunks::<8>()
            .0
            .iter()
            .zip(bottom.as_chunks::<8>().0)
            .zip(y_top.as_chunks_mut::<2>().0.iter_mut().zip(y_bottom.as_chunks_mut::<2>().0))
            .enumerate()
        {
            *yt = [luma(a[0], a[1], a[2]), luma(a[4], a[5], a[6])];
            *yb = [luma(b[0], b[1], b[2]), luma(b[4], b[5], b[6])];
            // Chroma from the average of the 2×2 block, rounded.
            let avg = |i: usize| (a[i] as i32 + a[i + 4] as i32 + b[i] as i32 + b[i + 4] as i32 + 2) >> 2;
            let (bl, gr, rd) = (avg(0), avg(1), avg(2));
            let cb = (((-38 * rd - 74 * gr + 112 * bl + 128) >> 8) + 128) as u8;
            let cr = (((112 * rd - 94 * gr - 18 * bl + 128) >> 8) + 128) as u8;
            chroma(pair, x, cb, cr);
        }
    }
}

#[inline(always)]
fn luma(b: u8, g: u8, r: u8) -> u8 {
    (((66 * r as u32 + 129 * g as u32 + 25 * b as u32 + 128) >> 8) + 16) as u8
}

/// OpenH264 reads I420 pictures directly.
impl YUVSource for Picture {
    fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    fn strides(&self) -> (usize, usize, usize) {
        debug_assert_eq!(self.layout, Layout::I420);
        (self.width, self.width / 2, self.width / 2)
    }

    fn y(&self) -> &[u8] {
        &self.data[..self.width * self.height]
    }

    fn u(&self) -> &[u8] {
        let y = self.width * self.height;
        &self.data[y..y + y / 4]
    }

    fn v(&self) -> &[u8] {
        let y = self.width * self.height;
        &self.data[y + y / 4..]
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use openh264::formats::{BgraSliceU8, YUVBuffer};

    use super::*;

    /// A busy BGRA image: every pixel different, nothing in step with the 2×2 chroma blocks.
    pub fn busy_image(w: usize, h: usize, t: usize) -> Vec<u8> {
        let mut img = vec![0u8; w * h * 4];
        for (i, px) in img.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let (x, y) = (i % w + t * 7, i / w + t * 3);
            let n = (x.wrapping_mul(2654435761) ^ y.wrapping_mul(40503)) >> 7;
            *px = [(n + x) as u8, (n >> 8) as u8 ^ y as u8, ((x * y) >> 4) as u8, 255];
        }
        img
    }

    #[test]
    fn matches_openh264_to_within_rounding() {
        for (w, h) in [(64, 48), (1280, 720), (2, 2)] {
            let img = busy_image(w, h, 3);
            let mut ours = Picture::new(Layout::I420, w, h);
            ours.convert_from_bgra(&img, w * 4);
            let mut theirs = YUVBuffer::new(w, h);
            theirs.read_bgra8(BgraSliceU8::new(&img, (w, h)));
            for (name, a, b) in [("y", ours.y(), theirs.y()), ("u", ours.u(), theirs.u()), ("v", ours.v(), theirs.v())]
            {
                let worst = a.iter().zip(b).map(|(a, b)| a.abs_diff(*b)).max().unwrap();
                assert!(worst <= 1, "{w}×{h} {name} differs by {worst}");
            }
        }
    }

    #[test]
    fn reads_padded_rows() {
        let (w, h, stride) = (8, 4, 48);
        let tight = busy_image(w, h, 0);
        let mut padded = vec![0xAB; stride * h];
        for (row, src) in padded.chunks_exact_mut(stride).zip(tight.chunks_exact(w * 4)) {
            row[..w * 4].copy_from_slice(src);
        }
        let (mut a, mut b) = (Picture::new(Layout::I420, w, h), Picture::new(Layout::I420, w, h));
        a.convert_from_bgra(&tight, w * 4);
        b.convert_from_bgra(&padded, stride);
        assert_eq!(a.data, b.data);
    }

    #[test]
    fn nv12_interleaves_the_same_chroma() {
        let (w, h) = (1280, 720);
        let img = busy_image(w, h, 5);
        let (mut i420, mut nv12) = (Picture::new(Layout::I420, w, h), Picture::new(Layout::Nv12, w, h));
        i420.convert_from_bgra(&img, w * 4);
        nv12.convert_from_bgra(&img, w * 4);
        assert_eq!(i420.y(), &nv12.data[..w * h]);
        let uv: Vec<u8> = i420.u().iter().zip(i420.v()).flat_map(|(u, v)| [*u, *v]).collect();
        assert_eq!(uv, &nv12.data[w * h..]);
        assert_eq!(nv12.to_layout(Layout::I420).data, i420.data);
        assert_eq!(i420.to_layout(Layout::Nv12).data, nv12.data);
    }
}

/// `cargo test -p connect-host --release -- --ignored --nocapture bench_`
#[cfg(test)]
mod bench {
    use std::time::Instant;

    use openh264::{
        OpenH264API,
        encoder::{BitRate, Encoder, EncoderConfig, FrameRate, RateControlMode, UsageType},
        formats::{BgraSliceU8, YUVBuffer},
    };

    use super::{Layout, Picture, tests::busy_image};

    const W: usize = 2560;
    const H: usize = 1440;
    const FRAMES: usize = 60;

    /// A scrolling busy picture with a patch of fresh noise each frame, roughly like a game.
    fn game_frames() -> Vec<Vec<u8>> {
        (0..8)
            .map(|t| {
                let mut img = busy_image(W, H, t);
                let mut seed = 0x9E3779B97F4A7C15u64.wrapping_mul(t as u64 + 1);
                for row in img.as_chunks_mut::<{ W * 4 }>().0.iter_mut().skip(400).take(500) {
                    for b in &mut row[800 * 4..1400 * 4] {
                        seed ^= seed << 13;
                        seed ^= seed >> 7;
                        seed ^= seed << 17;
                        *b = seed as u8;
                    }
                }
                img
            })
            .collect()
    }

    fn per_frame(label: &str, mut f: impl FnMut(usize)) {
        f(0); // warm up
        let started = Instant::now();
        for i in 0..FRAMES {
            f(i);
        }
        println!("{label:<44} {:>6.2} ms/frame", started.elapsed().as_secs_f64() * 1000.0 / FRAMES as f64);
    }

    #[test]
    #[ignore]
    fn bench_conversion() {
        let frames = game_frames();
        let mut theirs = YUVBuffer::new(W, H);
        per_frame("openh264 read_bgra8", |i| theirs.read_bgra8(BgraSliceU8::new(&frames[i % 8], (W, H))));
        let mut ours = Picture::new(Layout::I420, W, H);
        per_frame("threaded integer convert, I420", |i| ours.convert_from_bgra(&frames[i % 8], W * 4));
        let mut ours = Picture::new(Layout::Nv12, W, H);
        per_frame("threaded integer convert, NV12", |i| ours.convert_from_bgra(&frames[i % 8], W * 4));
    }

    #[test]
    #[ignore]
    fn bench_encode() {
        let frames = game_frames();
        let yuv: Vec<Picture> = frames
            .iter()
            .map(|f| {
                let mut p = Picture::new(Layout::I420, W, H);
                p.convert_from_bgra(f, W * 4);
                p
            })
            .collect();
        let base = || {
            EncoderConfig::new()
                .usage_type(UsageType::ScreenContentRealTime)
                .rate_control_mode(RateControlMode::Bitrate)
                .bitrate(BitRate::from_bps(35_000_000))
                .max_frame_rate(FrameRate::from_hz(144.0))
                .adaptive_quantization(false)
                .background_detection(false)
        };
        let configs: [(&str, EncoderConfig); 5] = [
            ("1 thread, skip frames (v0.2.1)", base().skip_frames(true)),
            ("1 thread, no skipping", base().skip_frames(false)),
            ("4 threads", base().skip_frames(false).num_threads(4)),
            ("4 threads, 64 KB slices", base().skip_frames(false).num_threads(4).max_slice_len(64_000)),
            ("8 threads, 64 KB slices", base().skip_frames(false).num_threads(8).max_slice_len(64_000)),
        ];
        for (label, config) in configs {
            let mut encoder = Encoder::with_api_config(OpenH264API::from_source(), config).unwrap();
            let (mut bytes, mut skipped) = (0usize, 0usize);
            per_frame(label, |i| {
                let out = encoder.encode(&yuv[i % 8]).unwrap().to_vec();
                if out.is_empty() {
                    skipped += 1;
                }
                bytes += out.len();
            });
            println!("{:<44} {:>6} KB/frame, {skipped} skipped", "", bytes / (FRAMES + 1) / 1000);
        }
    }
}
