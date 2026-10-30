//! Turns the session window's mouse and keyboard events into input for the host.
//!
//! Keys go by physical position (scancode), so the host's keyboard layout decides what they
//! type, and WASD stays WASD in games. The mouse either follows the pointer over the video
//! (desktop use) or, once captured, sends raw relative motion (games that turn the camera).

use std::collections::HashSet;

use connect_client::ClientHandle;
use connect_proto::{
    DeviceClass, Permissions,
    input::{InputEvent, MouseButton, WHEEL_NOTCH, scancode},
};
use eframe::egui::{Event, Key, MouseWheelUnit, PointerButton, Pos2, Rect, Vec2};

/// Things the viewer handles itself; their keys never reach the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hotkey {
    /// Ctrl+Alt+F
    Fullscreen,
    /// Ctrl+Alt+S
    Stats,
    /// Ctrl+Alt+M
    MouseCapture,
}

#[derive(Default)]
pub struct InputCapture {
    /// The mouse is captured for a game: hidden, confined to the window, sending raw motion.
    pub captured: bool,
    /// Scancodes sent down and not yet up.
    keys: HashSet<u16>,
    buttons: HashSet<MouseButton>,
    /// Positions of hotkey keys held down, whose release must not reach the host either.
    swallowed: HashSet<Key>,
    /// Pointer position not sent yet, and the last one sent.
    position: Option<(u16, u16)>,
    sent_position: Option<(u16, u16)>,
    /// Relative motion and wheel movement not sent yet, kept fractional so slow movement adds up.
    motion: Vec2,
    wheel: Vec2,
}

impl InputCapture {
    /// Sends this frame's `events` to the host. `video` is where the remote screen is drawn;
    /// clicks inside `blocked` (the toolbar) stay local. Returns the hotkeys pressed.
    pub fn process(
        &mut self,
        events: &[Event],
        video: Rect,
        blocked: Option<Rect>,
        pointer: Option<Pos2>,
        permissions: Permissions,
        host: &ClientHandle,
    ) -> Vec<Hotkey> {
        let mouse = permissions.allows(DeviceClass::Mouse);
        let keyboard = permissions.allows(DeviceClass::Keyboard);
        let over_video = |pos: Pos2| video.contains(pos) && !blocked.is_some_and(|b| b.contains(pos));
        let mut hotkeys = Vec::new();

        for event in events {
            match event {
                Event::Key { key, physical_key, pressed, modifiers, .. } => {
                    // Hotkeys go by the letter on the key; everything sent goes by position.
                    let position = physical_key.unwrap_or(*key);
                    if *pressed
                        && modifiers.ctrl
                        && modifiers.alt
                        && let Some(hotkey) = hotkey_of(*key)
                    {
                        if self.swallowed.insert(position) {
                            hotkeys.push(hotkey);
                        }
                        continue;
                    }
                    if !*pressed && self.swallowed.remove(&position) {
                        continue;
                    }
                    if keyboard && let Some(code) = scancode_of(position) {
                        self.key(host, code, *pressed);
                    }
                }
                // With the usual clipboard shortcuts on, Ctrl+C/X/V arrive as these instead of key
                // presses; send a whole press, as the key's release may be at another position
                // (Dvorak). The host pastes from its own clipboard.
                Event::Copy if keyboard => tap(host, scancode::C),
                Event::Cut if keyboard => tap(host, scancode::X),
                Event::Paste(_) if keyboard => tap(host, scancode::V),
                Event::PointerMoved(pos) if mouse && !self.captured && over_video(*pos) => {
                    self.position = Some(normalise(*pos, video));
                }
                Event::MouseMoved(delta) if mouse && self.captured => self.motion += *delta,
                Event::PointerButton { pos, button, pressed, .. } if mouse => {
                    let Some(button) = mouse_button(*button) else { continue };
                    if *pressed && (self.captured || over_video(*pos)) {
                        if !self.captured {
                            self.position = Some(normalise(*pos, video));
                        }
                        self.flush_motion(host);
                        self.buttons.insert(button);
                        host.send_input(InputEvent::MouseButton { button, down: true });
                    } else if !*pressed && self.buttons.remove(&button) {
                        self.flush_motion(host);
                        host.send_input(InputEvent::MouseButton { button, down: false });
                    }
                }
                Event::MouseWheel { unit, delta, .. }
                    if mouse && (self.captured || pointer.is_some_and(over_video)) =>
                {
                    self.flush_motion(host);
                    self.wheel += *delta * notches_per_unit(*unit);
                    let step = whole(self.wheel);
                    self.wheel -= step;
                    if step != Vec2::ZERO {
                        // egui's positive x moves content right, i.e. scrolls left.
                        host.send_input(InputEvent::MouseWheel { dx: -step.x as i32, dy: step.y as i32 });
                    }
                }
                Event::WindowFocused(false) => self.release_all(host),
                _ => {}
            }
        }
        self.flush_motion(host);
        hotkeys
    }

