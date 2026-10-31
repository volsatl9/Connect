//! Bounded desktop audio playback: Opus decoded as it arrives, then played from a short
//! reordering buffer. The CPAL stream stays on its owning thread.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use connect_proto::audio::{self, AudioPacket, Payload};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

const TARGET_BUFFER_MS: u64 = 20;
const MAX_BUFFER_MS: u64 = 80;
const MAX_QUEUED_PACKETS: usize = 512;
/// Longest run of lost Opus packets filled in with Opus's concealment rather than silence.
const MAX_CONCEALED_MS: u64 = 20;

/// Decoded audio: interleaved frames in the source device's channel order.
#[derive(Clone, Debug, PartialEq)]
struct Pcm {
    stream_id: u64,
    sequence: u32,
    first_frame: u64,
    sample_rate: u32,
    channels: u16,
    samples: Vec<f32>,
}

/// Opus decoding for one capture stream from the host.
struct OpusStream {
    stream_id: u64,
    sample_rate: u32,
    channels: u16,
    decoder: opus::Decoder,
    /// The frame after the last one decoded.
    next_frame: u64,
}

/// Turns datagrams into [`Pcm`], decoding Opus in arrival order.
#[derive(Default)]
struct Depacketizer {
    opus: Option<OpusStream>,
}

impl Depacketizer {
    /// The audio in `packet`, or `None` if it should be dropped (late, or from an older stream).
    fn decode(&mut self, packet: AudioPacket) -> Result<Option<Pcm>> {
        let AudioPacket { stream_id, sequence, first_frame, sample_rate, channels, payload } = packet;
        let pcm = |samples| Pcm { stream_id, sequence, first_frame, sample_rate, channels, samples };
        let (frames, data) = match payload {
            Payload::Pcm(samples) => return Ok(Some(pcm(samples))),
            Payload::Opus { frames, data } => (u64::from(frames), data),
        };
        let current = self.opus.as_ref().map(|s| (s.stream_id, s.sample_rate, s.channels));
        if current.is_some_and(|(id, ..)| stream_id < id) {
            return Ok(None);
        }
        if current != Some((stream_id, sample_rate, channels)) {
            let layout = if channels == 1 { opus::Channels::Mono } else { opus::Channels::Stereo };
            let decoder = opus::Decoder::new(sample_rate, layout).context("starting the Opus decoder")?;
            self.opus = Some(OpusStream { stream_id, sample_rate, channels, decoder, next_frame: first_frame });
        }
        let stream = self.opus.as_mut().expect("set above");
        if first_frame < stream.next_frame {
            // Already concealed, or a duplicate: Opus decodes strictly in order.
            return Ok(None);
        }
        let width = usize::from(channels);
        let gap = first_frame - stream.next_frame;
        // A short loss sounds far better concealed than as a dropout; a long one is silence.
        let concealed = if gap <= u64::from(sample_rate) * MAX_CONCEALED_MS / 1000 { gap } else { 0 };
        let mut samples = vec![0.0; (concealed + frames) as usize * width];
        let (lost, current) = samples.split_at_mut(concealed as usize * width);
        // Concealment comes in whole 2.5ms steps, the shortest Opus frame; 5ms packets fit.
        let step = (sample_rate / 400) as usize * width;
        for chunk in lost.chunks_mut(step) {
            stream.decoder.decode_float(&[], chunk, false).context("concealing lost audio")?;
        }
        let decoded = stream.decoder.decode_float(&data, current, false).context("decoding Opus audio")?;
        ensure!(decoded as u64 == frames, "Opus packet length does not match header");
        stream.next_frame = first_frame + frames;
        Ok(Some(Pcm { first_frame: first_frame - concealed, ..pcm(samples) }))
    }
}

#[derive(Default)]
struct PlaybackBuffer {
    packets: BTreeMap<u64, Pcm>,
    stream_id: Option<u64>,
    sample_rate: u32,
    channels: u16,
    highest_sequence: Option<u32>,
    recent_sequences: VecDeque<u32>,
    cursor: Option<u64>,
    consumed_until: Option<u64>,
    fraction: f64,
    primed: bool,
    muted: bool,
}

