//! Picture order counts (H.265 8.3.1) and reference picture sets (8.3.2): which decoded
//! pictures each new one refers to, and which stop being needed.
//! (Connect addition, see VENDORED.md.)

use super::{
    H265ParseError, NalType,
    params::{Pps, Sps},
    slice::SliceHeader,
};

/// Names a decoded picture kept for reference, for the decoder to map to a DPB slot.
pub(crate) type RefId = u64;

/// A reference picture the current one uses.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Reference {
    pub id: RefId,
    pub poc: i32,
    pub long_term: bool,
}

/// One picture to decode, with everything Vulkan's picture information needs.
#[derive(Debug, Clone)]
pub(crate) struct H265Picture {
    pub id: RefId,
    pub vps_id: u8,
    pub sps_id: u8,
    pub pps_id: u8,
    pub poc: i32,
    pub irap: bool,
    pub idr: bool,
    pub is_reference: bool,
    pub short_term_ref_pic_set_sps_flag: bool,
    pub st_rps_bits: u16,
    pub num_delta_pocs_of_ref_rps_idx: u8,
    /// RefPicSetStCurrBefore, RefPicSetStCurrAfter and RefPicSetLtCurr, in order.
    pub st_curr_before: Vec<Reference>,
    pub st_curr_after: Vec<Reference>,
    pub lt_curr: Vec<Reference>,
    /// Pictures kept for later pictures (in the "Foll" sets) as well as the ones above.
    pub kept: Vec<Reference>,
    /// Pictures no longer needed, whose DPB slots can be reused, starting with this picture.
    pub dropped: Vec<RefId>,
    /// The slices, each after a 3-byte start code, and where each starts.
    pub data: Vec<u8>,
    pub slice_offsets: Vec<u32>,
    pub pts: Option<u64>,
    /// The picture's size after cropping.
    pub cropped_size: (u32, u32),
    pub max_dpb_slots: u32,
}

#[derive(Debug, Clone, Copy)]
struct Stored {
    id: RefId,
    poc: i32,
    long_term: bool,
}

#[derive(Debug, Default)]
pub(crate) struct References {
    dpb: Vec<Stored>,
    next_id: RefId,
    /// PicOrderCntVal of the previous TemporalId 0 picture that isn't RASL, RADL or a
    /// sub-layer non-reference picture (prevTid0Pic).
    prev_tid0_poc: i32,
    /// Whether the next random access point starts afresh: at the start, after an end of
    /// sequence, or after lost data (HandleCraAsBlaFlag).
    restart: bool,
    started: bool,
    /// NoRaslOutputFlag of the latest random access point: its RASL pictures are skipped.
    skip_rasl: bool,
}

impl References {
    pub fn mark_missed(&mut self) {
        self.started = false;
    }

    pub fn end_of_sequence(&mut self) {
        self.restart = true;
    }

