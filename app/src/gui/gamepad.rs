//! Sends the viewer's gamepads to the host while the session window has focus. Each controller
//! gets a slot (0–3) and turns up on the host as a virtual Xbox controller.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use connect_client::ClientHandle;
use connect_proto::input::{GamepadState, InputEvent, MAX_GAMEPADS};
use gilrs::{Axis, Button, EventType, Gamepad, GamepadId, Gilrs};
use tracing::warn;

/// How long one wait for controller events may block, so stopping and focus changes are
/// noticed promptly. Controller input itself is forwarded as soon as it happens.
const POLL: Duration = Duration::from_millis(50);

/// Runs on its own thread for the length of a session; stops when dropped.
pub struct Forwarder {
    active: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

impl Forwarder {
    pub fn start(host: ClientHandle) -> Self {
        let active = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (a, s) = (active.clone(), stop.clone());
        if let Err(e) = std::thread::Builder::new().name("connect-gamepads".into()).spawn(move || run(&host, &a, &s)) {
            warn!("couldn't start the gamepad thread: {e}");
        }
        Self { active, stop }
    }

    /// While inactive (window unfocused, or gamepads not allowed), every pad reads as idle.
    pub fn set_active(&self, active: bool) {
        self.active.store(active, Ordering::Relaxed);
    }
}

impl Drop for Forwarder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn run(host: &ClientHandle, active: &AtomicBool, stop: &AtomicBool) {
    let mut gilrs = match Gilrs::new() {
        Ok(gilrs) => gilrs,
        Err(e) => return warn!("gamepads unavailable: {e}"),
    };
    let mut slots: HashMap<GamepadId, u8> = HashMap::new();
    let mut sent = [None::<GamepadState>; MAX_GAMEPADS as usize];

    while !stop.load(Ordering::Relaxed) {
        // Drain everything pending: gilrs only updates its state as events are taken.
        let mut next = gilrs.next_event_blocking(Some(POLL));
        while let Some(event) = next {
            if event.event == EventType::Disconnected
                && let Some(slot) = slots.remove(&event.id)
            {
                sent[slot as usize] = None;
                host.send_input(InputEvent::GamepadRemoved { pad: slot });
            }
            // Preserve queued press/release edges instead of collapsing a burst to its final state.
            forward_current(host, &gilrs, &mut slots, &mut sent, active.load(Ordering::Relaxed));
            next = gilrs.next_event();
        }
        forward_current(host, &gilrs, &mut slots, &mut sent, active.load(Ordering::Relaxed));
    }
    for slot in slots.into_values() {
        host.send_input(InputEvent::GamepadRemoved { pad: slot });
    }
}

fn forward_current(
    host: &ClientHandle,
    gilrs: &Gilrs,
    slots: &mut HashMap<GamepadId, u8>,
    sent: &mut [Option<GamepadState>; MAX_GAMEPADS as usize],
    active: bool,
) {
    for (id, pad) in gilrs.gamepads() {
        let slot = match slots.get(&id) {
            Some(&slot) => slot,
            None => {
                let Some(free) = (0..MAX_GAMEPADS).find(|s| !slots.values().any(|v| v == s)) else { continue };
                slots.insert(id, free);
                free
            }
        };
        let state = if active { read(&pad) } else { GamepadState::NEUTRAL };
        if sent[slot as usize] != Some(state) {
            sent[slot as usize] = Some(state);
            host.send_input(InputEvent::Gamepad { pad: slot, state });
        }
    }
}

const BUTTONS: [(Button, u16); 15] = [
    (Button::South, GamepadState::A),
    (Button::East, GamepadState::B),
    (Button::West, GamepadState::X),
    (Button::North, GamepadState::Y),
    (Button::LeftTrigger, GamepadState::LEFT_SHOULDER),
    (Button::RightTrigger, GamepadState::RIGHT_SHOULDER),
    (Button::Select, GamepadState::BACK),
    (Button::Start, GamepadState::START),
    (Button::Mode, GamepadState::GUIDE),
    (Button::LeftThumb, GamepadState::LEFT_THUMB),
    (Button::RightThumb, GamepadState::RIGHT_THUMB),
    (Button::DPadUp, GamepadState::DPAD_UP),
    (Button::DPadDown, GamepadState::DPAD_DOWN),
    (Button::DPadLeft, GamepadState::DPAD_LEFT),
    (Button::DPadRight, GamepadState::DPAD_RIGHT),
];

/// The pad's state in Xbox layout. gilrs calls the bumpers "triggers" and the analogue
/// triggers "trigger 2".
fn read(pad: &Gamepad<'_>) -> GamepadState {
    let buttons = BUTTONS.iter().filter(|(button, _)| pad.is_pressed(*button)).fold(0, |all, (_, bit)| all | bit);
    let trigger = |button| (pad.button_data(button).map_or(0.0, |d| d.value()).clamp(0.0, 1.0) * 255.0).round() as u8;
    let stick = |axis| (pad.value(axis).clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16;
    GamepadState {
        buttons,
        left_trigger: trigger(Button::LeftTrigger2),
        right_trigger: trigger(Button::RightTrigger2),
        left_x: stick(Axis::LeftStickX),
        left_y: stick(Axis::LeftStickY),
        right_x: stick(Axis::RightStickX),
        right_y: stick(Axis::RightStickY),
    }
}
