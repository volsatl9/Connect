//! Peer-to-peer connectivity on top of iroh.
//!
//! Each install has a persistent ed25519 key; its public half is the user's **User-ID**.
//! Connecting by User-ID alone works anywhere: iroh looks the ID up via n0's DNS address
//! lookup, connects through a relay first, and hole-punches to a direct UDP path when it can.

use std::{path::Path, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use connect_proto::{ALPN, control};
use iroh::{
    Endpoint, EndpointAddr, EndpointId, RelayMode, SecretKey,
    endpoint::{Builder, Connection, QuicTransportConfig, RecvStream, SendStream, presets},
};
use serde::{Serialize, de::DeserializeOwned};

pub use iroh;

const KEY_FILE: &str = "identity.key";

/// Datagrams (video, audio) queued for sending beyond this push the oldest out. Room for the
/// largest keyframe; the host keeps the queue much shorter than this by sending less.
pub const DATAGRAM_SEND_BUFFER: usize = 2 * 1024 * 1024;

/// How long to spend looking up and reaching a host before giving up.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Default directory for persistent state (identity key, settings).
pub fn default_config_dir() -> Result<std::path::PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "Connect").context("no home directory")?;
    Ok(dirs.config_dir().to_path_buf())
}

/// Loads this machine's identity (its account) from `dir`; `None` if there isn't one yet.
pub fn load_identity(dir: &Path) -> Result<Option<SecretKey>> {
    let path = dir.join(KEY_FILE);
    match std::fs::read(&path) {
        Ok(bytes) => {
            let bytes: [u8; 32] = bytes
                .try_into()
                .map_err(|_| anyhow::anyhow!("{} is corrupt", path.display()))?;
            Ok(Some(SecretKey::from_bytes(&bytes)))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Generates a new identity and saves it in `dir`. Fails rather than replace an existing one.
pub fn create_identity(dir: &Path) -> Result<SecretKey> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let key = SecretKey::generate();
    write_private(&dir.join(KEY_FILE), &key.to_bytes())?;
    Ok(key)
}

/// Loads this machine's identity from `dir`, generating and saving one on first run.
pub fn load_or_create_identity(dir: &Path) -> Result<SecretKey> {
    match load_identity(dir)? {
        Some(key) => Ok(key),
        None => create_identity(dir),
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    #[cfg(unix)]
    {
        use std::{io::Write, os::unix::fs::OpenOptionsExt};
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("creating {}", path.display()))?;
        f.write_all(bytes)?;
    }
    #[cfg(not(unix))]
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .with_context(|| format!("creating {}", path.display()))?;
        f.write_all(bytes)?;
    }
    Ok(())
}

/// Formats a User-ID in groups of 8 hex digits so people can read it out or compare it.
pub fn format_user_id(id: &EndpointId) -> String {
    let hex = id.to_string();
    hex.as_bytes()
        .chunks(8)
        .map(|c| std::str::from_utf8(c).expect("hex is ascii"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Parses a User-ID as typed or pasted, ignoring spaces, dashes and case.
pub fn parse_user_id(input: &str) -> Result<EndpointId> {
    let cleaned: String = input
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| c.to_ascii_lowercase())
        .collect();
    cleaned.parse().map_err(|_| anyhow::anyhow!("that doesn't look like a valid User-ID"))
}

/// Creates the app's single endpoint, used both to accept and to make connections.
pub async fn bind_endpoint(secret_key: SecretKey) -> Result<Endpoint> {
    configure(Endpoint::builder(presets::N0), secret_key)
        .bind()
        .await
        .context("starting network endpoint")
}

/// An endpoint that only listens on localhost, with no relay or User-ID lookup.
/// Dial it with [`local_addr`]. For tests.
pub async fn bind_local_endpoint(secret_key: SecretKey) -> Result<Endpoint> {
    configure(Endpoint::builder(presets::Minimal), secret_key)
        .relay_mode(RelayMode::Disabled)
        .clear_ip_transports()
        .bind_addr("127.0.0.1:0")?
        .bind()
        .await
        .context("starting local endpoint")
}

/// The direct address of an endpoint made by [`bind_local_endpoint`].
pub fn local_addr(endpoint: &Endpoint) -> EndpointAddr {
    endpoint
        .bound_sockets()
        .into_iter()
        .fold(EndpointAddr::new(endpoint.id()), EndpointAddr::with_ip_addr)
}

fn configure(builder: Builder, secret_key: SecretKey) -> Builder {
    let transport = QuicTransportConfig::builder()
        // Fast detection of a dead peer, and keep NAT mappings alive.
        .keep_alive_interval(Duration::from_secs(1))
        .max_idle_timeout(Some(Duration::from_secs(10).try_into().expect("in range")))
        // Enough for a few large keyframes in flight; old datagrams are dropped past this.
        .datagram_receive_buffer_size(Some(8 * 1024 * 1024))
        .datagram_send_buffer_size(DATAGRAM_SEND_BUFFER)
        // BBR paces to the measured path rate and keeps queues short, and unlike the default
        // (CUBIC) it doesn't halve its rate at every stray lost packet, which on the internet
        // would starve a video stream that never retransmits anyway.
        .congestion_controller_factory(Arc::new(noq_proto::congestion::Bbr3Config::default()))
        .build();
    builder.secret_key(secret_key).alpns(vec![ALPN.to_vec()]).transport_config(transport)
}

/// Bytes of datagrams waiting to be sent on `conn`.
pub fn queued_datagram_bytes(conn: &Connection) -> usize {
    DATAGRAM_SEND_BUFFER.saturating_sub(conn.datagram_send_buffer_space())
}

/// Packets sent and lost so far over all of `conn`'s paths, leaving out the probes that test for
/// larger packet sizes: those are meant to get lost now and then, and say nothing about the path.
pub fn packet_counts(conn: &Connection) -> (u64, u64) {
    conn.paths().iter().map(|p| p.stats()).fold((0, 0), |(sent, lost), s| {
        (
            sent + s.sent_packets.saturating_sub(s.sent_plpmtud_probes),
            lost + s.lost_packets.saturating_sub(s.lost_plpmtud_probes),
        )
    })
}

/// Connects to a host by User-ID (or by full address), giving up after [`CONNECT_TIMEOUT`].
pub async fn connect(endpoint: &Endpoint, addr: impl Into<EndpointAddr>) -> Result<Connection> {
    let addr = addr.into();
    if addr.id == endpoint.id() {
        bail!("that's this PC's own User-ID");
    }
    match tokio::time::timeout(CONNECT_TIMEOUT, endpoint.connect(addr, ALPN)).await {
        Ok(Ok(conn)) => Ok(conn),
        Ok(Err(e)) => {
            Err(anyhow::Error::new(e).context("couldn't reach that User-ID. Is Connect running on the host?"))
        }
        Err(_) => bail!("the host didn't answer. Check the User-ID, and that Connect is running and online there"),
    }
}

/// How the connection currently reaches the peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    /// Direct UDP (LAN or hole-punched): lowest latency.
    Direct,
    /// Via an iroh relay server: works everywhere, adds latency.
    Relay,
    Unknown,
}

impl PathKind {
    pub fn label(self) -> &'static str {
        match self {
            PathKind::Direct => "Direct (P2P)",
            PathKind::Relay => "Relayed",
            PathKind::Unknown => "Connecting…",
        }
    }
}

/// The path QUIC is currently sending on, and its round-trip time.
pub fn selected_path(conn: &Connection) -> (PathKind, Option<Duration>) {
    let paths = conn.paths();
    match paths.iter().find(|p| p.is_selected()) {
        Some(p) if p.is_relay() => (PathKind::Relay, Some(p.rtt())),
        Some(p) => (PathKind::Direct, Some(p.rtt())),
        None => (PathKind::Unknown, None),
    }
}

/// Writes one length-prefixed control message.
pub async fn write_msg<T: Serialize>(send: &mut SendStream, msg: &T) -> Result<()> {
    let bytes = control::encode(msg)?;
    send.write_all(&bytes).await.context("sending control message")?;
    Ok(())
}

/// Reads one length-prefixed control message; `Ok(None)` when the peer finished the stream.
pub async fn read_msg<T: DeserializeOwned>(recv: &mut RecvStream) -> Result<Option<T>> {
    let mut prefix = [0u8; 4];
    match recv.read_exact(&mut prefix).await {
        Ok(()) => {}
        Err(iroh::endpoint::ReadExactError::FinishedEarly(0)) => return Ok(None),
        Err(e) => return Err(e).context("reading control message"),
    }
    let mut body = vec![0u8; control::decode_len(prefix)?];
    recv.read_exact(&mut body).await.context("reading control message")?;
    control::decode(&body).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_id_round_trips_through_display_format() {
        let id = SecretKey::generate().public();
        let shown = format_user_id(&id);
        assert_eq!(shown.split(' ').count(), 8);
        assert_eq!(parse_user_id(&shown).unwrap(), id);
        assert_eq!(parse_user_id(&shown.to_uppercase().replace(' ', "-")).unwrap(), id);
        assert!(parse_user_id("hello").is_err());
    }

    #[test]
    fn identity_persists() {
        let dir = std::env::temp_dir().join(format!("connect-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(load_identity(&dir).unwrap().is_none());
        let a = load_or_create_identity(&dir).unwrap();
        let b = load_or_create_identity(&dir).unwrap();
        assert_eq!(a.public(), b.public());
        assert_eq!(load_identity(&dir).unwrap().unwrap().public(), a.public());
        assert!(create_identity(&dir).is_err(), "an existing account is never overwritten");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
