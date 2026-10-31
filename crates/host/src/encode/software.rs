//! H.264 on the processor with OpenH264: the encoder every host has.

use anyhow::Result;
use bytes::Bytes;
use connect_proto::Codec;
use openh264::{
    OpenH264API,
    encoder::{BitRate, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, RateControlMode, UsageType},
};

use super::{Encoded, INTRA_PERIOD_FRAMES, Input, InputKind, VideoEncoder};
use crate::yuv::Layout;

pub(crate) struct OpenH264(Box<Encoder>);

impl OpenH264 {
    pub fn new(bitrate_bps: u32, fps: u32) -> Result<Self> {
        Ok(Self(Box::new(Encoder::with_api_config(OpenH264API::from_source(), config(bitrate_bps, fps))?)))
    }
}

pub(super) fn config(bitrate_bps: u32, fps: u32) -> EncoderConfig {
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get().min(4)) as u16;
    EncoderConfig::new()
        .usage_type(UsageType::ScreenContentRealTime)
        .rate_control_mode(RateControlMode::Bitrate)
        .bitrate(BitRate::from_bps(bitrate_bps))
        .max_frame_rate(FrameRate::from_hz(fps as f32))
        // A single slice forces one worker even with automatic threading. Bound CPU use for games.
        .max_slice_len(16 * 1024)
        .num_threads(threads)
        // Never drop a frame to save bits: in fast motion that stutters. The session holds
        // frames back instead when the network, rather than the encoder's budget, can't keep up.
        .skip_frames(false)
        // Not supported for screen content; openh264 warns and turns them off otherwise.
        .adaptive_quantization(false)
        .background_detection(false)
        .intra_frame_period(IntraFramePeriod::from_num_frames(INTRA_PERIOD_FRAMES))
}

impl VideoEncoder for OpenH264 {
    fn codec(&self) -> Codec {
        Codec::H264
    }

    fn name(&self) -> &str {
        "CPU: OpenH264"
    }

    fn hardware(&self) -> bool {
        false
    }

    fn input(&self) -> InputKind {
        InputKind::Picture(Layout::I420)
    }

    /// Changes OpenH264's target (and ceiling) bitrate in place, without a new keyframe.
    fn set_bitrate(&mut self, from: u32, to: u32) {
        use openh264_sys2::{ENCODER_OPTION_BITRATE, ENCODER_OPTION_MAX_BITRATE, SBitrateInfo, SPATIAL_LAYER_ALL};
        let mut info = SBitrateInfo { iLayer: SPATIAL_LAYER_ALL, iBitrate: to.min(i32::MAX as u32) as i32 };
        // The ceiling goes first when rising and last when falling, so it never sits below the target.
        let options = if to > from {
            [ENCODER_OPTION_MAX_BITRATE, ENCODER_OPTION_BITRATE]
        } else {
            [ENCODER_OPTION_BITRATE, ENCODER_OPTION_MAX_BITRATE]
        };
        for option in options {
            // SAFETY: the encoder is initialised (it has encoded a frame), and both options read an
            // SBitrateInfo that outlives the call.
            if unsafe { self.0.raw_api().set_option(option, (&raw mut info).cast()) } != 0 {
                tracing::debug!(bps = to, "OpenH264 refused a bitrate change");
            }
        }
    }

    fn encode(&mut self, input: Input<'_>, capture_time_us: u64, force_keyframe: bool) -> Result<Vec<Encoded>> {
        // Only one way in off Windows.
        #[cfg_attr(not(windows), expect(clippy::infallible_destructuring_match))]
        let picture = match input {
            Input::Picture(picture) => picture,
            #[cfg(windows)]
            Input::Texture(_) => anyhow::bail!("OpenH264 takes pictures in memory"),
        };
        anyhow::ensure!(picture.layout() == Layout::I420, "OpenH264 takes I420");
        if force_keyframe {
            self.0.force_intra_frame();
        }
        let encoded = self.0.encode_at(picture, openh264::Timestamp::from_millis(capture_time_us / 1000))?;
        if encoded.frame_type() == FrameType::Skip {
            return Ok(Vec::new());
        }
        let data = encoded.to_vec();
        if data.is_empty() {
            return Ok(Vec::new());
        }
        Ok(vec![Encoded { data: Bytes::from(data), keyframe: encoded.frame_type() == FrameType::IDR, capture_time_us }])
    }
}
