//! Sends the viewer's gamepads to the host while the session window has focus. Each controller
//! gets a slot (0–3) and turns up on the host as a virtual Xbox controller, with its buttons
//! changed as set on the Gamepad page. That happens here, so the host sees an ordinary controller.

use std::{
    collections::{BTreeMap, HashMap},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use connect_client::ClientHandle;
use connect_proto::input::{GamepadState, InputEvent, MAX_GAMEPADS};
use gilrs::{Axis, Button, EventType, Gamepad, GamepadId, Gilrs};
use serde::{Deserialize, Serialize};
use tracing::warn;

/// How long one wait for controller events may block, so stopping and focus changes are
/// noticed promptly. Controller input itself is forwarded as soon as it happens.
const POLL: Duration = Duration::from_millis(50);
/// How far an analogue trigger goes before it counts as pressing a button, as in XInput.
const TRIGGER_PRESSED: u8 = 30;

/// A button on an Xbox controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum XboxButton {
    A,
    B,
    X,
    Y,
    LeftBumper,
    RightBumper,
    LeftTrigger,
    RightTrigger,
    View,
    Menu,
    Guide,
    LeftStick,
    RightStick,
    DPadUp,
    DPadDown,
    DPadLeft,
    DPadRight,
}

impl XboxButton {
    pub const ALL: [XboxButton; 17] = [
        XboxButton::A,
        XboxButton::B,
        XboxButton::X,
        XboxButton::Y,
        XboxButton::LeftBumper,
        XboxButton::RightBumper,
        XboxButton::LeftTrigger,
        XboxButton::RightTrigger,
        XboxButton::View,
        XboxButton::Menu,
        XboxButton::Guide,
        XboxButton::LeftStick,
        XboxButton::RightStick,
        XboxButton::DPadUp,
        XboxButton::DPadDown,
        XboxButton::DPadLeft,
        XboxButton::DPadRight,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            XboxButton::A => "A",
            XboxButton::B => "B",
            XboxButton::X => "X",
            XboxButton::Y => "Y",
            XboxButton::LeftBumper => "Left bumper",
            XboxButton::RightBumper => "Right bumper",
            XboxButton::LeftTrigger => "Left trigger",
            XboxButton::RightTrigger => "Right trigger",
            XboxButton::View => "View",
            XboxButton::Menu => "Menu",
            XboxButton::Guide => "Guide",
            XboxButton::LeftStick => "Left stick click",
            XboxButton::RightStick => "Right stick click",
            XboxButton::DPadUp => "D-pad up",
            XboxButton::DPadDown => "D-pad down",
            XboxButton::DPadLeft => "D-pad left",
            XboxButton::DPadRight => "D-pad right",
        }
    }

    /// As few letters as fit on the button in the drawing.
    pub const fn short(self) -> &'static str {
        match self {
            XboxButton::A => "A",
            XboxButton::B => "B",
            XboxButton::X => "X",
            XboxButton::Y => "Y",
            XboxButton::LeftBumper => "LB",
            XboxButton::RightBumper => "RB",
            XboxButton::LeftTrigger => "LT",
            XboxButton::RightTrigger => "RT",
            XboxButton::View => "View",
            XboxButton::Menu => "Menu",
            XboxButton::Guide => "G",
            XboxButton::LeftStick => "L3",
            XboxButton::RightStick => "R3",
            XboxButton::DPadUp => "↑",
            XboxButton::DPadDown => "↓",
            XboxButton::DPadLeft => "←",
            XboxButton::DPadRight => "→",
        }
    }

    const fn index(self) -> usize {
        self as usize
    }

    /// Its bit in [`GamepadState::buttons`]. The triggers are analogue, so have none.
    const fn bit(self) -> Option<u16> {
        Some(match self {
            XboxButton::A => GamepadState::A,
            XboxButton::B => GamepadState::B,
            XboxButton::X => GamepadState::X,
            XboxButton::Y => GamepadState::Y,
            XboxButton::LeftBumper => GamepadState::LEFT_SHOULDER,
            XboxButton::RightBumper => GamepadState::RIGHT_SHOULDER,
            XboxButton::View => GamepadState::BACK,
            XboxButton::Menu => GamepadState::START,
            XboxButton::Guide => GamepadState::GUIDE,
            XboxButton::LeftStick => GamepadState::LEFT_THUMB,
            XboxButton::RightStick => GamepadState::RIGHT_THUMB,
            XboxButton::DPadUp => GamepadState::DPAD_UP,
            XboxButton::DPadDown => GamepadState::DPAD_DOWN,
            XboxButton::DPadLeft => GamepadState::DPAD_LEFT,
            XboxButton::DPadRight => GamepadState::DPAD_RIGHT,
            XboxButton::LeftTrigger | XboxButton::RightTrigger => return None,
        })
    }

    /// What gilrs calls it: the bumpers are "triggers", the analogue triggers "trigger 2".
    const fn gilrs(self) -> Button {
        match self {
            XboxButton::A => Button::South,
            XboxButton::B => Button::East,
            XboxButton::X => Button::West,
            XboxButton::Y => Button::North,
            XboxButton::LeftBumper => Button::LeftTrigger,
            XboxButton::RightBumper => Button::RightTrigger,
            XboxButton::LeftTrigger => Button::LeftTrigger2,
            XboxButton::RightTrigger => Button::RightTrigger2,
            XboxButton::View => Button::Select,
            XboxButton::Menu => Button::Start,
            XboxButton::Guide => Button::Mode,
            XboxButton::LeftStick => Button::LeftThumb,
            XboxButton::RightStick => Button::RightThumb,
            XboxButton::DPadUp => Button::DPadUp,
            XboxButton::DPadDown => Button::DPadDown,
            XboxButton::DPadLeft => Button::DPadLeft,
            XboxButton::DPadRight => Button::DPadRight,
        }
    }
}

