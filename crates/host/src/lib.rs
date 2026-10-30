//! The sharing side of a session.
//!
//! [`start`] accepts connections on the app's endpoint. For each one the host user is asked to
//! allow or deny it (unless the caller's User-ID is trusted), choosing which input devices they
//! may use; an allowed peer then gets the screen as H.264 over QUIC datagrams and may drive the
//! permitted devices. One session runs at a time.

mod audio;
mod capture;
mod cursor;
mod encode;
mod input;
mod rate;
mod session;
mod yuv;

use std::{
    collections::HashSet,
    sync::{
        Arc, Mutex, MutexGuard, PoisonError, RwLock,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::Duration,
};

use connect_proto::{APPROVAL_TIMEOUT, CLOSE_NORMAL, Permissions};
use iroh::{Endpoint, EndpointId, endpoint::Connection};
use tokio::sync::{Notify, mpsc, oneshot, watch};

pub use capture::VideoSource;
pub use input::{GAMEPAD_DRIVER_URL, InputSink, gamepad_support, install_gamepad_driver};

/// Default cap on the video bitrate.
pub const DEFAULT_BITRATE_MBPS: u32 = 20;

/// What the host starts with. Everything except `video`, `input` and `approval_timeout` can be
/// changed later through [`HostHandle`].
#[derive(Debug, Clone)]
pub struct HostConfig {
    /// Shown to the connecting user.
    pub host_name: String,
    /// When false, every connection is turned away without asking.
    pub accepting: bool,
    /// What a trusted device may use, since it isn't asked about; prompted connections get
    /// whatever the host user picks in [`Decision::Allow`].
    pub default_permissions: Permissions,
    /// User-IDs that are let in without asking.
    pub trusted: HashSet<EndpointId>,
    /// Upper bound on the video bitrate, in megabits per second; applies from the next session.
    pub bitrate_mbps: u32,
    pub video: VideoSource,
    pub input: InputSink,
    pub approval_timeout: Duration,
}

impl HostConfig {
    pub fn new(host_name: impl Into<String>) -> Self {
        Self {
            host_name: host_name.into(),
            accepting: true,
            default_permissions: Permissions::default(),
            trusted: HashSet::new(),
            bitrate_mbps: DEFAULT_BITRATE_MBPS,
            video: VideoSource::default(),
            input: InputSink::default(),
            approval_timeout: APPROVAL_TIMEOUT,
        }
    }
}

/// The host user's answer to a [`ConnectionRequest`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Let them in, able to use these input devices (changeable later with
    /// [`HostHandle::set_permissions`]).
    Allow(Permissions),
    Deny,
}

/// Someone wants to connect. Answer with [`respond`](Self::respond) before the approval
/// timeout runs out; dropping the request unanswered denies it.
#[derive(Debug)]
pub struct ConnectionRequest {
    /// The requester's User-ID, proven by the connection handshake.
    pub peer: EndpointId,
    /// The name the requester gave their PC. Not verified: anyone can claim any name.
    pub client_name: String,
    respond: oneshot::Sender<Decision>,
}

impl ConnectionRequest {
    pub fn respond(self, decision: Decision) {
        let _ = self.respond.send(decision);
    }

    /// False once the request timed out or the requester gave up, so its prompt should close.
    pub fn is_pending(&self) -> bool {
        !self.respond.is_closed()
    }
}

#[derive(Debug)]
pub enum HostEvent {
    Request(ConnectionRequest),
    SessionStarted { peer: EndpointId, client_name: String, permissions: Permissions },
    SessionEnded { peer: EndpointId, reason: String },
}

/// Controls a running host. Cheap to clone.
#[derive(Clone)]
pub struct HostHandle(Arc<Shared>);

struct Shared {
    host_name: RwLock<String>,
    accepting: AtomicBool,
    default_permissions: RwLock<Permissions>,
    /// The running session's permissions; reset when a session starts.
    permissions: watch::Sender<Permissions>,
    trusted: RwLock<HashSet<EndpointId>>,
    bitrate_mbps: AtomicU32,
    /// The one connection being asked about or served; anyone else is told the host is busy.
    slot: Mutex<Option<(EndpointId, Connection)>>,
    /// Signalled when the slot empties, i.e. a session has fully ended and released its input.
    idle: Notify,
    video: VideoSource,
    input: InputSink,
    approval_timeout: Duration,
}

