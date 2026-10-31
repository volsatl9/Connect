//! H.264 decoding on the graphics card through Vulkan Video, which current NVIDIA, AMD and Intel
//! drivers offer on Windows and Linux.
//!
//! When the window renders on the same graphics card ([`SharedGpu`]), decoded pictures stay
//! there as NV12 textures the window draws directly. Otherwise they come back to system memory
//! for the same upload path as software-decoded ones; what that still saves is the decode
//! itself.

use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result, ensure};
use connect_proto::Codec;
use gpu_video::{
    BytesDecoder, BytesDecoderH265, EncodedInputChunk, VulkanDevice, VulkanInstance, WgpuTexturesDecoder,
    WgpuTexturesDecoderH265,
    capabilities::VulkanDeviceType,
    parameters::{DecoderParameters, DecoderUsageFlags, VulkanDeviceDescriptor},
};

/// A decoded picture: in `yuv` as I420 (when asked for), or as an NV12 texture on the shared
/// graphics card.
pub(crate) struct Decoded {
    pub width: usize,
    pub height: usize,
    pub texture: Option<wgpu::Texture>,
}

/// A graphics card that the window and the video decoder share, so decoded pictures never
/// leave it.
pub struct SharedGpu {
    device: Arc<VulkanDevice>,
    _instance: Arc<VulkanInstance>,
    /// The card's name, for people to read.
    pub name: String,
}

static SHARED: OnceLock<Arc<SharedGpu>> = OnceLock::new();

impl SharedGpu {
    /// Opens the graphics card best placed to decode (see [`open_device`]) with a wgpu device
    /// on it for the window. Fails where no card decodes H.264 through Vulkan.
    pub fn open() -> Result<Arc<Self>> {
        let instance = VulkanInstance::new().context("loading Vulkan")?;
        let (device, name) = open_device(&instance, Codec::H264)?;
        Ok(Arc::new(Self { device, _instance: instance, name }))
    }

    /// The wgpu instance, adapter, device and queue for the window to render with.
    pub fn wgpu(&self) -> (wgpu::Instance, wgpu::Adapter, wgpu::Device, wgpu::Queue) {
        (
            self._instance.wgpu_instance(),
            self.device.wgpu_adapter(),
            self.device.wgpu_device(),
            self.device.wgpu_queue(),
        )
    }

    /// From now on, hardware decoders hand out textures on this card, for a window that
    /// renders with [`Self::wgpu`]. Only the first call counts.
    pub fn install(self: &Arc<Self>) {
        let _ = SHARED.set(self.clone());
    }
}

/// Opens the best graphics card for decoding `codec`, preferring a dedicated one over one built
/// into the processor.
fn open_device(instance: &Arc<VulkanInstance>, codec: Codec) -> Result<(Arc<VulkanDevice>, String)> {
    let decodes = |caps: &gpu_video::capabilities::DecodeCapabilities| match codec {
        Codec::H264 => caps.h264.is_some(),
        Codec::H265 => caps.h265.is_some_and(|h265| h265.main_profile.is_some()),
        Codec::Av1 => false,
    };
    let adapter = instance
        .iter_adapters()
        .context("listing graphics cards")?
        .filter(|adapter| adapter.supports_decoding() && decodes(&adapter.info().decode_capabilities))
        .max_by_key(|adapter| match adapter.info().device_type {
            VulkanDeviceType::DISCRETE_GPU => 2,
            VulkanDeviceType::INTEGRATED_GPU => 1,
            _ => 0,
        })
        .with_context(|| format!("no graphics card here decodes {codec} through Vulkan"))?;
    let name = adapter.info().name.clone();
    let device = adapter.create_device(&VulkanDeviceDescriptor::default()).context("opening the graphics card")?;
    Ok((device, name))
}

enum Output {
    /// Pictures copied back to memory.
    Bytes(BytesDecoder),
    /// Pictures as textures on the shared graphics card.
    Textures(WgpuTexturesDecoder),
    BytesH265(BytesDecoderH265),
    TexturesH265(WgpuTexturesDecoderH265),
}

pub(crate) struct GpuDecoder {
    codec: Codec,
    output: Output,
    /// Keeps a decoder's own device alive (the shared one lives in [`SHARED`]).
    _instance: Option<Arc<VulkanInstance>>,
    /// The graphics card doing the work, for the session panel.
    pub(crate) name: String,
}

impl GpuDecoder {
    /// Opens a hardware `codec` decoder: on the shared graphics card if the window has one
    /// (and it decodes `codec`), handing out textures; otherwise on the best card here,
    /// handing out pictures in memory.
    pub(crate) fn new(codec: Codec) -> Result<Self> {
        let parameters = DecoderParameters { usage_flags: DecoderUsageFlags::STREAMING, ..Default::default() };
        if let Some(shared) = SHARED.get() {
            let output = match codec {
                Codec::H264 => shared.device.create_wgpu_textures_decoder_h264(parameters).map(Output::Textures),
                Codec::H265 => shared.device.create_wgpu_textures_decoder_h265(parameters).map(Output::TexturesH265),
                Codec::Av1 => anyhow::bail!("no hardware AV1 decoding yet"),
            };
            match output {
                Ok(output) => return Ok(Self { codec, output, _instance: None, name: shared.name.clone() }),
                Err(e) => tracing::info!("the window's graphics card can't decode {codec}: {e}"),
            }
        }
        Self::in_memory(codec)
    }

