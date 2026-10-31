//! H.265 parameter sets (ITU-T H.265 7.3.2) and short-term reference picture sets (7.3.7),
//! parsed far enough to decode and converted to Vulkan's `StdVideoH265*` structures.
//! (Connect addition, see VENDORED.md.)

use ash::vk::native as std;

use super::{H265ParseError, bits::BitReader};

/// Sub-layers an H.265 stream may have.
const MAX_SUB_LAYERS: usize = 7;

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ProfileTierLevel {
    pub profile_idc: u8,
    pub tier: bool,
    pub level_idc: u8,
    pub progressive_source: bool,
    pub interlaced_source: bool,
    pub non_packed_constraint: bool,
    pub frame_only_constraint: bool,
}

/// profile_tier_level(1, max_sub_layers_minus1), 7.3.3.
fn profile_tier_level(r: &mut BitReader, max_sub_layers_minus1: u32) -> Result<ProfileTierLevel, H265ParseError> {
    r.skip(2)?; // general_profile_space
    let tier = r.bit()?;
    let profile_idc = r.bits(5)? as u8;
    r.skip(32)?; // general_profile_compatibility_flag[32]
    let progressive_source = r.bit()?;
    let interlaced_source = r.bit()?;
    let non_packed_constraint = r.bit()?;
    let frame_only_constraint = r.bit()?;
    r.skip(43 + 1)?; // constraint and reserved flags, general_inbld_flag / reserved
    let level_idc = r.bits(8)? as u8;
    let mut profile_present = [false; 8];
    let mut level_present = [false; 8];
    for i in 0..max_sub_layers_minus1 as usize {
        profile_present[i] = r.bit()?;
        level_present[i] = r.bit()?;
    }
    if max_sub_layers_minus1 > 0 {
        r.skip(2 * (8 - max_sub_layers_minus1 as usize))?;
    }
    for i in 0..max_sub_layers_minus1 as usize {
        if profile_present[i] {
            r.skip(88)?;
        }
        if level_present[i] {
            r.skip(8)?;
        }
    }
    Ok(ProfileTierLevel {
        profile_idc,
        tier,
        level_idc,
        progressive_source,
        interlaced_source,
        non_packed_constraint,
        frame_only_constraint,
    })
}

/// sps/vps_max_dec_pic_buffering_minus1, max_num_reorder_pics, max_latency_increase_plus1 per
/// sub-layer; values for lower sub-layers copied from the highest when only it is sent.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct DecPicBufMgr {
    pub max_dec_pic_buffering_minus1: [u8; MAX_SUB_LAYERS],
    pub max_num_reorder_pics: [u8; MAX_SUB_LAYERS],
    pub max_latency_increase_plus1: [u32; MAX_SUB_LAYERS],
}

fn dec_pic_buf_mgr(
    r: &mut BitReader,
    ordering_info_present: bool,
    max_sub_layers_minus1: u32,
) -> Result<DecPicBufMgr, H265ParseError> {
    let mut mgr = DecPicBufMgr::default();
    let first = if ordering_info_present { 0 } else { max_sub_layers_minus1 as usize };
    for i in first..=max_sub_layers_minus1 as usize {
        mgr.max_dec_pic_buffering_minus1[i] = r.ue_max(15, "max_dec_pic_buffering")? as u8;
        mgr.max_num_reorder_pics[i] = r.ue_max(15, "max_num_reorder_pics")? as u8;
        mgr.max_latency_increase_plus1[i] = r.ue()?;
    }
    for i in 0..first {
        mgr.max_dec_pic_buffering_minus1[i] = mgr.max_dec_pic_buffering_minus1[first];
        mgr.max_num_reorder_pics[i] = mgr.max_num_reorder_pics[first];
        mgr.max_latency_increase_plus1[i] = mgr.max_latency_increase_plus1[first];
    }
    Ok(mgr)
}

/// A short-term reference picture set: its syntax, and the deltas it works out to (7.4.8).
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ShortTermRps {
    pub inter_ref_pic_set_prediction: bool,
    pub delta_idx_minus1: u32,
    pub delta_rps_sign: bool,
    pub abs_delta_rps_minus1: u32,
    /// Bit j: used_by_curr_pic_flag[j] (predicted sets).
    pub used_by_curr_pic_flag: u32,
    /// Bit j: use_delta_flag[j] (predicted sets).
    pub use_delta_flag: u32,
    /// DeltaPocS0 (negative, nearest first) and UsedByCurrPicS0.
    pub delta_poc_s0: Vec<i32>,
    pub used_s0: Vec<bool>,
    /// DeltaPocS1 (positive, nearest first) and UsedByCurrPicS1.
    pub delta_poc_s1: Vec<i32>,
    pub used_s1: Vec<bool>,
}

impl ShortTermRps {
    pub fn num_delta_pocs(&self) -> usize {
        self.delta_poc_s0.len() + self.delta_poc_s1.len()
    }