impl PlaybackBuffer {
    fn push(&mut self, packet: Pcm) -> Result<()> {
        if self.stream_id.is_some_and(|id| packet.stream_id < id) {
            return Ok(());
        }
        if self.stream_id != Some(packet.stream_id) {
            let muted = self.muted;
            *self = Self {
                muted,
                stream_id: Some(packet.stream_id),
                sample_rate: packet.sample_rate,
                channels: packet.channels,
                ..Self::default()
            };
        }
        ensure!(
            packet.sample_rate == self.sample_rate && packet.channels == self.channels,
            "audio format changed within capture epoch"
        );
        if self.recent_sequences.contains(&packet.sequence) {
            return Ok(());
        }
        if let Some(highest) = self.highest_sequence {
            let delta = packet.sequence.wrapping_sub(highest) as i32;
            if delta <= -(MAX_QUEUED_PACKETS as i32) {
                return Ok(());
            }
            if delta > 0 {
                self.highest_sequence = Some(packet.sequence);
            }
        } else {
            self.highest_sequence = Some(packet.sequence);
        }
        self.recent_sequences.push_back(packet.sequence);
        if self.recent_sequences.len() > MAX_QUEUED_PACKETS {
            self.recent_sequences.pop_front();
        }
        if self.muted {
            return Ok(());
        }
        let end = packet.first_frame + (packet.samples.len() / usize::from(packet.channels)) as u64;
        if self.consumed_until.is_some_and(|played| end <= played) || self.packets.contains_key(&packet.first_frame) {
            return Ok(());
        }
        if let Some((_, previous)) = self.packets.range(..packet.first_frame).next_back() {
            ensure!(
                previous.first_frame + (previous.samples.len() / usize::from(previous.channels)) as u64
                    <= packet.first_frame,
                "overlapping audio packets"
            );
        }
        if let Some((&next, _)) = self.packets.range(packet.first_frame..).next() {
            ensure!(end <= next, "overlapping audio packets");
        }
        self.packets.insert(packet.first_frame, packet);
        let first = *self.packets.first_key_value().unwrap().0;
        let last = self.newest_end();
        if last - first > u64::from(self.sample_rate) * MAX_BUFFER_MS / 1000 || self.packets.len() > MAX_QUEUED_PACKETS
        {
            let cutoff = last.saturating_sub(u64::from(self.sample_rate) * TARGET_BUFFER_MS / 1000);
            self.cursor = Some(self.cursor.unwrap_or(first).max(cutoff));
            self.fraction = 0.0;
            self.discard_played();
        } else if !self.primed && self.consumed_until.is_none() {
            self.cursor = Some(first); // startup packets may arrive out of order
        }
        while self.packets.len() > MAX_QUEUED_PACKETS {
            let (_, removed) = self.packets.pop_first().unwrap();
            let end = removed.first_frame + (removed.samples.len() / usize::from(removed.channels)) as u64;
            self.cursor = Some(self.cursor.unwrap_or(end).max(end));
            self.fraction = 0.0;
        }
        Ok(())
    }

    fn newest_end(&self) -> u64 {
        self.packets
            .last_key_value()
            .map_or(0, |(_, packet)| packet.first_frame + (packet.samples.len() / usize::from(packet.channels)) as u64)
    }

    fn discard_played(&mut self) {
        let Some(cursor) = self.cursor else {
            return;
        };
        while self
            .packets
            .first_key_value()
            .is_some_and(|(_, p)| p.first_frame + (p.samples.len() / usize::from(p.channels)) as u64 <= cursor)
        {
            self.packets.pop_first();
        }
    }

    fn set_muted(&mut self, muted: bool) {
        if muted != self.muted {
            self.packets.clear();
            self.cursor = None;
            self.consumed_until = None;
            self.fraction = 0.0;
            self.primed = false;
            self.muted = muted;
        }
    }

    fn sample(&self, frame: u64, channel: usize) -> f32 {
        let Some((_, packet)) = self.packets.range(..=frame).next_back() else {
            return 0.0;
        };
        let offset = (frame - packet.first_frame) as usize;
        packet
            .samples
            .get(offset.saturating_mul(usize::from(packet.channels)).saturating_add(channel))
            .copied()
            .unwrap_or(0.0)
    }

