//! Independent audio datagrams: Opus, or little-endian float PCM where the source's format
//! has no Opus equivalent.

use anyhow::{Result, ensure};
use bytes::{BufMut, Bytes, BytesMut};

pub const KIND_AUDIO: u8 = 0x41;
pub const HEADER_LEN: usize = 32;
pub const MAX_DATAGRAM_LEN: usize = 65_535;
pub const MAX_CHANNELS: u16 = 8;
pub const MIN_SAMPLE_RATE: u32 = 8_000;
pub const MAX_SAMPLE_RATE: u32 = 192_000;
/// Flag byte for float PCM.
const ENCODING_PCM: u8 = 0;
/// Flag byte for one Opus packet.
const ENCODING_OPUS: u8 = 1;
/// The largest Opus packet ([RFC 6716](https://www.rfc-editor.org/rfc/rfc6716#section-3.4)).
pub const MAX_OPUS_LEN: usize = 1275;

#[derive(Clone, Debug, PartialEq)]
pub struct AudioPacket {
    pub stream_id: u64,
    pub sequence: u32,
    pub first_frame: u64,
    pub sample_rate: u32,
    pub channels: u16,
    pub payload: Payload,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Payload {
    /// Interleaved frames in the source device's channel order.
    Pcm(Vec<f32>),
    /// One Opus packet holding `frames` frames.
    Opus { frames: u16, data: Vec<u8> },
}

impl AudioPacket {
    /// How many frames (one sample per channel) the packet covers.
    pub fn frames(&self) -> usize {
        match &self.payload {
            Payload::Pcm(samples) => samples.len() / usize::from(self.channels.max(1)),
            Payload::Opus { frames, .. } => usize::from(*frames),
        }
    }
}

/// Whether Opus can carry audio in this format: it takes one or two channels at these rates.
pub fn opus_supports(sample_rate: u32, channels: u16) -> bool {
    matches!(sample_rate, 8_000 | 12_000 | 16_000 | 24_000 | 48_000) && matches!(channels, 1 | 2)
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
    ensure!(max_datagram <= MAX_DATAGRAM_LEN, "audio datagram limit too large");
    let channels = usize::from(packet.channels);
    let (encoding, frames, body_len) = match &packet.payload {
        Payload::Pcm(samples) => {
            let capacity = max_frames_per_packet(max_datagram, packet.channels)?;
            ensure!(samples.len().is_multiple_of(channels), "incomplete audio frame");
            ensure!(samples.len() / channels <= capacity, "audio packet exceeds negotiated datagram limit");
            ensure!(samples.iter().all(|sample| sample.is_finite()), "nonfinite audio sample");
            (ENCODING_PCM, samples.len() / channels, samples.len() * 4)
        }
        Payload::Opus { frames, data } => {
            ensure!(opus_supports(packet.sample_rate, packet.channels), "Opus can't carry this audio format");
            ensure!((1..=MAX_OPUS_LEN).contains(&data.len()), "invalid Opus packet length");
            ensure!(HEADER_LEN + data.len() <= max_datagram, "audio packet exceeds negotiated datagram limit");
            (ENCODING_OPUS, usize::from(*frames), data.len())
        }
    };
    validate_format(packet.sample_rate, packet.channels, frames, packet.first_frame)?;
    let mut out = BytesMut::with_capacity(HEADER_LEN + body_len);
    out.put_u8(KIND_AUDIO);
    out.put_u8(encoding);
    out.put_u16_le(packet.channels);
    out.put_u32_le(packet.sample_rate);
    out.put_u32_le(packet.sequence);
    out.put_u16_le(frames as u16);
    out.put_u16_le(0);
    out.put_u64_le(packet.stream_id);
    out.put_u64_le(packet.first_frame);
    match &packet.payload {
        Payload::Pcm(samples) => samples.iter().for_each(|sample| out.put_slice(&sample.to_le_bytes())),
        Payload::Opus { data, .. } => out.put_slice(data),
    }
    Ok(out.freeze())
}

pub fn decode(data: &[u8]) -> Result<AudioPacket> {
    ensure!((HEADER_LEN..=MAX_DATAGRAM_LEN).contains(&data.len()), "invalid audio datagram length");
    ensure!(
        data[0] == KIND_AUDIO && matches!(data[1], ENCODING_PCM | ENCODING_OPUS) && data[14..16] == [0, 0],
        "unknown audio encoding"
    );
    let u16_at = |i| u16::from_le_bytes(data[i..i + 2].try_into().unwrap());
    let u32_at = |i| u32::from_le_bytes(data[i..i + 4].try_into().unwrap());
    let u64_at = |i| u64::from_le_bytes(data[i..i + 8].try_into().unwrap());
    let channels = u16_at(2);
    let sample_rate = u32_at(4);
    let frames = usize::from(u16_at(12));
    let first_frame = u64_at(24);
    validate_format(sample_rate, channels, frames, first_frame)?;
    let body = &data[HEADER_LEN..];
    let payload = if data[1] == ENCODING_OPUS {
        ensure!(opus_supports(sample_rate, channels), "Opus can't carry this audio format");
        ensure!((1..=MAX_OPUS_LEN).contains(&body.len()), "invalid Opus packet length");
        Payload::Opus { frames: frames as u16, data: body.to_vec() }
    } else {
        ensure!(body.len() == frames * usize::from(channels) * 4, "audio PCM length does not match header");
        let samples: Vec<_> = body.as_chunks::<4>().0.iter().map(|bytes| f32::from_le_bytes(*bytes)).collect();
        ensure!(samples.iter().all(|sample| sample.is_finite()), "nonfinite audio sample");
        Payload::Pcm(samples)
    };
    Ok(AudioPacket { stream_id: u64_at(16), sequence: u32_at(8), first_frame, sample_rate, channels, payload })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcm(packet: &mut AudioPacket) -> &mut Vec<f32> {
        match &mut packet.payload {
            Payload::Pcm(samples) => samples,
            Payload::Opus { .. } => panic!("not PCM"),
        }
    }

    fn packet() -> AudioPacket {
        AudioPacket {
            stream_id: 29,
            sequence: u32::MAX,
            first_frame: 101,
            sample_rate: 48_000,
            channels: 2,
            payload: Payload::Pcm(vec![0.0, -0.0, 1.0, -1.0, f32::MIN_POSITIVE, 0.125]),
        }
    }

    #[test]
    fn pcm_round_trip_preserves_float_bits_and_timeline() {
        let p = packet();
        let bytes = encode(&p, 1200).unwrap();
        assert_eq!(&bytes[..4], &[0x41, 0, 2, 0]);
        let mut got = decode(&bytes).unwrap();
        assert_eq!(got.stream_id, 29);
        assert_eq!(got.sequence, u32::MAX);
        assert_eq!(got.first_frame, 101);
        assert_eq!(got.sample_rate, 48_000);
        assert_eq!(got.channels, 2);
        assert_eq!(
            pcm(&mut got).iter().map(|x| x.to_bits()).collect::<Vec<_>>(),
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
        p.payload = Payload::Pcm(vec![0.5; 292]);
        assert_eq!(encode(&p, 1200).unwrap().len(), 1200);
        pcm(&mut p).extend([0.0, 0.0]);
        assert!(encode(&p, 1200).is_err());
        pcm(&mut p).pop();
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
            pcm(&mut p)[0] = value;
            assert!(encode(&p, 1200).is_err());
        }
        let mut p = packet();
        pcm(&mut p).clear();
        assert!(encode(&p, 1200).is_err());
        p.payload = Payload::Pcm(vec![0.0; 1922]); // 20ms at 48kHz is at most 960 stereo frames.
        assert!(encode(&p, MAX_DATAGRAM_LEN).is_err());
        p.payload = Payload::Pcm(vec![0.0; 2]);
        p.first_frame = u64::MAX;
        assert!(encode(&p, 1200).is_err());
    }

    #[test]
    fn decoder_rejects_truncation_flags_count_and_nonfinite_payload() {
        let good = encode(&packet(), 1200).unwrap();
        for len in 0..good.len() {
            assert!(decode(&good[..len]).is_err());
        }
        for (index, value) in [(0, 0x56), (1, 2), (2, 0), (12, 4), (14, 1)] {
            let mut bad = good.to_vec();
            bad[index] = value;
            assert!(decode(&bad).is_err(), "index {index}");
        }
        let mut bad = good.to_vec();
        bad[32..36].copy_from_slice(&f32::NAN.to_le_bytes());
        assert!(decode(&bad).is_err());
        assert!(decode(&vec![0; MAX_DATAGRAM_LEN + 1]).is_err());
    }

    #[test]
    fn opus_round_trip_and_limits() {
        let mut p = packet();
        p.payload = Payload::Opus { frames: 240, data: vec![0xfc, 1, 2, 3] };
        let bytes = encode(&p, 1200).unwrap();
        assert_eq!(&bytes[..4], &[0x41, 1, 2, 0]);
        assert_eq!(bytes.len(), HEADER_LEN + 4);
        let got = decode(&bytes).unwrap();
        assert_eq!(got, p);
        assert_eq!(got.frames(), 240);
        // Opus has no 44.1kHz mode and takes at most two channels without multistream.
        for (rate, channels) in [(44_100, 2), (48_000, 6)] {
            let mut bad = p.clone();
            (bad.sample_rate, bad.channels) = (rate, channels);
            assert!(encode(&bad, 1200).is_err());
        }
        for len in [0, MAX_OPUS_LEN + 1] {
            p.payload = Payload::Opus { frames: 240, data: vec![0; len] };
            assert!(encode(&p, MAX_DATAGRAM_LEN).is_err());
        }
        p.payload = Payload::Opus { frames: 240, data: vec![0; 1200] };
        assert!(encode(&p, 1200).is_err(), "header plus packet must fit the datagram");
        p.payload = Payload::Opus { frames: 961, data: vec![0; 10] };
        assert!(encode(&p, 1200).is_err(), "more than 20ms");
        let mut bad = bytes.to_vec();
        bad[1] = 2;
        assert!(decode(&bad).is_err());
        assert!(decode(&bytes[..HEADER_LEN]).is_err(), "empty Opus packet");
    }
}