    /// st_ref_pic_set(idx), 7.3.7. `sets` are the SPS's sets before this one (all of them for
    /// a set in a slice header, where `idx == num_sets`).
    pub fn parse(
        r: &mut BitReader,
        idx: usize,
        num_sets: usize,
        sets: &[ShortTermRps],
    ) -> Result<Self, H265ParseError> {
        let mut rps = ShortTermRps::default();
        if idx != 0 {
            rps.inter_ref_pic_set_prediction = r.bit()?;
        }
        if rps.inter_ref_pic_set_prediction {
            if idx == num_sets {
                rps.delta_idx_minus1 = r.ue_max(idx as u32 - 1, "delta_idx_minus1")?;
            }
            rps.delta_rps_sign = r.bit()?;
            rps.abs_delta_rps_minus1 = r.ue_max(0x7fff, "abs_delta_rps_minus1")?;
            let reference = &sets[idx - (rps.delta_idx_minus1 as usize + 1)];
            for j in 0..=reference.num_delta_pocs() {
                let used = r.bit()?;
                let use_delta = if used { true } else { r.bit()? };
                rps.used_by_curr_pic_flag |= u32::from(used) << j;
                rps.use_delta_flag |= u32::from(use_delta) << j;
            }
            rps.derive_from(reference);
        } else {
            let negative = r.ue_max(16, "num_negative_pics")? as usize;
            let positive = r.ue_max(16 - negative as u32, "num_positive_pics")? as usize;
            let mut poc = 0;
            for _ in 0..negative {
                poc -= r.ue_max(0x7fff, "delta_poc_s0_minus1")? as i32 + 1;
                rps.delta_poc_s0.push(poc);
                rps.used_s0.push(r.bit()?);
            }
            poc = 0;
            for _ in 0..positive {
                poc += r.ue_max(0x7fff, "delta_poc_s1_minus1")? as i32 + 1;
                rps.delta_poc_s1.push(poc);
                rps.used_s1.push(r.bit()?);
            }
        }
        Ok(rps)
    }

    #[cfg(test)]
    pub(crate) fn derive_from_for_test(&mut self, reference: &ShortTermRps) {
        self.derive_from(reference);
    }

    /// The deltas of a set predicted from `reference` (equations 7-61 and 7-62).
    fn derive_from(&mut self, reference: &ShortTermRps) {
        let d_rps = (1 - 2 * i32::from(self.delta_rps_sign)) * (self.abs_delta_rps_minus1 as i32 + 1);
        let (neg, pos) = (reference.delta_poc_s0.len(), reference.delta_poc_s1.len());
        let used = |j: usize| self.used_by_curr_pic_flag >> j & 1 == 1;
        let use_delta = |j: usize| self.use_delta_flag >> j & 1 == 1;
        let (mut s0, mut u0, mut s1, mut u1) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for j in (0..pos).rev() {
            let d_poc = reference.delta_poc_s1[j] + d_rps;
            if d_poc < 0 && use_delta(neg + j) {
                s0.push(d_poc);
                u0.push(used(neg + j));
            }
        }
        if d_rps < 0 && use_delta(neg + pos) {
            s0.push(d_rps);
            u0.push(used(neg + pos));
        }
        for j in 0..neg {
            let d_poc = reference.delta_poc_s0[j] + d_rps;
            if d_poc < 0 && use_delta(j) {
                s0.push(d_poc);
                u0.push(used(j));
            }
        }
        for j in (0..neg).rev() {
            let d_poc = reference.delta_poc_s0[j] + d_rps;
            if d_poc > 0 && use_delta(j) {
                s1.push(d_poc);
                u1.push(used(j));
            }
        }
        if d_rps > 0 && use_delta(neg + pos) {
            s1.push(d_rps);
            u1.push(used(neg + pos));
        }
        for j in 0..pos {
            let d_poc = reference.delta_poc_s1[j] + d_rps;
            if d_poc > 0 && use_delta(neg + j) {
                s1.push(d_poc);
                u1.push(used(neg + j));
            }
        }
        (self.delta_poc_s0, self.used_s0, self.delta_poc_s1, self.used_s1) = (s0, u0, s1, u1);
    }

    /// As Vulkan's structure: the syntax of a predicted set, and the derived deltas either way,
    /// which is what drivers read when they don't derive them themselves.
    pub fn to_std(&self) -> std::StdVideoH265ShortTermRefPicSet {
        let mut set: std::StdVideoH265ShortTermRefPicSet = unsafe { ::std::mem::zeroed() };
        set.flags.set_inter_ref_pic_set_prediction_flag(self.inter_ref_pic_set_prediction.into());
        set.flags.set_delta_rps_sign(self.delta_rps_sign.into());
        set.delta_idx_minus1 = self.delta_idx_minus1;
        set.use_delta_flag = self.use_delta_flag as u16;
        set.abs_delta_rps_minus1 = self.abs_delta_rps_minus1 as u16;
        set.used_by_curr_pic_flag = self.used_by_curr_pic_flag as u16;
        set.num_negative_pics = self.delta_poc_s0.len() as u8;
        set.num_positive_pics = self.delta_poc_s1.len() as u8;
        let mut previous = 0;
        for (i, (&poc, &used)) in self.delta_poc_s0.iter().zip(&self.used_s0).enumerate() {
            set.delta_poc_s0_minus1[i] = (previous - poc - 1) as u16;
            set.used_by_curr_pic_s0_flag |= u16::from(used) << i;
            previous = poc;
        }
        previous = 0;
        for (i, (&poc, &used)) in self.delta_poc_s1.iter().zip(&self.used_s1).enumerate() {
            set.delta_poc_s1_minus1[i] = (poc - previous - 1) as u16;
            set.used_by_curr_pic_s1_flag |= u16::from(used) << i;
            previous = poc;
        }
        set
    }
}

