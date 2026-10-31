//! Video decoders, one or more per codec, behind one interface; and which codecs this PC can
//! decode, for the host to choose from.
//!
//! H.264 decodes on the graphics card through Vulkan Video where the driver offers it, and
//! always on the processor with OpenH264. Codecs without a software decoder (H.265 and AV1, from
//! 0.7.0 on) are only offered where the graphics card decodes them.

use std::sync::{OnceLock, atomic::Ordering};

use anyhow::{Context, Result, bail};
use connect_proto::{Codec, CodecSupport};
use openh264::formats::YUVSource;
use tracing::info;

use crate::gpu_decode::GpuDecoder;

/// Turns one codec's frames into pictures.
pub(crate) trait VideoDecoder {
    fn codec(&self) -> Codec;

    /// Whether the graphics card does the work.
    fn hardware(&self) -> bool;

    /// What is decoding, for people to read: "Decoder: GPU · <card>", "Decoder: CPU (OpenH264)".
    fn label(&self) -> String;

    /// Decodes one whole frame, writing the picture to `yuv` as tightly packed I420 if given
    /// one. Returns the picture's size, or `None` if this frame finished no picture.
    fn decode(&mut self, data: &[u8], yuv: Option<&mut Vec<u8>>) -> Result<Option<(usize, usize)>>;
}

/// Opens a decoder for `codec`: on the graphics card if `hardware` and it can, otherwise on the
/// processor if this codec has a software decoder.
pub(crate) fn open(codec: Codec, hardware: bool) -> Result<Box<dyn VideoDecoder>> {
    if hardware {
        match codec {
            Codec::H264 => match GpuDecoder::new() {
                Ok(gpu) => return Ok(Box::new(gpu)),
                Err(e) => info!("hardware {codec} decoding unavailable, using the processor: {e:#}"),
            },
            Codec::H265 | Codec::Av1 => {}
        }
    }
    software(codec)
}

/// The processor's decoder for `codec`, where there is one.
pub(crate) fn software(codec: Codec) -> Result<Box<dyn VideoDecoder>> {
    match codec {
        Codec::H264 => Ok(Box::new(OpenH264(
            openh264::decoder::Decoder::new().context("starting the H.264 decoder")?,
        ))),
        Codec::H265 | Codec::Av1 => bail!("no software {codec} decoder"),
    }
}

/// The codecs this PC can decode, as offered to hosts. Hardware decoding counts only while the
/// user allows it.
pub(crate) fn supported() -> Vec<CodecSupport> {
    let mut list = Vec::new();
    if crate::HARDWARE_DECODING.load(Ordering::Relaxed) {
        let gpus = probe();
        // Only codecs Connect can drive on the graphics card; the card may list more.
        if gpus.iter().any(|gpu| gpu.h264) {
            list.push(CodecSupport::hardware(Codec::H264));
        }
    }
    list.push(CodecSupport::software(Codec::H264));
    list
}

/// What one graphics card can decode through Vulkan Video.
#[derive(Debug, Clone)]
pub struct GpuDecodeSupport {
    pub name: String,
    pub driver: String,
    pub discrete: bool,
    pub h264: bool,
    pub h265: bool,
}

/// What each graphics card here can decode, looked up once per run (it takes a moment).
pub(crate) fn probe() -> &'static [GpuDecodeSupport] {
    static PROBE: OnceLock<Vec<GpuDecodeSupport>> = OnceLock::new();
    PROBE.get_or_init(|| match gpu_decode_support() {
        Ok(list) => list,
        Err(e) => {
            info!("no Vulkan Video decoding here: {e:#}");
            Vec::new()
        }
    })
}

/// Lists the graphics cards that decode anything through Vulkan Video, and what.
pub fn gpu_decode_support() -> Result<Vec<GpuDecodeSupport>> {
    use gpu_video::{VulkanInstance, capabilities::VulkanDeviceType};
    let instance = VulkanInstance::new().context("loading Vulkan")?;
    Ok(instance
        .iter_adapters()
        .context("listing graphics cards")?
        .filter(|adapter| adapter.supports_decoding())
        .map(|adapter| {
            let info = adapter.info();
            let caps = &info.decode_capabilities;
            GpuDecodeSupport {
                name: info.name.clone(),
                driver: format!("{} {}", info.driver_name, info.driver_info).trim().to_owned(),
                discrete: info.device_type == VulkanDeviceType::DISCRETE_GPU,
                h264: caps.h264.is_some(),
                h265: caps.h265.is_some_and(|h265| h265.main_profile.is_some()),
            }
        })
        .collect())
}

struct OpenH264(openh264::decoder::Decoder);

impl VideoDecoder for OpenH264 {
    fn codec(&self) -> Codec {
        Codec::H264
    }

    fn hardware(&self) -> bool {
        false
    }

    fn label(&self) -> String {
        "Decoder: CPU (OpenH264)".into()
    }

    fn decode(&mut self, data: &[u8], yuv: Option<&mut Vec<u8>>) -> Result<Option<(usize, usize)>> {
        let Some(picture) = self.0.decode(data)? else { return Ok(None) };
        let (w, h) = picture.dimensions();
        if let Some(buf) = yuv {
            let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
            buf.resize(w * h + 2 * cw * ch, 0);
            let (y, chroma) = buf.split_at_mut(w * h);
            let (u, v) = chroma.split_at_mut(cw * ch);
            let (ys, us, vs) = picture.strides();
            for (dst, src, stride, width) in [(y, picture.y(), ys, w), (u, picture.u(), us, cw), (v, picture.v(), vs, cw)]
            {
                for (row, line) in dst.chunks_exact_mut(width).enumerate() {
                    line.copy_from_slice(&src[row * stride..][..width]);
                }
            }
        }
        Ok(Some((w, h)))
    }
}

impl VideoDecoder for GpuDecoder {
    fn codec(&self) -> Codec {
        Codec::H264
    }

    fn hardware(&self) -> bool {
        true
    }

    fn label(&self) -> String {
        format!("Decoder: GPU · {}", self.name)
    }

    fn decode(&mut self, data: &[u8], yuv: Option<&mut Vec<u8>>) -> Result<Option<(usize, usize)>> {
        GpuDecoder::decode(self, data, yuv)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn software_h264_is_always_offered_and_others_need_hardware() {
        let offered = supported();
        assert_eq!(offered.last(), Some(&CodecSupport::software(Codec::H264)));
        assert!(software(Codec::H264).is_ok());
        assert!(software(Codec::H265).is_err());
        assert!(open(Codec::Av1, true).is_err());
        let decoder = open(Codec::H264, false).unwrap();
        assert!(!decoder.hardware());
        assert_eq!(decoder.codec(), Codec::H264);
    }
}