/// Starts accepting connections on `endpoint`. Events must be drained, or connection requests stall.
pub fn start(endpoint: Endpoint, config: HostConfig) -> (HostHandle, mpsc::Receiver<HostEvent>) {
    let shared = Arc::new(Shared {
        host_name: RwLock::new(config.host_name),
        accepting: AtomicBool::new(config.accepting),
        default_permissions: RwLock::new(config.default_permissions),
        permissions: watch::Sender::new(config.default_permissions),
        trusted: RwLock::new(config.trusted),
        bitrate_mbps: AtomicU32::new(config.bitrate_mbps),
        slot: Mutex::new(None),
        idle: Notify::new(),
        video: config.video,
        input: config.input,
        approval_timeout: config.approval_timeout,
    });
    let (events_tx, events_rx) = mpsc::channel(16);
    tokio::spawn(accept_loop(endpoint, shared.clone(), events_tx));
    (HostHandle(shared), events_rx)
}

async fn accept_loop(endpoint: Endpoint, shared: Arc<Shared>, events: mpsc::Sender<HostEvent>) {
    while let Some(incoming) = endpoint.accept().await {
        tokio::spawn(session::run(incoming, shared.clone(), events.clone()));
    }
    tracing::debug!("endpoint closed; host stopped");
}

impl HostHandle {
    pub fn set_host_name(&self, name: impl Into<String>) {
        *self.0.host_name.write().unwrap_or_else(PoisonError::into_inner) = name.into();
    }

    pub fn set_accepting(&self, accepting: bool) {
        self.0.accepting.store(accepting, Ordering::Relaxed);
    }

    pub fn accepting(&self) -> bool {
        self.0.accepting.load(Ordering::Relaxed)
    }

    /// Changes which input devices the connected user may use, effective at once: anything they
    /// hold on a device that's turned off is released.
    pub fn set_permissions(&self, permissions: Permissions) {
        self.0.permissions.send_replace(permissions);
    }

    /// The connected user's current permissions.
    pub fn permissions(&self) -> Permissions {
        *self.0.permissions.borrow()
    }

    /// What trusted devices, which aren't asked about, may use from their next session.
    pub fn set_default_permissions(&self, permissions: Permissions) {
        *self.0.default_permissions.write().unwrap_or_else(PoisonError::into_inner) = permissions;
    }

    /// Caps the video bitrate from the next session on.
    pub fn set_bitrate_mbps(&self, mbps: u32) {
        self.0.bitrate_mbps.store(mbps.max(1), Ordering::Relaxed);
    }

    pub fn set_trusted(&self, peer: EndpointId, trusted: bool) {
        let mut set = self.0.trusted.write().unwrap_or_else(PoisonError::into_inner);
        if trusted {
            set.insert(peer);
        } else {
            set.remove(&peer);
        }
    }

    /// The peer currently connected or waiting for an answer.
    pub fn current_peer(&self) -> Option<EndpointId> {
        lock(&self.0.slot).as_ref().map(|(peer, _)| *peer)
    }

    /// Ends the current session or pending request, if any.
    pub fn disconnect(&self) {
        if let Some((_, conn)) = &*lock(&self.0.slot) {
            conn.close(CLOSE_NORMAL.into(), b"The host ended the session.");
        }
    }

    /// Stops accepting, ends any session, and waits until it has let go of everything the remote
    /// user was holding (keys, buttons, gamepads). Call before the app exits.
    pub async fn shutdown(&self) {
        self.set_accepting(false);
        loop {
            let idle = self.0.idle.notified();
            tokio::pin!(idle);
            idle.as_mut().enable();
            if lock(&self.0.slot).is_none() {
                return;
            }
            self.disconnect();
            idle.await;
        }
    }
}

impl Shared {
    fn host_name(&self) -> String {
        self.host_name.read().unwrap_or_else(PoisonError::into_inner).clone()
    }

    fn default_permissions(&self) -> Permissions {
        *self.default_permissions.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn is_trusted(&self, peer: &EndpointId) -> bool {
        self.trusted.read().unwrap_or_else(PoisonError::into_inner).contains(peer)
    }

    /// Claims the session slot for `conn`, or `None` if another peer holds it.
    fn reserve(&self, conn: &Connection) -> Option<SlotGuard<'_>> {
        let mut slot = lock(&self.slot);
        if slot.is_some() {
            return None;
        }
        *slot = Some((conn.remote_id(), conn.clone()));
        Some(SlotGuard(self))
    }
}

struct SlotGuard<'a>(&'a Shared);

impl Drop for SlotGuard<'_> {
    fn drop(&mut self) {
        *lock(&self.0.slot) = None;
        self.0.idle.notify_waiters();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