fn skip_sub_layer_hrd(r: &mut BitReader, cpb_count: u32, sub_pic: bool) -> Result<(), H265ParseError> {
    for _ in 0..cpb_count {
        r.ue()?;
        r.ue()?;
        if sub_pic {
            r.ue()?;
            r.ue()?;
        }
        r.bit()?;
    }
    Ok(())
}

/// hrd_parameters(), E.2.2: read past, not kept.
fn skip_hrd(r: &mut BitReader, common: bool, max_sub_layers_minus1: u32) -> Result<(), H265ParseError> {
    let (mut nal, mut vcl, mut sub_pic) = (false, false, false);
    if common {
        nal = r.bit()?;
        vcl = r.bit()?;
        if nal || vcl {
            sub_pic = r.bit()?;
            if sub_pic {
                r.skip(8 + 5 + 1 + 5)?;
            }
            r.skip(4 + 4)?;
            if sub_pic {
                r.skip(4)?;
            }
            r.skip(5 + 5 + 5)?;
        }
    }
    for _ in 0..=max_sub_layers_minus1 {
        let fixed_general = r.bit()?;
        let fixed_within_cvs = if fixed_general { true } else { r.bit()? };
        let mut low_delay = false;
        if fixed_within_cvs {
            r.ue()?;
        } else {
            low_delay = r.bit()?;
        }
        let cpb_count = if low_delay { 1 } else { r.ue_max(31, "cpb_cnt_minus1")? + 1 };
        if nal {
            skip_sub_layer_hrd(r, cpb_count, sub_pic)?;
        }
        if vcl {
            skip_sub_layer_hrd(r, cpb_count, sub_pic)?;
        }
    }
    Ok(())
}

