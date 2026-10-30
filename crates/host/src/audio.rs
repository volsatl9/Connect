//! Windows default-output loopback; microphone devices are never opened.
// Capture is Windows-only; elsewhere only the tests use most of this.
#![cfg_attr(not(windows), allow(dead_code, unused_imports))]

use anyhow::{Context, Result, bail, ensure};
use bytes::Bytes;
use connect_proto::audio::{self, AudioPacket};
#[cfg(windows)]
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc as sync_mpsc,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

struct Packetizer {
    stream_id: u64,
    sequence: u32,
    first_frame: u64,
    sample_rate: u32,
    channels: u16,
    max_datagram: usize,
    frames_per_packet: usize,
}

impl Packetizer {
    fn new(stream_id: u64, sample_rate: u32, channels: u16, max_datagram: usize) -> Result<Self> {
        ensure!(
            (audio::MIN_SAMPLE_RATE..=audio::MAX_SAMPLE_RATE).contains(&sample_rate),
            "unsupported desktop audio sample rate"
        );
        // At most 5ms per packet even with an unusually large negotiated QUIC datagram.
        let frames_per_packet = audio::max_frames_per_packet(max_datagram, channels)?.min((sample_rate / 200) as usize);
        Ok(Self { stream_id, sequence: 0, first_frame: 0, sample_rate, channels, max_datagram, frames_per_packet })
    }

    fn push<T: cpal::SizedSample>(&mut self, data: &[T], output: &mpsc::Sender<Result<Bytes>>, stopped: &AtomicBool)
    where
        f32: cpal::FromSample<T>,
    {
        let channels = usize::from(self.channels);
        let aligned = data.len() / channels * channels;
        for chunk in data[..aligned].chunks(self.frames_per_packet * channels) {
            if stopped.load(Ordering::Acquire) {
                return;
            }
            let packet = AudioPacket {
                stream_id: self.stream_id,
                sequence: self.sequence,
                first_frame: self.first_frame,
                sample_rate: self.sample_rate,
                channels: self.channels,
                samples: chunk
                    .iter()
                    .map(|&sample| {
                        let value = <f32 as cpal::FromSample<T>>::from_sample_(sample);
                        if value.is_finite() { value } else { 0.0 }
                    })
                    .collect(),
            };
            self.sequence = self.sequence.wrapping_add(1);
            let Some(next_frame) = self.first_frame.checked_add((chunk.len() / channels) as u64) else {
                let _ = output.try_send(Err(anyhow::anyhow!("desktop audio timeline exhausted")));
                stopped.store(true, Ordering::Release);
                return;
            };
            self.first_frame = next_frame;
            // A full queue drops PCM, preserving sequence/timeline gaps for client silence.
            let encoded = audio::encode(&packet, self.max_datagram);
            if stopped.load(Ordering::Acquire) {
                return;
            }
            if let Err(mpsc::error::TrySendError::Closed(_)) = output.try_send(encoded) {
                stopped.store(true, Ordering::Release);
                return;
            }
        }
    }
}

/// Send-safe receiver. All device objects and their teardown stay on one native thread.
pub(crate) struct AudioStream {
    receiver: mpsc::Receiver<Result<Bytes>>,
    stopped: Arc<AtomicBool>,
    stop: sync_mpsc::Sender<()>,
    worker: Option<JoinHandle<()>>,
}

impl AudioStream {
    #[cfg(windows)]
    pub(crate) fn start(max_datagram: usize) -> Result<Self> {
        let stopped = Arc::new(AtomicBool::new(false));
        let worker_stopped = stopped.clone();
        let (stop, stopping) = sync_mpsc::channel();
        let (ready, started) = sync_mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("connect-desktop-loopback".into())
            .spawn(move || {
                let (stream, receiver) = match open_loopback(max_datagram, worker_stopped.clone()) {
                    Ok(stream) => stream,
                    Err(error) => {
                        let _ = ready.send(Err(error));
                        return;
                    }
                };
                if ready.send(Ok(receiver)).is_err() {
                    return;
                }
                while !worker_stopped.load(Ordering::Acquire) {
                    match stopping.recv_timeout(Duration::from_millis(20)) {
                        Ok(()) | Err(sync_mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(sync_mpsc::RecvTimeoutError::Timeout) => {}
                    }
                }
                worker_stopped.store(true, Ordering::Release);
                drop(stream);
            })
            .context("start desktop audio capture thread")?;
        match started.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(receiver)) => Ok(Self { receiver, stopped, stop, worker: Some(worker) }),
            Ok(Err(error)) => {
                let _ = worker.join();
                Err(error)
            }
            Err(error) => {
                stopped.store(true, Ordering::Release);
                let _ = stop.send(());
                bail!("desktop audio startup timed out: {error}")
            }
        }
    }

    #[cfg(not(windows))]
    pub(crate) fn start(_max_datagram: usize) -> Result<Self> {
        bail!("desktop audio capture is only supported on Windows")
    }

    pub(crate) async fn next(&mut self) -> Option<Result<Bytes>> {
        self.receiver.recv().await
    }
}

