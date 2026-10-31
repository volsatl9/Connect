//! Wire protocol shared by the host and client.
//!
//! A session is one iroh (QUIC) connection using [`ALPN`]:
//! - one bi-directional stream carries length-prefixed [`control`] messages (reliable),
//! - QUIC datagrams carry [`video`] chunks (unreliable, never retransmitted),
//! - one uni-directional stream from the client carries [`input`] events (reliable).

pub mod audio;
pub mod control;
pub mod cursor;
pub mod input;
pub mod permissions;
pub mod video;

pub use cursor::{CursorPosition, CursorShape};
pub use permissions::{DeviceClass, Permissions};

/// ALPN identifying the Connect remote-control protocol. Bump the suffix on breaking changes.
pub const ALPN: &[u8] = b"connect/remote/1";

/// Protocol version exchanged in the handshake.
pub const PROTOCOL_VERSION: u32 = 5;

/// How long the host user has to answer a connection request before it counts as declined.
pub const APPROVAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// QUIC application close code: the session ended normally (either side hung up).
pub const CLOSE_NORMAL: u32 = 0;
/// QUIC application close code: the host turned the connection down. The close reason
/// carries the same text as [`control::HostMsg::Rejected`].
pub const CLOSE_REJECTED: u32 = 1;
/// QUIC application close code: the peer broke the protocol.
pub const CLOSE_PROTOCOL: u32 = 2;

/// Microseconds since the Unix epoch, used for capture timestamps and clock-offset estimation.
pub fn now_micros() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_micros() as u64).unwrap_or(0)
}
