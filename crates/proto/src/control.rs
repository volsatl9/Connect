//! Reliable control messages, sent on the session's bi-directional stream as
//! `u32` little-endian length prefix + postcard body.

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{Codec, CodecSupport, DeviceClass, Permissions, cursor::CursorShape};

/// Upper bound on a single control message; anything larger is a protocol violation.
pub const MAX_MESSAGE_LEN: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ClientMsg {
    /// First message on the control stream.
    Hello { version: u32, client_name: String },
    /// The client lost video data and cannot decode until the next keyframe.
    RequestKeyframe,
    /// Clock/RTT probe; the host answers with [`HostMsg::Pong`].
    Ping { client_time_us: u64 },
    /// Straight after the hello: the codecs this PC can decode. (Separate from the hello, so a
    /// host of another version can still read the hello and say why it won't connect.)
    Decoders(Vec<CodecSupport>),
    /// This PC couldn't decode `codec` after all; the host switches to another one.
    DecoderFailed { codec: Codec },
    /// How the video is arriving here, about once a second. It can only lower the bitrate.
    Feedback(ViewerFeedback),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum HostMsg {
    /// The host user accepted the connection; video datagrams follow.
    Welcome {
        host_name: String,
        width: u32,
        height: u32,
        permissions: Permissions,
    },
    /// The host user declined (or the host is busy / incompatible). The connection closes next.
    Rejected {
        reason: String,
    },
    /// The host user changed which input devices are allowed.
    PermissionsChanged(Permissions),
    /// The captured screen changed size; subsequent frames use this resolution.
    VideoFormat {
        width: u32,
        height: u32,
    },
    Pong {
        client_time_us: u64,
        host_time_us: u64,
    },
    /// The host can't act on this kind of input even though it's allowed, e.g. the gamepad
    /// driver is missing. The text says why.
    InputUnavailable {
        device: DeviceClass,
        reason: String,
    },
    /// Actual selected encoder, including software fallback when necessary, and its codec.
    VideoEncoder {
        name: String,
        codec: Codec,
    },
    AudioStatus {
        active: bool,
        reason: Option<String>,
    },
    CursorShape(CursorShape),
    /// Where the other connectors' pointers are, for drawing over the video.
    Pointers(Vec<PeerPointer>),
    /// How the host is fitting video to the connection, about once a second.
    VideoStats {
        /// The bitrate the encoder is aiming for now, at most the host's cap.
        bitrate_kbps: u32,
        /// The most it may use.
        max_kbps: u32,
        /// Why it last changed.
        reason: String,
        /// Whether the host's encoder is what's holding the video back.
        encoder_limited: bool,
        /// Share of packets the path has been losing, in thousandths.
        loss_permille: u16,
        /// How busy the host's graphics card is, where it can tell.
        gpu: Option<connect_gpu_load::GpuLoad>,
        /// Where the host's time goes, unless the host has tracing off.
        perf: Option<Box<crate::perf::HostPerf>>,
    },
}

/// What a viewer measures of the video reaching it, over the last report.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewerFeedback {
    pub rtt_us: u32,
    /// How much frames' arrival times wander, smoothed.
    pub jitter_us: u32,
    /// Share of frames that never arrived whole, in thousandths.
    pub loss_permille: u16,
    pub receive_kbps: u32,
    pub decode_us: u32,
    pub frames_decoded: u32,
    /// Frames lost, or thrown away because the decoder fell behind.
    pub frames_dropped: u32,
    /// Frames waiting for the decoder as this was sent.
    pub frames_queued: u32,
}

impl ViewerFeedback {
    /// Within what a real connection could report: figures from the far side aren't trusted.
    #[must_use]
    pub fn bounded(self) -> Self {
        const SECOND_US: u32 = 1_000_000;
        Self {
            rtt_us: self.rtt_us.min(10 * SECOND_US),
            jitter_us: self.jitter_us.min(SECOND_US),
            loss_permille: self.loss_permille.min(1000),
            receive_kbps: self.receive_kbps.min(10_000_000),
            decode_us: self.decode_us.min(SECOND_US),
            frames_decoded: self.frames_decoded.min(10_000),
            frames_dropped: self.frames_dropped.min(10_000),
            frames_queued: self.frames_queued.min(10_000),
        }
    }
}

/// Another connector's pointer, as a share of the picture in sixteen-bit units.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerPointer {
    pub name: String,
    pub x: u16,
    pub y: u16,
}

/// Serialises `msg` with its length prefix, ready to write to the stream.
pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>> {
    let body = postcard::to_stdvec(msg).context("serialising control message")?;
    ensure!(body.len() <= MAX_MESSAGE_LEN, "control message too large ({} bytes)", body.len());
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Parses the length prefix, validating it against [`MAX_MESSAGE_LEN`].
pub fn decode_len(prefix: [u8; 4]) -> Result<usize> {
    let len = u32::from_le_bytes(prefix) as usize;
    ensure!(len <= MAX_MESSAGE_LEN, "control message too large ({len} bytes)");
    Ok(len)
}

pub fn decode<T: DeserializeOwned>(body: &[u8]) -> Result<T> {
    postcard::from_bytes(body).context("malformed control message")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let msg = HostMsg::Welcome {
            host_name: "desk".into(),
            width: 1920,
            height: 1080,
            permissions: Permissions::VIEW_ONLY,
        };
        let bytes = encode(&msg).unwrap();
        let len = decode_len(bytes[..4].try_into().unwrap()).unwrap();
        assert_eq!(len, bytes.len() - 4);
        assert_eq!(decode::<HostMsg>(&bytes[4..]).unwrap(), msg);
    }

    #[test]
    fn rejects_oversized_prefix() {
        assert!(decode_len(((MAX_MESSAGE_LEN + 1) as u32).to_le_bytes()).is_err());
    }
}
