//! H.265 decoding on Vulkan Video (Connect addition, see VENDORED.md): the same images,
//! buffers, synchronisation and output paths as H.264, with an H.265 session, parameter sets
//! and picture information.

use std::sync::Arc;

use ash::vk;
use rustc_hash::FxHashMap;

use super::{
    DecodeResult, DecodeResultMetadata, DecodeSubmission, DecodeSubmissionImageInfo, DecoderTrackerWaitState,
    InFlightDecodeResources, VulkanDecoder, VulkanDecoderError, session_resources::images::DecodingImages,
};
use crate::{
    codec::{
        h264::parameters::H264DecodeProfileInfo,
        h265::{H265Codec, H265VkParameters},
    },
    device::{ColorRange, ColorSpace, DecodingDevice, caps::NativeDecodeProfileCapabilities},
    parser::h265::{
        H265Instruction,
        params::{Pps, Sps, VkSps, VkVps, Vps},
        refs::{H265Picture, RefId},
    },
    wrappers::{
        DecodeInputBufferPool, DecodingQueryPool, ProfileInfo, VideoDecodeQueueExt as _, VideoQueueExt as _,
        VideoSession, VideoSessionParameters,
    },
};

/// Marks an unused entry of the RefPicSet slot lists.
const NO_SLOT: u8 = 0xff;

pub(crate) struct H265State<'a> {
    caps: NativeDecodeProfileCapabilities<H265Codec>,
    profile: Arc<H264DecodeProfileInfo<'a>>,
    session: Option<H265Session<'a>>,
    vps: Vec<Vps>,
    sps: Vec<Sps>,
    pps: Vec<Pps>,
    parameters_changed: bool,
    /// The DPB slot holding each reference picture.
    slots: FxHashMap<RefId, usize>,
}

struct H265Session<'a> {
    video_session: Arc<VideoSession>,
    parameters: Option<Arc<VideoSessionParameters>>,
    decoding_images: DecodingImages<'a>,
    decode_buffer_pool: DecodeInputBufferPool<'a>,
    decode_query_pool: Option<Arc<DecodingQueryPool>>,
    coded_extent: vk::Extent2D,
    max_dpb_slots: u32,
    /// The session hasn't been reset since it was made.
    needs_reset: bool,
}

impl<'a> H265State<'a> {
    pub(crate) fn new(
        caps: NativeDecodeProfileCapabilities<H265Codec>,
        usage_info: vk::VideoDecodeUsageInfoKHR<'a>,
    ) -> Self {
        let h265_profile = vk::VideoDecodeH265ProfileInfoKHR::default()
            .std_profile_idc(vk::native::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN);
        let profile = vk::VideoProfileInfoKHR::default()
            .video_codec_operation(vk::VideoCodecOperationFlagsKHR::DECODE_H265)
            .chroma_subsampling(vk::VideoChromaSubsamplingFlagsKHR::TYPE_420)
            .luma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8)
            .chroma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8);
        // The H.264 wrapper type is codec-neutral apart from its comparison fields.
        let profile = Arc::new(H264DecodeProfileInfo {
            profile_info: ProfileInfo::new(profile, vec![Box::new(h265_profile), Box::new(usage_info)]),
            profile_idc: vk::native::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN,
            picture_layout: vk::VideoDecodeH264PictureLayoutFlagsKHR::empty(),
        });
        Self {
            caps,
            profile,
            session: None,
            vps: Vec::new(),
            sps: Vec::new(),
            pps: Vec::new(),
            parameters_changed: false,
            slots: FxHashMap::default(),
        }
    }
}

impl<'a> VulkanDecoder<'a> {
    /// A decoder for H.265 on `decoding_device` (see [`crate::VulkanDevice::decoding_device_h265`]).
    pub(crate) fn new_h265(
        decoding_device: Arc<DecodingDevice>,
        caps: NativeDecodeProfileCapabilities<H265Codec>,
        usage_flags: crate::parameters::DecoderUsageFlags,
        image_modifiers: super::ImageModifiers,
    ) -> Result<Self, VulkanDecoderError> {
        let mut decoder = Self::new(decoding_device, usage_flags, image_modifiers)?;
        decoder.h265 = Some(Box::new(H265State::new(caps, decoder.usage_info)));
        Ok(decoder)
    }

