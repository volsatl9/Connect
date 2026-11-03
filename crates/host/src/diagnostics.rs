//! `connect gpu-check`, hosting half: what this PC's graphics hardware offers for sharing the
//! screen, and a short run of the real capture → GPU conversion → hardware encoder path, checked
//! against the processor path. Lines go to `out` as they're found, for a report to paste.

/// Runs the hosting checks.
pub fn run(out: &mut dyn FnMut(String)) {
    out("== Sharing this PC ==".into());
    #[cfg(windows)]
    windows::run(out);
    #[cfg(not(windows))]
    out("Hosting needs Windows; nothing to check here.".into());
}

#[cfg(windows)]
mod windows {
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result, ensure};
    use connect_proto::Codec;
    use openh264::{decoder::Decoder, formats::YUVSource};
    use windows::Win32::Graphics::Direct3D11::ID3D11Device;
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, IDXGIFactory1};

    use crate::{
        capture::{Capturer, dxgi::DxgiCapturer},
        encode::{Input, VideoEncoder, mf},
        gpu::{ConvertMethod, Nv12Converter},
        yuv::{Layout, Picture},
    };

    /// How long the pipeline runs for timing.
    const RUN: Duration = Duration::from_secs(3);
    const BITRATE: u32 = 20_000_000;

    pub fn run(out: &mut dyn FnMut(String)) {
        adapters(out);
        for codec in [Codec::H264, Codec::H265, Codec::Av1] {
            match mf::encoder_names(codec) {
                Ok(names) if names.is_empty() => out(format!("Hardware {codec} encoder: none")),
                Ok(names) => out(format!("Hardware {codec} encoder: {}", names.join(", "))),
                Err(e) => out(format!("Hardware {codec} encoder: can't tell ({e:#})")),
            }
        }
        if let Err(e) = pipeline(out) {
            out(format!("GPU-only pipeline: FAILED: {e:#}"));
            out("  Sharing still works, through memory as in 0.5.0.".into());
        }
    }

    fn adapters(out: &mut dyn FnMut(String)) {
        let factory = match unsafe { CreateDXGIFactory1::<IDXGIFactory1>() } {
            Ok(factory) => factory,
            Err(e) => return out(format!("Graphics adapters: can't list them ({e})")),
        };
        for index in 0.. {
            let Ok(adapter) = (unsafe { factory.EnumAdapters1(index) }) else { break };
            let Ok(desc) = (unsafe { adapter.GetDesc1() }) else { continue };
            let name = String::from_utf16_lossy(&desc.Description);
            let name = name.trim_end_matches('\0');
            let software = desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0;
            let memory = desc.DedicatedVideoMemory / (1024 * 1024);
            out(format!(
                "Graphics adapter {index}: {name}, {memory} MB{}{}",
                if software { " (software)" } else { "" },
                if index == 0 { " (captures the screen)" } else { "" }
            ));
        }
    }

    fn pipeline(out: &mut dyn FnMut(String)) -> Result<()> {
        let mut capturer = DxgiCapturer::new().context("starting screen capture")?;
        let fps = capturer.refresh_hz().unwrap_or(60);
        let started = Instant::now();
        let (w, h) = loop {
            if let Some(size) = capturer.next_texture(Duration::from_millis(200))? {
                break size;
            }
            ensure!(started.elapsed() < Duration::from_secs(3), "no screen image within 3 s");
        };
        let (w, h) = (w & !1, h & !1);
        out(format!("Screen: {w}×{h} at {fps} Hz"));
        let device = capturer.device().clone();

        // Colour conversion on the GPU, both ways, against the processor's conversion of the same
        // image, and timed on the GPU.
        let frame = capturer.read_latest()?.context("no image to compare")?;
        let mut on_cpu = Picture::new(Layout::Nv12, w as usize, h as usize);
        let convert_started = Instant::now();
        on_cpu.convert_from_bgra(frame.bgra, frame.stride);
        let cpu_convert = convert_started.elapsed();
        let luma = (w * h) as usize;
        for method in [ConvertMethod::Shaders, ConvertMethod::VideoProcessor] {
            let line = Nv12Converter::with_method(&device, w, h, method).and_then(|mut converter| {
                converter.convert(capturer.latest(), None)?;
                let on_gpu = converter.read_back()?;
                let (y_mean, y_max) = difference(&on_gpu.data()[..luma], &on_cpu.data()[..luma]);
                let (c_mean, c_max) = difference(&on_gpu.data()[luma..], &on_cpu.data()[luma..]);
                let verdict = match (y_max, c_max) {
                    (0, 0) => "OK, identical to the processor's".to_owned(),
                    _ if y_mean < 1.5 && c_mean < 1.5 && y_max <= 16 && c_max <= 16 => format!(
                        "OK (difference from the processor's: brightness {y_mean:.2} on average, at most {y_max}; \
                         colour {c_mean:.2}, at most {c_max})"
                    ),
                    _ => format!(
                        "MISMATCH (brightness {y_mean:.2} on average, at most {y_max}; colour {c_mean:.2}, at most {c_max})"
                    ),
                };
                Ok(format!("{verdict}; {}", gpu_time(&mut converter, capturer.latest())))
            });
            match line {
                Ok(line) => out(format!("GPU colour conversion ({method}): {line}")),
                Err(error) => out(format!("GPU colour conversion ({method}): unavailable: {error:#}")),
            }
        }
        let mut converter = Nv12Converter::new(&device, w, h).context("GPU colour conversion")?;
        out(format!("GPU colour conversion in sessions: {}", converter.method()));
        converter.convert(capturer.latest(), None)?;
        let on_gpu = converter.read_back()?;

        // The encoder reading the converted textures directly, on live frames for a few seconds.
        let mut encoder = mf::HardwareEncoder::new(Codec::H264, w, h, BITRATE, fps, Some(&device))
            .context("hardware H.264 encoder reading textures")?;
        out(format!("GPU encoder: {}, reading textures directly", encoder.name()));
        out(format!("  Burst controls: {}", burst(encoder.burst())));
        let mut first_keyframe = None;
        let (mut times, mut fresh, mut timestamp) = (Vec::new(), 0, 0u64);
        let (mut sizes, mut keyframe_sizes) = (Vec::new(), Vec::new());
        let run_started = Instant::now();
        while run_started.elapsed() < RUN {
            if let Some(size) = capturer.next_texture(Duration::from_millis(4))?
                && (size.0 & !1, size.1 & !1) == (w, h)
            {
                converter.convert(capturer.latest(), None)?;
                fresh += 1;
            }
            let encode_started = Instant::now();
            // A keyframe every second of video, to see how big the cap lets them get.
            let keyframe = times.len().is_multiple_of(fps as usize);
            let encoded = encoder.encode(Input::Texture(converter.texture()), timestamp, keyframe)?;
            times.push(encode_started.elapsed());
            for frame in &encoded {
                if frame.keyframe { keyframe_sizes.push(frame.data.len()) } else { sizes.push(frame.data.len()) }
            }
            if first_keyframe.is_none() {
                first_keyframe = encoded.into_iter().find(|frame| frame.keyframe).map(|frame| frame.data);
            }
            timestamp += 1_000_000 / u64::from(fps);
        }
        let quality = match &first_keyframe {
            Some(data) => quality(data, &on_gpu.data()[..luma], w as usize).map_or_else(
                |e| format!("but its first keyframe didn't decode ({e:#})"),
                |db| format!("quality {db:.1} dB"),
            ),
            None => "but no keyframe came out".into(),
        };
        out(format!(
            "GPU-only pipeline: OK. {} frames in {} s ({fresh} new screen images): encode {}; {quality}",
            times.len(),
            RUN.as_secs(),
            timing(&times),
        ));
        let kb = |bytes: usize| format!("{:.1} KB", bytes as f64 / 1000.0);
        let largest = |sizes: &[usize]| sizes.iter().copied().max().map_or("–".into(), kb);
        out(format!(
            "  Frame sizes at {} Mb/s: average frame {} (target {}), largest {}; keyframes up to {}{}",
            BITRATE / 1_000_000,
            kb(sizes.iter().sum::<usize>() / sizes.len().max(1)),
            kb((BITRATE / 8 / fps) as usize),
            largest(&sizes),
            largest(&keyframe_sizes),
            encoder.burst().buffer_bytes.map_or(String::new(), |cap| format!(" (cap {})", kb(cap as usize))),
        ));

        // The memory path, as 0.5.0 always did it, for comparison.
        let mut memory = mf::HardwareEncoder::new(Codec::H264, w, h, BITRATE, fps, None)
            .context("hardware H.264 encoder reading memory")?;
        let mut times = Vec::new();
        for frame in 0..60 {
            let encode_started = Instant::now();
            memory.encode(Input::Picture(&on_cpu), frame * 16_667, frame == 0)?;
            times.push(encode_started.elapsed());
        }
        out(format!(
            "Memory path (as in 0.5.0): processor colour conversion {:.2} ms per frame, plus the copy from the GPU; \
             encode {}",
            cpu_convert.as_secs_f64() * 1000.0,
            timing(&times)
        ));

        // H.265, which sessions pick when the viewer decodes it too. Listed isn't enough: an
        // encoder can still refuse what a stream needs, and then the stream falls back to H.264.
        let h265 = |device: Option<&ID3D11Device>, fps: u32| -> Result<String> {
            let mut encoder = mf::HardwareEncoder::new(Codec::H265, w, h, BITRATE, fps, device)?;
            let controls = burst(encoder.burst());
            let (mut times, mut frames) = (Vec::new(), 0);
            for frame in 0..60u64 {
                let input =
                    if device.is_some() { Input::Texture(converter.texture()) } else { Input::Picture(&on_cpu) };
                let encode_started = Instant::now();
                frames += encoder.encode(input, frame * 1_000_000 / u64::from(fps), frame == 0)?.len();
                times.push(encode_started.elapsed());
            }
            Ok(format!("{}, encode {}, {frames} frames out of 60; {controls}", encoder.name(), timing(&times)))
        };
        for (how, device) in [("reading textures", Some(&device)), ("reading memory", None)] {
            match h265(device, fps) {
                Ok(line) => out(format!("Hardware H.265 encoding, {how}: OK. {line}")),
                Err(e) => {
                    out(format!("Hardware H.265 encoding, {how}: FAILED at {fps} Hz: {e:#}"));
                    if fps != 60 {
                        let at_60 = h265(device, 60).map_or_else(|e| format!("FAILED too: {e:#}"), |_| "OK".into());
                        out(format!("  At 60 Hz instead: {at_60}"));
                    }
                }
            }
        }
        Ok(())
    }

    /// The conversion's run time on the GPU over 60 conversions, from timestamp queries.
    fn gpu_time(
        converter: &mut Nv12Converter,
        image: &windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
    ) -> String {
        let mut times = Vec::new();
        for tag in 0..60 {
            if converter.convert(image, Some(tag)).is_err() {
                break;
            }
            converter.flush();
            std::thread::sleep(Duration::from_millis(2));
            converter.poll_timings(|_, us| times.push(us));
        }
        let waited = Instant::now();
        while times.len() < 50 && waited.elapsed() < Duration::from_millis(300) {
            converter.flush();
            std::thread::sleep(Duration::from_millis(5));
            converter.poll_timings(|_, us| times.push(us));
        }
        if times.is_empty() {
            return "GPU time not measurable with this driver".into();
        }
        times.sort_unstable();
        format!(
            "{:.3} ms on the GPU (median of {}, slowest {:.3})",
            f64::from(times[times.len() / 2]) / 1000.0,
            times.len(),
            f64::from(times[times.len() - 1]) / 1000.0
        )
    }

    /// The burst controls as the report shows them.
    fn burst(controls: mf::BurstControls) -> String {
        let refresh = controls
            .intra_refresh_frames
            .map_or("intra refresh not offered (keyframes every 30 s instead)".to_owned(), |frames| {
                format!("intra refresh over {frames} frames")
            });
        let cap = controls.buffer_bytes.map_or("no frame-size cap".to_owned(), |bytes| {
            format!("frame-size cap {:.1} KB ({} frames)", bytes as f64 / 1000.0, controls.buffer_frames.unwrap_or(0))
        });
        format!("{refresh}, {cap}")
    }

    /// Mean and largest absolute difference between two planes.
    fn difference(a: &[u8], b: &[u8]) -> (f64, u8) {
        let (sum, max) = a.iter().zip(b).fold((0u64, 0u8), |(sum, max), (x, y)| {
            let d = x.abs_diff(*y);
            (sum + u64::from(d), max.max(d))
        });
        (sum as f64 / a.len().max(1) as f64, max)
    }

    /// PSNR of the decoded keyframe's brightness against what went in.
    fn quality(keyframe: &[u8], luma: &[u8], width: usize) -> Result<f64> {
        let mut decoder = Decoder::new()?;
        let picture = decoder.decode(keyframe)?.context("no picture")?;
        let (w, h) = picture.dimensions();
        ensure!(w == width, "decoded {w}×{h}");
        let (stride, y) = (picture.strides().0, picture.y());
        let mut squared = 0f64;
        for row in 0..h {
            for (a, b) in y[row * stride..][..w].iter().zip(&luma[row * w..][..w]) {
                squared += f64::from(a.abs_diff(*b)).powi(2);
            }
        }
        let mse = squared / (w * h) as f64;
        Ok(if mse == 0.0 { 99.0 } else { 10.0 * (255.0f64.powi(2) / mse).log10() })
    }

    fn timing(times: &[Duration]) -> String {
        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
        let average = times.iter().map(|&d| ms(d)).sum::<f64>() / times.len().max(1) as f64;
        let slowest = times.iter().map(|&d| ms(d)).fold(0.0, f64::max);
        format!("{average:.2} ms per frame on average, {slowest:.2} ms at most")
    }
}
