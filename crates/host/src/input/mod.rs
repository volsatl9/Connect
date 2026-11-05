//! Acting on the client's input: every event is checked against the session's permissions, and
//! whatever the client is holding down is released the moment its permission is turned off or
//! the session ends, so nothing stays stuck on the host.

#[cfg(windows)]
mod windows;

use std::{collections::HashSet, sync::Arc};

use anyhow::Result;
use connect_net as net;
use connect_proto::{
    DeviceClass, Permissions,
    input::{GamepadState, InputEvent, MAX_MOUSE_STEP, MouseButton, TimedInput},
};
use iroh::endpoint::{Connection, RecvStream};
use tokio::sync::{mpsc, oneshot, watch};
use tracing::debug;

use crate::trace::{HostTrace, InputOutcome, InputStamp};

/// Where allowed input goes.
#[derive(Debug, Clone, Default)]
pub enum InputSink {
    /// This PC's mouse, keyboard and (through the ViGEmBus driver) virtual Xbox gamepads.
    /// Windows only; elsewhere input is dropped, since only Windows can host.
    #[default]
    System,
    /// Every event that passes the permission check, releases included. For tests.
    Channel(mpsc::UnboundedSender<InputEvent>),
}

/// Something the client should be told about its input, e.g. that gamepads can't work here.
pub(crate) struct Unavailable {
    pub device: DeviceClass,
    pub reason: String,
}

/// Where the gamepad driver (ViGEmBus) can be downloaded.
pub const GAMEPAD_DRIVER_URL: &str = "https://github.com/nefarius/ViGEmBus/releases";

/// Whether this PC can host virtual gamepads: `Err` says why not.
pub fn gamepad_support() -> Result<(), String> {
    #[cfg(windows)]
    return windows::gamepad_support();
    #[cfg(not(windows))]
    Err("Gamepads need a Windows host.".into())
}

/// Sets up gamepad support with the driver built into the app. Windows asks the user for
/// permission first. Blocks until the installer finishes, so call it off the UI thread.
pub fn install_gamepad_driver() -> Result<(), String> {
    #[cfg(windows)]
    return windows::install_gamepad_driver();
    #[cfg(not(windows))]
    Err("Gamepads need a Windows host.".into())
}

/// Reads the client's input stream until the session ends.
pub(crate) async fn serve(
    conn: Connection,
    mut permissions: watch::Receiver<Permissions>,
    sink: InputSink,
    unavailable: mpsc::UnboundedSender<Unavailable>,
    trace: Arc<HostTrace>,
    taken: oneshot::Sender<()>,
) {
    let recv = match conn.accept_uni().await {
        Ok(recv) => recv,
        Err(e) => return debug!("no input stream: {e}"),
    };
    let _ = taken.send(());
    // Reading isn't cancel-safe (a half-read message would be lost), so it gets its own task.
    let (event_tx, mut events) = mpsc::channel(256);
    tokio::spawn(read_loop(recv, event_tx));

    let mut output = Output::open(sink, unavailable, trace.clone());
    let mut held = Held::default();
    loop {
        tokio::select! {
            input = events.recv() => {
                let Some((TimedInput { seq, client_time_us, event }, received_us)) = input else { break };
                let stamp = InputStamp { seq, client_time_us, received_us };
                if !event.is_valid() {
                    debug!(?event, "ignoring malformed input");
                } else if permissions.borrow().allows(event.device()) {
                    let event = clamp(event);
                    held.note(&event);
                    output.send(event, Some(stamp));
                } else {
                    trace.inputs.push(stamp.record(event.kind(), InputOutcome::NotAllowed));
                }
            }
            Ok(()) = permissions.changed() => {
                let now = *permissions.borrow_and_update();
                for event in held.release_revoked(now) {
                    output.send(event, None);
                }
            }
        }
    }
    for event in held.release_all() {
        output.send(event, None);
    }
}

/// Reads input events, noting when each arrived.
async fn read_loop(mut recv: RecvStream, tx: mpsc::Sender<(TimedInput, u64)>) {
    loop {
        match net::read_msg::<TimedInput>(&mut recv).await {
            Ok(Some(input)) => {
                if tx.send((input, connect_proto::now_micros())).await.is_err() {
                    return;
                }
            }
            Ok(None) => return,
            Err(e) => return debug!("input stream ended: {e:#}"),
        }
    }
}

fn clamp(event: InputEvent) -> InputEvent {
    match event {
        InputEvent::MouseMoveBy { dx, dy } => InputEvent::MouseMoveBy {
            dx: dx.clamp(-MAX_MOUSE_STEP, MAX_MOUSE_STEP),
            dy: dy.clamp(-MAX_MOUSE_STEP, MAX_MOUSE_STEP),
        },
        other => other,
    }
}