    /// Forgets what the host has already let go of because a permission was turned off.
    pub fn permissions_changed(&mut self, permissions: Permissions) {
        if !permissions.allows(DeviceClass::Keyboard) {
            self.keys.clear();
        }
        if !permissions.allows(DeviceClass::Mouse) {
            self.buttons.clear();
        }
    }

    /// Lets go of every key and button, e.g. when the window loses focus mid-press.
    pub fn release_all(&mut self, host: &ClientHandle) {
        for scancode in self.keys.drain() {
            host.send_input(InputEvent::Key { scancode, down: false });
        }
        for button in self.buttons.drain() {
            host.send_input(InputEvent::MouseButton { button, down: false });
        }
        self.swallowed.clear();
    }

    fn key(&mut self, host: &ClientHandle, scancode: u16, down: bool) {
        // A repeat while held is sent as another key-down, which is what auto-repeat looks like.
        if down {
            self.keys.insert(scancode);
        } else if !self.keys.remove(&scancode) {
            return;
        }
        host.send_input(InputEvent::Key { scancode, down });
    }

    fn flush_motion(&mut self, host: &ClientHandle) {
        if let Some(position) = self.position.take()
            && self.sent_position != Some(position)
        {
            self.sent_position = Some(position);
            host.send_input(InputEvent::MouseMoveTo { x: position.0, y: position.1 });
        }
        let step = whole(self.motion);
        if step != Vec2::ZERO {
            self.motion -= step;
            host.send_input(InputEvent::MouseMoveBy { dx: step.x as i32, dy: step.y as i32 });
        }
    }
}

fn hotkey_of(key: Key) -> Option<Hotkey> {
    match key {
        Key::F => Some(Hotkey::Fullscreen),
        Key::S => Some(Hotkey::Stats),
        Key::M => Some(Hotkey::MouseCapture),
        _ => None,
    }
}

fn tap(host: &ClientHandle, scancode: u16) {
    host.send_input(InputEvent::Key { scancode, down: true });
    host.send_input(InputEvent::Key { scancode, down: false });
}

/// The whole-number part of `v`, rounded towards zero.
fn whole(v: Vec2) -> Vec2 {
    Vec2::new(v.x.trunc(), v.y.trunc())
}

/// `pos` inside `video` as 0..=65535 on each axis.
pub(super) fn normalise(pos: Pos2, video: Rect) -> (u16, u16) {
    let fraction = |v: f32, min: f32, size: f32| ((v - min) / size).clamp(0.0, 1.0);
    let to_u16 = |f: f32| (f * u16::MAX as f32).round() as u16;
    (to_u16(fraction(pos.x, video.min.x, video.width())), to_u16(fraction(pos.y, video.min.y, video.height())))
}

fn notches_per_unit(unit: MouseWheelUnit) -> f32 {
    let notch = WHEEL_NOTCH as f32;
    match unit {
        MouseWheelUnit::Line => notch,
        MouseWheelUnit::Page => notch * 3.0,
        // Trackpads: about 50 points of scrolling per notch.
        MouseWheelUnit::Point => notch / 50.0,
    }
}

fn mouse_button(button: PointerButton) -> Option<MouseButton> {
    Some(match button {
        PointerButton::Primary => MouseButton::Left,
        PointerButton::Secondary => MouseButton::Right,
        PointerButton::Middle => MouseButton::Middle,
        PointerButton::Extra1 => MouseButton::Back,
        PointerButton::Extra2 => MouseButton::Forward,
    })
}