impl Drop for AudioStream {
    fn drop(&mut self) {
        // Revoke the callback before waiting for device teardown; no more PCM is queued.
        self.stopped.store(true, Ordering::Release);
        self.receiver.close();
        let _ = self.stop.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(windows)]
fn next_stream_id() -> u64 {
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now =
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_micros().min(u128::from(u64::MAX)) as u64;
    let mut previous = LAST.load(Ordering::Relaxed);
    loop {
        let next = now.max(previous.saturating_add(1));
        match LAST.compare_exchange_weak(previous, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return next,
            Err(actual) => previous = actual,
        }
    }
}

#[cfg(windows)]
fn input_stream<T: cpal::SizedSample>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut packetizer: Packetizer,
    output: mpsc::Sender<Result<Bytes>>,
    stopped: Arc<AtomicBool>,
) -> Result<cpal::Stream>
where
    f32: cpal::FromSample<T>,
{
    let error_output = output.clone();
    let error_stopped = stopped.clone();
    Ok(device.build_input_stream(
        config,
        move |data: &[T], _| packetizer.push(data, &output, &stopped),
        move |error| {
            tracing::warn!(%error, "desktop audio capture failed");
            let _ = error_output.try_send(Err(anyhow::anyhow!("desktop audio device error: {error}")));
            error_stopped.store(true, Ordering::Release);
        },
        None,
    )?)
}

#[cfg(windows)]
fn open_loopback(
    max_datagram: usize,
    stopped: Arc<AtomicBool>,
) -> Result<(cpal::Stream, mpsc::Receiver<Result<Bytes>>)> {
    // CPAL's WASAPI backend sets AUDCLNT_STREAMFLAGS_LOOPBACK when an OUTPUT device
    // is passed to build_input_stream. Never use default_input_device or input_devices.
    let device = cpal::default_host().default_output_device().context("no default desktop audio output device")?;
    let supported = device.default_output_config().context("desktop audio mix format")?;
    let preferred = cpal::StreamConfig { channels: 2, sample_rate: 48_000, buffer_size: cpal::BufferSize::Default };
    // CPAL WASAPI enables AUTOCONVERTPCM + SRC_DEFAULT_QUALITY: Windows performs
    // rate conversion AND channel matrixing (including center/surround downmix).
    // Stereo float PCM caps the normal payload at 3.072Mbps without a lossy codec.
    match start_loopback(&device, &preferred, cpal::SampleFormat::F32, max_datagram, stopped.clone()) {
        Ok(stream) => {
            tracing::info!(
                mix_rate = supported.sample_rate(),
                mix_channels = supported.channels(),
                sample_rate = 48000,
                channels = 2,
                "desktop loopback ready: 48kHz stereo PCM, Windows audio engine converts the desktop mix"
            );
            return Ok(stream);
        }
        Err(error) => {
            tracing::warn!(%error, "48kHz stereo loopback unavailable; trying native desktop mix format")
        }
    }
    let config: cpal::StreamConfig = supported.clone().into();
    let stream = start_loopback(&device, &config, supported.sample_format(), max_datagram, stopped)?;
    tracing::info!(
        sample_rate = config.sample_rate,
        channels = config.channels,
        "desktop loopback ready: native PCM fallback (bandwidth depends on device mix format)"
    );
    Ok(stream)
}