/// vui_parameters(), E.2.1: read past, not kept (decoding doesn't need it).
fn skip_vui(r: &mut BitReader, max_sub_layers_minus1: u32) -> Result<(), H265ParseError> {
    if r.bit()? && r.bits(8)? == 255 {
        r.skip(32)?; // sar_width, sar_height
    }
    if r.bit()? {
        r.bit()?; // overscan_appropriate_flag
    }
    if r.bit()? {
        r.skip(3 + 1)?; // video_format, video_full_range_flag
        if r.bit()? {
            r.skip(24)?; // colour_primaries, transfer_characteristics, matrix_coeffs
        }
    }
    if r.bit()? {
        r.ue()?;
        r.ue()?;
    }
    r.skip(3)?; // neutral_chroma_indication, field_seq, frame_field_info_present
    if r.bit()? {
        for _ in 0..4 {
            r.ue()?; // default display window offsets
        }
    }
    if r.bit()? {
        r.skip(64)?; // num_units_in_tick, time_scale
        if r.bit()? {
            r.ue()?; // num_ticks_poc_diff_one_minus1
        }
        if r.bit()? {
            skip_hrd(r, true, max_sub_layers_minus1)?;
        }
    }
    if r.bit()? {
        r.skip(3)?; // tiles_fixed_structure, motion_vectors_over_pic_boundaries, restricted_ref_pic_lists
        for _ in 0..5 {
            r.ue()?;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Vps {
    pub id: u8,
    pub max_sub_layers_minus1: u8,
    pub temporal_id_nesting: bool,
    pub ptl: ProfileTierLevel,
    pub sub_layer_ordering_info_present: bool,
    pub dpb: DecPicBufMgr,
    pub timing_info_present: bool,
    pub num_units_in_tick: u32,
    pub time_scale: u32,
    pub poc_proportional_to_timing: bool,
    pub num_ticks_poc_diff_one_minus1: u32,
}

impl Vps {
    /// 7.3.2.1, from the RBSP after the NAL header.
    pub fn parse(rbsp: &[u8]) -> Result<Self, H265ParseError> {
        let r = &mut BitReader::new(rbsp);
        let id = r.bits(4)? as u8;
        r.skip(2 + 6)?; // base_layer_internal/available, max_layers_minus1
        let max_sub_layers_minus1 = r.bits(3)?;
        if max_sub_layers_minus1 as usize >= MAX_SUB_LAYERS {
            return Err(H265ParseError::Invalid("vps_max_sub_layers_minus1"));
        }
        let temporal_id_nesting = r.bit()?;
        r.skip(16)?;
        let ptl = profile_tier_level(r, max_sub_layers_minus1)?;
        let sub_layer_ordering_info_present = r.bit()?;
        let dpb = dec_pic_buf_mgr(r, sub_layer_ordering_info_present, max_sub_layers_minus1)?;
        let max_layer_id = r.bits(6)?;
        let num_layer_sets_minus1 = r.ue_max(1023, "vps_num_layer_sets_minus1")?;
        r.skip(num_layer_sets_minus1 as usize * (max_layer_id as usize + 1))?;
        let timing_info_present = r.bit()?;
        let (mut num_units_in_tick, mut time_scale, mut proportional, mut ticks) = (0, 0, false, 0);
        if timing_info_present {
            num_units_in_tick = r.bits(32)?;
            time_scale = r.bits(32)?;
            proportional = r.bit()?;
            if proportional {
                ticks = r.ue()?;
            }
            // The HRD parameters that follow aren't needed for decoding.
        }
        Ok(Self {
            id,
            max_sub_layers_minus1: max_sub_layers_minus1 as u8,
            temporal_id_nesting,
            ptl,
            sub_layer_ordering_info_present,
            dpb,
            timing_info_present,
            num_units_in_tick,
            time_scale,
            poc_proportional_to_timing: proportional,
            num_ticks_poc_diff_one_minus1: ticks,
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Sps {
    pub vps_id: u8,
    pub max_sub_layers_minus1: u8,
    pub temporal_id_nesting: bool,
    pub ptl: ProfileTierLevel,
    pub id: u8,
    pub chroma_format_idc: u8,
    pub separate_colour_plane: bool,
    pub width: u32,
    pub height: u32,
    pub conformance_window: Option<[u32; 4]>,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
    pub log2_max_poc_lsb_minus4: u8,
    pub sub_layer_ordering_info_present: bool,
    pub dpb: DecPicBufMgr,
    pub log2_min_luma_coding_block_size_minus3: u8,
    pub log2_diff_max_min_luma_coding_block_size: u8,
    pub log2_min_luma_transform_block_size_minus2: u8,
    pub log2_diff_max_min_luma_transform_block_size: u8,
    pub max_transform_hierarchy_depth_inter: u8,
    pub max_transform_hierarchy_depth_intra: u8,
    pub scaling_list_enabled: bool,
    pub amp_enabled: bool,
    pub sample_adaptive_offset_enabled: bool,
    pub pcm: Option<Pcm>,
    pub st_rps: Vec<ShortTermRps>,
    pub long_term_ref_pics_present: bool,
    pub lt_ref_pic_poc_lsb_sps: Vec<u32>,
    pub used_by_curr_pic_lt_sps: Vec<bool>,
    pub temporal_mvp_enabled: bool,
    pub strong_intra_smoothing_enabled: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Pcm {
    pub bit_depth_luma_minus1: u8,
    pub bit_depth_chroma_minus1: u8,
    pub log2_min_coding_block_size_minus3: u8,
    pub log2_diff_max_min_coding_block_size: u8,
    pub loop_filter_disabled: bool,
}

impl Sps {
    /// 7.3.2.2, from the RBSP after the NAL header.
    pub fn parse(rbsp: &[u8]) -> Result<Self, H265ParseError> {
        let r = &mut BitReader::new(rbsp);
        let mut sps = Sps { vps_id: r.bits(4)? as u8, ..Default::default() };
        let max_sub_layers_minus1 = r.bits(3)?;
        if max_sub_layers_minus1 as usize >= MAX_SUB_LAYERS {
            return Err(H265ParseError::Invalid("sps_max_sub_layers_minus1"));
        }
        sps.max_sub_layers_minus1 = max_sub_layers_minus1 as u8;
        sps.temporal_id_nesting = r.bit()?;
        sps.ptl = profile_tier_level(r, max_sub_layers_minus1)?;
        sps.id = r.ue_max(15, "sps_seq_parameter_set_id")? as u8;
        sps.chroma_format_idc = r.ue_max(3, "chroma_format_idc")? as u8;
        if sps.chroma_format_idc == 3 {
            sps.separate_colour_plane = r.bit()?;
        }
        sps.width = r.ue()?;
        sps.height = r.ue()?;
        if r.bit()? {
            sps.conformance_window = Some([r.ue()?, r.ue()?, r.ue()?, r.ue()?]);
        }
        sps.bit_depth_luma_minus8 = r.ue_max(8, "bit_depth_luma_minus8")? as u8;
        sps.bit_depth_chroma_minus8 = r.ue_max(8, "bit_depth_chroma_minus8")? as u8;
        sps.log2_max_poc_lsb_minus4 = r.ue_max(12, "log2_max_pic_order_cnt_lsb_minus4")? as u8;
        sps.sub_layer_ordering_info_present = r.bit()?;
        sps.dpb = dec_pic_buf_mgr(r, sps.sub_layer_ordering_info_present, max_sub_layers_minus1)?;
        sps.log2_min_luma_coding_block_size_minus3 = r.ue_max(3, "log2_min_luma_coding_block_size")? as u8;
        sps.log2_diff_max_min_luma_coding_block_size = r.ue_max(3, "log2_diff_max_min_luma_coding")? as u8;
        sps.log2_min_luma_transform_block_size_minus2 = r.ue_max(3, "log2_min_luma_transform")? as u8;
        sps.log2_diff_max_min_luma_transform_block_size = r.ue_max(3, "log2_diff_max_min_transform")? as u8;
        sps.max_transform_hierarchy_depth_inter = r.ue_max(4, "max_transform_hierarchy_depth_inter")? as u8;
        sps.max_transform_hierarchy_depth_intra = r.ue_max(4, "max_transform_hierarchy_depth_intra")? as u8;
        sps.scaling_list_enabled = r.bit()?;
        if sps.scaling_list_enabled && r.bit()? {
            return Err(H265ParseError::Unsupported("custom scaling lists"));
        }
        sps.amp_enabled = r.bit()?;
        sps.sample_adaptive_offset_enabled = r.bit()?;
        if r.bit()? {
            sps.pcm = Some(Pcm {
                bit_depth_luma_minus1: r.bits(4)? as u8,
                bit_depth_chroma_minus1: r.bits(4)? as u8,
                log2_min_coding_block_size_minus3: r.ue_max(2, "log2_min_pcm_luma_coding_block_size")? as u8,
                log2_diff_max_min_coding_block_size: r.ue_max(2, "log2_diff_max_min_pcm")? as u8,
                loop_filter_disabled: r.bit()?,
            });
        }
        let num_sets = r.ue_max(64, "num_short_term_ref_pic_sets")? as usize;
        for idx in 0..num_sets {
            let set = ShortTermRps::parse(r, idx, num_sets, &sps.st_rps)?;
            sps.st_rps.push(set);
        }
        sps.long_term_ref_pics_present = r.bit()?;
        if sps.long_term_ref_pics_present {
            let count = r.ue_max(32, "num_long_term_ref_pics_sps")?;
            for _ in 0..count {
                sps.lt_ref_pic_poc_lsb_sps.push(r.bits(u32::from(sps.log2_max_poc_lsb_minus4) + 4)?);
                sps.used_by_curr_pic_lt_sps.push(r.bit()?);
            }
        }
        sps.temporal_mvp_enabled = r.bit()?;
        sps.strong_intra_smoothing_enabled = r.bit()?;
        if r.bit()? {
            skip_vui(r, max_sub_layers_minus1)?;
        }
        if r.bit()? {
            // sps_range/multilayer/3d/scc extension flags, sps_extension_4bits
            if r.bits(8)? != 0 {
                return Err(H265ParseError::Unsupported("SPS extensions (only the Main profile is supported)"));
            }
        }
        if sps.chroma_format_idc != 1 || sps.bit_depth_luma_minus8 != 0 || sps.bit_depth_chroma_minus8 != 0 {
            return Err(H265ParseError::Unsupported("only 8-bit 4:2:0 (Main profile) is supported"));
        }
        Ok(sps)
    }

    pub fn max_poc_lsb(&self) -> u32 {
        1 << (u32::from(self.log2_max_poc_lsb_minus4) + 4)
    }

    /// PicSizeInCtbsY, for the slice segment address's length.
    pub fn pic_size_in_ctbs(&self) -> u32 {
        let ctb_log2 = u32::from(self.log2_min_luma_coding_block_size_minus3)
            + 3
            + u32::from(self.log2_diff_max_min_luma_coding_block_size);
        let ctb = 1 << ctb_log2;
        self.width.div_ceil(ctb) * self.height.div_ceil(ctb)
    }

    /// The picture's size with the conformance window cropped off.
    pub fn cropped_size(&self) -> (u32, u32) {
        let [left, right, top, bottom] = self.conformance_window.unwrap_or_default();
        // SubWidthC = SubHeightC = 2 for 4:2:0.
        (self.width - 2 * (left + right), self.height - 2 * (top + bottom))
    }

    /// Largest DPB the stream needs, including the picture being decoded.
    pub fn max_dpb_slots(&self) -> u32 {
        u32::from(self.dpb.max_dec_pic_buffering_minus1[self.max_sub_layers_minus1 as usize]) + 2
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Pps {
    pub id: u8,
    pub sps_id: u8,
    pub dependent_slice_segments_enabled: bool,
    pub output_flag_present: bool,
    pub num_extra_slice_header_bits: u8,
    pub sign_data_hiding_enabled: bool,
    pub cabac_init_present: bool,
    pub num_ref_idx_l0_default_active_minus1: u8,
    pub num_ref_idx_l1_default_active_minus1: u8,
    pub init_qp_minus26: i8,
    pub constrained_intra_pred: bool,
    pub transform_skip_enabled: bool,
    pub cu_qp_delta_enabled: bool,
    pub diff_cu_qp_delta_depth: u8,
    pub cb_qp_offset: i8,
    pub cr_qp_offset: i8,
    pub slice_chroma_qp_offsets_present: bool,
    pub weighted_pred: bool,
    pub weighted_bipred: bool,
    pub transquant_bypass_enabled: bool,
    pub tiles_enabled: bool,
    pub entropy_coding_sync_enabled: bool,
    pub num_tile_columns_minus1: u8,
    pub num_tile_rows_minus1: u8,
    pub uniform_spacing: bool,
    pub column_width_minus1: Vec<u16>,
    pub row_height_minus1: Vec<u16>,
    pub loop_filter_across_tiles_enabled: bool,
    pub loop_filter_across_slices_enabled: bool,
    pub deblocking_filter_control_present: bool,
    pub deblocking_filter_override_enabled: bool,
    pub deblocking_filter_disabled: bool,
    pub beta_offset_div2: i8,
    pub tc_offset_div2: i8,
    pub lists_modification_present: bool,
    pub log2_parallel_merge_level_minus2: u8,
    pub slice_segment_header_extension_present: bool,
}

impl Pps {
    /// 7.3.2.3, from the RBSP after the NAL header.
    pub fn parse(rbsp: &[u8]) -> Result<Self, H265ParseError> {
        let r = &mut BitReader::new(rbsp);
        let mut pps = Pps {
            id: r.ue_max(63, "pps_pic_parameter_set_id")? as u8,
            sps_id: r.ue_max(15, "pps_seq_parameter_set_id")? as u8,
            ..Default::default()
        };
        pps.dependent_slice_segments_enabled = r.bit()?;
        pps.output_flag_present = r.bit()?;
        pps.num_extra_slice_header_bits = r.bits(3)? as u8;
        pps.sign_data_hiding_enabled = r.bit()?;
        pps.cabac_init_present = r.bit()?;
        pps.num_ref_idx_l0_default_active_minus1 = r.ue_max(14, "num_ref_idx_l0_default_active")? as u8;
        pps.num_ref_idx_l1_default_active_minus1 = r.ue_max(14, "num_ref_idx_l1_default_active")? as u8;
        pps.init_qp_minus26 = r.se()?.clamp(-64, 25) as i8;
        pps.constrained_intra_pred = r.bit()?;
        pps.transform_skip_enabled = r.bit()?;
        pps.cu_qp_delta_enabled = r.bit()?;
        if pps.cu_qp_delta_enabled {
            pps.diff_cu_qp_delta_depth = r.ue_max(6, "diff_cu_qp_delta_depth")? as u8;
        }
        pps.cb_qp_offset = r.se()?.clamp(-12, 12) as i8;
        pps.cr_qp_offset = r.se()?.clamp(-12, 12) as i8;
        pps.slice_chroma_qp_offsets_present = r.bit()?;
        pps.weighted_pred = r.bit()?;
        pps.weighted_bipred = r.bit()?;
        pps.transquant_bypass_enabled = r.bit()?;
        pps.tiles_enabled = r.bit()?;
        pps.entropy_coding_sync_enabled = r.bit()?;
        if pps.tiles_enabled {
            pps.num_tile_columns_minus1 = r.ue_max(19, "num_tile_columns_minus1")? as u8;
            pps.num_tile_rows_minus1 = r.ue_max(21, "num_tile_rows_minus1")? as u8;
            pps.uniform_spacing = r.bit()?;
            if !pps.uniform_spacing {
                for _ in 0..pps.num_tile_columns_minus1 {
                    pps.column_width_minus1.push(r.ue_max(0xffff, "column_width_minus1")? as u16);
                }
                for _ in 0..pps.num_tile_rows_minus1 {
                    pps.row_height_minus1.push(r.ue_max(0xffff, "row_height_minus1")? as u16);
                }
            }
            pps.loop_filter_across_tiles_enabled = r.bit()?;
        }
        pps.loop_filter_across_slices_enabled = r.bit()?;
        pps.deblocking_filter_control_present = r.bit()?;
        if pps.deblocking_filter_control_present {
            pps.deblocking_filter_override_enabled = r.bit()?;
            pps.deblocking_filter_disabled = r.bit()?;
            if !pps.deblocking_filter_disabled {
                pps.beta_offset_div2 = r.se()?.clamp(-6, 6) as i8;
                pps.tc_offset_div2 = r.se()?.clamp(-6, 6) as i8;
            }
        }
        if r.bit()? {
            return Err(H265ParseError::Unsupported("custom scaling lists"));
        }
        pps.lists_modification_present = r.bit()?;
        pps.log2_parallel_merge_level_minus2 = r.ue_max(4, "log2_parallel_merge_level_minus2")? as u8;
        pps.slice_segment_header_extension_present = r.bit()?;
        if r.bit()? && r.bits(8)? != 0 {
            return Err(H265ParseError::Unsupported("PPS extensions (only the Main profile is supported)"));
        }
        Ok(pps)
    }
}

/// `StdVideoH265LevelIdc` for a `general_level_idc` (30 × the level).
fn level(level_idc: u8) -> std::StdVideoH265LevelIdc {
    match level_idc {
        0..=30 => std::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_1_0,
        31..=60 => std::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_2_0,
        61..=63 => std::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_2_1,
        64..=90 => std::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_3_0,
        91..=93 => std::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_3_1,
        94..=120 => std::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_4_0,
        121..=123 => std::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_4_1,
        124..=150 => std::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_5_0,
        151..=153 => std::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_5_1,
        154..=156 => std::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_5_2,
        157..=180 => std::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_6_0,
        181..=183 => std::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_6_1,
        _ => std::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_6_2,
    }
}

fn ptl_to_std(ptl: &ProfileTierLevel) -> std::StdVideoH265ProfileTierLevel {
    let mut out: std::StdVideoH265ProfileTierLevel = unsafe { ::std::mem::zeroed() };
    out.flags.set_general_tier_flag(ptl.tier.into());
    out.flags.set_general_progressive_source_flag(ptl.progressive_source.into());
    out.flags.set_general_interlaced_source_flag(ptl.interlaced_source.into());
    out.flags.set_general_non_packed_constraint_flag(ptl.non_packed_constraint.into());
    out.flags.set_general_frame_only_constraint_flag(ptl.frame_only_constraint.into());
    out.general_profile_idc = u32::from(ptl.profile_idc);
    out.general_level_idc = level(ptl.level_idc);
    out
}

fn dpb_to_std(dpb: &DecPicBufMgr) -> std::StdVideoH265DecPicBufMgr {
    std::StdVideoH265DecPicBufMgr {
        max_latency_increase_plus1: dpb.max_latency_increase_plus1,
        max_dec_pic_buffering_minus1: dpb.max_dec_pic_buffering_minus1,
        max_num_reorder_pics: dpb.max_num_reorder_pics,
    }
}

/// A VPS as Vulkan takes it, with the structures it points to.
pub(crate) struct VkVps {
    pub vps: std::StdVideoH265VideoParameterSet,
    _ptl: Box<std::StdVideoH265ProfileTierLevel>,
    _dpb: Box<std::StdVideoH265DecPicBufMgr>,
}

// SAFETY: the pointers only point into the boxes owned alongside.
unsafe impl Send for VkVps {}
unsafe impl Sync for VkVps {}

impl From<&Vps> for VkVps {
    fn from(vps: &Vps) -> Self {
        let ptl = Box::new(ptl_to_std(&vps.ptl));
        let dpb = Box::new(dpb_to_std(&vps.dpb));
        let mut out: std::StdVideoH265VideoParameterSet = unsafe { ::std::mem::zeroed() };
        out.flags.set_vps_temporal_id_nesting_flag(vps.temporal_id_nesting.into());
        out.flags.set_vps_sub_layer_ordering_info_present_flag(vps.sub_layer_ordering_info_present.into());
        out.flags.set_vps_timing_info_present_flag(vps.timing_info_present.into());
        out.flags.set_vps_poc_proportional_to_timing_flag(vps.poc_proportional_to_timing.into());
        out.vps_video_parameter_set_id = vps.id;
        out.vps_max_sub_layers_minus1 = vps.max_sub_layers_minus1;
        out.vps_num_units_in_tick = vps.num_units_in_tick;
        out.vps_time_scale = vps.time_scale;
        out.vps_num_ticks_poc_diff_one_minus1 = vps.num_ticks_poc_diff_one_minus1;
        out.pDecPicBufMgr = &*dpb;
        out.pProfileTierLevel = &*ptl;
        Self { vps: out, _ptl: ptl, _dpb: dpb }
    }
}

/// An SPS as Vulkan takes it, with the structures it points to.
pub(crate) struct VkSps {
    pub sps: std::StdVideoH265SequenceParameterSet,
    _ptl: Box<std::StdVideoH265ProfileTierLevel>,
    _dpb: Box<std::StdVideoH265DecPicBufMgr>,
    _st_rps: Box<[std::StdVideoH265ShortTermRefPicSet]>,
    _lt: Box<std::StdVideoH265LongTermRefPicsSps>,
}

// SAFETY: the pointers only point into the boxes owned alongside.
unsafe impl Send for VkSps {}
unsafe impl Sync for VkSps {}

impl From<&Sps> for VkSps {
    fn from(sps: &Sps) -> Self {
        let ptl = Box::new(ptl_to_std(&sps.ptl));
        let dpb = Box::new(dpb_to_std(&sps.dpb));
        let st_rps: Box<[_]> = sps.st_rps.iter().map(ShortTermRps::to_std).collect();
        let mut lt: Box<std::StdVideoH265LongTermRefPicsSps> = Box::new(unsafe { ::std::mem::zeroed() });
        for (i, (&lsb, &used)) in sps.lt_ref_pic_poc_lsb_sps.iter().zip(&sps.used_by_curr_pic_lt_sps).enumerate() {
            lt.lt_ref_pic_poc_lsb_sps[i] = lsb;
            lt.used_by_curr_pic_lt_sps_flag |= u32::from(used) << i;
        }
        let mut out: std::StdVideoH265SequenceParameterSet = unsafe { ::std::mem::zeroed() };
        let f = &mut out.flags;
        f.set_sps_temporal_id_nesting_flag(sps.temporal_id_nesting.into());
        f.set_separate_colour_plane_flag(sps.separate_colour_plane.into());
        f.set_conformance_window_flag(sps.conformance_window.is_some().into());
        f.set_sps_sub_layer_ordering_info_present_flag(sps.sub_layer_ordering_info_present.into());
        f.set_scaling_list_enabled_flag(sps.scaling_list_enabled.into());
        f.set_amp_enabled_flag(sps.amp_enabled.into());
        f.set_sample_adaptive_offset_enabled_flag(sps.sample_adaptive_offset_enabled.into());
        f.set_pcm_enabled_flag(sps.pcm.is_some().into());
        f.set_pcm_loop_filter_disabled_flag(sps.pcm.as_ref().is_some_and(|p| p.loop_filter_disabled).into());
        f.set_long_term_ref_pics_present_flag(sps.long_term_ref_pics_present.into());
        f.set_sps_temporal_mvp_enabled_flag(sps.temporal_mvp_enabled.into());
        f.set_strong_intra_smoothing_enabled_flag(sps.strong_intra_smoothing_enabled.into());
        // VUI isn't passed on: decoding doesn't need it.
        out.chroma_format_idc = u32::from(sps.chroma_format_idc);
        out.pic_width_in_luma_samples = sps.width;
        out.pic_height_in_luma_samples = sps.height;
        out.sps_video_parameter_set_id = sps.vps_id;
        out.sps_max_sub_layers_minus1 = sps.max_sub_layers_minus1;
        out.sps_seq_parameter_set_id = sps.id;
        out.bit_depth_luma_minus8 = sps.bit_depth_luma_minus8;
        out.bit_depth_chroma_minus8 = sps.bit_depth_chroma_minus8;
        out.log2_max_pic_order_cnt_lsb_minus4 = sps.log2_max_poc_lsb_minus4;
        out.log2_min_luma_coding_block_size_minus3 = sps.log2_min_luma_coding_block_size_minus3;
        out.log2_diff_max_min_luma_coding_block_size = sps.log2_diff_max_min_luma_coding_block_size;
        out.log2_min_luma_transform_block_size_minus2 = sps.log2_min_luma_transform_block_size_minus2;
        out.log2_diff_max_min_luma_transform_block_size = sps.log2_diff_max_min_luma_transform_block_size;
        out.max_transform_hierarchy_depth_inter = sps.max_transform_hierarchy_depth_inter;
        out.max_transform_hierarchy_depth_intra = sps.max_transform_hierarchy_depth_intra;
        out.num_short_term_ref_pic_sets = sps.st_rps.len() as u8;
        out.num_long_term_ref_pics_sps = sps.lt_ref_pic_poc_lsb_sps.len() as u8;
        if let Some(pcm) = &sps.pcm {
            out.pcm_sample_bit_depth_luma_minus1 = pcm.bit_depth_luma_minus1;
            out.pcm_sample_bit_depth_chroma_minus1 = pcm.bit_depth_chroma_minus1;
            out.log2_min_pcm_luma_coding_block_size_minus3 = pcm.log2_min_coding_block_size_minus3;
            out.log2_diff_max_min_pcm_luma_coding_block_size = pcm.log2_diff_max_min_coding_block_size;
        }
        if let Some([left, right, top, bottom]) = sps.conformance_window {
            (out.conf_win_left_offset, out.conf_win_right_offset) = (left, right);
            (out.conf_win_top_offset, out.conf_win_bottom_offset) = (top, bottom);
        }
        out.pProfileTierLevel = &*ptl;
        out.pDecPicBufMgr = &*dpb;
        out.pShortTermRefPicSet = if st_rps.is_empty() { ::std::ptr::null() } else { st_rps.as_ptr() };
        out.pLongTermRefPicsSps = &*lt;
        Self { sps: out, _ptl: ptl, _dpb: dpb, _st_rps: st_rps, _lt: lt }
    }
}

impl From<&Pps> for std::StdVideoH265PictureParameterSet {
    fn from(pps: &Pps) -> Self {
        let mut out: std::StdVideoH265PictureParameterSet = unsafe { ::std::mem::zeroed() };
        let f = &mut out.flags;
        f.set_dependent_slice_segments_enabled_flag(pps.dependent_slice_segments_enabled.into());
        f.set_output_flag_present_flag(pps.output_flag_present.into());
        f.set_sign_data_hiding_enabled_flag(pps.sign_data_hiding_enabled.into());
        f.set_cabac_init_present_flag(pps.cabac_init_present.into());
        f.set_constrained_intra_pred_flag(pps.constrained_intra_pred.into());
        f.set_transform_skip_enabled_flag(pps.transform_skip_enabled.into());
        f.set_cu_qp_delta_enabled_flag(pps.cu_qp_delta_enabled.into());
        f.set_pps_slice_chroma_qp_offsets_present_flag(pps.slice_chroma_qp_offsets_present.into());
        f.set_weighted_pred_flag(pps.weighted_pred.into());
        f.set_weighted_bipred_flag(pps.weighted_bipred.into());
        f.set_transquant_bypass_enabled_flag(pps.transquant_bypass_enabled.into());
        f.set_tiles_enabled_flag(pps.tiles_enabled.into());
        f.set_entropy_coding_sync_enabled_flag(pps.entropy_coding_sync_enabled.into());
        f.set_uniform_spacing_flag(pps.uniform_spacing.into());
        f.set_loop_filter_across_tiles_enabled_flag(pps.loop_filter_across_tiles_enabled.into());
        f.set_pps_loop_filter_across_slices_enabled_flag(pps.loop_filter_across_slices_enabled.into());
        f.set_deblocking_filter_control_present_flag(pps.deblocking_filter_control_present.into());
        f.set_deblocking_filter_override_enabled_flag(pps.deblocking_filter_override_enabled.into());
        f.set_pps_deblocking_filter_disabled_flag(pps.deblocking_filter_disabled.into());
        f.set_lists_modification_present_flag(pps.lists_modification_present.into());
        f.set_slice_segment_header_extension_present_flag(pps.slice_segment_header_extension_present.into());
        out.pps_pic_parameter_set_id = pps.id;
        out.pps_seq_parameter_set_id = pps.sps_id;
        out.num_extra_slice_header_bits = pps.num_extra_slice_header_bits;
        out.num_ref_idx_l0_default_active_minus1 = pps.num_ref_idx_l0_default_active_minus1;
        out.num_ref_idx_l1_default_active_minus1 = pps.num_ref_idx_l1_default_active_minus1;
        out.init_qp_minus26 = pps.init_qp_minus26;
        out.diff_cu_qp_delta_depth = pps.diff_cu_qp_delta_depth;
        out.pps_cb_qp_offset = pps.cb_qp_offset;
        out.pps_cr_qp_offset = pps.cr_qp_offset;
        out.pps_beta_offset_div2 = pps.beta_offset_div2;
        out.pps_tc_offset_div2 = pps.tc_offset_div2;
        out.log2_parallel_merge_level_minus2 = pps.log2_parallel_merge_level_minus2;
        out.num_tile_columns_minus1 = pps.num_tile_columns_minus1;
        out.num_tile_rows_minus1 = pps.num_tile_rows_minus1;
        for (i, &width) in pps.column_width_minus1.iter().enumerate().take(19) {
            out.column_width_minus1[i] = width;
        }
        for (i, &height) in pps.row_height_minus1.iter().enumerate().take(21) {
            out.row_height_minus1[i] = height;
        }
        out
    }
}
