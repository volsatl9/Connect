//! H.265 bitstreams from hardware encoders, made self-contained for the viewer: the parameter
//! sets (VPS, SPS, PPS) go in front of every keyframe, so a viewer can start, or recover, at
//! any of them.
// Only the Windows hardware encoder produces H.265; elsewhere just the tests use this.
#![cfg_attr(not(windows), allow(dead_code))]

use anyhow::{Context, Result, bail, ensure};

/// Largest frame accepted from an encoder.
const MAX_FRAME: usize = 16 * 1024 * 1024;

const VPS: u8 = 32;
const SPS: u8 = 33;
const PPS: u8 = 34;
const AUD: u8 = 35;

fn nal_type(unit: &[u8]) -> u8 {
    (unit[0] >> 1) & 0x3f
}

/// Video coding layer NAL units: slices.
fn is_slice(kind: u8) -> bool {
    kind <= 21
}

/// Random access points: IDR (19, 20) and CRA (21), where decoding can start.
fn is_keyframe(kind: u8) -> bool {
    (16..=21).contains(&kind)
}

fn validate(unit: &[u8]) -> Result<()> {
    ensure!(unit.len() >= 2, "truncated H.265 NAL header");
    // forbidden_zero_bit, nuh_layer_id (base layer only), nuh_temporal_id_plus1 != 0.
    let layer = ((unit[0] & 1) << 5) | (unit[1] >> 3);
    ensure!(unit[0] & 0x80 == 0 && layer == 0 && unit[1] & 7 != 0, "invalid H.265 NAL header");
    Ok(())
}

/// Splits an Annex-B stream into NAL units (without start codes).
fn annex_b(data: &[u8]) -> Result<Vec<&[u8]>> {
    let start_code = |at: usize| match data.get(at..at + 4) {
        Some([0, 0, 0, 1]) => 4,
        _ if data.get(at..at + 3) == Some(&[0, 0, 1]) => 3,
        _ => 0,
    };
    ensure!(start_code(0) > 0, "H.265 frame doesn't start with a start code");
    let mut units = Vec::new();
    let mut at = 0;
    while at < data.len() {
        let begin = at + start_code(at);
        let mut end = begin;
        while end < data.len() && start_code(end) == 0 {
            end += 1;
        }
        let mut payload_end = end;
        while payload_end > begin && data[payload_end - 1] == 0 {
            payload_end -= 1;
        }
        let unit = &data[begin..payload_end];
        validate(unit)?;
        units.push(unit);
        at = end;
    }
    Ok(units)
}

#[derive(Default)]
pub(crate) struct HevcBitstream {
    vps: Vec<u8>,
    sps: Vec<u8>,
    pps: Vec<u8>,
}

impl HevcBitstream {
    /// Takes the parameter sets from an encoder's sequence header: Annex B, or an `hvcC`
    /// configuration record.
    pub fn sequence_header(&mut self, data: &[u8]) -> Result<()> {
        if data.first() != Some(&1) {
            let units = annex_b(data)?;
            self.remember(&units);
            return Ok(());
        }
        // HEVCDecoderConfigurationRecord: 22 bytes of profile and format, then arrays of
        // parameter sets, each with 16-bit lengths.
        ensure!(data.len() >= 23, "truncated hvcC");
        let mut at = 23;
        let mut units = Vec::new();
        for _ in 0..data[22] {
            let header = data.get(at..at + 3).context("truncated hvcC array")?;
            let count = usize::from(u16::from_be_bytes([header[1], header[2]]));
            at += 3;
            for _ in 0..count {
                let len = data.get(at..at + 2).context("truncated hvcC NAL length")?;
                let len = usize::from(u16::from_be_bytes([len[0], len[1]]));
                let unit = data.get(at + 2..at + 2 + len).context("truncated hvcC NAL")?;
                validate(unit)?;
                units.push(unit);
                at += 2 + len;
            }
        }
        self.remember(&units);
        Ok(())
    }

    fn remember(&mut self, units: &[&[u8]]) {
        for unit in units {
            match nal_type(unit) {
                VPS => self.vps = unit.to_vec(),
                SPS => self.sps = unit.to_vec(),
                PPS => self.pps = unit.to_vec(),
                _ => {}
            }
        }
    }

