//! H.264 decoding on the graphics card through Vulkan Video, which current NVIDIA, AMD and Intel
//! drivers offer on Windows and Linux. Pictures come back to system memory as NV12, for the same
//! upload path as software-decoded ones; what this saves is the decode itself, the biggest cost
//! per frame on the viewer at high resolutions and frame rates.

use anyhow::{Context, Result, ensure};
use gpu_video::{
    BytesDecoder, EncodedInputChunk, VulkanInstance,
    capabilities::VulkanDeviceType,
    parameters::{DecoderParameters, DecoderUsageFlags, VulkanDeviceDescriptor},
};

pub(crate) struct GpuDecoder {
    decoder: BytesDecoder,
    /// The graphics card doing the work, for the session panel.
    pub(crate) name: String,
}

impl GpuDecoder {
    /// Opens the graphics card best placed to decode, preferring a dedicated one over one built
    /// into the processor. Fails where the driver lacks Vulkan Video H.264 decoding.
    pub(crate) fn new() -> Result<Self> {
        let instance = VulkanInstance::new().context("loading Vulkan")?;
        let adapter = instance
            .iter_adapters()
            .context("listing graphics cards")?
            .filter(|adapter| adapter.supports_decoding())
            .max_by_key(|adapter| match adapter.info().device_type {
                VulkanDeviceType::DISCRETE_GPU => 2,
                VulkanDeviceType::INTEGRATED_GPU => 1,
                _ => 0,
            })
            .context("no graphics card here decodes H.264 through Vulkan")?;
        let name = adapter.info().name.clone();
        let device = adapter.create_device(&VulkanDeviceDescriptor::default()).context("opening the graphics card")?;
        let decoder = device
            .create_bytes_decoder_h264(DecoderParameters {
                usage_flags: DecoderUsageFlags::STREAMING,
                ..Default::default()
            })
            .context("starting the hardware decoder")?;
        Ok(Self { decoder, name })
    }

    /// Decodes one whole frame, writing the picture to `yuv` as tightly packed I420 if given
    /// one. Returns the picture's size, or `None` when the frame produced no picture.
    pub(crate) fn decode(&mut self, data: &[u8], yuv: Option<&mut Vec<u8>>) -> Result<Option<(usize, usize)>> {
        let mut pictures = self.decoder.decode(EncodedInputChunk { data, pts: None })?;
        // Each chunk is a whole frame and the host never reorders frames, so the picture can be
        // finished now rather than when the next frame's data shows this one has ended.
        pictures.extend(self.decoder.flush()?);
        let Some(picture) = pictures.pop() else { return Ok(None) };
        let (w, h) = (picture.data.width as usize, picture.data.height as usize);
        if let Some(yuv) = yuv {
            nv12_to_i420(&picture.data.frame, w, h, yuv)?;
        }
        Ok(Some((w, h)))
    }
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
