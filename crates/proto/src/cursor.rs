//! Cursor position travels independently of encoded video. Shapes use reliable control messages.
use anyhow::{Result, ensure};
use bytes::Bytes;
use serde::{Deserialize, Serialize};

pub const KIND_CURSOR: u8 = 0x43;
pub const MAX_CURSOR_DIMENSION: u16 = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CursorPosition {
    pub seq: u32,
    pub x: u16,
    pub y: u16,
    pub visible: bool,
}

impl CursorPosition {
    pub fn encode(self) -> Bytes {
        let mut out = Vec::with_capacity(10);
        out.push(KIND_CURSOR);
        out.extend_from_slice(&self.seq.to_le_bytes());
        out.extend_from_slice(&self.x.to_le_bytes());
        out.extend_from_slice(&self.y.to_le_bytes());
        out.push(u8::from(self.visible));
        out.into()
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        ensure!(data.len() == 10 && data[0] == KIND_CURSOR, "invalid cursor packet");
        ensure!(data[9] <= 1, "invalid cursor visibility");
        Ok(Self {
            seq: u32::from_le_bytes(data[1..5].try_into()?),
            x: u16::from_le_bytes(data[5..7].try_into()?),
            y: u16::from_le_bytes(data[7..9].try_into()?),
            visible: data[9] == 1,
        })
    }

    pub fn is_newer_than(self, previous: u32) -> bool {
        (self.seq.wrapping_sub(previous) as i32) > 0
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CursorShape {
    pub id: u64,
    pub width: u16,
    pub height: u16,
    pub hotspot_x: u16,
    pub hotspot_y: u16,
    pub rgba: Vec<u8>,
}

impl CursorShape {
    pub fn is_valid(&self) -> bool {
        self.width > 0
            && self.height > 0
            && self.width <= MAX_CURSOR_DIMENSION
            && self.height <= MAX_CURSOR_DIMENSION
            && self.hotspot_x < self.width
            && self.hotspot_y < self.height
            && self.rgba.len() == usize::from(self.width) * usize::from(self.height) * 4
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_round_trip_and_ignore_stale_updates() {
        let p = CursorPosition { seq: u32::MAX, x: 10, y: 65535, visible: true };
        assert_eq!(CursorPosition::decode(&p.encode()).unwrap(), p);
        assert!(CursorPosition { seq: 0, ..p }.is_newer_than(u32::MAX));
        assert!(!p.is_newer_than(0));
        assert!(CursorPosition::decode(&[0; 3]).is_err());
    }

    #[test]
    fn validates_cursor_images_and_hotspots() {
        let mut shape = CursorShape { id: 1, width: 2, height: 2, hotspot_x: 1, hotspot_y: 1, rgba: vec![255; 16] };
        assert!(shape.is_valid());
        shape.rgba.pop();
        assert!(!shape.is_valid());
        shape.rgba.push(255);
        shape.hotspot_x = 2;
        assert!(!shape.is_valid());
        shape.width = 65535;
        assert!(!shape.is_valid());
    }
}
