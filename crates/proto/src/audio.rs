//! Independent, lossless little-endian float PCM audio datagrams.

use anyhow::{Result, ensure};
use bytes::{BufMut, Bytes, BytesMut};

pub const KIND_AUDIO: u8 = 0x41;
pub const HEADER_LEN: usize = 32;
pub const MAX_DATAGRAM_LEN: usize = 65_535;
pub const MAX_CHANNELS: u16 = 8;
pub const MIN_SAMPLE_RATE: u32 = 8_000;
pub const MAX_SAMPLE_RATE: u32 = 192_000;

#[derive(Clone, Debug, PartialEq)]
pub struct AudioPacket {
    pub stream_id: u64,
    pub sequence: u32,
    pub first_frame: u64,
    pub sample_rate: u32,
    pub channels: u16,
    /// Interleaved frames in the source device's channel order.
    pub samples: Vec<f32>,
}

pub fn max_frames_per_packet(max_datagram: usize, channels: u16) -> Result<usize> {
    ensure!((1..=MAX_CHANNELS).contains(&channels), "unsupported audio channel count");
    ensure!(max_datagram <= MAX_DATAGRAM_LEN, "audio datagram limit too large");
    let payload = max_datagram.saturating_sub(HEADER_LEN);
    let frames = payload / (usize::from(channels) * 4);
    ensure!(frames > 0, "datagram limit cannot fit an audio frame");
    Ok(frames.min(u16::MAX as usize))
}

fn validate_format(sample_rate: u32, channels: u16, frames: usize, first_frame: u64) -> Result<()> {
    ensure!((MIN_SAMPLE_RATE..=MAX_SAMPLE_RATE).contains(&sample_rate), "unsupported audio sample rate");
    ensure!((1..=MAX_CHANNELS).contains(&channels), "unsupported audio channel count");
    ensure!(frames > 0 && frames <= (sample_rate / 50) as usize, "audio packet must contain at most 20ms of PCM");
    ensure!(first_frame.checked_add(frames as u64).is_some(), "audio timeline overflow");
    Ok(())
}

pub fn encode(packet: &AudioPacket, max_datagram: usize) -> Result<Bytes> {
    let capacity = max_frames_per_packet(max_datagram, packet.channels)?;
    let channels = usize::from(packet.channels);
    ensure!(packet.samples.len().is_multiple_of(channels), "incomplete audio frame");
    let frames = packet.samples.len() / channels;
    validate_format(packet.sample_rate, packet.channels, frames, packet.first_frame)?;
    ensure!(frames <= capacity, "audio packet exceeds negotiated datagram limit");
    ensure!(packet.samples.iter().all(|sample| sample.is_finite()), "nonfinite audio sample");
    let mut out = BytesMut::with_capacity(HEADER_LEN + packet.samples.len() * 4);
    out.put_u8(KIND_AUDIO);
    out.put_u8(0); // Reserved flags; peers must understand future encodings explicitly.
    out.put_u16_le(packet.channels);
    out.put_u32_le(packet.sample_rate);
    out.put_u32_le(packet.sequence);
    out.put_u16_le(frames as u16);
    out.put_u16_le(0);
    out.put_u64_le(packet.stream_id);
    out.put_u64_le(packet.first_frame);
    for sample in &packet.samples {
        out.put_slice(&sample.to_le_bytes());
    }
    Ok(out.freeze())
}