#[cfg(windows)]
fn start_loopback(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    format: cpal::SampleFormat,
    max_datagram: usize,
    stopped: Arc<AtomicBool>,
) -> Result<(cpal::Stream, mpsc::Receiver<Result<Bytes>>)> {
    let (stream, receiver) = build_loopback(device, config, format, max_datagram, stopped.clone())?;
    if stopped.load(Ordering::Acquire) {
        bail!("desktop audio capture cancelled");
    }
    stream.play().context("start desktop output loopback")?;
    Ok((stream, receiver))
}

#[cfg(windows)]
fn build_loopback(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    format: cpal::SampleFormat,
    max_datagram: usize,
    stopped: Arc<AtomicBool>,
) -> Result<(cpal::Stream, mpsc::Receiver<Result<Bytes>>)> {
    let packetizer = Packetizer::new(next_stream_id(), config.sample_rate, config.channels, max_datagram)?;
    let queue_capacity = ((config.sample_rate as usize / 25).div_ceil(packetizer.frames_per_packet)).clamp(2, 8192);
    let (output, receiver) = mpsc::channel(queue_capacity);
    macro_rules! build {
        ($sample:ty) => {
            input_stream::<$sample>(&device, &config, packetizer, output, stopped.clone())?
        };
    }
    let stream = match format {
        cpal::SampleFormat::F32 => build!(f32),
        cpal::SampleFormat::F64 => build!(f64),
        cpal::SampleFormat::I8 => build!(i8),
        cpal::SampleFormat::I16 => build!(i16),
        cpal::SampleFormat::I24 => build!(cpal::I24),
        cpal::SampleFormat::I32 => build!(i32),
        cpal::SampleFormat::I64 => build!(i64),
        cpal::SampleFormat::U8 => build!(u8),
        cpal::SampleFormat::U16 => build!(u16),
        cpal::SampleFormat::U32 => build!(u32),
        cpal::SampleFormat::U64 => build!(u64),
        format => bail!("unsupported desktop audio mix format {format}"),
    };
    Ok((stream, receiver))
}

#[cfg(test)]
mod tests {
    use super::*;
    use connect_proto::audio;

    #[test]
    fn chunks_capture_to_negotiated_size_without_losing_pcm_or_timeline() {
        let mut packetizer = Packetizer::new(91, 48_000, 2, 1200).unwrap();
        let (tx, mut rx) = mpsc::channel(16);
        let samples: Vec<_> = (0..960).map(|x| x as f32 / 960.0).collect();
        packetizer.push(&samples, &tx, &AtomicBool::new(false));
        let mut combined = vec![];
        let mut first_frame = 0;
        let mut sequence = 0;
        while let Ok(bytes) = rx.try_recv() {
            let bytes = bytes.unwrap();
            assert!(bytes.len() <= 1200);
            let p = audio::decode(&bytes).unwrap();
            assert_eq!(p.stream_id, 91);
            assert_eq!(p.sequence, sequence);
            assert_eq!(p.first_frame, first_frame);
            first_frame += (p.samples.len() / 2) as u64;
            sequence += 1;
            combined.extend(p.samples);
        }
        assert_eq!(first_frame, 480);
        assert_eq!(combined, samples);
    }

