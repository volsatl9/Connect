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
    match h265_test() {
        Ok(name) => {
            out(format!("Hardware H.265 decoding: OK on {name}: test streams decode bit-exactly as ffmpeg does"))
        }
        Err(e) => {
            out(format!("Hardware H.265 decoding: FAILED: {e:#}"));
            out("  Hosts send H.264 instead.".into());
        }
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
    let mut gpu = GpuDecoder::in_memory(connect_proto::Codec::H264)?;
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
    let difference =
        picture.iter().zip(&cpu).map(|(a, b)| u64::from(a.abs_diff(*b))).sum::<u64>() as f64 / picture.len() as f64;
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

/// x265 test streams (176x144, 30 frames) and the CRC-32 of each frame's luma as ffmpeg decodes
/// them, in display order: a low-delay stream like a host sends, and one with B frames that
/// works the reference handling harder.
const H265_TESTS: [(&str, &[u8], [u32; 30]); 2] = [
    (
        "low delay",
        include_bytes!("../../gpu-video/tests/h265/lowdelay.h265"),
        [
            2680801201, 2706340561, 3428723100, 3827177676, 4222864057, 3331503525, 126921905, 1635602348, 3408518273,
            3661659198, 1977283083, 4291790588, 3499091936, 785495038, 3907414605, 1712269329, 3400917616, 3260464851,
            1725827080, 757002816, 3633936468, 207661002, 4037602008, 2204165625, 2272130151, 4276119516, 4116371183,
            527357933, 2113062526, 1743109935,
        ],
    ),
    (
        "B frames",
        include_bytes!("../../gpu-video/tests/h265/bframes.h265"),
        [
            433723984, 2865580011, 1939495866, 2381803595, 259662080, 2190886234, 446092662, 2657781154, 1913791838,
            1626528673, 1255849615, 3251519201, 637944387, 3705700641, 91642415, 719838132, 3688468637, 2174629592,
            1193270157, 489434224, 3351316705, 3457744616, 3504589240, 682238175, 2023990787, 3336100267, 2289029523,
            1504116125, 3363090329, 3903031157,
        ],
    ),
];

/// Decodes the H.265 test streams on the graphics card and checks every frame.
fn h265_test() -> Result<String> {
    let mut name = String::new();
    for (label, stream, expected) in H265_TESTS {
        let mut gpu = GpuDecoder::in_memory(connect_proto::Codec::H265)?;
        name.clone_from(&gpu.name);
        let frames = gpu.decode_nv12(stream)?;
        ensure!(frames.len() == expected.len(), "{label}: decoded {} of {} frames", frames.len(), expected.len());
        for (index, (frame, crc)) in frames.iter().zip(expected).enumerate() {
            ensure!(frame.len() >= 176 * 144, "{label}: frame {index} too small");
            ensure!(crc32(&frame[..176 * 144]) == crc, "{label}: frame {index} differs from the reference decode");
        }
    }
    Ok(name)
}

/// CRC-32 (IEEE), as zlib computes it.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    #[test]
    fn crc32_matches_zlib() {
        assert_eq!(super::crc32(b"123456789"), 0xcbf4_3926);
    }
}