    /// A hardware decoder on its own device, handing out pictures in memory.
    pub(crate) fn in_memory(codec: Codec) -> Result<Self> {
        let parameters = DecoderParameters { usage_flags: DecoderUsageFlags::STREAMING, ..Default::default() };
        let instance = VulkanInstance::new().context("loading Vulkan")?;
        let (device, name) = open_device(&instance, codec)?;
        let output = match codec {
            Codec::H264 => Output::Bytes(device.create_bytes_decoder_h264(parameters)?),
            Codec::H265 => Output::BytesH265(device.create_bytes_decoder_h265(parameters)?),
            Codec::Av1 => anyhow::bail!("no hardware AV1 decoding yet"),
        };
        Ok(Self { codec, output, _instance: Some(instance), name })
    }

    pub(crate) fn codec(&self) -> Codec {
        self.codec
    }

    /// Whether pictures stay on the graphics card.
    pub(crate) fn textures(&self) -> bool {
        matches!(self.output, Output::Textures(_) | Output::TexturesH265(_))
    }

    /// Decodes one whole frame. In memory, writes the picture to `yuv` as tightly packed I420
    /// if given one. `None` when the frame produced no picture.
    pub(crate) fn decode(&mut self, data: &[u8], yuv: Option<&mut Vec<u8>>) -> Result<Option<Decoded>> {
        let chunk = EncodedInputChunk { data, pts: None };
        // Each chunk is a whole frame and the host never reorders frames, so the picture can be
        // finished now rather than when the next frame's data shows this one has ended.
        let in_memory = match &mut self.output {
            Output::Bytes(decoder) => {
                let mut pictures = decoder.decode(chunk)?;
                pictures.extend(decoder.flush()?);
                pictures.pop()
            }
            Output::BytesH265(decoder) => {
                let mut pictures = decoder.decode(chunk)?;
                pictures.extend(decoder.flush()?);
                pictures.pop()
            }
            Output::Textures(decoder) => {
                let mut pictures = decoder.decode(chunk)?;
                pictures.extend(decoder.flush()?);
                return Ok(pictures.pop().map(texture));
            }
            Output::TexturesH265(decoder) => {
                let mut pictures = decoder.decode(chunk)?;
                pictures.extend(decoder.flush()?);
                return Ok(pictures.pop().map(texture));
            }
        };
        let Some(picture) = in_memory else { return Ok(None) };
        let (width, height) = (picture.data.width as usize, picture.data.height as usize);
        if let Some(yuv) = yuv {
            nv12_to_i420(&picture.data.frame, width, height, yuv)?;
        }
        Ok(Some(Decoded { width, height, texture: None }))
    }

    /// Decodes one whole frame, returning the NV12 picture as it came back (for checks).
    pub(crate) fn decode_nv12(&mut self, data: &[u8]) -> Result<Vec<Vec<u8>>> {
        let chunk = EncodedInputChunk { data, pts: None };
        let pictures = match &mut self.output {
            Output::Bytes(decoder) => {
                let mut pictures = decoder.decode(chunk)?;
                pictures.extend(decoder.flush()?);
                pictures
            }
            Output::BytesH265(decoder) => {
                let mut pictures = decoder.decode(chunk)?;
                pictures.extend(decoder.flush()?);
                pictures
            }
            Output::Textures(_) | Output::TexturesH265(_) => anyhow::bail!("decoder hands out textures"),
        };
        Ok(pictures.into_iter().map(|picture| picture.data.frame).collect())
    }
}

/// A decoded texture, as [`Decoded`].
fn texture(picture: gpu_video::OutputFrame<wgpu::Texture>) -> Decoded {
    let size = picture.data.size();
    Decoded { width: size.width as usize, height: size.height as usize, texture: Some(picture.data) }
}

/// NV12 (a Y plane, then U and V interleaved) to I420 (Y, then all of U, then all of V).
fn nv12_to_i420(nv12: &[u8], w: usize, h: usize, yuv: &mut Vec<u8>) -> Result<()> {
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    ensure!(nv12.len() >= w * h + 2 * cw * ch, "hardware decoder returned a short picture");
    yuv.resize(w * h + 2 * cw * ch, 0);
    let (y, chroma) = yuv.split_at_mut(w * h);
    let (u, v) = chroma.split_at_mut(cw * ch);
    y.copy_from_slice(&nv12[..w * h]);
    for ((pair, u), v) in nv12[w * h..][..2 * cw * ch].as_chunks::<2>().0.iter().zip(u).zip(v) {
        *u = pair[0];
        *v = pair[1];
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nv12_splits_into_separate_chroma_planes() {
        // 4×2 picture: 8 luma bytes, then one row of two interleaved U/V pairs.
        let nv12 = [1, 2, 3, 4, 5, 6, 7, 8, 10, 20, 11, 21];
        let mut yuv = vec![9; 3];
        nv12_to_i420(&nv12, 4, 2, &mut yuv).unwrap();
        assert_eq!(yuv, [1, 2, 3, 4, 5, 6, 7, 8, 10, 11, 20, 21]);
        assert!(nv12_to_i420(&nv12[..11], 4, 2, &mut yuv).is_err());
    }
}