    fn mapped_sample(&self, frame: u64, channel: usize, output_channels: usize) -> f64 {
        let source_channels = usize::from(self.channels);
        if source_channels == output_channels {
            return f64::from(self.sample(frame, channel));
        }
        if output_channels == 1 {
            return (0..source_channels).map(|c| f64::from(self.sample(frame, c))).sum::<f64>()
                / source_channels as f64;
        }
        if source_channels == 1 {
            return f64::from(self.sample(frame, 0));
        }
        if output_channels == 2 && source_channels > 2 {
            // Conventional Windows/ALSA order: L, R, center, LFE, rear/side L, R.
            // Attenuate the downmix to leave headroom; LFE is omitted in 5.1/7.1.
            let mut value = f64::from(self.sample(frame, channel));
            let mut weight = 1.0;
            if source_channels >= 3 {
                value += f64::from(self.sample(frame, 2)) * 0.707;
                weight += 0.707;
            }
            let surround_start = if source_channels >= 6 { 4 } else { 3 };
            for c in surround_start..source_channels {
                if (c - surround_start) % 2 == channel {
                    value += f64::from(self.sample(frame, c)) * 0.5;
                    weight += 0.5;
                }
            }
            return value / weight;
        }
        if channel < source_channels { f64::from(self.sample(frame, channel)) } else { 0.0 }
    }

    fn render<T: cpal::SizedSample + cpal::FromSample<f32>>(&mut self, out: &mut [T], rate: u32, channels: usize) {
        out.fill(T::from_sample(0.0));
        if self.muted || self.packets.is_empty() || rate == 0 || channels == 0 {
            return;
        }
        let mut cursor = self.cursor.unwrap_or(*self.packets.first_key_value().unwrap().0);
        let end = self.newest_end();
        if !self.primed {
            if end.saturating_sub(cursor) < u64::from(self.sample_rate) * TARGET_BUFFER_MS / 1000
                && self.packets.len() < MAX_QUEUED_PACKETS
            {
                return;
            }
            self.primed = true;
        }
        let step = f64::from(self.sample_rate) / f64::from(rate);
        for frame in out.chunks_exact_mut(channels) {
            if cursor >= end {
                self.primed = false;
                break;
            }
            for (channel, sample) in frame.iter_mut().enumerate() {
                let a = self.mapped_sample(cursor, channel, channels);
                let b = self.mapped_sample(cursor.saturating_add(1), channel, channels);
                // f64 intermediates prevent overflow even for finite extreme wire PCM.
                let value = a + (b - a) * self.fraction;
                *sample = T::from_sample(value.clamp(-1.0, 1.0) as f32);
            }
            self.fraction += step;
            let whole = self.fraction.floor();
            cursor = cursor.saturating_add(whole as u64);
            self.fraction -= whole;
        }
        self.cursor = Some(cursor);
        self.consumed_until = Some(cursor);
        self.discard_played();
        if self.packets.is_empty() {
            self.primed = false;
        }
    }
}

/// Send-safe handle; dropping it closes the output device on its owning thread.
pub(crate) struct Playback {
    depacketizer: Mutex<Depacketizer>,
    buffer: Arc<Mutex<PlaybackBuffer>>,
    failure: Arc<Mutex<Option<String>>>,
    stop: mpsc::Sender<()>,
    worker: Option<JoinHandle<()>>,
}

