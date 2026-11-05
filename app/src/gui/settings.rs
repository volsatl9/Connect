use std::{
    collections::HashMap,
    time::{SystemTime, UNIX_EPOCH},
};

use connect_net::iroh::EndpointId;
use connect_proto::Permissions;
use serde::{Deserialize, Serialize};

use super::gamepad::ButtonMap;

/// Offered maximums for the video bitrate when sharing.
pub const BITRATE_CHOICES_MBPS: [u32; 9] = [5, 10, 15, 20, 30, 40, 50, 75, 100];
/// Offered minimums.
pub const MIN_BITRATE_CHOICES_MBPS: [u32; 6] = [2, 3, 5, 10, 15, 20];
/// 0 is no limit.
pub const FRAME_RATE_CHOICES: [u32; 5] = [0, 60, 120, 144, 240];

/// Everything the app remembers between runs.
#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Shown to the other side.
    pub device_name: String,
    pub accepting: bool,
    /// Pre-selected in the Allow prompt for a device not seen before.
    pub permissions: Permissions,
    /// Fit the bitrate to the picture as well as the connection.
    pub bitrate_automatic: bool,
    /// The most video a second when sharing: a ceiling, which the connection may keep it under.
    pub bitrate_mbps: u32,
    /// The least it's let fall to while the connection is usable.
    pub bitrate_min_mbps: u32,
    /// How many may be connected at once.
    pub max_connections: u32,
    /// Most frames a second sent when sharing; 0 for no limit.
    pub frame_rate_limit: u32,
    /// Devices let in without asking, before 0.10.0 kept that with each device. Read once by
    /// [`Self::adopt_trusted`].
    #[serde(skip_serializing)]
    trusted: Vec<KnownDevice>,
    /// Computers this PC has connected to and devices it has let in, most recently used first.
    /// (Called "recent" before 0.4.0, when only the last few were kept.)
    #[serde(alias = "recent")]
    pub computers: Vec<KnownDevice>,
    pub sidebar_collapsed: bool,
    /// Show frames evenly paced rather than as soon as they arrive.
    pub smooth_pacing: bool,
    /// What this PC's controllers' buttons do.
    pub button_map: ButtonMap,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct KnownDevice {
    /// User-ID, stored as text so the settings file stays readable.
    pub id: String,
    pub name: String,
    /// When this PC last connected to it, in seconds since 1970.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used: Option<u64>,
    /// What it may use on this PC, once this PC has let it in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permissions: Option<Permissions>,
    /// Let it in without asking.
    #[serde(default)]
    pub always_allow: bool,
}

impl KnownDevice {
    fn new(id: EndpointId, name: &str) -> Self {
        Self { id: id.to_string(), name: name.to_owned(), last_used: None, permissions: None, always_allow: false }
    }

    pub fn user_id(&self) -> Option<EndpointId> {
        self.id.parse().ok()
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            device_name: crate::device_name(),
            accepting: true,
            permissions: Permissions::default(),
            bitrate_automatic: true,
            bitrate_mbps: connect_host::DEFAULT_BITRATE_MBPS,
            bitrate_min_mbps: connect_host::DEFAULT_MIN_BITRATE_MBPS,
            max_connections: connect_host::DEFAULT_MAX_CONNECTIONS,
            frame_rate_limit: 0,
            trusted: Vec::new(),
            computers: Vec::new(),
            sidebar_collapsed: false,
            smooth_pacing: false,
            button_map: ButtonMap::default(),
        }
    }
}

impl Settings {
    /// The bitrate settings as the host takes them.
    pub fn bitrate(&self) -> connect_host::BitrateSettings {
        connect_host::BitrateSettings {
            automatic: self.bitrate_automatic,
            max_mbps: self.bitrate_mbps,
            min_mbps: self.bitrate_min_mbps,
        }
    }

    /// Moves devices saved by earlier versions in with all the others, each with the permissions
    /// that used to apply to them all.
    pub fn adopt_trusted(&mut self) {
        let permissions = self.permissions;
        for device in std::mem::take(&mut self.trusted) {
            let Some(id) = device.user_id() else { continue };
            if self.device(id).is_none() {
                self.computers.push(device);
            }
            if let Some(known) = self.device_mut(id) {
                known.permissions = Some(permissions);
                known.always_allow = true;
            }
        }
    }

    pub fn device(&self, id: EndpointId) -> Option<&KnownDevice> {
        self.computers.iter().find(|d| d.user_id() == Some(id))
    }

    pub fn device_mut(&mut self, id: EndpointId) -> Option<&mut KnownDevice> {
        self.computers.iter_mut().find(|d| d.user_id() == Some(id))
    }

    /// Who gets in without asking, and with what.
    pub fn always_allowed(&self) -> HashMap<EndpointId, Permissions> {
        self.computers
            .iter()
            .filter(|d| d.always_allow)
            .filter_map(|d| Some((d.user_id()?, d.permissions.unwrap_or(self.permissions))))
            .collect()
    }

    /// What the Allow prompt starts from for this device.
    pub fn permissions_for(&self, id: EndpointId) -> Permissions {
        self.device(id).and_then(|d| d.permissions).unwrap_or(self.permissions)
    }

    /// Takes the device out of the list, or a new one, for [`Self::remember`] to put back in front.
    fn take_device(&mut self, id: EndpointId, name: &str) -> KnownDevice {
        let device = self.computers.iter().position(|d| d.user_id() == Some(id)).map(|i| self.computers.remove(i));
        KnownDevice { name: name.to_owned(), ..device.unwrap_or_else(|| KnownDevice::new(id, name)) }
    }

