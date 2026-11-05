//! Wire protocol shared by the host and client.
//!
//! A session is one iroh (QUIC) connection using [`ALPN`]:
//! - one bi-directional stream carries length-prefixed [`control`] messages (reliable),
//! - QUIC datagrams carry [`video`] chunks (unreliable, never retransmitted),
//! - one uni-directional stream from the client carries [`input`] events (reliable).

pub mod audio;
pub mod codec;
pub mod control;
pub mod cursor;
pub mod input;
pub mod perf;
pub mod permissions;
pub mod video;

pub use codec::{Codec, CodecSupport};
pub use cursor::{CursorPosition, CursorShape};
pub use permissions::{DeviceClass, Permissions};

use std::{
    sync::OnceLock,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

/// ALPN identifying the Connect remote-control protocol. Bump the suffix on breaking changes.
pub const ALPN: &[u8] = b"connect/remote/1";

/// Protocol version exchanged in the handshake.
pub const PROTOCOL_VERSION: u32 = 9;

/// How long the host user has to answer a connection request before it counts as declined.
pub const APPROVAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// QUIC application close code: the session ended normally (either side hung up).
pub const CLOSE_NORMAL: u32 = 0;
/// QUIC application close code: the host turned the connection down. The close reason
/// carries the same text as [`control::HostMsg::Rejected`].
pub const CLOSE_REJECTED: u32 = 1;
/// QUIC application close code: the peer broke the protocol.
pub const CLOSE_PROTOCOL: u32 = 2;

/// Microseconds on this process's clock: the wall-clock time when the process first asked, plus
/// time since on the monotonic clock. Capture times and pings use it, so a timestamp never jumps
/// when the system clock is adjusted mid-session. Two PCs' clocks still differ by an unknown
/// offset (and drift apart slowly); the viewer estimates it from pings, so nothing should subtract
/// one PC's timestamp from the other's without it.
pub fn now_micros() -> u64 {
    micros_at(Instant::now())
}

/// `instant` on the [`now_micros`] clock.
pub fn micros_at(instant: Instant) -> u64 {
    static START: OnceLock<(Instant, u64)> = OnceLock::new();
    let &(start, wall_us) = START.get_or_init(|| {
        let wall = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        (Instant::now(), wall.as_micros() as u64)
    });
    match instant.checked_duration_since(start) {
        Some(after) => wall_us + after.as_micros() as u64,
        None => wall_us.saturating_sub(start.duration_since(instant).as_micros() as u64),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn the_clock_is_monotonic_and_starts_at_wall_time() {
        let wall = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_micros() as u64;
        let first = now_micros();
        assert!(first.abs_diff(wall) < 5_000_000, "anchored to the wall clock: {first} vs {wall}");
        let mut last = first;
        for _ in 0..1000 {
            let now = now_micros();
            assert!(now >= last);
            last = now;
        }
    }

    #[test]
    fn instants_map_onto_the_same_clock() {
        let now = Instant::now();
        let later = now + Duration::from_millis(250);
        // Each is rounded down to the µs on its own, so the gap can be a µs short.
        assert!((249_999..=250_000).contains(&(micros_at(later) - micros_at(now))));
        // Before the clock started: still ordered, not wrapped.
        let earlier = now.checked_sub(Duration::from_secs(1)).unwrap();
        assert!((999_999..=1_000_001).contains(&(micros_at(now) - micros_at(earlier))));
    }
}
