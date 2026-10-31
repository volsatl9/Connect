//! Captured BGRA → NV12 on the graphics card, with Direct3D 11's video processor: every
//! Windows GPU driver has one. The result stays in a texture on the capture device, for a
//! hardware encoder on the same device to read; nothing crosses to system memory.
//!
//! Colours match the processor path in `yuv`: BT.601, limited range, which the viewer expects.

use std::mem::ManuallyDrop;

use anyhow::{Context, Result, ensure};
use windows::{
    Win32::Graphics::{
        Direct3D11::{
            D3D11_BIND_RENDER_TARGET, D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
            D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
            D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_COLOR_SPACE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
            D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT, D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT,
            D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
            D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
            D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_OPTIMAL_SPEED, D3D11_VPIV_DIMENSION_TEXTURE2D,
            D3D11_VPOV_DIMENSION_TEXTURE2D, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, ID3D11VideoContext,
            ID3D11VideoContext1, ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
            ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
        },
        Dxgi::Common::{
            DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601,
            DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
        },
    },
    core::Interface,
};

use crate::yuv::{Layout, Picture};

pub(crate) struct Nv12Converter {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    video: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    nv12: ID3D11Texture2D,
    output: ID3D11VideoProcessorOutputView,
    /// The view of the capture texture last converted, reused while capture keeps the texture.
    input: Option<(ID3D11Texture2D, ID3D11VideoProcessorInputView)>,
    /// For reading the picture back, made when first needed.
    staging: Option<ID3D11Texture2D>,
    width: u32,
    height: u32,
}

impl Nv12Converter {
    /// A converter from `width`×`height` BGRA captures on `device`.
    pub fn new(device: &ID3D11Device, width: u32, height: u32) -> Result<Self> {
        ensure!(width > 0 && height > 0 && width.is_multiple_of(2) && height.is_multiple_of(2), "NV12 needs even dimensions");
        unsafe {
            let video: ID3D11VideoDevice = device.cast().context("the Direct3D device has no video support")?;
            let context = device.GetImmediateContext()?;
            let video_context: ID3D11VideoContext = context.cast().context("no video context")?;
            let rate = DXGI_RATIONAL { Numerator: 60, Denominator: 1 };
            let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: rate,
                InputWidth: width,
                InputHeight: height,
                OutputFrameRate: rate,
                OutputWidth: width,
                OutputHeight: height,
                Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
            };
            let enumerator = video.CreateVideoProcessorEnumerator(&content).context("no video processor")?;
            let input_support = enumerator.CheckVideoProcessorFormat(DXGI_FORMAT_B8G8R8A8_UNORM)?;
            ensure!(
                input_support & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT.0 as u32 != 0,
                "the video processor can't read BGRA"
            );
            let output_support = enumerator.CheckVideoProcessorFormat(DXGI_FORMAT_NV12)?;
            ensure!(
                output_support & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0 as u32 != 0,
                "the video processor can't write NV12"
            );
            let processor = video.CreateVideoProcessor(&enumerator, 0).context("creating the video processor")?;

            // Exactly the conversion asked for: no driver "enhancements", whole frames.
            video_context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            video_context.VideoProcessorSetStreamFrameFormat(&processor, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            if let Ok(context1) = video_context.cast::<ID3D11VideoContext1>() {
                context1.VideoProcessorSetStreamColorSpace1(&processor, 0, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
                context1.VideoProcessorSetOutputColorSpace1(&processor, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601);
            } else {
                // Usage 0, RGB full range, BT.601 matrix, YCbCr nominal range 16–235 (bits 4–5 = 1).
                video_context.VideoProcessorSetStreamColorSpace(&processor, 0, &D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
                    _bitfield: 0,
                });
                video_context.VideoProcessorSetOutputColorSpace(&processor, &D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
                    _bitfield: 1 << 4,
                });
            }

            let desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut nv12 = None;
            device.CreateTexture2D(&desc, None, Some(&mut nv12)).context("creating the NV12 texture")?;
            let nv12 = nv12.context("no NV12 texture")?;
            let view = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
            };
            let mut output = None;
            video
                .CreateVideoProcessorOutputView(&nv12, &enumerator, &view, Some(&mut output))
                .context("creating the NV12 output view")?;
            Ok(Self {
                device: device.clone(),
                context,
                video,
                video_context,
                enumerator,
                processor,
                nv12,
                output: output.context("no output view")?,
                input: None,
                staging: None,
                width,
                height,
            })
        }
    }

    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// The converted picture, for the encoder.
    pub fn texture(&self) -> &ID3D11Texture2D {
        &self.nv12
    }

    /// Converts `bgra`, a capture at least this converter's size (an odd last row or column
    /// is left out), into [`Self::texture`].
    pub fn convert(&mut self, bgra: &ID3D11Texture2D) -> Result<()> {
        unsafe {
            if self.input.as_ref().is_none_or(|(texture, _)| texture != bgra) {
                let view = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                    FourCC: 0,
                    ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 },
                    },
                };
                let mut input = None;
                self.video
                    .CreateVideoProcessorInputView(bgra, &self.enumerator, &view, Some(&mut input))
                    .context("creating the capture input view")?;
                self.input = Some((bgra.clone(), input.context("no input view")?));
            }
            let (_, input) = self.input.as_ref().expect("set above");
            // Only the even-sized part of an odd-sized capture.
            let rect = windows::Win32::Foundation::RECT {
                left: 0,
                top: 0,
                right: self.width as i32,
                bottom: self.height as i32,
            };
            self.video_context.VideoProcessorSetStreamSourceRect(&self.processor, 0, true, Some(&rect));
            let stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: ManuallyDrop::new(Some(input.clone())),
                ..Default::default()
            };
            let mut streams = [stream];
            let result = self.video_context.VideoProcessorBlt(&self.processor, &self.output, 0, &streams);
            ManuallyDrop::drop(&mut streams[0].pInputSurface);
            result.context("converting the capture to NV12")
        }
    }

    /// The converted picture, copied into memory.
    pub fn read_back(&mut self) -> Result<Picture> {
        unsafe {
            if self.staging.is_none() {
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                self.nv12.GetDesc(&mut desc);
                desc.Usage = D3D11_USAGE_STAGING;
                desc.BindFlags = 0;
                desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
                let mut staging = None;
                self.device.CreateTexture2D(&desc, None, Some(&mut staging))?;
                self.staging = Some(staging.context("no staging texture")?);
            }
            let staging = self.staging.as_ref().expect("set above");
            self.context.CopyResource(staging, &self.nv12);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            let (w, h, pitch) = (self.width as usize, self.height as usize, mapped.RowPitch as usize);
            let mut picture = Picture::new(Layout::Nv12, w, h);
            // The chroma plane follows the luma plane's `h` rows, at the same pitch.
            let source = std::slice::from_raw_parts(mapped.pData as *const u8, pitch * (h + h / 2));
            for (row, line) in picture.data_mut().chunks_exact_mut(w).enumerate() {
                line.copy_from_slice(&source[row * pitch..][..w]);
            }
            self.context.Unmap(staging, 0);
            Ok(picture)
        }
    }
}