impl Playback {
    pub(crate) fn start() -> Result<Self> {
        let buffer = Arc::new(Mutex::new(PlaybackBuffer::default()));
        let failure = Arc::new(Mutex::new(None));
        let (stop, stopping) = mpsc::channel();
        let (ready, started) = mpsc::sync_channel(1);
        let worker_buffer = buffer.clone();
        let worker_failure = failure.clone();
        let worker = thread::Builder::new()
            .name("connect-audio-output".into())
            .spawn(move || {
                let stream = match open_output(worker_buffer, worker_failure.clone()) {
                    Ok(stream) => stream,
                    Err(error) => {
                        let _ = ready.send(Err(error));
                        return;
                    }
                };
                if ready.send(Ok(())).is_err() {
                    return;
                }
                loop {
                    match stopping.recv_timeout(Duration::from_millis(50)) {
                        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    if worker_failure.lock().is_ok_and(|error| error.is_some()) {
                        break;
                    }
                }
                drop(stream);
            })
            .context("start audio playback thread")?;
        match started.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Self { depacketizer: Default::default(), buffer, failure, stop, worker: Some(worker) }),
            Ok(Err(error)) => {
                let _ = worker.join();
                Err(error)
            }
            Err(error) => {
                let _ = stop.send(());
                bail!("audio output startup timed out: {error}")
            }
        }
    }

    pub(crate) fn push_datagram(&self, data: &[u8]) -> Result<()> {
        if let Some(error) = self.failure.lock().map_err(|_| anyhow::anyhow!("audio failure lock poisoned"))?.as_ref() {
            bail!("audio output unavailable: {error}");
        }
        let packet = audio::decode(data)?;
        let decoded =
            self.depacketizer.lock().map_err(|_| anyhow::anyhow!("audio decoder lock poisoned"))?.decode(packet)?;
        let Some(pcm) = decoded else { return Ok(()) };
        self.buffer.lock().map_err(|_| anyhow::anyhow!("audio playback lock poisoned"))?.push(pcm)
    }

    pub(crate) fn set_muted(&self, muted: bool) {
        if let Ok(mut buffer) = self.buffer.lock() {
            buffer.set_muted(muted);
        }
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn output_stream<T: cpal::SizedSample + cpal::FromSample<f32>>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    buffer: Arc<Mutex<PlaybackBuffer>>,
    failure: Arc<Mutex<Option<String>>>,
) -> Result<cpal::Stream> {
    let rate = config.sample_rate;
    let channels = usize::from(config.channels);
    Ok(device.build_output_stream(
        config,
        move |out: &mut [T], _| {
            // Never block a device callback behind datagram parsing or UI mute changes.
            match buffer.try_lock() {
                Ok(mut buffer) => buffer.render(out, rate, channels),
                Err(_) => out.fill(T::from_sample(0.0)),
            }
        },
        move |error| {
            tracing::warn!(%error, "desktop audio playback failed");
            if let Ok(mut failure) = failure.lock() {
                *failure = Some(error.to_string());
            }
        },
        None,
    )?)
}