/// The PS/2 set-1 scancode of the key at `key`'s position, `0xE0__` for extended keys.
fn scancode_of(key: Key) -> Option<u16> {
    use Key::*;
    Some(match key {
        Escape => 0x01,
        Num1 | Exclamationmark => 0x02,
        Num2 => 0x03,
        Num3 => 0x04,
        Num4 => 0x05,
        Num5 => 0x06,
        Num6 => 0x07,
        Num7 => 0x08,
        Num8 => 0x09,
        Num9 => 0x0A,
        Num0 => 0x0B,
        Minus => 0x0C,
        Equals => 0x0D,
        Backspace => 0x0E,
        Tab => 0x0F,
        Q => 0x10,
        W => 0x11,
        E => 0x12,
        R => 0x13,
        T => 0x14,
        Y => 0x15,
        U => 0x16,
        I => 0x17,
        O => 0x18,
        P => 0x19,
        OpenBracket | OpenCurlyBracket => 0x1A,
        CloseBracket | CloseCurlyBracket => 0x1B,
        Enter => 0x1C,
        ControlLeft => scancode::LEFT_CTRL,
        A => 0x1E,
        S => 0x1F,
        D => 0x20,
        F => 0x21,
        G => 0x22,
        H => 0x23,
        J => 0x24,
        K => 0x25,
        L => 0x26,
        Semicolon | Colon => 0x27,
        Quote => 0x28,
        Backtick => 0x29,
        ShiftLeft => scancode::LEFT_SHIFT,
        Backslash | Pipe => 0x2B,
        Z => 0x2C,
        X => scancode::X,
        C => scancode::C,
        V => scancode::V,
        B => 0x30,
        N => 0x31,
        M => 0x32,
        Comma => 0x33,
        Period => 0x34,
        Slash | Questionmark => 0x35,
        ShiftRight => 0x36,
        AltLeft => scancode::LEFT_ALT,
        Space => 0x39,
        CapsLock => 0x3A,
        F1 => 0x3B,
        F2 => 0x3C,
        F3 => 0x3D,
        F4 => 0x3E,
        F5 => 0x3F,
        F6 => 0x40,
        F7 => 0x41,
        F8 => 0x42,
        F9 => 0x43,
        F10 => 0x44,
        NumLock => 0x45,
        ScrollLock => 0x46,
        // Only the numpad has a physical plus key.
        Plus => 0x4E,
        IntlBackslash => 0x56,
        F11 => 0x57,
        F12 => 0x58,
        F13 => 0x64,
        F14 => 0x65,
        F15 => 0x66,
        F16 => 0x67,
        F17 => 0x68,
        F18 => 0x69,
        F19 => 0x6A,
        F20 => 0x6B,
        F21 => 0x6C,
        F22 => 0x6D,
        F23 => 0x6E,
        F24 => 0x76,
        MediaTrackPrevious => 0xE010,
        MediaTrackNext => 0xE019,
        ControlRight => 0xE01D,
        AudioVolumeMute => 0xE020,
        LaunchApp2 => 0xE021,
        MediaPlayPause => 0xE022,
        MediaStop => 0xE024,
        AudioVolumeDown => 0xE02E,
        AudioVolumeUp => 0xE030,
        BrowserHome => 0xE032,
        PrintScreen => 0xE037,
        AltRight => 0xE038,
        Home => 0xE047,
        ArrowUp => 0xE048,
        PageUp => 0xE049,
        ArrowLeft => 0xE04B,
        ArrowRight => 0xE04D,
        End => 0xE04F,
        ArrowDown => 0xE050,
        PageDown => 0xE051,
        Insert => 0xE052,
        Delete => 0xE053,
        SuperLeft => 0xE05B,
        SuperRight => 0xE05C,
        Menu => 0xE05D,
        BrowserSearch => 0xE065,
        BrowserFavorites => 0xE066,
        BrowserRefresh => 0xE067,
        BrowserStop => 0xE068,
        BrowserForward => 0xE069,
        BrowserBack => 0xE06A,
        LaunchApp1 => 0xE06B,
        LaunchMail => 0xE06C,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use eframe::egui;

    use super::*;

    #[test]
    fn every_mapped_key_is_a_valid_scancode() {
        let keys = Key::ALL;
        let mut seen = HashSet::new();
        for &key in keys {
            if let Some(code) = scancode_of(key) {
                assert!(InputEvent::Key { scancode: code, down: true }.is_valid(), "{key:?} → {code:#x}");
                seen.insert(code);
            }
        }
        // Letters, digits, F1–F12, arrows and modifiers at least.
        assert!(seen.len() > 90, "only {} keys mapped", seen.len());
    }

    #[test]
    fn positions_cover_the_whole_video() {
        let video = Rect::from_min_size(egui::pos2(100.0, 50.0), egui::vec2(800.0, 450.0));
        assert_eq!(normalise(video.min, video), (0, 0));
        assert_eq!(normalise(video.max, video), (u16::MAX, u16::MAX));
        assert_eq!(normalise(video.center(), video), (32768, 32768));
        assert_eq!(normalise(egui::pos2(0.0, 1000.0), video), (0, u16::MAX));
    }
}
