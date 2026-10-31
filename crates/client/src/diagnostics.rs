//! `connect gpu-check`, viewing half: which graphics cards here decode video through Vulkan,
//! and a test stream decoded on the GPU and on the processor, compared for speed and picture.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use openh264::{
    OpenH264API,
    encoder::{BitRate, Encoder, EncoderConfig, FrameRate},
    formats::YUVBuffer,
};

use crate::{
    decoder::{self, gpu_decode_support},
    gpu_decode::GpuDecoder,
};

const WIDTH: usize = 1920;
const HEIGHT: usize = 1080;
const FRAMES: usize = 120;

/// Runs the viewing checks.
pub fn run(out: &mut dyn FnMut(String)) {
    out("== Viewing another PC ==".into());
    match gpu_decode_support() {
        Ok(gpus) if gpus.is_empty() => out("Vulkan Video: no graphics card here decodes video through Vulkan".into()),
        Ok(gpus) => {
            for gpu in gpus {
                let yes = |b| if b { "yes" } else { "no" };
                out(format!(
                    "Vulkan Video: {} ({}, driver {}): H.264 {}, H.265 {}",
                    gpu.name,
                    if gpu.discrete { "dedicated" } else { "integrated" },
                    gpu.driver,
                    yes(gpu.h264),
                    yes(gpu.h265),
                ));
            }
        }
        Err(e) => out(format!("Vulkan Video: not available ({e:#})")),
    }
    let offered = decoder::supported();
    out(format!(
        "Offered to hosts: {}",
        offered
            .iter()
            .map(|s| format!("{} ({})", s.codec, if s.hardware { "GPU" } else { "CPU" }))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    if let Err(e) = decode_test(out) {
        out(format!("Hardware H.264 decoding: FAILED: {e:#}"));
        out("  Viewing still works, decoding on the processor.".into());
    }
}

fn decode_test(out: &mut dyn FnMut(String)) -> Result<()> {
    let stream = test_stream()?;
    let mut software = decoder::software(connect_proto::Codec::H264)?;
    let (mut cpu, mut cpu_times) = (Vec::new(), Vec::new());
    for frame in &stream {
        let started = Instant::now();
        software.decode(frame, Some(&mut cpu))?;
        cpu_times.push(started.elapsed());
    }
    out(format!("Processor H.264 decoding (OpenH264): {WIDTH}×{HEIGHT}: {}", timing(&cpu_times)));
    let mut gpu = GpuDecoder::new()?;
    let (mut picture, mut gpu_times, mut decoded) = (Vec::new(), Vec::new(), 0);
    for frame in &stream {
        let started = Instant::now();
        if gpu.decode(frame, Some(&mut picture))?.is_some() {
            decoded += 1;
        }
        gpu_times.push(started.elapsed());
    }
    ensure!(decoded == stream.len(), "decoded {decoded} of {} frames", stream.len());
    ensure!(picture.len() == cpu.len(), "picture sizes differ");
    // Decoders must agree to the bit for H.264; allow a hair for rounding in the copy back.
    let difference = picture.iter().zip(&cpu).map(|(a, b)| u64::from(a.abs_diff(*b))).sum::<u64>() as f64
        / picture.len() as f64;
    out(format!(
        "Hardware H.264 decoding: {} on {}. {WIDTH}×{HEIGHT}: {} (including the copy back); difference from the \
         processor's picture {difference:.3}",
        if difference < 0.5 { "OK" } else { "MISMATCH" },
        gpu.name,
        timing(&gpu_times),
    ));
    Ok(())
}

/// A second of moving 1080p, encoded as a host would send it.
fn test_stream() -> Result<Vec<Vec<u8>>> {
    let config = EncoderConfig::new()
        .bitrate(BitRate::from_bps(20_000_000))
        .max_frame_rate(FrameRate::from_hz(60.0))
        .skip_frames(false);
    let mut encoder = Encoder::with_api_config(OpenH264API::from_source(), config).context("test encoder")?;
    let (cw, ch) = (WIDTH / 2, HEIGHT / 2);
    (0..FRAMES)
        .map(|n| {
            let mut yuv = vec![0u8; WIDTH * HEIGHT + 2 * cw * ch];
            let (y, chroma) = yuv.split_at_mut(WIDTH * HEIGHT);
            for (i, p) in y.iter_mut().enumerate() {
                let (x, row) = (i % WIDTH + n * 6, i / WIDTH);
                *p = (((x as f32 * 0.02).sin() + (row as f32 * 0.015 + n as f32 * 0.05).cos()) * 50.0 + 128.0) as u8;
            }
            for (i, p) in chroma.iter_mut().enumerate() {
                *p = (128 + ((i % cw + n) / 40 % 2) * 30) as u8;
            }
            Ok(encoder.encode(&YUVBuffer::from_vec(yuv, WIDTH, HEIGHT))?.to_vec())
        })
        .collect()
}

fn timing(times: &[Duration]) -> String {
    let ms = |d: &Duration| d.as_secs_f64() * 1000.0;
    // The first frame includes setting up; leave it out of the average.
    let steady = times.get(1..).unwrap_or(times);
    let average = steady.iter().map(ms).sum::<f64>() / steady.len().max(1) as f64;
    format!("{average:.2} ms per frame")
}
