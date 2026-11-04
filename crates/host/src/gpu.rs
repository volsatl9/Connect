//! Captured BGRA → NV12 on the graphics card. The result stays in a texture on the capture
//! device, for a hardware encoder on the same device to read; nothing crosses to system memory.
//!
//! Two ways to convert, both matching the processor path in `yuv` (BT.601, limited range, which
//! the viewer expects):
//! - **Shaders** (the default): two small pixel shaders draw the brightness and colour planes
//!   straight into the NV12 texture, through its two plane views, as OBS does for NVENC. The
//!   integer maths is the processor path's, so the result is identical on every vendor, and the
//!   draws are ordinary 3D work that timestamp queries can time.
//! - **Direct3D 11's video processor**, which every Windows GPU driver has: the fallback where
//!   the shaders can't run (no shader compiler, or no NV12 render targets). Some drivers run it
//!   outside the command stream timestamps see (NVIDIA's), so it often can't be timed.
//!
//! `CONNECT_GPU_CONVERT=video-processor` (or `shaders`) picks one, e.g. to compare.

use std::{ffi::c_void, mem::ManuallyDrop, sync::OnceLock};

use anyhow::{Context, Result, bail, ensure};
use windows::{
    Win32::{
        Foundation::{HMODULE, S_OK},
        Graphics::{
            Direct3D::{D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, ID3DBlob},
            Direct3D11::{
                D3D11_ASYNC_GETDATA_DONOTFLUSH, D3D11_BIND_RENDER_TARGET, D3D11_CPU_ACCESS_READ, D3D11_MAP_READ,
                D3D11_MAPPED_SUBRESOURCE, D3D11_QUERY, D3D11_QUERY_DATA_TIMESTAMP_DISJOINT, D3D11_QUERY_DESC,
                D3D11_QUERY_TIMESTAMP, D3D11_QUERY_TIMESTAMP_DISJOINT, D3D11_RENDER_TARGET_VIEW_DESC,
                D3D11_RENDER_TARGET_VIEW_DESC_0, D3D11_RTV_DIMENSION_TEXTURE2D, D3D11_TEX2D_RTV, D3D11_TEX2D_VPIV,
                D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_COLOR_SPACE,
                D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT,
                D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
                D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
                D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
                D3D11_VIDEO_USAGE_OPTIMAL_SPEED, D3D11_VIEWPORT, D3D11_VPIV_DIMENSION_TEXTURE2D,
                D3D11_VPOV_DIMENSION_TEXTURE2D, ID3D11DepthStencilView, ID3D11Device, ID3D11DeviceContext,
                ID3D11InputLayout, ID3D11Multithread, ID3D11PixelShader, ID3D11Query, ID3D11RenderTargetView,
                ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader, ID3D11VideoContext, ID3D11VideoContext1,
                ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView,
                ID3D11VideoProcessorOutputView,
            },
            Dxgi::Common::{
                DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601, DXGI_FORMAT,
                DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R8G8_UNORM,
                DXGI_RATIONAL, DXGI_SAMPLE_DESC,
            },
        },
        System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW},
    },
    core::{HRESULT, Interface, PCSTR, s, w},
};

use crate::{
    trace::TimerVerdict,
    yuv::{Layout, Picture},
};

/// How a converter turns BGRA into NV12.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConvertMethod {
    Shaders,
    VideoProcessor,
}

impl std::fmt::Display for ConvertMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ConvertMethod::Shaders => "shaders",
            ConvertMethod::VideoProcessor => "video processor",
        })
    }
}

pub(crate) struct Nv12Converter {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    nv12: ID3D11Texture2D,
    method: Method,
    /// For reading the picture back, made when first needed.
    staging: Option<ID3D11Texture2D>,
    /// `None` where the device can't time its work.
    timer: Option<GpuTimer>,
    width: u32,
    height: u32,
}

enum Method {
    Shaders {
        shaders: Shaders,
        /// The NV12 texture's two planes, as render targets.
        luma: ID3D11RenderTargetView,
        chroma: ID3D11RenderTargetView,
    },
    VideoProcessor(Processor),
}

