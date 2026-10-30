//! Primary-monitor capture with DXGI Desktop Duplication.
//!
//! Frames are copied GPU → CPU through a staging texture, and handed out straight from the
//! mapped staging memory: the encoder converts them to YUV from there. The mouse pointer is not
//! part of the duplicated image; it travels separately.

use std::time::Duration;

use anyhow::{Context, Result};
use windows::{
    Win32::{
        Foundation::HMODULE,
        Graphics::{
            Direct3D::D3D_DRIVER_TYPE_HARDWARE,
            Direct3D11::{
                D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
                D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
                D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
            },
            Dxgi::{
                DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, IDXGIDevice,
                IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
            },
        },
    },
    core::Interface,
};

use super::{Capturer, Frame};

pub(crate) struct DxgiCapturer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    output: IDXGIOutput1,
    duplication: Option<IDXGIOutputDuplication>,
    staging: Option<(ID3D11Texture2D, u32, u32)>,
    /// Whether the staging texture is mapped, i.e. the last frame handed out is still readable.
    mapped: bool,
    refresh_hz: Option<u32>,
    width: u32,
    height: u32,
}

impl DxgiCapturer {
    pub fn new() -> Result<Self> {
        unsafe {
            let mut device = None;
            let mut context = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .context("creating Direct3D 11 device")?;
            let device = device.context("no Direct3D device")?;
            let context = context.context("no Direct3D context")?;

            let dxgi = device.cast::<IDXGIDevice>()?;
            // Ahead of the game's rendering, the copy out of the desktop finishes sooner. Windows
            // only grants this to administrators; without it, the copy just waits its turn.
            let _ = dxgi.SetGPUThreadPriority(7);
            let adapter = dxgi.GetAdapter()?;
            let output = adapter
                .EnumOutputs(0)
                .context("no monitor attached to the primary graphics adapter")?
                .cast::<IDXGIOutput1>()?;

            let mut capturer = Self {
                device,
                context,
                output,
                duplication: None,
                staging: None,
                mapped: false,
                refresh_hz: None,
                width: 0,
                height: 0,
            };
            capturer.duplicate()?;
            Ok(capturer)
        }
    }

    /// (Re)starts duplication; needed at start and after `DXGI_ERROR_ACCESS_LOST`
    /// (mode change, UAC prompt, full-screen exclusive app).
    fn duplicate(&mut self) -> Result<()> {
        self.duplication = None;
        let duplication =
            unsafe { self.output.DuplicateOutput(&self.device) }.context("starting desktop duplication")?;
        let rate = unsafe { duplication.GetDesc() }.ModeDesc.RefreshRate;
        if rate.Numerator > 0 && rate.Denominator > 0 {
            self.refresh_hz = Some((rate.Numerator as f64 / rate.Denominator as f64).round() as u32);
        }
        self.duplication = Some(duplication);
        Ok(())
    }

    fn ensure_staging(&mut self, desc: &D3D11_TEXTURE2D_DESC) -> Result<ID3D11Texture2D> {
        if let Some((tex, w, h)) = &self.staging
            && (*w, *h) == (desc.Width, desc.Height)
        {
            return Ok(tex.clone());
        }
        let staging_desc = D3D11_TEXTURE2D_DESC {
            Width: desc.Width,
            Height: desc.Height,
            MipLevels: 1,
            ArraySize: 1,
            Format: desc.Format,
            SampleDesc: desc.SampleDesc,
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut tex = None;
        unsafe { self.device.CreateTexture2D(&staging_desc, None, Some(&mut tex)) }
            .context("creating staging texture")?;
        let tex = tex.context("no staging texture")?;
        self.staging = Some((tex.clone(), desc.Width, desc.Height));
        Ok(tex)
    }
}

impl DxgiCapturer {
    fn unmap(&mut self) {
        if std::mem::take(&mut self.mapped)
            && let Some((staging, ..)) = &self.staging
        {
            unsafe { self.context.Unmap(staging, 0) };
        }
    }
}

impl Drop for DxgiCapturer {
    fn drop(&mut self) {
        self.unmap();
    }
}

impl Capturer for DxgiCapturer {
    fn refresh_hz(&self) -> Option<u32> {
        self.refresh_hz
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame<'_>>> {
        // The previous frame has been read by now; its staging texture takes the next one.
        self.unmap();
        if self.duplication.is_none() {
            // Lost earlier and couldn't restart yet (e.g. the secure desktop is showing).
            if self.duplicate().is_err() {
                std::thread::sleep(timeout);
                return Ok(None);
            }
        }
        let duplication = self.duplication.clone().expect("set above");

        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        match unsafe { duplication.AcquireNextFrame(timeout.as_millis() as u32, &mut info, &mut resource) } {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(None),
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                self.duplication = None;
                return Ok(None);
            }
            Err(e) => return Err(e).context("acquiring desktop frame"),
        }

        // Only the pointer moved: there's no new image.
        if info.LastPresentTime == 0 {
            unsafe { duplication.ReleaseFrame() }?;
            return Ok(None);
        }

        let copied = (|| -> Result<()> {
            let texture: ID3D11Texture2D = resource.context("no desktop image")?.cast()?;
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            unsafe { texture.GetDesc(&mut desc) };
            let staging = self.ensure_staging(&desc)?;
            unsafe { self.context.CopyResource(&staging, &texture) };
            self.width = desc.Width;
            self.height = desc.Height;
            Ok(())
        })();
        unsafe { duplication.ReleaseFrame() }?;
        copied?;

        let (staging, w, h) = self.staging.clone().expect("created above");
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe { self.context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }
            .context("reading desktop image")?;
        self.mapped = true;
        let stride = mapped.RowPitch as usize;
        // SAFETY: the texture stays mapped, and so this memory readable, until the next call
        // (or drop), which the returned frame's borrow of `self` outlives.
        let len = stride * (h as usize - 1) + w as usize * 4;
        let bgra = unsafe { std::slice::from_raw_parts(mapped.pData as *const u8, len) };
        Ok(Some(Frame { width: self.width, height: self.height, stride, bgra }))
    }
}
