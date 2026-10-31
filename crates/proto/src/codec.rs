//! Video codecs, what each PC can do with them, and how a session picks one.
//!
//! The viewer lists the codecs it can decode right after its hello; the host lists those it can
//! encode, and [`choose`] settles on one. H.264 is always available on both sides in software,
//! so a session can always fall back to it.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum Codec {
    #[default]
    H264,
    H265,
    Av1,
}

impl Codec {
    /// Every codec, most efficient first: at the same bitrate H.265 looks better than H.264,
    /// and AV1 better again.
    pub const BY_EFFICIENCY: [Codec; 3] = [Codec::Av1, Codec::H265, Codec::H264];

    /// The two bits identifying the codec in a video datagram's flags.
    pub(crate) fn wire_bits(self) -> u8 {
        match self {
            Codec::H264 => 0,
            Codec::H265 => 1,
            Codec::Av1 => 2,
        }
    }

    pub(crate) fn from_wire_bits(bits: u8) -> Option<Codec> {
        match bits {
            0 => Some(Codec::H264),
            1 => Some(Codec::H265),
            2 => Some(Codec::Av1),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Codec::H264 => "H.264",
            Codec::H265 => "H.265",
            Codec::Av1 => "AV1",
        }
    }
}

impl std::fmt::Display for Codec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// One codec a PC can encode or decode, and whether its graphics card does the work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodecSupport {
    pub codec: Codec,
    pub hardware: bool,
}

impl CodecSupport {
    pub const fn hardware(codec: Codec) -> Self {
        Self { codec, hardware: true }
    }

    pub const fn software(codec: Codec) -> Self {
        Self { codec, hardware: false }
    }
}

/// Picks the session's codec from what the host can encode and the viewer can decode, leaving
/// out any in `excluded` (codecs that already failed this session). `None` only if they share
/// nothing, which can't happen between two Connect builds while H.264 is excluded nowhere.
///
/// Work done by graphics cards on both ends beats work on one, which beats none: a processor
/// struggling at high frame rates costs more than a less efficient codec. Among equals, the
/// more efficient codec wins.
pub fn choose(encoders: &[CodecSupport], decoders: &[CodecSupport], excluded: &[Codec]) -> Option<Codec> {
    let best = |list: &[CodecSupport], codec| list.iter().filter(|s| s.codec == codec).map(|s| s.hardware).max();
    Codec::BY_EFFICIENCY
        .into_iter()
        .filter(|codec| !excluded.contains(codec))
        .filter_map(|codec| {
            let (encode, decode) = (best(encoders, codec)?, best(decoders, codec)?);
            let rank = usize::from(encode) + usize::from(decode);
            let efficiency = Codec::BY_EFFICIENCY.len() - Codec::BY_EFFICIENCY.iter().position(|&c| c == codec)?;
            Some(((rank, efficiency), codec))
        })
        .max_by_key(|&(key, _)| key)
        .map(|(_, codec)| codec)
}

#[cfg(test)]
mod tests {
    use super::*;
    use Codec::*;

    const SW_H264: CodecSupport = CodecSupport::software(H264);

    #[test]
    fn software_h264_is_the_floor() {
        assert_eq!(choose(&[SW_H264], &[SW_H264], &[]), Some(H264));
        assert_eq!(choose(&[SW_H264], &[CodecSupport::hardware(H264)], &[]), Some(H264));
        assert_eq!(choose(&[SW_H264], &[SW_H264], &[H264]), None);
        assert_eq!(choose(&[], &[SW_H264], &[]), None);
    }

    #[test]
    fn hardware_on_both_ends_beats_a_better_codec_on_one() {
        // The host encodes H.265 and H.264 on its GPU; the viewer decodes only H.264 on its GPU
        // and H.265 nowhere: H.264.
        let host = [CodecSupport::hardware(H265), CodecSupport::hardware(H264), SW_H264];
        assert_eq!(choose(&host, &[CodecSupport::hardware(H264), SW_H264], &[]), Some(H264));
        // A viewer whose GPU also decodes H.265 gets H.265.
        let viewer = [CodecSupport::hardware(H265), CodecSupport::hardware(H264), SW_H264];
        assert_eq!(choose(&host, &viewer, &[]), Some(H265));
        // H.265 failed on the viewer: back to H.264, still all on the GPU.
        assert_eq!(choose(&host, &viewer, &[H265]), Some(H264));
    }

    #[test]
    fn among_equals_the_more_efficient_codec_wins() {
        let all = [CodecSupport::hardware(Av1), CodecSupport::hardware(H265), CodecSupport::hardware(H264)];
        assert_eq!(choose(&all, &all, &[]), Some(Av1));
        // Hardware H.265 at both ends outranks AV1 decoded in software.
        let viewer = [CodecSupport::software(Av1), CodecSupport::hardware(H265), SW_H264];
        assert_eq!(choose(&all, &viewer, &[]), Some(H265));
        // A software H.264 encoder with a hardware decoder ties a hardware encoder with a
        // software decoder: the better codec takes it.
        let host = [CodecSupport::hardware(H265), SW_H264];
        let viewer = [CodecSupport::software(H265), CodecSupport::hardware(H264)];
        assert_eq!(choose(&host, &viewer, &[]), Some(H265));
    }

    #[test]
    fn wire_bits_round_trip() {
        for codec in Codec::BY_EFFICIENCY {
            assert_eq!(Codec::from_wire_bits(codec.wire_bits()), Some(codec));
        }
        assert_eq!(Codec::from_wire_bits(3), None);
    }
}
