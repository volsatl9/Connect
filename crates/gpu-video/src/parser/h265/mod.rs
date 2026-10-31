//! H.265 (HEVC) Annex B parsing for Vulkan Video decoding: NAL units, parameter sets, slice
//! headers, and the picture order counts and reference pictures they imply.
//! (Connect addition, see VENDORED.md.)

pub(crate) mod bits;
pub(crate) mod params;
pub(crate) mod refs;
pub(crate) mod slice;

use std::collections::BTreeMap;

use params::{Pps, Sps, Vps};
use refs::{H265Picture, References};
use slice::SliceHeader;

use crate::parser::nalu_splitter::NALUSplitter;

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum H265ParseError {
    #[error("H.265 data ends early")]
    Truncated,
    #[error("invalid H.265 syntax: {0}")]
    Invalid(&'static str),
    #[error("H.265 feature not supported: {0}")]
    Unsupported(&'static str),
    #[error("H.265 slice refers to a missing {0}")]
    MissingParameterSet(&'static str),
    #[error("H.265 picture refers to a reference picture that isn't there (order count {0})")]
    MissingReference(i32),
    #[error("H.265 stream needs more reference pictures than its SPS allows")]
    DpbOverflow,
}

/// nal_unit_type (Table 7-1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NalType(pub u8);

impl NalType {
    pub const VPS: u8 = 32;
    pub const SPS: u8 = 33;
    pub const PPS: u8 = 34;
    pub const EOS: u8 = 36;

    pub fn is_slice(self) -> bool {
        self.0 <= 31 && !matches!(self.0, 10..=15 | 22..=31)
    }

    pub fn is_irap(self) -> bool {
        (16..=23).contains(&self.0)
    }

    pub fn is_idr(self) -> bool {
        matches!(self.0, 19 | 20)
    }

    pub fn is_bla(self) -> bool {
        (16..=18).contains(&self.0)
    }

    pub fn is_rasl(self) -> bool {
        matches!(self.0, 8 | 9)
    }

    pub fn is_radl(self) -> bool {
        matches!(self.0, 6 | 7)
    }

    /// Sub-layer non-reference pictures: TRAIL_N, TSA_N, STSA_N, RADL_N, RASL_N, RSV_VCL_N.
    pub fn is_sub_layer_non_reference(self) -> bool {
        self.0 <= 14 && self.0 % 2 == 0
    }
}

/// One NAL unit: its type, temporal sub-layer, and the bytes as they came (without the start
/// code), which is what the decoder is given.
#[derive(Debug, Clone)]
pub(crate) struct Nalu {
    pub kind: NalType,
    pub temporal_id: u8,
    pub bytes: Vec<u8>,
    pub pts: Option<u64>,
}

/// A NAL unit as the splitter hands it out, without its start code or trailing zero bytes;
/// `None` if there's nothing in it.
fn strip_start_code(chunk: &[u8]) -> Option<&[u8]> {
    let start = chunk.windows(3).position(|w| w == [0, 0, 1]).map_or(0, |i| i + 3);
    let mut end = chunk.len();
    while end > start && chunk[end - 1] == 0 {
        end -= 1;
    }
    (end > start).then(|| &chunk[start..end])
}

impl Nalu {
    fn new(bytes: Vec<u8>, pts: Option<u64>) -> Result<Self, H265ParseError> {
        if bytes.len() < 2 || bytes[0] & 0x80 != 0 {
            return Err(H265ParseError::Invalid("NAL unit header"));
        }
        let layer = ((bytes[0] & 1) << 5) | (bytes[1] >> 3);
        if layer != 0 {
            return Err(H265ParseError::Unsupported("layers above the base layer"));
        }
        let temporal_id_plus1 = bytes[1] & 7;
        if temporal_id_plus1 == 0 {
            return Err(H265ParseError::Invalid("nuh_temporal_id_plus1"));
        }
        Ok(Self { kind: NalType((bytes[0] >> 1) & 0x3f), temporal_id: temporal_id_plus1 - 1, bytes, pts })
    }

    /// The payload after the 2-byte header, with emulation prevention removed.
    fn rbsp(&self) -> Vec<u8> {
        bits::rbsp(&self.bytes[2..])
    }
}

/// What the Vulkan decoder is asked to do.
#[derive(Debug, Clone)]
pub(crate) enum H265Instruction {
    /// The parameter sets in force from now on, all of them.
    Parameters {
        vps: Vec<Vps>,
        sps: Vec<Sps>,
        pps: Vec<Pps>,
    },
    Decode(Box<H265Picture>),
}

/// Turns an Annex B byte stream into decoder instructions.
#[derive(Default)]
pub(crate) struct H265Parser {
    splitter: NALUSplitter,
    vps: BTreeMap<u8, Vps>,
    sps: BTreeMap<u8, Sps>,
    pps: BTreeMap<u8, Pps>,
    parameters_changed: bool,
    /// The access unit being collected: its slices, with their headers.
    current: Vec<(Nalu, SliceHeader)>,
    references: References,
}

impl H265Parser {
    pub fn parse(&mut self, bytes: &[u8], pts: Option<u64>) -> Result<Vec<H265Instruction>, H265ParseError> {
        let nalus = self.splitter.push(bytes, pts);
        self.put_nalus(nalus)
    }

    /// Finishes the picture being collected: the data so far is a whole access unit.
    pub fn flush(&mut self) -> Result<Vec<H265Instruction>, H265ParseError> {
        let nalus = self.splitter.flush();
        let mut out = self.put_nalus(nalus)?;
        out.extend(self.finish_picture()?);
        Ok(out)
    }

    /// Video went missing: wait for a keyframe before decoding again.
    pub fn mark_missed(&mut self) {
        self.current.clear();
        self.references.mark_missed();
    }

    fn put_nalus(&mut self, nalus: Vec<(Vec<u8>, Option<u64>)>) -> Result<Vec<H265Instruction>, H265ParseError> {
        let mut out = Vec::new();
        for (chunk, pts) in nalus {
            let Some(bytes) = strip_start_code(&chunk) else { continue };
            let nalu = Nalu::new(bytes.to_vec(), pts)?;
            let kind = nalu.kind;
            // Parameter sets, delimiters and prefix SEI after a picture's slices start the next
            // access unit (7.4.2.4.4).
            if !kind.is_slice() && matches!(kind.0, 32..=39 | 41..=44 | 48..=55) && !self.current.is_empty() {
                out.extend(self.finish_picture()?);
            }
            match kind.0 {
                NalType::VPS => {
                    let vps = Vps::parse(&nalu.rbsp())?;
                    self.parameters_changed |= self.vps.get(&vps.id) != Some(&vps);
                    self.vps.insert(vps.id, vps);
                }
                NalType::SPS => {
                    let sps = Sps::parse(&nalu.rbsp())?;
                    self.parameters_changed |= self.sps.get(&sps.id) != Some(&sps);
                    self.sps.insert(sps.id, sps);
                }
                NalType::PPS => {
                    let pps = Pps::parse(&nalu.rbsp())?;
                    self.parameters_changed |= self.pps.get(&pps.id) != Some(&pps);
                    self.pps.insert(pps.id, pps);
                }
                NalType::EOS => {
                    out.extend(self.finish_picture()?);
                    self.references.end_of_sequence();
                }
                _ if kind.is_slice() => {
                    let header = SliceHeader::parse(&nalu.rbsp(), kind, |id| {
                        let pps = self.pps.get(&id)?;
                        Some((pps.clone(), self.sps.get(&pps.sps_id)?.clone()))
                    })?;
                    if header.first_slice_segment_in_pic && !self.current.is_empty() {
                        out.extend(self.finish_picture()?);
                    }
                    self.current.push((nalu, header));
                }
                _ => {}
            }
        }
        Ok(out)
    }

    fn finish_picture(&mut self) -> Result<Vec<H265Instruction>, H265ParseError> {
        let slices = std::mem::take(&mut self.current);
        let Some((first, header)) = slices.first() else { return Ok(Vec::new()) };
        let mut out = Vec::new();
        if std::mem::take(&mut self.parameters_changed) {
            out.push(H265Instruction::Parameters {
                vps: self.vps.values().cloned().collect(),
                sps: self.sps.values().cloned().collect(),
                pps: self.pps.values().cloned().collect(),
            });
        }
        let pps = self.pps.get(&header.pps_id).ok_or(H265ParseError::MissingParameterSet("PPS"))?;
        let sps = self.sps.get(&pps.sps_id).ok_or(H265ParseError::MissingParameterSet("SPS"))?;
        if !self.vps.contains_key(&sps.vps_id) {
            return Err(H265ParseError::MissingParameterSet("VPS"));
        }
        let (kind, temporal_id, pts) = (first.kind, first.temporal_id, first.pts);
        let header = header.clone();
        // The slices as the decoder takes them: each after a start code.
        let mut data = Vec::new();
        let mut slice_offsets = Vec::new();
        for (nalu, _) in &slices {
            slice_offsets.push(data.len() as u32);
            data.extend_from_slice(&[0, 0, 1]);
            data.extend_from_slice(&nalu.bytes);
        }
        if let Some(picture) =
            self.references.put_picture(kind, temporal_id, &header, sps, pps, data, slice_offsets, pts)?
        {
            out.push(H265Instruction::Decode(Box::new(picture)));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pictures decoded from a whole stream fed in uneven pieces, as (POC, RPS bits).
    fn pictures(stream: &[u8]) -> Vec<H265Picture> {
        let mut parser = H265Parser::default();
        let mut out = Vec::new();
        for chunk in stream.chunks(997) {
            out.extend(parser.parse(chunk, None).unwrap());
        }
        out.extend(parser.flush().unwrap());
        out.into_iter()
            .filter_map(|i| match i {
                H265Instruction::Decode(p) => Some(*p),
                H265Instruction::Parameters { .. } => None,
            })
            .collect()
    }

    /// Expected (POC, NumBitsForSTRefPicSetInSlice) in decode order, from ffmpeg's
    /// `trace_headers` on the same streams (x265, see tests/h265).
    fn check(stream: &[u8], expected: &[(i32, u16)]) {
        let pictures = pictures(stream);
        let got: Vec<(i32, u16)> = pictures.iter().map(|p| (p.poc, p.st_rps_bits)).collect();
        assert_eq!(got, expected);
        for p in &pictures {
            assert!(p.st_curr_before.iter().all(|r| r.poc < p.poc), "{p:?}");
            assert!(p.st_curr_after.iter().all(|r| r.poc > p.poc), "{p:?}");
            assert!(p.kept.len() < p.max_dpb_slots as usize);
            assert_eq!(p.cropped_size, (176, 144));
            assert_eq!(p.data[..3], [0, 0, 1]);
        }
    }

    #[test]
    fn low_delay_stream_wraps_its_order_count() {
        // log2_max_pic_order_cnt_lsb = 4: the 16th picture's LSB wraps to 0, its POC goes on.
        let expected: Vec<(i32, u16)> =
            (0..30).map(|poc| (poc, [0, 6].get(poc as usize).copied().unwrap_or(8))).collect();
        check(include_bytes!("../../../tests/h265/lowdelay.h265"), &expected);
    }

    #[test]
    fn b_frames_reorder_and_closed_gops_restart_at_idr() {
        let pocs = [0, 1, 5, 3, 2, 4, 9, 7, 6, 8, 13, 11, 10, 12, 15, 14, 0, 2, 1, 5, 4, 3, 9, 7, 6, 8, 12, 11, 10, 13];
        let bits = [
            0, 6, 12, 16, 16, 18, 22, 24, 18, 20, 24, 26, 18, 20, 22, 22, 0, 8, 10, 12, 16, 16, 22, 22, 16, 18, 20, 22,
            16, 18,
        ];
        let expected: Vec<_> = pocs.into_iter().zip(bits).collect();
        check(include_bytes!("../../../tests/h265/bframes.h265"), &expected);
        let pictures = pictures(include_bytes!("../../../tests/h265/bframes.h265"));
        // A B picture refers both ways; the second IDR drops everything before it.
        assert!(pictures[3].st_curr_before.len() + pictures[3].st_curr_after.len() >= 2);
        assert!(!pictures[3].st_curr_after.is_empty());
        assert!(pictures[16].idr && pictures[16].kept.is_empty() && !pictures[16].dropped.is_empty());
    }

    #[test]
    fn open_gop_carries_order_counts_across_cra() {
        let pocs = [
            0, 1, 5, 3, 2, 4, 9, 7, 6, 8, 12, 11, 10, 15, 14, 13, 18, 17, 16, 21, 20, 19, 24, 23, 22, 28, 26, 25, 27,
            29,
        ];
        let bits = [
            0, 6, 12, 16, 16, 18, 22, 24, 18, 20, 22, 24, 16, 8, 12, 12, 16, 20, 14, 18, 20, 14, 18, 20, 14, 10, 14,
            14, 14, 16,
        ];
        let expected: Vec<_> = pocs.into_iter().zip(bits).collect();
        check(include_bytes!("../../../tests/h265/opengop.h265"), &expected);
    }

    #[test]
    fn multi_slice_pictures_keep_every_slice() {
        let expected: Vec<(i32, u16)> = (0..30)
            .map(|poc| {
                let bits = match poc % 8 {
                    0 if poc == 0 => 0,
                    1 => 6,
                    2 => 8,
                    _ if poc % 8 == 0 => 12,
                    _ => 12,
                };
                (poc, bits)
            })
            .collect();
        let stream = include_bytes!("../../../tests/h265/slices.h265");
        let pictures = pictures(stream);
        let got: Vec<(i32, u16)> = pictures.iter().map(|p| (p.poc, p.st_rps_bits)).collect();
        assert_eq!(got, expected);
        assert!(pictures.iter().all(|p| p.slice_offsets.len() == 3), "three slices each");
        for p in &pictures {
            for &offset in &p.slice_offsets {
                assert_eq!(p.data[offset as usize..][..3], [0, 0, 1]);
            }
        }
    }

    #[test]
    fn decoding_waits_for_a_random_access_point_after_loss() {
        let stream = include_bytes!("../../../tests/h265/lowdelay.h265");
        let mut parser = H265Parser::default();
        let first = parser.parse(stream, None).unwrap();
        assert!(!first.is_empty());
        parser.mark_missed();
        // The same pictures again, but no random access point among them past the first.
        let again: Vec<_> = pictures(stream).into_iter().skip(1).collect();
        assert!(!again.is_empty());
        let mut lost = H265Parser::default();
        lost.references.mark_missed();
        assert!(
            lost.references
                .put_picture(NalType(1), 0, &dummy_header(), &dummy_sps(), &Pps::default(), vec![], vec![], None)
                .unwrap()
                .is_none()
        );
    }

    fn dummy_sps() -> Sps {
        Sps { log2_max_poc_lsb_minus4: 4, width: 16, height: 16, ..Default::default() }
    }

    fn dummy_header() -> SliceHeader {
        SliceHeader {
            first_slice_segment_in_pic: true,
            pps_id: 0,
            dependent: false,
            slice_type: 1,
            poc_lsb: 1,
            short_term_ref_pic_set_sps_flag: false,
            st_rps: params::ShortTermRps::default(),
            st_rps_bits: 0,
            num_delta_pocs_of_ref_rps_idx: 0,
            long_term: Vec::new(),
        }
    }

    #[test]
    fn predicted_rps_follows_equations_7_61_and_7_62() {
        use params::ShortTermRps;
        // Reference set: -1 (used), -3 (used), +2 (not used).
        let reference = ShortTermRps {
            delta_poc_s0: vec![-1, -3],
            used_s0: vec![true, true],
            delta_poc_s1: vec![2],
            used_s1: vec![false],
            ..Default::default()
        };
        // Predicted with deltaRps = -1 (sign 1, abs_minus1 0); entries j = 0..=3 over
        // (S0[0], S0[1], S1[0], deltaRps itself); all used, all kept.
        let bits: [u8; 1] = [0b1_1_1111_00]; // sign 1, abs_delta_rps_minus1 ue(0) = "1", four used flags
        let mut r = bits::BitReader::new(&bits);
        let mut predicted = ShortTermRps::default();
        predicted.inter_ref_pic_set_prediction = true;
        predicted.delta_rps_sign = r.bit().unwrap();
        predicted.abs_delta_rps_minus1 = r.ue().unwrap();
        for j in 0..4 {
            assert!(r.bit().unwrap());
            predicted.used_by_curr_pic_flag |= 1 << j;
            predicted.use_delta_flag |= 1 << j;
        }
        predicted.derive_from_for_test(&reference);
        // S1[0] + dRps = 1 stays positive; dRps = -1 itself joins S0; S0 shifts by -1.
        assert_eq!(predicted.delta_poc_s0, [-1, -2, -4]);
        assert_eq!(predicted.delta_poc_s1, [1]);
        assert_eq!(predicted.used_s1, [true]);
        let std = predicted.to_std();
        assert_eq!((std.num_negative_pics, std.num_positive_pics), (3, 1));
        assert_eq!(&std.delta_poc_s0_minus1[..3], &[0, 0, 1]);
        assert_eq!(std.delta_poc_s1_minus1[0], 0);
    }
}