    /// Works out the next picture. `None` when it can't be decoded: before the first random
    /// access point, and RASL pictures belonging to one decoding started at.
    #[allow(clippy::too_many_arguments)]
    pub fn put_picture(
        &mut self,
        kind: NalType,
        temporal_id: u8,
        header: &SliceHeader,
        sps: &Sps,
        pps: &Pps,
        data: Vec<u8>,
        slice_offsets: Vec<u32>,
        pts: Option<u64>,
    ) -> Result<Option<H265Picture>, H265ParseError> {
        let irap = kind.is_irap();
        if !self.started && !irap {
            return Ok(None);
        }
        let mut dropped = Vec::new();
        if irap {
            let no_rasl_output = kind.is_idr() || kind.is_bla() || !self.started || self.restart;
            self.skip_rasl = no_rasl_output;
            self.started = true;
            self.restart = false;
            if no_rasl_output {
                // Nothing before a fresh start is referenced again.
                dropped.extend(self.dpb.drain(..).map(|stored| stored.id));
            }
        }
        if kind.is_rasl() && self.skip_rasl {
            return Ok(None);
        }

        // 8.3.1: PicOrderCntVal.
        let max_lsb = sps.max_poc_lsb() as i32;
        let lsb = header.poc_lsb as i32;
        let msb = if irap && self.skip_rasl {
            0
        } else {
            let prev_lsb = self.prev_tid0_poc & (max_lsb - 1);
            let prev_msb = self.prev_tid0_poc - prev_lsb;
            if lsb < prev_lsb && prev_lsb - lsb >= max_lsb / 2 {
                prev_msb + max_lsb
            } else if lsb > prev_lsb && lsb - prev_lsb > max_lsb / 2 {
                prev_msb - max_lsb
            } else {
                prev_msb
            }
        };
        let poc = msb + lsb;

        // 8.3.2: the reference picture set.
        let rps = &header.st_rps;
        let mut st_curr_before = Vec::new();
        let mut st_curr_after = Vec::new();
        let mut lt_curr = Vec::new();
        let mut kept = Vec::new();
        if !kind.is_idr() {
            let short_term =
                |dpb: &[Stored], target: i32| dpb.iter().find(|s| !s.long_term && s.poc == target).copied();
            for (&delta, &used) in
                rps.delta_poc_s0.iter().zip(&rps.used_s0).chain(rps.delta_poc_s1.iter().zip(&rps.used_s1))
            {
                let target = poc + delta;
                let found = short_term(&self.dpb, target);
                match (found, used) {
                    (Some(stored), true) => {
                        let reference = Reference { id: stored.id, poc: stored.poc, long_term: false };
                        if delta < 0 {
                            st_curr_before.push(reference)
                        } else {
                            st_curr_after.push(reference)
                        }
                        kept.push(reference);
                    }
                    (Some(stored), false) => kept.push(Reference { id: stored.id, poc: stored.poc, long_term: false }),
                    // A missing picture this one uses can't be worked around; one only kept for
                    // later ones just doesn't come back.
                    (None, true) => return Err(H265ParseError::MissingReference(target)),
                    (None, false) => {}
                }
            }
            for lt in &header.long_term {
                let found = match lt.delta_poc_msb_cycle {
                    Some(cycle) => {
                        let target = poc - (cycle as i32) * max_lsb - (lsb - lt.poc_lsb as i32);
                        self.dpb.iter().find(|s| s.poc == target).copied()
                    }
                    None => self.dpb.iter().find(|s| (s.poc & (max_lsb - 1)) == lt.poc_lsb as i32).copied(),
                };
                match found {
                    Some(stored) => {
                        let reference = Reference { id: stored.id, poc: stored.poc, long_term: true };
                        if lt.used_by_curr_pic {
                            lt_curr.push(reference);
                        }
                        kept.push(reference);
                    }
                    None if lt.used_by_curr_pic => return Err(H265ParseError::MissingReference(lt.poc_lsb as i32)),
                    None => {}
                }
            }
        }
        // Everything not in the set is no longer used for reference.
        let (keep, gone): (Vec<Stored>, Vec<Stored>) =
            self.dpb.drain(..).partition(|stored| kept.iter().any(|k| k.id == stored.id));
        dropped.extend(gone.iter().map(|stored| stored.id));
        self.dpb = keep
            .into_iter()
            .map(|mut stored| {
                stored.long_term = kept.iter().any(|k| k.id == stored.id && k.long_term);
                stored
            })
            .collect();
        let max_dpb_slots = sps.max_dpb_slots();
        if self.dpb.len() + 1 > max_dpb_slots as usize {
            return Err(H265ParseError::DpbOverflow);
        }

        if temporal_id == 0 && !kind.is_rasl() && !kind.is_radl() && !kind.is_sub_layer_non_reference() {
            self.prev_tid0_poc = poc;
        }
        let id = self.next_id;
        self.next_id += 1;
        // The decoded picture is kept as a short-term reference until a later set leaves it out.
        self.dpb.push(Stored { id, poc, long_term: false });

        Ok(Some(H265Picture {
            id,
            vps_id: sps.vps_id,
            sps_id: sps.id,
            pps_id: pps.id,
            poc,
            irap,
            idr: kind.is_idr(),
            is_reference: !kind.is_sub_layer_non_reference(),
            short_term_ref_pic_set_sps_flag: header.short_term_ref_pic_set_sps_flag,
            st_rps_bits: header.st_rps_bits,
            num_delta_pocs_of_ref_rps_idx: header.num_delta_pocs_of_ref_rps_idx,
            st_curr_before,
            st_curr_after,
            lt_curr,
            kept,
            dropped,
            data,
            slice_offsets,
            pts,
            cropped_size: sps.cropped_size(),
            max_dpb_slots,
        }))
    }
}