    #[test]
    fn queue_overflow_leaves_exact_timeline_gap_and_revocation_sends_nothing() {
        let mut packetizer = Packetizer::new(91, 8000, 1, 1200).unwrap();
        let (tx, mut rx) = mpsc::channel(1);
        let stopped = AtomicBool::new(false);
        packetizer.push(&[0.5f32; 160], &tx, &stopped);
        assert_eq!(audio::decode(&rx.try_recv().unwrap().unwrap()).unwrap().first_frame, 0);
        packetizer.push(&[0.75f32; 40], &tx, &stopped);
        let p = audio::decode(&rx.try_recv().unwrap().unwrap()).unwrap();
        assert_eq!(p.first_frame, 160);
        assert_eq!(p.sequence, 4);
        stopped.store(true, Ordering::Release);
        packetizer.push(&[1.0f32; 160], &tx, &stopped);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn converts_integer_source_and_silences_nonfinite_device_samples() {
        let mut packetizer = Packetizer::new(91, 8000, 1, 1200).unwrap();
        let (tx, mut rx) = mpsc::channel(4);
        packetizer.push(&[i16::MIN, 0, i16::MAX], &tx, &AtomicBool::new(false));
        let p = audio::decode(&rx.try_recv().unwrap().unwrap()).unwrap();
        assert_eq!(p.samples[0], -1.0);
        assert_eq!(p.samples[1], 0.0);
        assert!((p.samples[2] - 0.9999695).abs() < 1e-7);
        packetizer.push(&[f32::NAN, f32::INFINITY, 0.5], &tx, &AtomicBool::new(false));
        let p = audio::decode(&rx.try_recv().unwrap().unwrap()).unwrap();
        assert_eq!(p.samples, [0.0, 0.0, 0.5]);
    }

    #[cfg(windows)]
    #[test]
    fn capture_epochs_increase_across_restarts() {
        fn send<T: Send>() {}
        send::<AudioStream>();
        assert!(next_stream_id() < next_stream_id());
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "opens a paused loopback endpoint without starting capture"]
    fn native_output_loopback_builds_and_drops_without_capturing() {
        let device = cpal::default_host().default_output_device().expect("default output");
        let supported = device.default_output_config().unwrap();
        let native: cpal::StreamConfig = supported.clone().into();
        let preferred = cpal::StreamConfig { channels: 2, sample_rate: 48000, buffer_size: cpal::BufferSize::Default };
        for (config, format) in [(&preferred, cpal::SampleFormat::F32), (&native, supported.sample_format())] {
            let (stream, mut rx) =
                build_loopback(&device, config, format, 1200, Arc::new(AtomicBool::new(true))).unwrap();
            // Never play; the revoked callback additionally forbids PCM processing.
            thread::sleep(Duration::from_millis(50));
            assert!(rx.try_recv().is_err());
            drop(stream);
            eprintln!(
                "Native paused loopback endpoint: {}Hz, {} channels, {}. No capture started.",
                config.sample_rate, config.channels, format
            );
        }
    }

    #[cfg(windows)]
    fn quiet_test_tone<T: cpal::SizedSample + cpal::FromSample<f32>>(
        device: &cpal::Device,
        config: &cpal::StreamConfig,
        failed: Arc<AtomicBool>,
    ) -> cpal::Stream {
        let rate = config.sample_rate;
        let channels = usize::from(config.channels);
        let center_only = channels >= 3;
        let mut position = 0u64;
        device
            .build_output_stream(
                config,
                move |out: &mut [T], _| {
                    for frame in out.chunks_exact_mut(channels) {
                        frame.fill(T::from_sample(0.0));
                        // -60dBFS, with a short fade and an automatic 800ms stop even if
                        // the test executor stalls. No captured PCM is ever played back.
                        let remaining = u64::from(rate) * 4 / 5;
                        if position < remaining {
                            let fade = (f64::from(rate) / 200.0).max(1.0);
                            let envelope = ((position.min(remaining - position)) as f64 / fade).min(1.0);
                            let value = (0.001
                                * envelope
                                * (std::f64::consts::TAU * 997.0 * position as f64 / f64::from(rate)).sin())
                                as f32;
                            if center_only {
                                frame[2] = T::from_sample(value);
                            } else {
                                for sample in frame {
                                    *sample = T::from_sample(value);
                                }
                            }
                        }
                        position = position.saturating_add(1);
                    }
                },
                move |_| {
                    failed.store(true, Ordering::Release);
                },
                None,
            )
            .expect("quiet tone output stream builds")
    }

    #[cfg(windows)]
    #[tokio::test]
    #[ignore = "briefly emits a very quiet local test tone; collects only in-memory audio aggregates"]
    async fn native_loopback_receives_quiet_center_tone_and_reopens() {
        let device = cpal::default_host().default_output_device().expect("default output");
        let supported = device.default_output_config().expect("native output format");
        let config: cpal::StreamConfig = supported.clone().into();
        let output_failed = Arc::new(AtomicBool::new(false));
        macro_rules! tone {
            ($sample:ty) => {
                quiet_test_tone::<$sample>(&device, &config, output_failed.clone())
            };
        }
        let output = match supported.sample_format() {
            cpal::SampleFormat::F32 => tone!(f32),
            cpal::SampleFormat::F64 => tone!(f64),
            cpal::SampleFormat::I16 => tone!(i16),
            cpal::SampleFormat::I24 => tone!(cpal::I24),
            cpal::SampleFormat::I32 => tone!(i32),
            cpal::SampleFormat::I64 => tone!(i64),
            cpal::SampleFormat::U8 => tone!(u8),
            format => panic!("unexpected native WASAPI format {format}"),
        };
        output.play().expect("quiet tone output starts");
        // RAII releases both streams on any assertion/panic; raw PCM never leaves
        // this process or reaches disk. Only aggregate diagnostics are printed.
        let mut capture = AudioStream::start(1200).expect("actual loopback starts");
        let deadline = tokio::time::Instant::now() + Duration::from_millis(900);
        let mut previous: Option<(u32, u64)> = None;
        let mut stream_id = None;
        let mut packets = 0u64;
        let mut frames = 0u64;
        let mut missing_frames = 0u64;
        let mut energy = [0.0f64; 2];
        let mut sine = [0.0f64; 2];
        let mut cosine = [0.0f64; 2];
        while frames < 12_000 {
            let bytes = tokio::time::timeout_at(deadline, capture.next())
                .await
                .expect("loopback PCM arrives within 900ms")
                .expect("loopback stays open")
                .expect("loopback device stays healthy");
            assert!(bytes.len() <= 1200);
            let packet = audio::decode(&bytes).expect("captured datagram validates");
            assert_eq!((packet.sample_rate, packet.channels), (48_000, 2), "preferred stereo mix is active");
            assert_eq!(*stream_id.get_or_insert(packet.stream_id), packet.stream_id);
            let packet_frames = (packet.samples.len() / 2) as u64;
            if let Some((sequence, end)) = previous {
                let advance = packet.sequence.wrapping_sub(sequence);
                assert!(advance > 0 && advance <= i32::MAX as u32, "sequence advances");
                assert!(packet.first_frame >= end, "capture timeline never overlaps or reverses");
                let gap = packet.first_frame - end;
                assert_eq!(gap > 0, advance > 1, "dropped packets expose timeline gaps");
                missing_frames += gap;
            }
            previous = Some((packet.sequence, packet.first_frame + packet_frames));
            for (offset, frame) in packet.samples.as_chunks::<2>().0.iter().enumerate() {
                let phase = std::f64::consts::TAU * 997.0 * (packet.first_frame + offset as u64) as f64 / 48_000.0;
                for channel in 0..2 {
                    let sample = f64::from(frame[channel]);
                    energy[channel] += sample * sample;
                    sine[channel] += sample * phase.sin();
                    cosine[channel] += sample * phase.cos();
                }
            }
            frames += packet_frames;
            packets += 1;
        }
        let rms = energy.map(|sum| (sum / frames as f64).sqrt());
        let tone_amplitude =
            std::array::from_fn::<_, 2, _>(|channel| 2.0 * sine[channel].hypot(cosine[channel]) / frames as f64);
        eprintln!(
            "Quiet tone probe: native {}Hz/{}ch ({}); capture 48000Hz/2ch, {packets} packets, {frames} frames, {missing_frames} missing frames; RMS={rms:?}, 997Hz amplitude={tone_amplitude:?}.",
            config.sample_rate,
            config.channels,
            if config.channels >= 3 { "center only" } else { "stereo/mono" }
        );
        assert!(!output_failed.load(Ordering::Acquire), "tone output stayed healthy");
        assert!(
            tone_amplitude.iter().all(|&amplitude| amplitude > 0.000_001),
            "quiet center tone reaches both stereo channels through Windows downmix"
        );
        assert!(rms.iter().all(|&value| value > 0.000_001));
        assert!(missing_frames < frames / 2, "capture cadence stays usable");
        let first_epoch = stream_id.expect("captured stream epoch");
        drop(capture);
        let mut restarted = AudioStream::start(1200).expect("capture reopens after drop");
        let bytes = tokio::time::timeout(Duration::from_millis(400), restarted.next())
            .await
            .expect("reopened loopback produces PCM promptly")
            .expect("reopened stream remains open")
            .expect("reopened loopback stays healthy");
        let packet = audio::decode(&bytes).unwrap();
        assert_eq!((packet.sample_rate, packet.channels), (48_000, 2));
        assert!(packet.stream_id > first_epoch, "reopened capture has a fresh epoch");
        drop(restarted);
        drop(output);
        eprintln!("Actual loopback stopped and reopened successfully; all native audio streams released.");
    }
}