pub fn decode(data: &[u8]) -> Result<AudioPacket> {
    ensure!((HEADER_LEN..=MAX_DATAGRAM_LEN).contains(&data.len()), "invalid audio datagram length");
    ensure!(data[0] == KIND_AUDIO && data[1] == 0 && data[14..16] == [0, 0], "unknown audio encoding");
    let u16_at = |i| u16::from_le_bytes(data[i..i + 2].try_into().unwrap());
    let u32_at = |i| u32::from_le_bytes(data[i..i + 4].try_into().unwrap());
    let u64_at = |i| u64::from_le_bytes(data[i..i + 8].try_into().unwrap());
    let channels = u16_at(2);
    let sample_rate = u32_at(4);
    let frames = usize::from(u16_at(12));
    let first_frame = u64_at(24);
    validate_format(sample_rate, channels, frames, first_frame)?;
    ensure!(data.len() == HEADER_LEN + frames * usize::from(channels) * 4, "audio PCM length does not match header");
    let samples: Vec<_> =
        data[HEADER_LEN..].as_chunks::<4>().0.iter().map(|bytes| f32::from_le_bytes(*bytes)).collect();
    ensure!(samples.iter().all(|sample| sample.is_finite()), "nonfinite audio sample");
    Ok(AudioPacket { stream_id: u64_at(16), sequence: u32_at(8), first_frame, sample_rate, channels, samples })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet() -> AudioPacket {
        AudioPacket {
            stream_id: 29,
            sequence: u32::MAX,
            first_frame: 101,
            sample_rate: 48_000,
            channels: 2,
            samples: vec![0.0, -0.0, 1.0, -1.0, f32::MIN_POSITIVE, 0.125],
        }
    }

    #[test]
    fn pcm_round_trip_preserves_float_bits_and_timeline() {
        let p = packet();
        let bytes = encode(&p, 1200).unwrap();
        assert_eq!(&bytes[..4], &[0x41, 0, 2, 0]);
        let got = decode(&bytes).unwrap();
        assert_eq!(got.stream_id, 29);
        assert_eq!(got.sequence, u32::MAX);
        assert_eq!(got.first_frame, 101);
        assert_eq!(got.sample_rate, 48_000);
        assert_eq!(got.channels, 2);
        assert_eq!(
            got.samples.iter().map(|x| x.to_bits()).collect::<Vec<_>>(),
            vec![0, 0x8000_0000, 0x3f80_0000, 0xbf80_0000, 0x0080_0000, 0x3e00_0000]
        );
    }

    #[test]
    fn respects_negotiated_datagram_size_and_frame_alignment() {
        assert_eq!(max_frames_per_packet(1200, 2).unwrap(), 146);
        assert!(max_frames_per_packet(39, 2).is_err());
        assert!(max_frames_per_packet(1200, 0).is_err());
        assert!(max_frames_per_packet(65_536, 2).is_err());
        let mut p = packet();
        p.samples = vec![0.5; 292];
        assert_eq!(encode(&p, 1200).unwrap().len(), 1200);
        p.samples.extend([0.0, 0.0]);
        assert!(encode(&p, 1200).is_err());
        p.samples.pop();
        assert!(encode(&p, 1200).is_err());
    }

    #[test]
    fn rejects_invalid_format_nonfinite_empty_and_long_packets() {
        for (rate, channels) in [(0, 2), (7999, 2), (192001, 2), (48000, 0), (48000, 9)] {
            let mut p = packet();
            p.sample_rate = rate;
            p.channels = channels;
            assert!(encode(&p, 1200).is_err());
        }
        for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut p = packet();
            p.samples[0] = value;
            assert!(encode(&p, 1200).is_err());
        }
        let mut p = packet();
        p.samples.clear();
        assert!(encode(&p, 1200).is_err());
        p.samples = vec![0.0; 1922]; // 20ms at 48kHz is at most 960 stereo frames.
        assert!(encode(&p, MAX_DATAGRAM_LEN).is_err());
        p.samples = vec![0.0; 2];
        p.first_frame = u64::MAX;
        assert!(encode(&p, 1200).is_err());
    }

    #[test]
    fn decoder_rejects_truncation_flags_count_and_nonfinite_payload() {
        let good = encode(&packet(), 1200).unwrap();
        for len in 0..good.len() {
            assert!(decode(&good[..len]).is_err());
        }
        for (index, value) in [(0, 0x56), (1, 1), (2, 0), (12, 4), (14, 1)] {
            let mut bad = good.to_vec();
            bad[index] = value;
            assert!(decode(&bad).is_err(), "index {index}");
        }
        let mut bad = good.to_vec();
        bad[32..36].copy_from_slice(&f32::NAN.to_le_bytes());
        assert!(decode(&bad).is_err());
        assert!(decode(&vec![0; MAX_DATAGRAM_LEN + 1]).is_err());
    }
}