/// How far each button is pressed, 0–255, by [`XboxButton::index`]. Only the triggers go between.
type Pressed = [u8; XboxButton::ALL.len()];

/// What each button does when it isn't what its label says. Saved, so only changes are kept.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ButtonMap(BTreeMap<XboxButton, XboxButton>);

impl ButtonMap {
    pub fn target(&self, button: XboxButton) -> XboxButton {
        self.0.get(&button).copied().unwrap_or(button)
    }

    pub fn set(&mut self, button: XboxButton, target: XboxButton) {
        if button == target {
            self.0.remove(&button);
        } else {
            self.0.insert(button, target);
        }
    }

    pub fn is_default(&self) -> bool {
        self.0.is_empty()
    }

    pub fn reset(&mut self) {
        self.0.clear();
    }

    /// What's left with no button doing it: two buttons doing the same leaves one spare.
    pub fn unassigned(&self) -> Vec<XboxButton> {
        XboxButton::ALL.into_iter().filter(|t| !XboxButton::ALL.iter().any(|b| self.target(*b) == *t)).collect()
    }

    /// The buttons and triggers to send for what's held down. A trigger moved onto the other
    /// trigger stays analogue; a button moved onto a trigger pulls it all the way.
    fn apply(&self, pressed: &Pressed) -> (u16, u8, u8) {
        let (mut buttons, mut left, mut right) = (0, 0u8, 0u8);
        for button in XboxButton::ALL {
            let value = pressed[button.index()];
            match self.target(button) {
                XboxButton::LeftTrigger => left = left.max(value),
                XboxButton::RightTrigger => right = right.max(value),
                target if value >= TRIGGER_PRESSED => buttons |= target.bit().unwrap_or(0),
                _ => {}
            }
        }
        (buttons, left, right)
    }
}

