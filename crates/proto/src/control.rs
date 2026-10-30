//! Reliable control messages, sent on the session's bi-directional stream as
//! `u32` little-endian length prefix + postcard body.

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{DeviceClass, Permissions, cursor::CursorShape};

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
    /// Actual selected encoder, including software fallback when necessary.
    VideoEncoder {
        name: String,
    },
    AudioStatus {
        active: bool,
        reason: Option<String>,
    },
    CursorShape(CursorShape),
    /// How the host is fitting video to the connection, about once a second.
    VideoStats {
        /// The bitrate the encoder is aiming for now, at most the host's cap.
        bitrate_kbps: u32,
        /// Share of packets the path has been losing, in thousandths.
        loss_permille: u16,
    },
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