fn open_output(buffer: Arc<Mutex<PlaybackBuffer>>, failure: Arc<Mutex<Option<String>>>) -> Result<cpal::Stream> {
    let device = cpal::default_host().default_output_device().context("no default audio output device")?;
    let supported = device.default_output_config().context("default audio output format")?;
    let config: cpal::StreamConfig = supported.clone().into();
    ensure!(
        config.channels > 0 && config.channels <= 32 && config.sample_rate > 0,
        "invalid audio output configuration"
    );
    macro_rules! build {
        ($sample:ty) => {
            output_stream::<$sample>(&device, &config, buffer, failure)?
        };
    }
    let stream = match supported.sample_format() {
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
        format => bail!("unsupported audio output format {format}"),
    };
    stream.play().context("start audio output device")?;
    tracing::info!(sample_rate = config.sample_rate, channels = config.channels, "desktop audio output ready");
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(sequence: u32, first_frame: u64, value: f32) -> Pcm {
        Pcm { stream_id: 1, sequence, first_frame, sample_rate: 8000, channels: 1, samples: vec![value; 160] }
    }

    /// 5ms of a 1kHz tone at 48kHz stereo as host-style Opus packets, `count` of them.
    fn opus_packets(count: u64) -> Vec<AudioPacket> {
        let mut encoder = opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::LowDelay).unwrap();
        (0..count)
            .map(|n| {
                let tone: Vec<f32> = (n * 240..(n + 1) * 240)
                    .flat_map(|i| [(std::f32::consts::TAU * 1000.0 * i as f32 / 48_000.0).sin() * 0.5; 2])
                    .collect();
                let data = encoder.encode_vec_float(&tone, audio::MAX_OPUS_LEN).unwrap();
                AudioPacket {
                    stream_id: 3,
                    sequence: n as u32,
                    first_frame: n * 240,
                    sample_rate: 48_000,
                    channels: 2,
                    payload: Payload::Opus { frames: 240, data },
                }
            })
            .collect()
    }

    #[test]
    fn opus_decodes_in_order_and_conceals_short_losses() {
        let packets = opus_packets(12);
        let mut depacketizer = Depacketizer::default();
        let first = depacketizer.decode(packets[0].clone()).unwrap().unwrap();
        assert_eq!((first.first_frame, first.samples.len()), (0, 480));
        depacketizer.decode(packets[1].clone()).unwrap().unwrap();
        // Packets 2 and 3 are lost: 4 arrives covering them with concealed audio, not silence.
        let filled = depacketizer.decode(packets[4].clone()).unwrap().unwrap();
        assert_eq!((filled.first_frame, filled.samples.len(), filled.sequence), (480, 3 * 480, 4));
        assert!(filled.samples[..960].iter().any(|x| x.abs() > 0.01), "concealment carries the tone on");
        // 3 turning up late has already been covered; a duplicate of 4 is dropped too.
        assert_eq!(depacketizer.decode(packets[3].clone()).unwrap(), None);
        assert_eq!(depacketizer.decode(packets[4].clone()).unwrap(), None);
        // A gap longer than 20ms stays silent: the packet after it decodes alone.
        let late = depacketizer.decode(packets[11].clone()).unwrap().unwrap();
        assert_eq!((late.first_frame, late.samples.len()), (11 * 240, 480));
        // Everything decoded plays through the buffer without overlapping.
        let mut buffer = PlaybackBuffer::default();
        for pcm in [first, filled, late] {
            buffer.push(pcm).unwrap();
        }
    }

    #[test]
    fn opus_restarts_for_a_new_capture_and_ignores_older_ones() {
        let packets = opus_packets(2);
        let mut depacketizer = Depacketizer::default();
        depacketizer.decode(packets[0].clone()).unwrap().unwrap();
        let mut restarted = packets[0].clone();
        restarted.stream_id = 4;
        assert_eq!(depacketizer.decode(restarted).unwrap().unwrap().first_frame, 0);
        assert_eq!(depacketizer.decode(packets[1].clone()).unwrap(), None, "older capture");
        let mut bad = packets[1].clone();
        bad.stream_id = 4;
        if let Payload::Opus { frames, .. } = &mut bad.payload {
            *frames = 480;
        }
        assert!(depacketizer.decode(bad).is_err(), "header disagrees with the packet");
    }

    #[test]
    fn reorders_packets_and_inserts_silence_for_missing_frames() {
        let mut buffer = PlaybackBuffer::default();
        buffer.push(packet(2, 320, 0.75)).unwrap();
        buffer.push(packet(0, 0, 0.25)).unwrap();
        let mut out = vec![9.0f32; 480];
        buffer.render(&mut out, 8000, 1);
        assert!(out[..160].iter().all(|x| *x == 0.25));
        assert!(out[160..320].iter().all(|x| *x == 0.0));
        assert!(out[320..].iter().all(|x| *x == 0.75));
    }

    #[test]
    fn ignores_duplicates_late_packets_and_old_capture_epochs() {
        let mut buffer = PlaybackBuffer::default();
        buffer.push(packet(u32::MAX, 0, 0.25)).unwrap();
        buffer.push(packet(u32::MAX, 0, 0.9)).unwrap();
        let mut out = [0.0f32; 160];
        buffer.render(&mut out, 8000, 1);
        assert!(out.iter().all(|x| *x == 0.25));
        buffer.push(packet(u32::MAX, 0, 0.9)).unwrap();
        buffer.push(packet(0, 160, 0.5)).unwrap(); // sequence wrap is forward progress
        buffer.render(&mut out, 8000, 1);
        assert!(out.iter().all(|x| *x == 0.5));
        let mut new = packet(0, 0, 0.75);
        new.stream_id = 2;
        buffer.push(new).unwrap();
        buffer.push(packet(1, 320, 0.9)).unwrap();
        buffer.render(&mut out, 8000, 1);
        assert!(out.iter().all(|x| *x == 0.75));
    }

    #[test]
    fn bounds_overflow_and_trims_drift_to_recent_audio() {
        let mut buffer = PlaybackBuffer::default();
        for seq in 0..20 {
            buffer.push(packet(seq, u64::from(seq) * 160, seq as f32 / 20.0)).unwrap();
        }
        let first = *buffer.packets.first_key_value().unwrap().0;
        let last = buffer.packets.last_key_value().unwrap().1;
        assert!(last.first_frame + 160 - first <= 640);
        assert!(buffer.packets.len() <= MAX_QUEUED_PACKETS);
        let mut out = [0.0f32; 160];
        buffer.render(&mut out, 8000, 1);
        assert!(out.iter().all(|x| *x >= 0.75));
    }

    #[test]
    fn mute_flushes_backlog_and_underruns_are_silent() {
        let mut buffer = PlaybackBuffer::default();
        let mut out = [9.0f32; 160];
        buffer.render(&mut out, 8000, 1);
        assert_eq!(out, [0.0; 160]);
        buffer.push(packet(0, 0, 0.25)).unwrap();
        buffer.set_muted(true);
        buffer.push(packet(1, 160, 0.5)).unwrap();
        buffer.render(&mut out, 8000, 1);
        assert_eq!(out, [0.0; 160]);
        buffer.set_muted(false);
        buffer.render(&mut out, 8000, 1);
        assert_eq!(out, [0.0; 160]);
        buffer.push(packet(2, 320, 0.75)).unwrap();
        buffer.render(&mut out, 8000, 1);
        assert_eq!(out, [0.75; 160]);
        buffer.render(&mut out, 8000, 1);
        assert_eq!(out, [0.0; 160]);
    }

    #[test]
    fn adapts_sample_rate_and_mono_stereo_channels() {
        let mut p = packet(0, 0, 0.0);
        for frame in p.samples.as_chunks_mut::<4>().0 {
            frame.copy_from_slice(&[0.0, 1.0, 0.0, -1.0]);
        }
        let mut buffer = PlaybackBuffer::default();
        buffer.push(p).unwrap();
        let mut stereo = [0.0f32; 16];
        buffer.render(&mut stereo, 16000, 2);
        assert_eq!(stereo, [0.0, 0.0, 0.5, 0.5, 1.0, 1.0, 0.5, 0.5, 0.0, 0.0, -0.5, -0.5, -1.0, -1.0, -0.5, -0.5]);
        let mut p = packet(0, 0, 0.0);
        p.channels = 2;
        p.samples = [0.25, 0.75].repeat(160);
        let mut buffer = PlaybackBuffer::default();
        buffer.push(p).unwrap();
        let mut mono = [0.0f32; 8];
        buffer.render(&mut mono, 8000, 1);
        assert_eq!(mono, [0.5; 8]);
    }

    #[test]
    fn rejects_inconsistent_format_within_capture_epoch() {
        let mut buffer = PlaybackBuffer::default();
        buffer.push(packet(0, 0, 0.5)).unwrap();
        let mut bad = packet(1, 160, 0.5);
        bad.sample_rate = 16000;
        assert!(buffer.push(bad).is_err());
    }

    #[test]
    fn rejects_replayed_sequence_even_with_a_new_timestamp() {
        let mut buffer = PlaybackBuffer::default();
        buffer.push(packet(0, 0, 0.25)).unwrap();
        let mut out = [0.0f32; 160];
        buffer.render(&mut out, 8000, 1);
        buffer.push(packet(0, 160, 0.9)).unwrap();
        buffer.render(&mut out, 8000, 1);
        assert_eq!(out, [0.0; 160]);
    }

    #[test]
    fn tiny_packets_cannot_exceed_packet_count_limit_or_prevent_playback() {
        let mut buffer = PlaybackBuffer::default();
        for seq in 0..600 {
            let mut p = packet(seq, u64::from(seq), 0.5);
            p.sample_rate = 192_000;
            p.samples.truncate(1);
            buffer.push(p).unwrap();
            assert!(buffer.packets.len() <= MAX_QUEUED_PACKETS);
        }
        let mut out = [0.0f32; 8];
        buffer.render(&mut out, 192_000, 1);
        assert_eq!(out, [0.5; 8]);
    }

    #[test]
    fn finite_extreme_pcm_cannot_generate_nonfinite_output_during_conversion() {
        let mut p = packet(0, 0, f32::MAX);
        p.channels = 2;
        p.samples = [f32::MAX, f32::MAX, -f32::MAX, -f32::MAX].repeat(80);
        let mut buffer = PlaybackBuffer::default();
        buffer.push(p).unwrap();
        let mut out = [0.0f32; 16];
        buffer.render(&mut out, 16000, 1);
        assert!(out.iter().all(|value| value.is_finite() && value.abs() <= 1.0));
        assert_eq!(&out[..4], &[1.0, 0.0, -1.0, 0.0]);
    }

    #[test]
    #[ignore = "opens the default output device; only plays silence"]
    fn native_silent_output_starts_mutes_and_releases_device() {
        fn send_sync<T: Send + Sync>() {}
        send_sync::<Playback>();
        for _ in 0..2 {
            let playback = Playback::start().expect("default output opens");
            playback.set_muted(true);
            let bytes = audio::encode(&opus_packets(1)[0], 1200).unwrap();
            playback.push_datagram(&bytes).unwrap();
            thread::sleep(Duration::from_millis(100));
            playback.push_datagram(&bytes).expect("output stayed healthy");
            playback.set_muted(false);
            drop(playback);
        }
        eprintln!("Native silent playback initialized and released the default output twice.");
    }
}