/// Hands events to the sink. For [`InputSink::System`] that's a dedicated thread, so a slow
/// driver call never holds up the network. Dropping it waits for that thread to apply what was
/// sent and unplug its gamepads. Events from the viewer carry their stamp, for the trace;
/// releases the host makes up don't.
struct Output {
    send: Option<Sink>,
    #[cfg(windows)]
    thread: Option<std::thread::JoinHandle<()>>,
}

/// Takes an event, and its stamp if it came from the viewer.
type Sink = Box<dyn Fn(InputEvent, Option<InputStamp>) + Send>;

impl Output {
    fn open(sink: InputSink, unavailable: mpsc::UnboundedSender<Unavailable>, trace: Arc<HostTrace>) -> Self {
        match sink {
            InputSink::Channel(tx) => Output {
                send: Some(Box::new(move |event, stamp: Option<InputStamp>| {
                    let _ = tx.send(event);
                    if let Some(stamp) = stamp {
                        trace.inputs.push(stamp.record(event.kind(), InputOutcome::Channel));
                    }
                })),
                #[cfg(windows)]
                thread: None,
            },
            #[cfg(windows)]
            InputSink::System => {
                // A plain channel, so the thread can also wake on a timer to watch the host's input.
                let (tx, rx) = std::sync::mpsc::channel();
                let thread = std::thread::Builder::new()
                    .name("connect-input".into())
                    .spawn(move || windows::run(rx, unavailable, trace))
                    .map_err(|e| tracing::warn!("couldn't start the input thread: {e}"))
                    .ok();
                let send: Sink = Box::new(move |event, stamp| {
                    let _ = tx.send((event, stamp));
                });
                Output { send: thread.as_ref().map(|_| send), thread }
            }
            #[cfg(not(windows))]
            InputSink::System => {
                drop((unavailable, trace));
                Output { send: None }
            }
        }
    }

    fn send(&mut self, event: InputEvent, stamp: Option<InputStamp>) {
        if let Some(send) = &self.send {
            send(event, stamp);
        }
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        self.send = None; // ends the thread once it has applied everything queued
        #[cfg(windows)]
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

const ESCAPE: u16 = 0x01;
const LEFT_CTRL: u16 = 0x1D;
const RIGHT_CTRL: u16 = 0xE01D;
const LEFT_SHIFT: u16 = 0x2A;
const RIGHT_SHIFT: u16 = 0x36;

/// Whether the window in front keeps remote input out, given what's known of its process
/// (`None` when Windows wouldn't say, as with a game under anti-cheat) and if input just failed.
#[cfg_attr(not(windows), allow(dead_code))]
fn keeps_input_out(elevated: Option<bool>, refused: bool) -> bool {
    elevated.unwrap_or(refused)
}

/// What the client is holding down right now, as far as the host has acted on it.
#[derive(Default)]
pub(crate) struct Held {
    keys: HashSet<u16>,
    buttons: HashSet<MouseButton>,
    /// Gamepad slots not in their neutral state.
    pads: HashSet<u8>,
}

impl Held {
    /// Whether `event` would complete Ctrl+Shift+Esc: Task Manager, which runs as administrator
    /// on most PCs and so can't be controlled once it's open.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn opens_task_manager(&self, event: &InputEvent) -> bool {
        let held = |left, right| self.keys.contains(&left) || self.keys.contains(&right);
        matches!(*event, InputEvent::Key { scancode: ESCAPE, down: true })
            && held(LEFT_CTRL, RIGHT_CTRL)
            && held(LEFT_SHIFT, RIGHT_SHIFT)
    }

    pub(crate) fn note(&mut self, event: &InputEvent) {
        match *event {
            InputEvent::Key { scancode, down } => toggle(&mut self.keys, scancode, down),
            InputEvent::MouseButton { button, down } => toggle(&mut self.buttons, button, down),
            InputEvent::Gamepad { pad, state } => toggle(&mut self.pads, pad, state != GamepadState::NEUTRAL),
            InputEvent::GamepadRemoved { pad } => toggle(&mut self.pads, pad, false),
            InputEvent::MouseMoveTo { .. } | InputEvent::MouseMoveBy { .. } | InputEvent::MouseWheel { .. } => {}
        }
    }

    /// Events that let go of everything belonging to devices `permissions` no longer allows.
    fn release_revoked(&mut self, permissions: Permissions) -> Vec<InputEvent> {
        let mut out = Vec::new();
        for device in DeviceClass::ALL {
            if !permissions.allows(device) {
                out.extend(self.release(device));
            }
        }
        out
    }

    fn release_all(&mut self) -> Vec<InputEvent> {
        DeviceClass::ALL.into_iter().flat_map(|d| self.release(d)).collect()
    }

    pub(crate) fn release(&mut self, device: DeviceClass) -> Vec<InputEvent> {
        match device {
            DeviceClass::Audio => Vec::new(),
            DeviceClass::Keyboard => {
                self.keys.drain().map(|scancode| InputEvent::Key { scancode, down: false }).collect()
            }
            DeviceClass::Mouse => {
                self.buttons.drain().map(|button| InputEvent::MouseButton { button, down: false }).collect()
            }
            DeviceClass::Gamepad => {
                self.pads.drain().map(|pad| InputEvent::Gamepad { pad, state: GamepadState::NEUTRAL }).collect()
            }
        }
    }
}

fn toggle<T: std::hash::Hash + Eq>(set: &mut HashSet<T>, item: T, on: bool) {
    if on {
        set.insert(item);
    } else {
        set.remove(&item);
    }
}

/// What the host's own mouse and keyboard have done: running totals, or the change between two.
/// Injected input isn't in here. Whatever is, set host priority off.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct LocalActivity {
    pub mouse_moves: u64,
    /// Distance moved, across and down added up: a hand moves hundreds, a jittery sensor a few.
    pub mouse_pixels: u64,
    /// Events from something that tagged them, as mouse software does.
    pub mouse_tagged: u64,
    pub mouse_buttons: u64,
    pub keys: u64,
}

