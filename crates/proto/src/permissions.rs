use serde::{Deserialize, Serialize};

/// Session permissions controlled by the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DeviceClass {
    Mouse,
    Keyboard,
    Gamepad,
    Audio,
}

impl DeviceClass {
    pub const ALL: [DeviceClass; 4] =
        [DeviceClass::Mouse, DeviceClass::Keyboard, DeviceClass::Gamepad, DeviceClass::Audio];

    const fn bit(self) -> u8 {
        match self {
            DeviceClass::Mouse => 1 << 0,
            DeviceClass::Keyboard => 1 << 1,
            DeviceClass::Gamepad => 1 << 2,
            DeviceClass::Audio => 1 << 3,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            DeviceClass::Mouse => "Mouse",
            DeviceClass::Keyboard => "Keyboard",
            DeviceClass::Gamepad => "Gamepad",
            DeviceClass::Audio => "Desktop audio",
        }
    }
}

/// Which input devices the host currently allows the remote user to use.
///
/// The host is the only authority: it filters every input event against its own copy,
/// and sends a copy to the client purely so the client can show what is blocked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Permissions(u8);

impl Permissions {
    pub const ALL: Permissions = Permissions(0b1111);
    /// Screen share only: no remote input at all.
    pub const VIEW_ONLY: Permissions = Permissions(0);

    pub const fn from_bits(bits: u8) -> Self {
        Permissions(bits & Self::ALL.0)
    }

    pub const fn bits(self) -> u8 {
        self.0
    }

    pub const fn allows(self, device: DeviceClass) -> bool {
        self.0 & device.bit() != 0
    }

    pub const fn with(self, device: DeviceClass, allowed: bool) -> Self {
        if allowed { Permissions(self.0 | device.bit()) } else { Permissions(self.0 & !device.bit()) }
    }

    pub const fn is_view_only(self) -> bool {
        self.0 & 0b111 == 0
    }
}

impl Default for Permissions {
    fn default() -> Self {
        Self::ALL
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggles_are_independent() {
        let p = Permissions::ALL.with(DeviceClass::Keyboard, false);
        assert!(p.allows(DeviceClass::Mouse));
        assert!(!p.allows(DeviceClass::Keyboard));
        assert!(p.allows(DeviceClass::Gamepad));

        let p = p.with(DeviceClass::Mouse, false).with(DeviceClass::Gamepad, false).with(DeviceClass::Audio, false);
        assert!(p.is_view_only());
        assert_eq!(p, Permissions::VIEW_ONLY);

        let p = p.with(DeviceClass::Gamepad, true);
        assert_eq!(DeviceClass::ALL.map(|d| p.allows(d)), [false, false, true, false]);
    }

    #[test]
    fn from_bits_masks_unknown_bits() {
        assert_eq!(Permissions::from_bits(0xff), Permissions::ALL);
    }

    #[test]
    fn old_saved_permissions_do_not_grant_desktop_audio() {
        let old = Permissions::from_bits(0b111);
        assert!(!old.allows(DeviceClass::Audio));
        assert!(old.with(DeviceClass::Audio, true).allows(DeviceClass::Audio));
        assert!(Permissions::VIEW_ONLY.with(DeviceClass::Audio, true).is_view_only());
    }
}