/// Runs on its own thread for the length of a session; stops when dropped.
pub struct Forwarder {
    active: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

impl Forwarder {
    pub fn start(host: ClientHandle, map: ButtonMap) -> Self {
        let active = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (a, s) = (active.clone(), stop.clone());
        if let Err(e) =
            std::thread::Builder::new().name("connect-gamepads".into()).spawn(move || run(&host, &map, &a, &s))
        {
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

fn run(host: &ClientHandle, map: &ButtonMap, active: &AtomicBool, stop: &AtomicBool) {
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
            forward_current(host, map, &gilrs, &mut slots, &mut sent, active.load(Ordering::Relaxed));
            next = gilrs.next_event();
        }
        forward_current(host, map, &gilrs, &mut slots, &mut sent, active.load(Ordering::Relaxed));
    }
    for slot in slots.into_values() {
        host.send_input(InputEvent::GamepadRemoved { pad: slot });
    }
}

fn forward_current(
    host: &ClientHandle,
    map: &ButtonMap,
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
        let state = if active { read(&pad, map) } else { GamepadState::NEUTRAL };
        if sent[slot as usize] != Some(state) {
            sent[slot as usize] = Some(state);
            host.send_input(InputEvent::Gamepad { pad: slot, state });
        }
    }
}

fn pressed(pad: &Gamepad<'_>) -> Pressed {
    XboxButton::ALL.map(|button| match button {
        XboxButton::LeftTrigger | XboxButton::RightTrigger => {
            (pad.button_data(button.gilrs()).map_or(0.0, |d| d.value()).clamp(0.0, 1.0) * 255.0).round() as u8
        }
        _ if pad.is_pressed(button.gilrs()) => 255,
        _ => 0,
    })
}

/// The pad's state in Xbox layout, with its buttons changed as the user asked.
fn read(pad: &Gamepad<'_>, map: &ButtonMap) -> GamepadState {
    let (buttons, left_trigger, right_trigger) = map.apply(&pressed(pad));
    let stick = |axis| (pad.value(axis).clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16;
    GamepadState {
        buttons,
        left_trigger,
        right_trigger,
        left_x: stick(Axis::LeftStickX),
        left_y: stick(Axis::LeftStickY),
        right_x: stick(Axis::RightStickX),
        right_y: stick(Axis::RightStickY),
    }
}

/// Watches the controllers for the Gamepad page, outside a session.
pub struct Tester {
    gilrs: Result<Gilrs, String>,
}

/// What the page shows from the controllers.
#[derive(Default)]
pub struct Tested {
    pub connected: bool,
    /// Held down, by label on the controller.
    pub held: Vec<XboxButton>,
    /// What the host would be sent for that.
    pub sends: Vec<XboxButton>,
}

impl Tester {
    pub fn new() -> Self {
        Self { gilrs: Gilrs::new().map_err(|e| e.to_string()) }
    }

    pub fn error(&self) -> Option<&str> {
        self.gilrs.as_ref().err().map(String::as_str)
    }

    pub fn poll(&mut self, map: &ButtonMap) -> Tested {
        let Ok(gilrs) = &mut self.gilrs else { return Tested::default() };
        while gilrs.next_event().is_some() {}
        let mut tested = Tested::default();
        let (mut buttons, mut left, mut right) = (0, 0, 0);
        for (_, pad) in gilrs.gamepads() {
            tested.connected = true;
            let pressed = pressed(&pad);
            let held = |b: &XboxButton| pressed[b.index()] >= TRIGGER_PRESSED;
            tested.held.extend(XboxButton::ALL.iter().filter(|b| held(b)));
            let (pad_buttons, pad_left, pad_right) = map.apply(&pressed);
            buttons |= pad_buttons;
            left = left.max(pad_left);
            right = right.max(pad_right);
        }
        tested.sends = XboxButton::ALL
            .into_iter()
            .filter(|b| match b {
                XboxButton::LeftTrigger => left >= TRIGGER_PRESSED,
                XboxButton::RightTrigger => right >= TRIGGER_PRESSED,
                _ => b.bit().is_some_and(|bit| buttons & bit != 0),
            })
            .collect();
        tested
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn held(buttons: &[(XboxButton, u8)]) -> Pressed {
        let mut pressed = [0; XboxButton::ALL.len()];
        for (button, value) in buttons {
            pressed[button.index()] = *value;
        }
        pressed
    }

    #[test]
    fn unchanged_buttons_do_what_they_say() {
        let map = ButtonMap::default();
        for button in XboxButton::ALL {
            let (buttons, left, right) = map.apply(&held(&[(button, 255)]));
            match button {
                XboxButton::LeftTrigger => assert_eq!((buttons, left, right), (0, 255, 0)),
                XboxButton::RightTrigger => assert_eq!((buttons, left, right), (0, 0, 255)),
                _ => assert_eq!((buttons, left, right), (button.bit().unwrap(), 0, 0), "{button:?}"),
            }
        }
    }

    #[test]
    fn every_button_has_its_own_bit_and_place() {
        let bits: Vec<u16> = XboxButton::ALL.iter().filter_map(|b| b.bit()).collect();
        assert_eq!(bits.iter().fold(0, |all, bit| all | bit).count_ones() as usize, bits.len());
        assert!(XboxButton::ALL.iter().enumerate().all(|(i, b)| b.index() == i));
    }

    #[test]
    fn swapped_buttons_swap() {
        let mut map = ButtonMap::default();
        map.set(XboxButton::A, XboxButton::B);
        map.set(XboxButton::B, XboxButton::A);
        assert_eq!(map.apply(&held(&[(XboxButton::A, 255)])).0, GamepadState::B);
        assert_eq!(map.apply(&held(&[(XboxButton::B, 255)])).0, GamepadState::A);
        assert_eq!(map.apply(&held(&[(XboxButton::X, 255)])).0, GamepadState::X);
    }

    #[test]
    fn two_buttons_can_do_the_same_thing() {
        let mut map = ButtonMap::default();
        map.set(XboxButton::X, XboxButton::A);
        assert_eq!(map.apply(&held(&[(XboxButton::X, 255), (XboxButton::A, 255)])).0, GamepadState::A);
        assert_eq!(map.unassigned(), [XboxButton::X]);
    }

    #[test]
    fn triggers_stay_analogue_between_them_and_digital_elsewhere() {
        let mut map = ButtonMap::default();
        map.set(XboxButton::LeftTrigger, XboxButton::RightTrigger);
        assert_eq!(map.apply(&held(&[(XboxButton::LeftTrigger, 100)])), (0, 0, 100));

        map.set(XboxButton::LeftTrigger, XboxButton::A);
        assert_eq!(map.apply(&held(&[(XboxButton::LeftTrigger, 100)])), (GamepadState::A, 0, 0));
        assert_eq!(map.apply(&held(&[(XboxButton::LeftTrigger, 10)])), (0, 0, 0), "a resting trigger");

        map.set(XboxButton::A, XboxButton::LeftTrigger);
        assert_eq!(map.apply(&held(&[(XboxButton::A, 255)])), (0, 255, 0));
    }

    #[test]
    fn setting_a_button_back_forgets_the_change() {
        let mut map = ButtonMap::default();
        map.set(XboxButton::Y, XboxButton::X);
        assert!(!map.is_default());
        map.set(XboxButton::Y, XboxButton::Y);
        assert!(map.is_default());
    }

    #[test]
    fn saves_and_loads() {
        let mut map = ButtonMap::default();
        map.set(XboxButton::DPadUp, XboxButton::RightTrigger);
        let text = ron::to_string(&map).unwrap();
        assert_eq!(ron::from_str::<ButtonMap>(&text).unwrap(), map);
    }
}