impl LocalActivity {
    pub fn since(self, earlier: Self) -> Self {
        Self {
            mouse_moves: self.mouse_moves.saturating_sub(earlier.mouse_moves),
            mouse_pixels: self.mouse_pixels.saturating_sub(earlier.mouse_pixels),
            mouse_tagged: self.mouse_tagged.saturating_sub(earlier.mouse_tagged),
            mouse_buttons: self.mouse_buttons.saturating_sub(earlier.mouse_buttons),
            keys: self.keys.saturating_sub(earlier.keys),
        }
    }
}

#[cfg(windows)]
pub(crate) fn local_activity() -> LocalActivity {
    windows::local_activity()
}

#[cfg(not(windows))]
pub(crate) fn local_activity() -> LocalActivity {
    LocalActivity::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_refused_or_admin_window_counts_as_keeping_input_out() {
        assert!(keeps_input_out(Some(true), false));
        assert!(!keeps_input_out(Some(false), true), "not what stopped it");
        assert!(!keeps_input_out(None, false), "a protected game, taking input");
        assert!(keeps_input_out(None, true));
    }

    #[test]
    fn ctrl_shift_esc_is_task_manager() {
        let mut held = Held::default();
        let escape = key(ESCAPE, true);
        assert!(!held.opens_task_manager(&escape));
        held.note(&key(RIGHT_CTRL, true));
        assert!(!held.opens_task_manager(&escape), "Ctrl+Esc is the Start menu");
        held.note(&key(LEFT_SHIFT, true));
        assert!(held.opens_task_manager(&escape));
        assert!(!held.opens_task_manager(&key(ESCAPE, false)), "a release always goes through");
    }

    fn key(scancode: u16, down: bool) -> InputEvent {
        InputEvent::Key { scancode, down }
    }

    #[test]
    fn revoking_a_device_releases_only_what_it_holds() {
        let mut held = Held::default();
        held.note(&key(0x11, true)); // W
        held.note(&key(0x1E, true)); // A
        held.note(&key(0x1E, false));
        held.note(&InputEvent::MouseButton { button: MouseButton::Left, down: true });

        let released = held.release_revoked(Permissions::ALL.with(DeviceClass::Keyboard, false));
        assert_eq!(released, vec![key(0x11, false)]);
        assert!(held.release_revoked(Permissions::ALL.with(DeviceClass::Keyboard, false)).is_empty());

        assert_eq!(held.release_all(), vec![InputEvent::MouseButton { button: MouseButton::Left, down: false }]);
        assert!(held.release_all().is_empty());
    }

    #[test]
    fn gamepads_return_to_neutral() {
        let mut held = Held::default();
        let pressed = GamepadState { buttons: GamepadState::A, ..GamepadState::NEUTRAL };
        held.note(&InputEvent::Gamepad { pad: 1, state: pressed });
        held.note(&InputEvent::Gamepad { pad: 2, state: GamepadState::NEUTRAL });
        assert_eq!(
            held.release_revoked(Permissions::VIEW_ONLY),
            vec![InputEvent::Gamepad { pad: 1, state: GamepadState::NEUTRAL }]
        );
    }

    #[test]
    fn huge_mouse_steps_are_clamped() {
        assert_eq!(
            clamp(InputEvent::MouseMoveBy { dx: i32::MAX, dy: -5 }),
            InputEvent::MouseMoveBy { dx: MAX_MOUSE_STEP, dy: -5 }
        );
    }
}
