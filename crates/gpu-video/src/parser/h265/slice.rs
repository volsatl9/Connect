//! H.265 slice segment headers (7.3.6.1), as far as the reference picture set: what working
//! out the picture order count and the references needs. The decoder reads the rest itself.
//! (Connect addition, see VENDORED.md.)

use super::{
    H265ParseError, NalType,
    bits::{BitReader, ceil_log2},
    params::{Pps, ShortTermRps, Sps},
};

/// A long-term reference named by a slice header.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct LongTermRef {
    pub poc_lsb: u32,
    pub used_by_curr_pic: bool,
    /// DeltaPocMsbCycleLt, when the full order count is given.
    pub delta_poc_msb_cycle: Option<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SliceHeader {
    pub first_slice_segment_in_pic: bool,
    pub pps_id: u8,
    pub dependent: bool,
    pub slice_type: u32,
    pub poc_lsb: u32,
    pub short_term_ref_pic_set_sps_flag: bool,
    /// The slice's own set, or a copy of the SPS set it picks.
    pub st_rps: ShortTermRps,
    /// NumBitsForSTRefPicSetInSlice: bits of the slice's own set (0 when it picks one).
    pub st_rps_bits: u16,
    /// NumDeltaPocs of the set a slice's own set is predicted from (0 otherwise).
    pub num_delta_pocs_of_ref_rps_idx: u8,
    pub long_term: Vec<LongTermRef>,
}

impl SliceHeader {
    /// Parses a slice segment header from the RBSP after the 2-byte NAL header.
    pub fn parse(
        rbsp: &[u8],
        nal_type: NalType,
        pps_of: impl Fn(u8) -> Option<(Pps, Sps)>,
    ) -> Result<Self, H265ParseError> {
        let r = &mut BitReader::new(rbsp);
        let first_slice_segment_in_pic = r.bit()?;
        if nal_type.is_irap() {
            r.bit()?; // no_output_of_prior_pics_flag
        }
        let pps_id = r.ue_max(63, "slice_pic_parameter_set_id")? as u8;
        let (pps, sps) = pps_of(pps_id).ok_or(H265ParseError::MissingParameterSet("PPS"))?;
        let mut dependent = false;
        if !first_slice_segment_in_pic {
            if pps.dependent_slice_segments_enabled {
                dependent = r.bit()?;
            }
            r.bits(ceil_log2(sps.pic_size_in_ctbs()))?; // slice_segment_address
        }
        let mut header = SliceHeader {
            first_slice_segment_in_pic,
            pps_id,
            dependent,
            slice_type: 0,
            poc_lsb: 0,
            short_term_ref_pic_set_sps_flag: false,
            st_rps: ShortTermRps::default(),
            st_rps_bits: 0,
            num_delta_pocs_of_ref_rps_idx: 0,
            long_term: Vec::new(),
        };
        if dependent {
            // Everything else comes from the segment it depends on.
            return Ok(header);
        }
        r.skip(usize::from(pps.num_extra_slice_header_bits))?;
        header.slice_type = r.ue_max(2, "slice_type")?;
        if pps.output_flag_present {
            r.bit()?; // pic_output_flag
        }
        if sps.separate_colour_plane {
            r.bits(2)?; // colour_plane_id
        }
        if nal_type.is_idr() {
            return Ok(header);
        }
        header.poc_lsb = r.bits(u32::from(sps.log2_max_poc_lsb_minus4) + 4)?;
        header.short_term_ref_pic_set_sps_flag = r.bit()?;
        let num_sets = sps.st_rps.len();
        if !header.short_term_ref_pic_set_sps_flag {
            let start = r.position();
            header.st_rps = ShortTermRps::parse(r, num_sets, num_sets, &sps.st_rps)?;
            header.st_rps_bits = (r.position() - start) as u16;
            if header.st_rps.inter_ref_pic_set_prediction {
                let reference = num_sets - (header.st_rps.delta_idx_minus1 as usize + 1);
                header.num_delta_pocs_of_ref_rps_idx = sps.st_rps[reference].num_delta_pocs() as u8;
            }
        } else {
            let idx = if num_sets > 1 { r.bits(ceil_log2(num_sets as u32))? as usize } else { 0 };
            header.st_rps =
                sps.st_rps.get(idx).cloned().ok_or(H265ParseError::Invalid("short_term_ref_pic_set_idx"))?;
        }
        if sps.long_term_ref_pics_present {
            let num_sps = if sps.lt_ref_pic_poc_lsb_sps.is_empty() {
                0
            } else {
                r.ue_max(sps.lt_ref_pic_poc_lsb_sps.len() as u32, "num_long_term_sps")?
            };
            let num_pics = r.ue_max(32, "num_long_term_pics")?;
            let mut cycle = 0;
            for i in 0..num_sps + num_pics {
                let (poc_lsb, used_by_curr_pic) = if i < num_sps {
                    let lt_idx = if sps.lt_ref_pic_poc_lsb_sps.len() > 1 {
                        r.bits(ceil_log2(sps.lt_ref_pic_poc_lsb_sps.len() as u32))? as usize
                    } else {
                        0
                    };
                    (
                        *sps.lt_ref_pic_poc_lsb_sps.get(lt_idx).ok_or(H265ParseError::Invalid("lt_idx_sps"))?,
                        sps.used_by_curr_pic_lt_sps[lt_idx],
                    )
                } else {
                    (r.bits(u32::from(sps.log2_max_poc_lsb_minus4) + 4)?, r.bit()?)
                };
                // DeltaPocMsbCycleLt accumulates (an absent delta counts as 0), starting afresh
                // where the slice's own entries follow the SPS's (7-52).
                let present = r.bit()?;
                let delta = if present { r.ue()? } else { 0 };
                cycle = if i == 0 || i == num_sps { delta } else { cycle + delta };
                let delta_poc_msb_cycle = present.then_some(cycle);
                header.long_term.push(LongTermRef { poc_lsb, used_by_curr_pic, delta_poc_msb_cycle });
            }
        }
        Ok(header)
    }
}
