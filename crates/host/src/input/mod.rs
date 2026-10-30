//! Acting on the client's input: every event is checked against the session's permissions, and
//! whatever the client is holding down is released the moment its permission is turned off or
//! the session ends, so nothing stays stuck on the host.

#[cfg(windows)]
mod windows;

use std::collections::HashSet;

use anyhow::Result;
use connect_net as net;
use connect_proto::{
    DeviceClass, Permissions,
    input::{GamepadState, InputEvent, MAX_MOUSE_STEP, MouseButton},
};
use iroh::endpoint::{Connection, RecvStream};
use tokio::sync::{mpsc, watch};
use tracing::debug;

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
) {
    let recv = match conn.accept_uni().await {
        Ok(recv) => recv,
        Err(e) => return debug!("no input stream: {e}"),
    };
    // Reading isn't cancel-safe (a half-read message would be lost), so it gets its own task.
    let (event_tx, mut events) = mpsc::channel(256);
    tokio::spawn(read_loop(recv, event_tx));

    let mut output = Output::open(sink, unavailable);
    let mut held = Held::default();
    loop {
        tokio::select! {
            event = events.recv() => {
                let Some(event) = event else { break };
                if !event.is_valid() {
                    debug!(?event, "ignoring malformed input");
                } else if permissions.borrow().allows(event.device()) {
                    let event = clamp(event);
                    held.note(&event);
                    output.send(event);
                }
            }
            Ok(()) = permissions.changed() => {
                let now = *permissions.borrow_and_update();
                for event in held.release_revoked(now) {
                    output.send(event);
                }
            }
        }
    }
    for event in held.release_all() {
        output.send(event);
    }
}

async fn read_loop(mut recv: RecvStream, tx: mpsc::Sender<InputEvent>) {
    loop {
        match net::read_msg::<InputEvent>(&mut recv).await {
            Ok(Some(event)) => {
                if tx.send(event).await.is_err() {
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
/// sent and unplug its gamepads.
struct Output {
    send: Option<Box<dyn Fn(InputEvent) + Send>>,
    #[cfg(windows)]
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Output {
    fn open(sink: InputSink, unavailable: mpsc::UnboundedSender<Unavailable>) -> Self {
        match sink {
            InputSink::Channel(tx) => Output {
                send: Some(Box::new(move |event| {
                    let _ = tx.send(event);
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
                    .spawn(move || windows::run(rx, unavailable))
                    .map_err(|e| tracing::warn!("couldn't start the input thread: {e}"))
                    .ok();
                let send: Box<dyn Fn(InputEvent) + Send> = Box::new(move |event| {
                    let _ = tx.send(event);
                });
                Output { send: thread.as_ref().map(|_| send), thread }
            }
            #[cfg(not(windows))]
            InputSink::System => {
                drop(unavailable);
                Output { send: None }
            }
        }
    }

    fn send(&mut self, event: InputEvent) {
        if let Some(send) = &self.send {
            send(event);
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

/// What the client is holding down right now, as far as the host has acted on it.
#[derive(Default)]
pub(crate) struct Held {
    keys: HashSet<u16>,
    buttons: HashSet<MouseButton>,
    /// Gamepad slots not in their neutral state.
    pads: HashSet<u8>,
}

impl Held {
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

#[cfg(test)]
mod tests {
    use super::*;

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