impl Nv12Converter {
    /// A converter from `width`×`height` BGRA captures on `device`: shaders where they run, the
    /// video processor otherwise, unless `CONNECT_GPU_CONVERT` says which.
    pub fn new(device: &ID3D11Device, width: u32, height: u32) -> Result<Self> {
        let wanted = std::env::var("CONNECT_GPU_CONVERT").unwrap_or_default();
        match wanted.trim() {
            "video-processor" | "vp" => Self::with_method(device, width, height, ConvertMethod::VideoProcessor),
            "shaders" => Self::with_method(device, width, height, ConvertMethod::Shaders),
            _ => Self::with_method(device, width, height, ConvertMethod::Shaders).or_else(|error| {
                tracing::info!("GPU colour conversion with shaders unavailable, using the video processor: {error:#}");
                Self::with_method(device, width, height, ConvertMethod::VideoProcessor)
            }),
        }
    }

    /// A converter that uses `method`, or fails.
    pub fn with_method(device: &ID3D11Device, width: u32, height: u32, method: ConvertMethod) -> Result<Self> {
        ensure!(
            width > 0 && height > 0 && width.is_multiple_of(2) && height.is_multiple_of(2),
            "NV12 needs even dimensions"
        );
        unsafe {
            let context = device.GetImmediateContext()?;
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
            let method = match method {
                ConvertMethod::Shaders => {
                    let shaders = Shaders::new(device)?;
                    // An NV12 texture's planes can be drawn into through 8-bit views: one channel
                    // for brightness, two (at half size) for colour. Drivers that can't, refuse
                    // the views.
                    let luma = plane_target(device, &nv12, DXGI_FORMAT_R8_UNORM)
                        .context("drawing into NV12's brightness plane")?;
                    let chroma = plane_target(device, &nv12, DXGI_FORMAT_R8G8_UNORM)
                        .context("drawing into NV12's colour plane")?;
                    Method::Shaders { shaders, luma, chroma }
                }
                ConvertMethod::VideoProcessor => {
                    Method::VideoProcessor(Processor::new(device, &context, &nv12, width, height)?)
                }
            };
            Ok(Self {
                device: device.clone(),
                context,
                nv12,
                method,
                staging: None,
                timer: GpuTimer::new(device),
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

    pub fn method(&self) -> ConvertMethod {
        match self.method {
            Method::Shaders { .. } => ConvertMethod::Shaders,
            Method::VideoProcessor(_) => ConvertMethod::VideoProcessor,
        }
    }

    /// The converted picture, for the encoder.
    pub fn texture(&self) -> &ID3D11Texture2D {
        &self.nv12
    }

    /// Converts `bgra`, a capture at least this converter's size (an odd last row or column
    /// is left out), into [`Self::texture`]. With a `tag`, the conversion is timed on the
    /// graphics card while tracing is on; [`Self::poll_timings`] gives the result later.
    pub fn convert(&mut self, bgra: &ID3D11Texture2D, tag: Option<u64>) -> Result<()> {
        let (width, height) = (self.width, self.height);
        let (context, timer) = (&self.context, &mut self.timer);
        // Timed around exactly the conversion's own commands.
        let mut timed = |run: &mut dyn FnMut() -> Result<()>| -> Result<()> {
            let timing = match (timer.as_mut(), tag) {
                (Some(timer), Some(tag)) if connect_perf::enabled() => {
                    unsafe { timer.begin(context) }.map(|at| (at, tag))
                }
                _ => None,
            };
            let result = run();
            if let (Some(timer), Some((at, tag))) = (timer.as_mut(), timing) {
                unsafe { timer.end(context, at, tag) };
            }
            result
        };
        match &mut self.method {
            Method::Shaders { shaders, luma, chroma } => {
                let input = shaders.input_view(&self.device, bgra)?;
                // The encoder uses the device from its own threads: hold the device for the whole
                // sequence, so nothing changes the pipeline between our calls.
                let _lock = DeviceLock::new(shaders.lock.as_ref());
                timed(&mut || {
                    unsafe { shaders.draw(context, &input, luma, chroma, width, height) };
                    Ok(())
                })
            }
            Method::VideoProcessor(processor) => {
                let input = processor.input_view(bgra)?;
                timed(&mut || unsafe { processor.blt(&input, width, height) })
            }
        }
    }

    /// Sends the queued commands to the graphics card now; for `gpu-check`, which has no encoder
    /// doing it.
    pub fn flush(&self) {
        unsafe { self.context.Flush() };
    }

    /// Conversions the graphics card has finished timing since the last call, oldest first: their
    /// tags and run times in µs. Never waits for the card.
    pub fn poll_timings(&mut self, out: impl FnMut(u64, u32)) {
        if let Some(timer) = &mut self.timer
            && !timer.poll(&self.context, out)
        {
            tracing::info!("this driver's GPU timestamps don't cover the colour conversion; not timing it");
            self.timer = None;
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

/// Holds the device's lock (where it has one) until dropped.
struct DeviceLock<'a>(Option<&'a ID3D11Multithread>);

impl<'a> DeviceLock<'a> {
    fn new(lock: Option<&'a ID3D11Multithread>) -> Self {
        if let Some(lock) = lock {
            unsafe { lock.Enter() };
        }
        Self(lock)
    }
}

impl Drop for DeviceLock<'_> {
    fn drop(&mut self) {
        if let Some(lock) = self.0 {
            unsafe { lock.Leave() };
        }
    }
}

/// The conversion's shaders. A full-screen triangle per plane; each output pixel reads the
/// capture's pixels with `Load` (no filtering) and does the processor path's integer maths, so
/// every value comes out exactly as `yuv::Picture::convert_from_bgra` makes it.
const HLSL: &str = r#"
Texture2D<float4> image : register(t0);

float4 vs(uint id : SV_VertexID) : SV_Position {
    float2 corner = float2((id << 1) & 2, id & 2);
    return float4(corner * float2(2, -2) + float2(-1, 1), 0, 1);
}

int3 rgb(int2 at) {
    return int3(round(image.Load(int3(at, 0)).rgb * 255.0));
}

float luma(float4 position : SV_Position) : SV_Target {
    int3 c = rgb(int2(position.xy));
    return float(((66 * c.r + 129 * c.g + 25 * c.b + 128) >> 8) + 16) / 255.0;
}

float2 chroma(float4 position : SV_Position) : SV_Target {
    int2 at = int2(position.xy) * 2;
    int3 c = (rgb(at) + rgb(at + int2(1, 0)) + rgb(at + int2(0, 1)) + rgb(at + int2(1, 1)) + 2) >> 2;
    int cb = ((-38 * c.r - 74 * c.g + 112 * c.b + 128) >> 8) + 128;
    int cr = ((112 * c.r - 94 * c.g - 18 * c.b + 128) >> 8) + 128;
    return float2(cb, cr) / 255.0;
}
"#;

/// The compiled shaders, made once per process.
struct Bytecode {
    vertex: Vec<u8>,
    luma: Vec<u8>,
    chroma: Vec<u8>,
}

/// Compiles [`HLSL`] with the system's shader compiler (d3dcompiler_47.dll, part of Windows 10
/// and 11), loaded when first needed so the app still starts without it.
fn bytecode() -> Result<&'static Bytecode> {
    static COMPILED: OnceLock<std::result::Result<Bytecode, String>> = OnceLock::new();
    COMPILED
        .get_or_init(|| {
            let compile = compiler().map_err(|e| format!("{e:#}"))?;
            let stage = |entry: PCSTR, target: PCSTR| compile_one(compile, entry, target).map_err(|e| format!("{e:#}"));
            Ok(Bytecode {
                vertex: stage(s!("vs"), s!("vs_4_0"))?,
                luma: stage(s!("luma"), s!("ps_4_0"))?,
                chroma: stage(s!("chroma"), s!("ps_4_0"))?,
            })
        })
        .as_ref()
        .map_err(|e| anyhow::anyhow!("compiling the conversion shaders: {e}"))
}

type D3DCompile = unsafe extern "system" fn(
    source: *const c_void,
    length: usize,
    name: PCSTR,
    defines: *const c_void,
    include: *const c_void,
    entry: PCSTR,
    target: PCSTR,
    flags: u32,
    effect_flags: u32,
    code: *mut Option<ID3DBlob>,
    errors: *mut Option<ID3DBlob>,
) -> HRESULT;

fn compiler() -> Result<D3DCompile> {
    unsafe {
        let module: HMODULE = LoadLibraryExW(w!("d3dcompiler_47.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32)
            .context("loading the shader compiler")?;
        let function = GetProcAddress(module, s!("D3DCompile")).context("the shader compiler has no D3DCompile")?;
        // SAFETY: D3DCompile has exactly this signature.
        Ok(std::mem::transmute::<unsafe extern "system" fn() -> isize, D3DCompile>(function))
    }
}

fn compile_one(compile: D3DCompile, entry: PCSTR, target: PCSTR) -> Result<Vec<u8>> {
    /// D3DCOMPILE_OPTIMIZATION_LEVEL3 | D3DCOMPILE_WARNINGS_ARE_ERRORS
    const FLAGS: u32 = (1 << 15) | (1 << 18);
    let (mut code, mut errors) = (None, None);
    let result = unsafe {
        compile(
            HLSL.as_ptr().cast(),
            HLSL.len(),
            s!("connect-nv12"),
            std::ptr::null(),
            std::ptr::null(),
            entry,
            target,
            FLAGS,
            0,
            &mut code,
            &mut errors,
        )
    };
    let blob = |blob: &ID3DBlob| unsafe {
        std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize()).to_vec()
    };
    if result.is_err() {
        let message = errors.as_ref().map(|e| String::from_utf8_lossy(&blob(e)).trim().to_owned()).unwrap_or_default();
        bail!("{} ({result}): {message}", unsafe { entry.display() });
    }
    Ok(blob(code.as_ref().context("the shader compiler returned nothing")?))
}

struct Shaders {
    vertex: ID3D11VertexShader,
    luma: ID3D11PixelShader,
    chroma: ID3D11PixelShader,
    /// The view of the capture texture last converted, reused while capture keeps the texture.
    input: Option<(ID3D11Texture2D, ID3D11ShaderResourceView)>,
    lock: Option<ID3D11Multithread>,
}

impl Shaders {
    fn new(device: &ID3D11Device) -> Result<Self> {
        let code = bytecode()?;
        unsafe {
            let (mut vertex, mut luma, mut chroma) = (None, None, None);
            device.CreateVertexShader(&code.vertex, None, Some(&mut vertex)).context("creating the vertex shader")?;
            device.CreatePixelShader(&code.luma, None, Some(&mut luma)).context("creating the luma shader")?;
            device.CreatePixelShader(&code.chroma, None, Some(&mut chroma)).context("creating the chroma shader")?;
            Ok(Self {
                vertex: vertex.context("no vertex shader")?,
                luma: luma.context("no luma shader")?,
                chroma: chroma.context("no chroma shader")?,
                input: None,
                lock: device.cast::<ID3D11Multithread>().ok(),
            })
        }
    }

    fn input_view(&mut self, device: &ID3D11Device, bgra: &ID3D11Texture2D) -> Result<ID3D11ShaderResourceView> {
        if let Some((texture, view)) = &self.input
            && texture == bgra
        {
            return Ok(view.clone());
        }
        let mut view = None;
        unsafe { device.CreateShaderResourceView(bgra, None, Some(&mut view)) }
            .context("creating the capture's shader view")?;
        let view = view.context("no shader view")?;
        self.input = Some((bgra.clone(), view.clone()));
        Ok(view)
    }

    /// Draws both planes of `input` (at least `width`×`height`) into `luma` (full size) and
    /// `chroma` (half size). Leaves nothing bound, so the encoder can read the result.
    unsafe fn draw(
        &self,
        context: &ID3D11DeviceContext,
        input: &ID3D11ShaderResourceView,
        luma: &ID3D11RenderTargetView,
        chroma: &ID3D11RenderTargetView,
        width: u32,
        height: u32,
    ) {
        let viewport = |w: u32, h: u32| D3D11_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: w as f32,
            Height: h as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };
        unsafe {
            context.IASetInputLayout(None::<&ID3D11InputLayout>);
            context.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            context.VSSetShader(&self.vertex, None);
            context.GSSetShader(None::<&windows::Win32::Graphics::Direct3D11::ID3D11GeometryShader>, None);
            context.RSSetState(None::<&windows::Win32::Graphics::Direct3D11::ID3D11RasterizerState>);
            context.OMSetBlendState(None::<&windows::Win32::Graphics::Direct3D11::ID3D11BlendState>, None, u32::MAX);
            context.OMSetDepthStencilState(None::<&windows::Win32::Graphics::Direct3D11::ID3D11DepthStencilState>, 0);
            context.PSSetShaderResources(0, Some(&[Some(input.clone())]));
            context.RSSetViewports(Some(&[viewport(width, height)]));
            context.OMSetRenderTargets(Some(&[Some(luma.clone())]), None::<&ID3D11DepthStencilView>);
            context.PSSetShader(&self.luma, None);
            context.Draw(3, 0);
            context.RSSetViewports(Some(&[viewport(width / 2, height / 2)]));
            context.OMSetRenderTargets(Some(&[Some(chroma.clone())]), None::<&ID3D11DepthStencilView>);
            context.PSSetShader(&self.chroma, None);
            context.Draw(3, 0);
            context.OMSetRenderTargets(None, None::<&ID3D11DepthStencilView>);
            context.PSSetShaderResources(0, Some(&[None]));
        }
    }
}

/// A render target on `texture` in `format`: for NV12, R8 selects the brightness plane and
/// R8G8 the colour plane.
fn plane_target(
    device: &ID3D11Device,
    texture: &ID3D11Texture2D,
    format: DXGI_FORMAT,
) -> Result<ID3D11RenderTargetView> {
    let desc = D3D11_RENDER_TARGET_VIEW_DESC {
        Format: format,
        ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_RTV { MipSlice: 0 } },
    };
    let mut view = None;
    unsafe { device.CreateRenderTargetView(texture, Some(&desc), Some(&mut view)) }?;
    view.context("no render target view")
}

/// Direct3D 11's video processor, converting into the NV12 texture.
struct Processor {
    video: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    output: ID3D11VideoProcessorOutputView,
    /// The view of the capture texture last converted, reused while capture keeps the texture.
    input: Option<(ID3D11Texture2D, ID3D11VideoProcessorInputView)>,
}

impl Processor {
    unsafe fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        nv12: &ID3D11Texture2D,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        unsafe {
            let video: ID3D11VideoDevice = device.cast().context("the Direct3D device has no video support")?;
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
                video_context.VideoProcessorSetStreamColorSpace(
                    &processor,
                    0,
                    &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 },
                );
                video_context.VideoProcessorSetOutputColorSpace(
                    &processor,
                    &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 1 << 4 },
                );
            }
            let view = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
            };
            let mut output = None;
            video
                .CreateVideoProcessorOutputView(nv12, &enumerator, &view, Some(&mut output))
                .context("creating the NV12 output view")?;
            Ok(Self {
                video,
                video_context,
                enumerator,
                processor,
                output: output.context("no output view")?,
                input: None,
            })
        }
    }

    fn input_view(&mut self, bgra: &ID3D11Texture2D) -> Result<ID3D11VideoProcessorInputView> {
        if let Some((texture, view)) = &self.input
            && texture == bgra
        {
            return Ok(view.clone());
        }
        let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 },
            },
        };
        let mut view = None;
        unsafe { self.video.CreateVideoProcessorInputView(bgra, &self.enumerator, &desc, Some(&mut view)) }
            .context("creating the capture input view")?;
        let view = view.context("no input view")?;
        self.input = Some((bgra.clone(), view.clone()));
        Ok(view)
    }

    unsafe fn blt(&self, input: &ID3D11VideoProcessorInputView, width: u32, height: u32) -> Result<()> {
        unsafe {
            // Only the even-sized part of an odd-sized capture.
            let rect = windows::Win32::Foundation::RECT { left: 0, top: 0, right: width as i32, bottom: height as i32 };
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
}

/// Conversions timed at once. Results come back a frame or two later; one whose slot is still
/// waiting when its turn comes round again goes untimed rather than wait.
const TIMER_SLOTS: usize = 4;

/// Times work on the graphics card: a timestamp before and after, inside a disjoint query that
/// says whether the counter's frequency held steady in between.
struct GpuTimer {
    slots: Vec<TimerSlot>,
    check: crate::trace::TimerCheck,
    /// The slot the next timing uses; the oldest in flight.
    next: usize,
}

struct TimerSlot {
    disjoint: ID3D11Query,
    start: ID3D11Query,
    end: ID3D11Query,
    /// Set while the slot's queries are on the card.
    tag: Option<u64>,
}

impl GpuTimer {
    fn new(device: &ID3D11Device) -> Option<Self> {
        let query = |kind: D3D11_QUERY| {
            let mut query = None;
            // SAFETY: a valid description, and somewhere for the query to go.
            unsafe { device.CreateQuery(&D3D11_QUERY_DESC { Query: kind, MiscFlags: 0 }, Some(&mut query)) }.ok()?;
            query
        };
        let slot = || {
            Some(TimerSlot {
                disjoint: query(D3D11_QUERY_TIMESTAMP_DISJOINT)?,
                start: query(D3D11_QUERY_TIMESTAMP)?,
                end: query(D3D11_QUERY_TIMESTAMP)?,
                tag: None,
            })
        };
        Some(Self {
            slots: (0..TIMER_SLOTS).map(|_| slot()).collect::<Option<_>>()?,
            next: 0,
            check: Default::default(),
        })
    }

    /// Marks the start; `None` (untimed) if the slot is still waiting for an earlier result.
    unsafe fn begin(&mut self, context: &ID3D11DeviceContext) -> Option<usize> {
        let at = self.next;
        let slot = &self.slots[at];
        if slot.tag.is_some() {
            return None;
        }
        unsafe {
            context.Begin(&slot.disjoint);
            context.End(&slot.start);
        }
        Some(at)
    }

    unsafe fn end(&mut self, context: &ID3D11DeviceContext, at: usize, tag: u64) {
        let slot = &mut self.slots[at];
        unsafe {
            context.End(&slot.end);
            context.End(&slot.disjoint);
        }
        slot.tag = Some(tag);
        self.next = (at + 1) % self.slots.len();
    }

    /// Hands over the timings that are ready. False once the timings have proved meaningless
    /// (see [`crate::trace::TimerCheck`]), so the timer should be dropped.
    fn poll(&mut self, context: &ID3D11DeviceContext, mut out: impl FnMut(u64, u32)) -> bool {
        let count = self.slots.len();
        for i in 0..count {
            let slot = &mut self.slots[(self.next + i) % count];
            let Some(tag) = slot.tag else { continue };
            // SAFETY: each query is read into the type its kind returns.
            let Some(disjoint) = (unsafe { ready::<D3D11_QUERY_DATA_TIMESTAMP_DISJOINT>(context, &slot.disjoint) })
            else {
                break; // later slots finish later
            };
            let (start, end) = unsafe { (ready::<u64>(context, &slot.start), ready::<u64>(context, &slot.end)) };
            let (Some(start), Some(end)) = (start, end) else { break };
            slot.tag = None;
            if disjoint.Disjoint.as_bool() || disjoint.Frequency == 0 || end < start {
                continue;
            }
            match self.check.check(end - start, disjoint.Frequency) {
                TimerVerdict::Keep => {
                    let us = u128::from(end - start) * 1_000_000 / u128::from(disjoint.Frequency);
                    out(tag, us.min(u128::from(u32::MAX)) as u32);
                }
                TimerVerdict::Skip => {}
                TimerVerdict::GiveUp => return false,
            }
        }
        true
    }
}

/// A query's result if the card has it, without waiting or flushing. (The wrapper's `GetData`
/// folds "not ready yet", `S_FALSE`, into success, so this calls the method directly.)
unsafe fn ready<T: Default>(context: &ID3D11DeviceContext, query: &ID3D11Query) -> Option<T> {
    let mut value = T::default();
    let result = unsafe {
        (Interface::vtable(context).GetData)(
            Interface::as_raw(context),
            query.as_raw(),
            (&raw mut value).cast(),
            size_of::<T>() as u32,
            D3D11_ASYNC_GETDATA_DONOTFLUSH.0 as u32,
        )
    };
    (result == S_OK).then_some(value)
}

#[cfg(test)]
mod tests {
    use windows::Win32::{
        Foundation::HMODULE,
        Graphics::{
            Direct3D::{D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP},
            Direct3D11::{
                D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
                D3D11_SUBRESOURCE_DATA, D3D11CreateDevice,
            },
        },
    };

    use super::*;

    const W: u32 = 64;
    const H: u32 = 32;

    /// A Direct3D 11 device: the graphics card's, or Windows' software one.
    fn device() -> Option<ID3D11Device> {
        let create = |driver: D3D_DRIVER_TYPE| unsafe {
            let mut device = None;
            D3D11CreateDevice(
                None,
                driver,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )
            .ok()?;
            device
        };
        create(D3D_DRIVER_TYPE_HARDWARE).or_else(|| create(D3D_DRIVER_TYPE_WARP))
    }

    /// A BGRA test image with every kind of edge: gradients, noise, saturated colours.
    fn image() -> Vec<u8> {
        let mut seed = 0x2545_f491_u32;
        let mut random = move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed as u8
        };
        (0..W * H)
            .flat_map(|i| {
                let (x, y) = (i % W, i / W);
                match (x / 16, y / 16) {
                    (0, _) => [(x * 16) as u8, (y * 8) as u8, 255 - (x * 16) as u8, 255],
                    (1, 0) => [0, 0, 255, 255],
                    (1, _) => [255, 255, 255, 255],
                    _ => [random(), random(), random(), 255],
                }
            })
            .collect()
    }

    fn texture(
        device: &ID3D11Device,
        format: DXGI_FORMAT,
        w: u32,
        h: u32,
        bind: u32,
        data: Option<&[u8]>,
        pitch: u32,
    ) -> ID3D11Texture2D {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: bind,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let initial = data.map(|data| D3D11_SUBRESOURCE_DATA {
            pSysMem: data.as_ptr().cast(),
            SysMemPitch: pitch,
            SysMemSlicePitch: 0,
        });
        let mut texture = None;
        unsafe { device.CreateTexture2D(&desc, initial.as_ref().map(|d| d as *const _), Some(&mut texture)) }.unwrap();
        texture.unwrap()
    }

    /// `texture`'s rows of `row` bytes each, read back through a staging copy.
    fn read(device: &ID3D11Device, texture: &ID3D11Texture2D, row: usize, rows: usize) -> Vec<u8> {
        unsafe {
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            texture.GetDesc(&mut desc);
            desc.Usage = D3D11_USAGE_STAGING;
            desc.BindFlags = 0;
            desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            let mut staging = None;
            device.CreateTexture2D(&desc, None, Some(&mut staging)).unwrap();
            let staging = staging.unwrap();
            let context = device.GetImmediateContext().unwrap();
            context.CopyResource(&staging, texture);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).unwrap();
            let data = std::slice::from_raw_parts(mapped.pData as *const u8, mapped.RowPitch as usize * rows);
            let out = (0..rows).flat_map(|r| data[r * mapped.RowPitch as usize..][..row].to_vec()).collect();
            context.Unmap(&staging, 0);
            out
        }
    }

    /// The shaders' brightness and colour planes are exactly the processor path's.
    #[test]
    fn shaders_match_the_processor_path_exactly() {
        let Some(device) = device() else { return eprintln!("no Direct3D 11 device; skipped") };
        let bgra = image();
        let mut expected = Picture::new(Layout::Nv12, W as usize, H as usize);
        expected.convert_from_bgra(&bgra, W as usize * 4);
        let (y, uv) = expected.data().split_at((W * H) as usize);

        let source =
            texture(&device, DXGI_FORMAT_B8G8R8A8_UNORM, W, H, D3D11_BIND_SHADER_RESOURCE.0 as u32, Some(&bgra), W * 4);
        let target = D3D11_BIND_RENDER_TARGET.0 as u32;
        let luma = texture(&device, DXGI_FORMAT_R8_UNORM, W, H, target, None, 0);
        let chroma = texture(&device, DXGI_FORMAT_R8G8_UNORM, W / 2, H / 2, target, None, 0);
        let mut shaders = Shaders::new(&device).expect("shaders compile and load");
        let input = shaders.input_view(&device, &source).unwrap();
        let context = unsafe { device.GetImmediateContext() }.unwrap();
        let (luma_view, chroma_view) = (
            plane_target(&device, &luma, DXGI_FORMAT_R8_UNORM).unwrap(),
            plane_target(&device, &chroma, DXGI_FORMAT_R8G8_UNORM).unwrap(),
        );
        unsafe { shaders.draw(&context, &input, &luma_view, &chroma_view, W, H) };
        assert_eq!(read(&device, &luma, W as usize, H as usize), y, "brightness");
        assert_eq!(read(&device, &chroma, W as usize, H as usize / 2), uv, "colour");

        // Into NV12 itself, where the driver draws into its planes.
        match Nv12Converter::with_method(&device, W, H, ConvertMethod::Shaders) {
            Ok(mut converter) => {
                converter.convert(&source, None).unwrap();
                assert_eq!(converter.read_back().unwrap().data(), expected.data(), "NV12");
            }
            Err(error) => eprintln!("NV12 render targets unavailable here: {error:#}"),
        }
    }
}
