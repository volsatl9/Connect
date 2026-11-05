//! The sharing side of a session.
//!
//! [`start`] accepts connections on the app's endpoint. For each one the host user is asked to
//! allow or deny it (unless the caller's User-ID is trusted), choosing which input devices they
//! may use; an allowed peer then gets the screen as H.264 over QUIC datagrams and may drive the
//! permitted devices. Several sessions can run at once, each with its own encoder and bitrate.

mod audio;
mod capture;
mod cursor;
pub mod diagnostics;
mod encode;
#[cfg(windows)]
mod gpu;
mod input;
mod rate;
mod session;
mod trace;
mod uplink;
mod viewer_files;
mod yuv;

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, MutexGuard, PoisonError, RwLock,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use connect_proto::{APPROVAL_TIMEOUT, CLOSE_NORMAL, DeviceClass, Permissions, control::PeerPointer};
use iroh::{Endpoint, EndpointId, endpoint::Connection};
use tokio::sync::{Notify, mpsc, oneshot, watch};

use encode::VideoHub;
use rate::BitrateLimits;
use uplink::Uplink;

pub use capture::VideoSource;
pub use encode::{EncoderInfo, EncoderKind, encoder_capabilities};
pub use input::{GAMEPAD_DRIVER_URL, InputSink, gamepad_support, install_gamepad_driver};

/// Default most a stream may use, and least it's let fall to.
pub const DEFAULT_BITRATE_MBPS: u32 = 100;
pub const DEFAULT_MIN_BITRATE_MBPS: u32 = 3;
/// How many may connect at once until the host says otherwise.
pub const DEFAULT_MAX_CONNECTIONS: u32 = 8;

/// What the host starts with. Everything except `video`, `input` and `approval_timeout` can be
/// changed later through [`HostHandle`].
#[derive(Debug, Clone)]
pub struct HostConfig {
    /// Shown to the connecting user.
    pub host_name: String,
    /// When false, every connection is turned away without asking.
    pub accepting: bool,
    /// User-IDs that are let in without asking, and what each may use. Everyone else gets
    /// whatever the host user picks in [`Decision::Allow`].
    pub trusted: HashMap<EndpointId, Permissions>,
    /// What each stream may use; changes apply to running sessions too.
    pub bitrate: BitrateSettings,
    /// How many may be connected at once, those being asked about included.
    pub max_connections: u32,
    /// Most pictures sent a second, under the screen's refresh rate; 0 for no limit. Applies from
    /// the next session.
    pub frame_rate_limit: u32,
    pub video: VideoSource,
    pub input: InputSink,
    pub approval_timeout: Duration,
}

impl HostConfig {
    pub fn new(host_name: impl Into<String>) -> Self {
        Self {
            host_name: host_name.into(),
            accepting: true,
            trusted: HashMap::new(),
            bitrate: BitrateSettings::default(),
            max_connections: DEFAULT_MAX_CONNECTIONS,
            frame_rate_limit: 0,
            video: VideoSource::default(),
            input: InputSink::default(),
            approval_timeout: APPROVAL_TIMEOUT,
        }
    }
}

/// What each stream may use. The maximum is a ceiling, not a rate to hold: the bitrate follows the
/// connection underneath it in both modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BitrateSettings {
    /// Also fit the bitrate to the picture, so a small or slow one doesn't use all it may.
    pub automatic: bool,
    pub max_mbps: u32,
    pub min_mbps: u32,
}

impl Default for BitrateSettings {
    fn default() -> Self {
        Self { automatic: true, max_mbps: DEFAULT_BITRATE_MBPS, min_mbps: DEFAULT_MIN_BITRATE_MBPS }
    }
}

impl BitrateSettings {
    fn limits(self) -> BitrateLimits {
        BitrateLimits {
            automatic: self.automatic,
            max_bps: self.max_mbps.saturating_mul(1_000_000),
            min_bps: self.min_mbps.saturating_mul(1_000_000),
        }
    }
}

/// Identifies a connection for as long as it lasts.
pub type SessionId = u64;

/// How a session is going, about once a second.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionStats {
    /// Video going out now.
    pub sending_mbps: f32,
    /// What the encoder is aiming for.
    pub target_mbps: f32,
    /// The most it may use now.
    pub max_mbps: f32,
    pub reason: &'static str,
    pub rtt_ms: f32,
    /// Share of packets lost, 0..1.
    pub loss: f32,
    pub jitter_ms: f32,
    pub decode_ms: f32,
    pub relayed: bool,
    pub encoder_limited: bool,
    pub encoder: String,
    pub width: u32,
    pub height: u32,
}

/// Someone connected, or being asked about.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub id: SessionId,
    pub peer: EndpointId,
    pub client_name: String,
    /// Their place in the order, from 1, kept while they stay.
    pub player: u32,
    pub since: Instant,
    pub permissions: Permissions,
    pub stats: SessionStats,
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
    SessionStarted {
        id: SessionId,
        peer: EndpointId,
        client_name: String,
        permissions: Permissions,
    },
    /// `stamp` names the session's trace files.
    SessionEnded {
        id: SessionId,
        peer: EndpointId,
        reason: String,
        stamp: u64,
    },
}