    /// One encoded frame as Annex B, with the parameter sets in front if it's a keyframe; and
    /// whether it is one.
    pub fn frame(&mut self, data: &[u8]) -> Result<(Vec<u8>, bool)> {
        let units = annex_b(data)?;
        self.remember(&units);
        if !units.iter().any(|unit| is_slice(nal_type(unit))) {
            bail!("hardware output contains no video slice");
        }
        let keyframe = units.iter().any(|unit| is_keyframe(nal_type(unit)));
        let mut out = Vec::with_capacity(data.len() + self.vps.len() + self.sps.len() + self.pps.len() + 16);
        let mut put = |unit: &[u8]| {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(unit);
        };
        if keyframe {
            ensure!(
                !self.vps.is_empty() && !self.sps.is_empty() && !self.pps.is_empty(),
                "hardware keyframe has no VPS/SPS/PPS for decoder recovery"
            );
            // An access-unit delimiter, when present, comes first.
            for unit in units.iter().filter(|unit| nal_type(unit) == AUD) {
                put(unit);
            }
            for unit in [&self.vps, &self.sps, &self.pps] {
                put(unit);
            }
        }
        for unit in &units {
            // Parameter sets go in front of a keyframe exactly once.
            if keyframe && matches!(nal_type(unit), VPS..=AUD) {
                continue;
            }
            put(unit);
        }
        ensure!(out.len() <= MAX_FRAME, "H.265 frame with parameter sets exceeds limit");
        Ok((out, keyframe))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A NAL unit of `kind` with a one-byte payload.
    fn nal(kind: u8, payload: u8) -> Vec<u8> {
        vec![0, 0, 0, 1, kind << 1, 1, payload]
    }

    fn types(frame: &[u8]) -> Vec<u8> {
        annex_b(frame).unwrap().iter().map(|unit| nal_type(unit)).collect()
    }

    #[test]
    fn parameter_sets_go_in_front_of_every_keyframe_once() {
        let mut stream = HevcBitstream::default();
        let first = [nal(AUD, 0), nal(VPS, 1), nal(SPS, 2), nal(PPS, 3), nal(19, 4)].concat();
        let (frame, keyframe) = stream.frame(&first).unwrap();
        assert!(keyframe);
        assert_eq!(types(&frame), [AUD, VPS, SPS, PPS, 19]);
        let (frame, keyframe) = stream.frame(&nal(1, 5)).unwrap();
        assert!(!keyframe);
        assert_eq!(types(&frame), [1]);
        // A later IDR without its own parameter sets gets the remembered ones.
        let (frame, keyframe) = stream.frame(&nal(20, 6)).unwrap();
        assert!(keyframe);
        assert_eq!(types(&frame), [VPS, SPS, PPS, 20]);
    }

    #[test]
    fn hvcc_sequence_header_supplies_the_parameter_sets() {
        let mut record = vec![1];
        record.extend([0; 21]);
        record.push(3);
        for (kind, payload) in [(VPS, 1), (SPS, 2), (PPS, 3)] {
            record.extend([0x80 | kind, 0, 1, 0, 3, kind << 1, 1, payload]);
        }
        let mut stream = HevcBitstream::default();
        stream.sequence_header(&record).unwrap();
        let (frame, keyframe) = stream.frame(&nal(19, 9)).unwrap();
        assert!(keyframe);
        assert_eq!(types(&frame), [VPS, SPS, PPS, 19]);
    }

    #[test]
    fn rejects_bad_headers_missing_slices_and_keyframes_without_parameter_sets() {
        assert!(HevcBitstream::default().frame(&nal(19, 0)).is_err(), "no parameter sets yet");
        assert!(HevcBitstream::default().frame(&nal(VPS, 0)).is_err(), "no slice");
        for bad in [vec![0, 0, 1, 0x80, 1], vec![0, 0, 1, 2, 0], vec![0, 0, 1, 3, 9], vec![1, 2, 3]] {
            assert!(HevcBitstream::default().frame(&bad).is_err(), "accepted {bad:?}");
        }
        assert!(HevcBitstream::default().sequence_header(&[1, 0, 0]).is_err());
    }
}