    pub(crate) fn decode_h265<'b>(
        &'b mut self,
        instruction: &H265Instruction,
    ) -> Result<Option<DecodeSubmission<'b, 'a>>, VulkanDecoderError> {
        let state = self.h265.as_mut().ok_or(VulkanDecoderError::NoSession)?;
        match instruction {
            H265Instruction::Parameters { vps, sps, pps } => {
                (state.vps, state.sps, state.pps) = (vps.clone(), sps.clone(), pps.clone());
                state.parameters_changed = true;
                Ok(None)
            }
            H265Instruction::Decode(picture) => self.decode_h265_picture(picture).map(Some),
        }
    }

    pub(crate) fn decode_h265_to_bytes(
        &mut self,
        instructions: &[H265Instruction],
    ) -> Result<Vec<DecodeResult<crate::RawFrameData>>, VulkanDecoderError> {
        let mut result = Vec::new();
        for instruction in instructions {
            if let Some(output) = self.decode_h265(instruction)? {
                result.push(output.download_output()?);
            }
        }
        Ok(result)
    }

    #[cfg(feature = "wgpu")]
    pub(crate) fn decode_h265_to_wgpu_textures(
        &mut self,
        instructions: &[H265Instruction],
    ) -> Result<Vec<DecodeResult<wgpu::Texture>>, VulkanDecoderError> {
        let mut result = Vec::new();
        for instruction in instructions {
            if let Some(output) = self.decode_h265(instruction)? {
                result.push(output.output_to_wgpu_texture()?);
            }
        }
        Ok(result)
    }

    /// Makes the session fit `picture`'s SPS (only at a random access point, where nothing
    /// earlier is needed), and the parameter sets current.
    fn prepare_h265_session(&mut self, picture: &H265Picture) -> Result<(), VulkanDecoderError> {
        let Self { h265, tracker, decoding_device, image_modifiers, .. } = self;
        let state = h265.as_mut().ok_or(VulkanDecoderError::NoSession)?;
        let sps = state
            .sps
            .iter()
            .find(|sps| sps.id == picture.sps_id)
            .ok_or_else(|| VulkanDecoderError::InvalidInputData(format!("unknown H.265 SPS {}", picture.sps_id)))?;
        let video_caps = &state.caps.video_capabilities;
        let extent = vk::Extent2D { width: sps.width, height: sps.height };
        if extent.width > video_caps.max_coded_extent.width || extent.height > video_caps.max_coded_extent.height {
            return Err(VulkanDecoderError::InvalidInputData(format!(
                "{}x{} H.265 is larger than the GPU decodes",
                extent.width, extent.height
            )));
        }
        let max_dpb_slots = picture.max_dpb_slots.min(video_caps.max_dpb_slots);
        let fits = state.session.as_ref().is_some_and(|session| {
            session.coded_extent.width >= extent.width
                && session.coded_extent.height >= extent.height
                && session.max_dpb_slots >= max_dpb_slots
        });
        if !fits {
            if !picture.irap {
                return Err(VulkanDecoderError::InvalidInputData(
                    "H.265 stream grew without a random access point".into(),
                ));
            }
            let max_active_references =
                max_dpb_slots.saturating_sub(1).min(video_caps.max_active_reference_pictures).max(1);
            let video_session = Arc::new(VideoSession::new(
                &decoding_device.vulkan_device,
                &decoding_device.h264_decode_queues,
                &state.profile.profile_info.profile_info,
                extent,
                max_dpb_slots,
                max_active_references,
                vk::VideoSessionCreateFlagsKHR::empty(),
                &video_caps.std_header_version,
            )?);
            let mut command_buffer = tracker.command_buffer_pools.decode.begin_buffer()?;
            let mut dpb_format = decoding_device.profile_capabilities.dpb_format_properties;
            if decoding_device.profile_capabilities.dst_format_properties.is_none() {
                dpb_format.image_create_flags |= image_modifiers.create_flags;
                dpb_format.image_usage_flags |= image_modifiers.usage_flags;
            }
            let dst_format = decoding_device.profile_capabilities.dst_format_properties.map(|p| {
                p.image_create_flags(p.image_create_flags | image_modifiers.create_flags)
                    .image_usage_flags(p.image_usage_flags | image_modifiers.usage_flags)
            });
            let decoding_images = DecodingImages::new(
                decoding_device,
                &mut command_buffer,
                tracker.image_layout_tracker.clone(),
                &state.profile,
                &dpb_format,
                &dst_format,
                extent,
                max_dpb_slots,
                image_modifiers.additional_queue_index as u32,
            )?;
            decoding_device.h264_decode_queues.submit_chain_semaphore(
                command_buffer.end()?,
                tracker,
                vk::PipelineStageFlags2::ALL_COMMANDS,
                vk::PipelineStageFlags2::ALL_COMMANDS,
                DecoderTrackerWaitState::NewDecodingImagesLayoutTransition,
            )?;
            let decode_query_pool = if decoding_device.h264_decode_queues.supports_result_status_queries() {
                Some(Arc::new(DecodingQueryPool::new(
                    decoding_device.vulkan_device.device.clone(),
                    state.profile.profile_info.profile_info,
                )?))
            } else {
                None
            };
            state.session = Some(H265Session {
                video_session,
                parameters: None,
                decoding_images,
                decode_buffer_pool: DecodeInputBufferPool::new(
                    decoding_device.allocator.clone(),
                    state.profile.clone(),
                ),
                decode_query_pool,
                coded_extent: extent,
                max_dpb_slots,
                needs_reset: true,
            });
            state.slots.clear();
            state.parameters_changed = true;
        }
        let session = state.session.as_mut().expect("made above");
        if std::mem::take(&mut state.parameters_changed) || session.parameters.is_none() {
            // All parameter sets afresh: simpler than updating in place, and they only change
            // at keyframes.
            let vps: Vec<VkVps> = state.vps.iter().map(VkVps::from).collect();
            let sps: Vec<VkSps> = state.sps.iter().map(VkSps::from).collect();
            let parameters = H265VkParameters {
                vps: vps.iter().map(|v| v.vps).collect(),
                sps: sps.iter().map(|s| s.sps).collect(),
                pps: state.pps.iter().map(Into::into).collect(),
            };
            // `vps` and `sps` own what the structures point to; creation copies it all.
            session.parameters = Some(Arc::new(VideoSessionParameters::new::<H265Codec>(
                decoding_device.vulkan_device.device.clone(),
                session.video_session.session,
                parameters,
                None,
                None,
            )?));
            drop((vps, sps));
        }
        Ok(())
    }

    fn decode_h265_picture<'b>(
        &'b mut self,
        picture: &H265Picture,
    ) -> Result<DecodeSubmission<'b, 'a>, VulkanDecoderError> {
        self.prepare_h265_session(picture)?;
        let state = self.h265.as_mut().expect("checked by prepare");
        let sps = state.sps.iter().find(|sps| sps.id == picture.sps_id).expect("checked by prepare");
        let max_num_reorder_frames = u64::from(sps.dpb.max_num_reorder_pics[sps.max_sub_layers_minus1 as usize]);
        let session = state.session.as_mut().expect("made by prepare");

        for id in &picture.dropped {
            if let Some(slot) = state.slots.remove(id) {
                session.decoding_images.free_reference_picture(slot);
            }
        }
        // A fresh start: the decoder forgets everything, as at an H.264 IDR.
        let reset = std::mem::take(&mut session.needs_reset) || (picture.irap && picture.kept.is_empty());
        if reset {
            session.decoding_images.reset_all_allocations();
            state.slots.clear();
        }

        let size = (picture.data.len() as u64).next_multiple_of(
            self.decoding_device.profile_capabilities.video_capabilities.min_bitstream_buffer_size_alignment,
        );
        let mut buffer = session.decode_buffer_pool.buffer()?;
        buffer.upload_data(&picture.data, size, &state.profile)?;

        let device = &self.decoding_device.vulkan_device.device;
        let mut cmd_buffer = self.tracker.command_buffer_pools.decode.begin_buffer()?;
        let whole = vk::ImageSubresourceRange {
            base_array_layer: 0,
            layer_count: vk::REMAINING_ARRAY_LAYERS,
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
        };
        session.decoding_images.dpb.image.image_with_view.transition_layout(
            &mut cmd_buffer,
            vk::PipelineStageFlags2::VIDEO_DECODE_KHR..vk::PipelineStageFlags2::VIDEO_DECODE_KHR,
            vk::AccessFlags2::VIDEO_DECODE_WRITE_KHR
                ..vk::AccessFlags2::VIDEO_DECODE_WRITE_KHR | vk::AccessFlags2::VIDEO_DECODE_READ_KHR,
            vk::ImageLayout::VIDEO_DECODE_DPB_KHR,
            whole,
        )?;
        if let Some(dst) = &session.decoding_images.dst_image {
            dst.image_with_view.transition_layout(
                &mut cmd_buffer,
                vk::PipelineStageFlags2::VIDEO_DECODE_KHR..vk::PipelineStageFlags2::VIDEO_DECODE_KHR,
                vk::AccessFlags2::VIDEO_DECODE_WRITE_KHR
                    ..vk::AccessFlags2::VIDEO_DECODE_WRITE_KHR | vk::AccessFlags2::VIDEO_DECODE_READ_KHR,
                vk::ImageLayout::VIDEO_DECODE_DST_KHR,
                whole,
            )?;
        }
        let memory_barrier = vk::MemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::VIDEO_DECODE_KHR)
            .src_access_mask(vk::AccessFlags2::VIDEO_DECODE_WRITE_KHR)
            .dst_stage_mask(vk::PipelineStageFlags2::VIDEO_DECODE_KHR)
            .dst_access_mask(vk::AccessFlags2::VIDEO_DECODE_READ_KHR | vk::AccessFlags2::VIDEO_DECODE_WRITE_KHR);
        unsafe {
            device.cmd_pipeline_barrier2(
                cmd_buffer.buffer(),
                &vk::DependencyInfo::default().memory_barriers(&[memory_barrier]),
            )
        };
        if let Some(pool) = session.decode_query_pool.as_ref() {
            pool.reset(cmd_buffer.buffer());
        }

        let parameters = session.parameters.clone().expect("made by prepare");
        {
            let begin_slots = session.decoding_images.reference_slot_info();
            let begin_info = vk::VideoBeginCodingInfoKHR::default()
                .video_session(session.video_session.session)
                .video_session_parameters(parameters.parameters)
                .reference_slots(&begin_slots);
            unsafe { device.video_queue_ext.cmd_begin_video_coding_khr(cmd_buffer.buffer(), &begin_info) };
        }
        if reset {
            let control_info = vk::VideoCodingControlInfoKHR::default().flags(vk::VideoCodingControlFlagsKHR::RESET);
            unsafe { device.video_queue_ext.cmd_control_video_coding_khr(cmd_buffer.buffer(), &control_info) };
        }

        // The slot this picture is decoded into, and kept in while it's a reference.
        let new_slot = session.decoding_images.allocate_reference_picture()?;
        let mut setup_std: vk::native::StdVideoDecodeH265ReferenceInfo = unsafe { std::mem::zeroed() };
        setup_std.PicOrderCntVal = picture.poc;
        let mut setup_dpb_info = vk::VideoDecodeH265DpbSlotInfoKHR::default().std_reference_info(&setup_std);
        let setup_resource = *session.decoding_images.video_resource_info(new_slot).expect("allocated above");
        let setup_slot = vk::VideoReferenceSlotInfoKHR::default()
            .picture_resource(&setup_resource)
            .slot_index(new_slot as i32)
            .push_next(&mut setup_dpb_info);

        // The pictures this one uses: every distinct one in the three "Curr" lists.
        let slot_of =
            |id: RefId| state.slots.get(&id).copied().ok_or(VulkanDecoderError::NonExistentReferenceRequested);
        let mut used = Vec::new();
        for reference in picture.st_curr_before.iter().chain(&picture.st_curr_after).chain(&picture.lt_curr) {
            if !used.iter().any(|(r, _): &(crate::parser::h265::refs::Reference, usize)| r.id == reference.id) {
                used.push((*reference, slot_of(reference.id)?));
            }
        }
        let reference_std: Vec<vk::native::StdVideoDecodeH265ReferenceInfo> = used
            .iter()
            .map(|(reference, _)| {
                let mut info: vk::native::StdVideoDecodeH265ReferenceInfo = unsafe { std::mem::zeroed() };
                info.flags.set_used_for_long_term_reference(reference.long_term.into());
                info.PicOrderCntVal = reference.poc;
                info
            })
            .collect();
        let mut reference_dpb_info: Vec<vk::VideoDecodeH265DpbSlotInfoKHR> = reference_std
            .iter()
            .map(|info| vk::VideoDecodeH265DpbSlotInfoKHR::default().std_reference_info(info))
            .collect();
        let reference_resources: Vec<vk::VideoPictureResourceInfoKHR> = used
            .iter()
            .map(|(_, slot)| {
                session
                    .decoding_images
                    .video_resource_info(*slot)
                    .copied()
                    .ok_or(VulkanDecoderError::NonExistentReferenceRequested)
            })
            .collect::<Result<_, _>>()?;
        let reference_slots: Vec<vk::VideoReferenceSlotInfoKHR> = used
            .iter()
            .zip(&reference_resources)
            .zip(reference_dpb_info.iter_mut())
            .map(|(((_, slot), resource), dpb_info)| {
                vk::VideoReferenceSlotInfoKHR::default()
                    .picture_resource(resource)
                    .slot_index(*slot as i32)
                    .push_next(dpb_info)
            })
            .collect();

        let slot_list = |list: &[crate::parser::h265::refs::Reference]| -> Result<[u8; 8], VulkanDecoderError> {
            let mut out = [NO_SLOT; 8];
            for (entry, reference) in out.iter_mut().zip(list) {
                *entry = slot_of(reference.id)? as u8;
            }
            Ok(out)
        };
        let mut std_picture: vk::native::StdVideoDecodeH265PictureInfo = unsafe { std::mem::zeroed() };
        std_picture.flags.set_IrapPicFlag(picture.irap.into());
        std_picture.flags.set_IdrPicFlag(picture.idr.into());
        std_picture.flags.set_IsReference(picture.is_reference.into());
        std_picture.flags.set_short_term_ref_pic_set_sps_flag(picture.short_term_ref_pic_set_sps_flag.into());
        std_picture.sps_video_parameter_set_id = picture.vps_id;
        std_picture.pps_seq_parameter_set_id = picture.sps_id;
        std_picture.pps_pic_parameter_set_id = picture.pps_id;
        std_picture.NumDeltaPocsOfRefRpsIdx = picture.num_delta_pocs_of_ref_rps_idx;
        std_picture.PicOrderCntVal = picture.poc;
        std_picture.NumBitsForSTRefPicSetInSlice = picture.st_rps_bits;
        std_picture.RefPicSetStCurrBefore = slot_list(&picture.st_curr_before)?;
        std_picture.RefPicSetStCurrAfter = slot_list(&picture.st_curr_after)?;
        std_picture.RefPicSetLtCurr = slot_list(&picture.lt_curr)?;
        let mut picture_info = vk::VideoDecodeH265PictureInfoKHR::default()
            .std_picture_info(&std_picture)
            .slice_segment_offsets(&picture.slice_offsets);

        let dst_resource = session.decoding_images.target_picture_resource_info(new_slot).expect("allocated above");
        let (target_image, target_layer) = session.decoding_images.target_info(new_slot);
        let decode_info = vk::VideoDecodeInfoKHR::default()
            .src_buffer(*buffer.buffer)
            .src_buffer_offset(0)
            .src_buffer_range(size)
            .dst_picture_resource(dst_resource)
            .setup_reference_slot(&setup_slot)
            .reference_slots(&reference_slots)
            .push_next(&mut picture_info);
        if let Some(pool) = session.decode_query_pool.as_ref() {
            pool.begin_query(cmd_buffer.buffer());
        }
        unsafe { device.video_decode_queue_ext.cmd_decode_video_khr(cmd_buffer.buffer(), &decode_info) };
        if let Some(pool) = session.decode_query_pool.as_ref() {
            pool.end_query(cmd_buffer.buffer());
        }
        unsafe {
            device.video_queue_ext.cmd_end_video_coding_khr(cmd_buffer.buffer(), &vk::VideoEndCodingInfoKHR::default())
        };
        let semaphore_wait_value = self.decoding_device.h264_decode_queues.submit_chain_semaphore(
            cmd_buffer.end()?,
            &mut self.tracker,
            vk::PipelineStageFlags2::ALL_COMMANDS,
            vk::PipelineStageFlags2::ALL_COMMANDS,
            DecoderTrackerWaitState::Decode,
        )?;
        state.slots.insert(picture.id, new_slot);

        let in_flight_resources = InFlightDecodeResources {
            _video_session: session.video_session.clone(),
            _video_session_params: parameters,
            _dpb_image_with_view: session.decoding_images.dpb_image_with_view(),
            _dst_image_with_view: session.decoding_images.dst_image_with_view(),
        };
        let (width, height) = picture.cropped_size;
        let decode_query_pool = session.decode_query_pool.clone();
        Ok(DecodeSubmission {
            decode_result: DecodeResult {
                frame: DecodeSubmissionImageInfo {
                    image: target_image,
                    layer: target_layer as u32,
                    cropped_extent: vk::Extent2D { width, height },
                },
                metadata: DecodeResultMetadata {
                    pts: picture.pts,
                    pic_order_cnt: picture.poc,
                    max_num_reorder_frames,
                    is_idr: picture.irap && picture.kept.is_empty(),
                    color_space: ColorSpace::Unspecified,
                    color_range: ColorRange::Limited,
                },
            },
            semaphore_wait_value,
            decode_query_pool,
            in_flight_resources,
            input_buffer: buffer,
            decoder: self,
        })
    }
}