/// Controls a running host. Cheap to clone.
#[derive(Clone)]
pub struct HostHandle(Arc<Shared>);

struct Shared {
    host_name: RwLock<String>,
    accepting: AtomicBool,
    trusted: RwLock<HashMap<EndpointId, Permissions>>,
    bitrate: Mutex<BitrateSettings>,
    max_connections: AtomicU32,
    frame_rate_limit: AtomicU32,
    /// The connections being asked about or served.
    slots: Mutex<HashMap<SessionId, Slot>>,
    next_id: AtomicU64,
    /// Whether more than one may move the mouse, so nobody's moves go to the real pointer.
    sharing_pointer: Arc<AtomicBool>,
    uplink: Mutex<Uplink>,
    /// Signalled when a slot empties, i.e. a session has fully ended and released its input.
    idle: Notify,
    video: VideoSource,
    hub: VideoHub,
    input: InputSink,
    approval_timeout: Duration,
}

/// Starts accepting connections on `endpoint`. Events must be drained, or connection requests stall.
pub fn start(endpoint: Endpoint, config: HostConfig) -> (HostHandle, mpsc::Receiver<HostEvent>) {
    let shared = Arc::new(Shared {
        host_name: RwLock::new(config.host_name),
        accepting: AtomicBool::new(config.accepting),
        trusted: RwLock::new(config.trusted),
        bitrate: Mutex::new(config.bitrate),
        max_connections: AtomicU32::new(config.max_connections),
        frame_rate_limit: AtomicU32::new(config.frame_rate_limit),
        slots: Mutex::new(HashMap::new()),
        next_id: AtomicU64::new(1),
        sharing_pointer: Arc::default(),
        uplink: Mutex::new(Uplink::default()),
        idle: Notify::new(),
        hub: VideoHub::new(config.video.clone()),
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

    /// Changes what everyone connected may use, effective at once: anything they hold on a
    /// device that's turned off is released.
    pub fn set_permissions(&self, permissions: Permissions) {
        for slot in lock(&self.0.slots).values() {
            slot.permissions.send_replace(permissions);
        }
        self.0.refresh_pointer();
    }

    /// Changes what one connector may use, effective at once.
    pub fn set_session_permissions(&self, id: SessionId, permissions: Permissions) {
        if let Some(slot) = lock(&self.0.slots).get(&id) {
            slot.permissions.send_replace(permissions);
        }
        self.0.refresh_pointer();
    }

    /// Turns `device` off for everyone connected.
    pub fn disallow_all(&self, device: DeviceClass) {
        for slot in lock(&self.0.slots).values() {
            let permissions = slot.permissions.borrow().with(device, false);
            slot.permissions.send_replace(permissions);
        }
        self.0.refresh_pointer();
    }

    /// What the longest-connected user may use.
    pub fn permissions(&self) -> Permissions {
        let slots = lock(&self.0.slots);
        slots.iter().min_by_key(|(id, _)| **id).map_or(Permissions::default(), |(_, slot)| *slot.permissions.borrow())
    }

    /// Everyone connected, in the order they joined.
    pub fn sessions(&self) -> Vec<SessionInfo> {
        let slots = lock(&self.0.slots);
        let mut sessions: Vec<SessionInfo> = slots
            .iter()
            .filter(|(_, slot)| slot.active)
            .map(|(&id, slot)| SessionInfo {
                id,
                peer: slot.peer,
                client_name: slot.client_name.clone(),
                player: slot.player,
                since: slot.since,
                permissions: *slot.permissions.borrow(),
                stats: slot.stats.clone(),
            })
            .collect();
        sessions.sort_unstable_by_key(|session| session.id);
        sessions
    }

    /// How many may connect at once, from the next connection on: at least one.
    pub fn set_max_connections(&self, connections: u32) {
        self.0.max_connections.store(connections.max(1), Ordering::Relaxed);
    }

    /// Caps the video bitrate from the next session on.
    pub fn set_frame_rate_limit(&self, fps: u32) {
        self.0.frame_rate_limit.store(fps, Ordering::Relaxed);
    }

    /// New bitrate settings, which running sessions follow at once.
    pub fn set_bitrate(&self, settings: BitrateSettings) {
        *lock(&self.0.bitrate) = settings;
    }

    /// Lets `peer` in without asking, with these permissions from its next session, or with
    /// `None` asks again.
    pub fn set_trusted(&self, peer: EndpointId, permissions: Option<Permissions>) {
        let mut trusted = self.0.trusted.write().unwrap_or_else(PoisonError::into_inner);
        match permissions {
            Some(permissions) => trusted.insert(peer, permissions),
            None => trusted.remove(&peer),
        };
    }

    /// The longest-connected peer, or the one waiting for an answer.
    pub fn current_peer(&self) -> Option<EndpointId> {
        let slots = lock(&self.0.slots);
        slots.iter().min_by_key(|(id, _)| **id).map(|(_, slot)| slot.peer)
    }

    /// Ends every session and pending request.
    pub fn disconnect(&self) {
        for slot in lock(&self.0.slots).values() {
            slot.conn.close(CLOSE_NORMAL.into(), b"The host ended the session.");
        }
    }

    /// Ends one session.
    pub fn disconnect_session(&self, id: SessionId) {
        if let Some(slot) = lock(&self.0.slots).get(&id) {
            slot.conn.close(CLOSE_NORMAL.into(), b"The host ended the session.");
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
            if lock(&self.0.slots).is_empty() {
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

    fn trusted_permissions(&self, peer: &EndpointId) -> Option<Permissions> {
        self.trusted.read().unwrap_or_else(PoisonError::into_inner).get(peer).copied()
    }

    /// Claims a slot for `conn`, unless the host is full or it's already connected.
    fn reserve(self: &Arc<Self>, conn: &Connection, client_name: &str) -> Result<SlotGuard, Refusal> {
        let peer = conn.remote_id();
        let mut slots = lock(&self.slots);
        if slots.values().any(|slot| slot.peer == peer) {
            return Err(Refusal::AlreadyConnected);
        }
        if slots.len() >= self.max_connections.load(Ordering::Relaxed) as usize {
            return Err(Refusal::Full);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let player = (1..).find(|player| slots.values().all(|slot| slot.player != *player)).unwrap_or(1);
        slots.insert(
            id,
            Slot {
                peer,
                conn: conn.clone(),
                client_name: client_name.to_owned(),
                player,
                since: Instant::now(),
                active: false,
                permissions: watch::Sender::new(Permissions::default()),
                stats: SessionStats::default(),
                pointer: None,
            },
        );
        Ok(SlotGuard { shared: self.clone(), id })
    }

    fn limits(&self) -> BitrateLimits {
        lock(&self.bitrate).limits()
    }

    fn report_upload(&self, id: SessionId, now: Instant, report: uplink::Report) -> Option<u32> {
        lock(&self.uplink).report(id, now, report)
    }

    fn set_stats(&self, id: SessionId, stats: SessionStats) {
        if let Some(slot) = lock(&self.slots).get_mut(&id) {
            slot.stats = stats;
        }
    }

    fn set_pointer(&self, id: SessionId, pointer: Option<(u16, u16)>) {
        if let Some(slot) = lock(&self.slots).get_mut(&id) {
            slot.pointer = pointer;
        }
    }

    /// Counts who may use the mouse now.
    fn refresh_pointer(&self) {
        let slots = lock(&self.slots);
        let mice = slots.values().filter(|slot| slot.active && slot.permissions.borrow().allows(DeviceClass::Mouse));
        self.sharing_pointer.store(mice.count() > 1, Ordering::Relaxed);
    }

    /// The other connectors' pointers, as `id` should draw them.
    fn pointers_for(&self, id: SessionId) -> Vec<PeerPointer> {
        let slots = lock(&self.slots);
        let mut pointers: Vec<(SessionId, PeerPointer)> = slots
            .iter()
            .filter(|(other, slot)| {
                **other != id && slot.active && slot.permissions.borrow().allows(DeviceClass::Mouse)
            })
            .filter_map(|(other, slot)| {
                let (x, y) = slot.pointer?;
                Some((*other, PeerPointer { name: slot.client_name.clone(), x, y }))
            })
            .collect();
        pointers.sort_unstable_by_key(|(other, _)| *other);
        pointers.into_iter().map(|(_, pointer)| pointer).collect()
    }
}

/// Why a connection found no slot.
enum Refusal {
    Full,
    AlreadyConnected,
}

/// A connection the host is asking about or serving.
struct Slot {
    peer: EndpointId,
    conn: Connection,
    client_name: String,
    player: u32,
    since: Instant,
    /// Past the host's answer.
    active: bool,
    permissions: watch::Sender<Permissions>,
    stats: SessionStats,
    /// Where their pointer is over the picture, when it's over it.
    pointer: Option<(u16, u16)>,
}

struct SlotGuard {
    shared: Arc<Shared>,
    id: SessionId,
}

impl SlotGuard {
    fn id(&self) -> SessionId {
        self.id
    }

    /// Lets the session in with `permissions`.
    fn start(&self, permissions: Permissions) -> watch::Receiver<Permissions> {
        let mut slots = lock(&self.shared.slots);
        let slot = slots.get_mut(&self.id).expect("held while this guard lives");
        slot.active = true;
        slot.since = Instant::now();
        slot.permissions.send_replace(permissions);
        let receiver = slot.permissions.subscribe();
        drop(slots);
        self.shared.refresh_pointer();
        receiver
    }

    fn subscribe(&self) -> watch::Receiver<Permissions> {
        lock(&self.shared.slots).get(&self.id).expect("held while this guard lives").permissions.subscribe()
    }
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        lock(&self.shared.slots).remove(&self.id);
        lock(&self.shared.uplink).remove(self.id);
        self.shared.refresh_pointer();
        self.shared.idle.notify_waiters();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