    /// Puts `device` at the front, used just now.
    fn remember(&mut self, device: KnownDevice) {
        let last_used = SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs());
        self.computers.insert(0, KnownDevice { last_used, ..device });
    }

    /// Saves a computer that just let this PC in, or moves it to the front under the name its
    /// owner gives it now.
    pub fn remember_computer(&mut self, id: EndpointId, name: &str) {
        let device = self.take_device(id, name);
        self.remember(device);
    }

    /// Saves a device this PC just let in, with what it was given.
    pub fn remember_guest(&mut self, id: EndpointId, name: &str, permissions: Permissions, always_allow: bool) {
        let device = self.take_device(id, name);
        self.remember(KnownDevice { permissions: Some(permissions), always_allow, ..device });
    }

    pub fn computer_name(&self, id: EndpointId) -> Option<&str> {
        self.computers.iter().find(|d| d.user_id() == Some(id)).map(|d| d.name.as_str())
    }
}

/// "today", "yesterday", "3 days ago"… for when a computer was last used.
pub fn days_ago(then: u64, now: u64) -> String {
    match now.saturating_sub(then) / 86_400 {
        0 => "today".into(),
        1 => "yesterday".into(),
        days @ 2..=13 => format!("{days} days ago"),
        days @ 14..=59 => format!("{} weeks ago", days / 7),
        days => format!("{} months ago", days / 30),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(seed: u8) -> EndpointId {
        connect_net::iroh::SecretKey::from_bytes(&[seed; 32]).public()
    }

    #[test]
    fn computers_from_before_0_4_carry_over() {
        let old =
            format!(r#"(device_name: "Desk", accepting: true, recent: [(id: "{}", name: "Living room")])"#, id(1));
        let settings: Settings = ron::from_str(&old).unwrap();
        assert_eq!(settings.computers.len(), 1);
        assert_eq!(settings.computers[0].name, "Living room");
        assert_eq!(settings.computers[0].last_used, None);
        assert!(!settings.sidebar_collapsed);
    }

    /// Options 0.10.0 removed mustn't cost anyone their saved computers.
    #[test]
    fn settings_from_0_9_still_load() {
        let old = format!(
            r#"(device_name: "Desk", hardware_decoding: false, allow_h265: false, smooth_pacing: true,
                computers: [(id: "{}", name: "Kellu", last_used: Some(5))])"#,
            id(1)
        );
        let settings: Settings = ron::from_str(&old).unwrap();
        assert_eq!(settings.computers[0].name, "Kellu");
        assert!(settings.smooth_pacing);
    }

    #[test]
    fn trusted_devices_join_the_others_with_the_old_permissions() {
        let view_only = ron::to_string(&Permissions::VIEW_ONLY).unwrap();
        let old = format!(
            r#"(permissions: {view_only}, computers: [(id: "{0}", name: "Both")],
                trusted: [(id: "{0}", name: "Both"), (id: "{1}", name: "Guest"), (id: "nonsense", name: "Bad")])"#,
            id(1),
            id(2)
        );
        let mut settings: Settings = ron::from_str(&old).unwrap();
        settings.adopt_trusted();
        assert_eq!(settings.computers.len(), 2);
        assert_eq!(settings.always_allowed().get(&id(1)), Some(&Permissions::VIEW_ONLY));
        assert_eq!(settings.always_allowed().get(&id(2)), Some(&Permissions::VIEW_ONLY));

        let saved = ron::to_string(&settings).unwrap();
        assert!(!saved.contains("trusted"), "stored with each device from now on");
        let again: Settings = ron::from_str(&saved).unwrap();
        assert_eq!(again.always_allowed().len(), 2);
    }

    #[test]
    fn each_device_keeps_its_own_permissions() {
        let mut settings = Settings::default();
        let gamepad_only = Permissions::VIEW_ONLY.with(connect_proto::DeviceClass::Gamepad, true);
        assert_eq!(settings.permissions_for(id(1)), Permissions::default(), "not seen before");

        settings.remember_guest(id(1), "Guest", gamepad_only, false);
        assert_eq!(settings.permissions_for(id(1)), gamepad_only);
        assert!(settings.always_allowed().is_empty(), "still asked about");

        settings.remember_guest(id(1), "Guest", gamepad_only, true);
        assert_eq!(settings.computers.len(), 1);
        assert_eq!(settings.always_allowed().get(&id(1)), Some(&gamepad_only));

        // Connecting to that same device leaves what it may use here alone.
        settings.remember_computer(id(1), "Renamed");
        assert_eq!(settings.computers[0].name, "Renamed");
        assert_eq!(settings.always_allowed().get(&id(1)), Some(&gamepad_only));
    }

    #[test]
    fn remembering_keeps_every_computer_newest_first() {
        let mut settings = Settings::default();
        for seed in 0..12 {
            settings.remember_computer(id(seed), &format!("PC {seed}"));
        }
        assert_eq!(settings.computers.len(), 12);
        // Connecting again moves it to the front, under its owner's current name.
        settings.remember_computer(id(3), "Renamed");
        assert_eq!(settings.computers.len(), 12);
        assert_eq!(settings.computers[0].name, "Renamed");
        assert!(settings.computers[0].last_used.is_some());
        assert_eq!(settings.computer_name(id(3)), Some("Renamed"));
        assert_eq!(settings.computers[1].name, "PC 11");
    }

    #[test]
    fn last_used_reads_naturally() {
        let day = 86_400;
        assert_eq!(days_ago(10 * day, 10 * day + 60), "today");
        assert_eq!(days_ago(10 * day, 11 * day + 60), "yesterday");
        assert_eq!(days_ago(0, 5 * day), "5 days ago");
        assert_eq!(days_ago(0, 21 * day), "3 weeks ago");
        assert_eq!(days_ago(0, 95 * day), "3 months ago");
        assert_eq!(days_ago(100, 50), "today");
    }
}
