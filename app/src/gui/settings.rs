use std::{
    collections::HashSet,
    time::{SystemTime, UNIX_EPOCH},
};

use connect_net::iroh::EndpointId;
use connect_proto::Permissions;
use serde::{Deserialize, Serialize};

/// Offered caps on the video bitrate when sharing.
pub const BITRATE_CHOICES_MBPS: [u32; 5] = [10, 20, 35, 50, 80];

/// Everything the app remembers between runs.
#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Shown to the other side.
    pub device_name: String,
    pub accepting: bool,
    /// Pre-selected in the Allow prompt, and what trusted devices get.
    pub permissions: Permissions,
    /// Cap on the video bitrate when sharing.
    pub bitrate_mbps: u32,
    /// Devices let in without asking.
    pub trusted: Vec<KnownDevice>,
    /// Computers this PC has connected to, most recently used first. (Called "recent" before
    /// 0.4.0, when only the last few were kept.)
    #[serde(alias = "recent")]
    pub computers: Vec<KnownDevice>,
    pub sidebar_collapsed: bool,
    /// Decode video on the graphics card where it can, rather than the processor.
    pub hardware_decoding: bool,
    /// Show frames evenly paced rather than as soon as they arrive.
    pub smooth_pacing: bool,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct KnownDevice {
    /// User-ID, stored as text so the settings file stays readable.
    pub id: String,
    pub name: String,
    /// When this PC last connected to it, in seconds since 1970.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used: Option<u64>,
}

impl KnownDevice {
    fn new(id: EndpointId, name: &str) -> Self {
        Self { id: id.to_string(), name: name.to_owned(), last_used: None }
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
            bitrate_mbps: connect_host::DEFAULT_BITRATE_MBPS,
            trusted: Vec::new(),
            computers: Vec::new(),
            sidebar_collapsed: false,
            hardware_decoding: true,
            smooth_pacing: false,
        }
    }
}

impl Settings {
    pub fn trusted_ids(&self) -> HashSet<EndpointId> {
        self.trusted.iter().filter_map(KnownDevice::user_id).collect()
    }

    pub fn trust(&mut self, id: EndpointId, name: &str) {
        self.trusted.retain(|d| d.user_id() != Some(id));
        self.trusted.push(KnownDevice::new(id, name));
    }

    /// Saves a computer that just let this PC in, or moves it to the front under the name its
    /// owner gives it now.
    pub fn remember_computer(&mut self, id: EndpointId, name: &str) {
        self.computers.retain(|d| d.user_id() != Some(id));
        let last_used = SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs());
        self.computers.insert(0, KnownDevice { last_used, ..KnownDevice::new(id, name) });
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
        assert!(settings.hardware_decoding, "on for people upgrading");
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
