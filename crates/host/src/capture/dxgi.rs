//! Primary-monitor capture with DXGI Desktop Duplication.
//!
//! Frames either stay on the graphics card ([`DxgiCapturer::next_texture`]: copied into a
//! texture of our own, for colour conversion and a hardware encoder on the same device), or are
//! copied GPU → CPU through a staging texture and handed out straight from the mapped staging
//! memory ([`Capturer::next_frame`]). The mouse pointer is not part of the duplicated image; it
//! travels separately.

use std::time::Duration;

use anyhow::{Context, Result};
use windows::{
    Win32::{
        Foundation::HMODULE,
        System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency},
        Graphics::{
            Direct3D::D3D_DRIVER_TYPE_HARDWARE,
            Direct3D11::{
                D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ,
                D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_FLAG, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
                D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
                ID3D11Texture2D,
            },
            Dxgi::{
                DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, IDXGIDevice, IDXGIOutput1,
                IDXGIOutputDuplication, IDXGIResource,
            },
        },
    },
    core::Interface,
};

use super::{CaptureTiming, Capturer, Frame};

pub(crate) struct DxgiCapturer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    output: IDXGIOutput1,
    duplication: Option<IDXGIOutputDuplication>,
    staging: Option<(ID3D11Texture2D, u32, u32)>,
    /// The latest image, kept on the graphics card for [`Self::next_texture`].
    latest: Option<(ID3D11Texture2D, u32, u32)>,
    /// Whether the staging texture is mapped, i.e. the last frame handed out is still readable.
    mapped: bool,
    refresh_hz: Option<u32>,
    timing: CaptureTiming,
    width: u32,
    height: u32,
}

impl DxgiCapturer {
    pub fn new() -> Result<Self> {
        unsafe {
            // Video support lets the same device convert colours and feed the hardware encoder;
            // a driver without it still captures, through memory.
            let (device, context) = create_device(D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT)
                .or_else(|_| create_device(D3D11_CREATE_DEVICE_BGRA_SUPPORT))
                .context("creating Direct3D 11 device")?;
            // Media Foundation's encoder uses the device from its own threads too.
            if let Ok(multithread) = device.cast::<ID3D11Multithread>() {
                let _ = multithread.SetMultithreadProtected(true);
            }

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
                latest: None,
                mapped: false,
                refresh_hz: None,
                timing: CaptureTiming { acquired: std::time::Instant::now(), present_to_capture: None },
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

unsafe fn create_device(flags: D3D11_CREATE_DEVICE_FLAG) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device = None;
    let mut context = None;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            flags,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
    }
    Ok((device.context("no Direct3D device")?, context.context("no Direct3D context")?))
}

impl DxgiCapturer {
    /// The device capture runs on; colour conversion and the encoder must use the same one.
    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }

    /// Waits up to `timeout` for the screen to change and copies the new image into
    /// [`Self::latest`], on the graphics card. Returns its size, or `None` if nothing changed.
    pub fn next_texture(&mut self, timeout: Duration) -> Result<Option<(u32, u32)>> {
        self.unmap();
        let Some((texture, desc)) = self.acquire(timeout)? else { return Ok(None) };
        let copied = (|| -> Result<()> {
            if self.latest.as_ref().is_none_or(|(_, w, h)| (*w, *h) != (desc.Width, desc.Height)) {
                // Our own copy: the duplicated image has to be released straight away, and isn't
                // made to be read by the video processor.
                let latest_desc = D3D11_TEXTURE2D_DESC {
                    Width: desc.Width,
                    Height: desc.Height,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: desc.Format,
                    SampleDesc: desc.SampleDesc,
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: 0,
                };
                let mut tex = None;
                unsafe { self.device.CreateTexture2D(&latest_desc, None, Some(&mut tex)) }
                    .context("creating capture texture")?;
                self.latest = Some((tex.context("no capture texture")?, desc.Width, desc.Height));
            }
            let (latest, ..) = self.latest.as_ref().expect("set above");
            unsafe { self.context.CopyResource(latest, &texture) };
            self.width = desc.Width;
            self.height = desc.Height;
            Ok(())
        })();
        self.release()?;
        copied?;
        Ok(Some((self.width, self.height)))
    }

    /// The image [`Self::next_texture`] captured last.
    pub fn latest(&self) -> &ID3D11Texture2D {
        &self.latest.as_ref().expect("next_texture captured an image").0
    }

    /// The image [`Self::next_texture`] captured last, read back into memory: for carrying on
    /// through memory when something on the graphics card fails.
    pub fn read_latest(&mut self) -> Result<Option<Frame<'_>>> {
        self.unmap();
        let Some((latest, ..)) = self.latest.clone() else { return Ok(None) };
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { latest.GetDesc(&mut desc) };
        let staging = self.ensure_staging(&desc)?;
        unsafe { self.context.CopyResource(&staging, &latest) };
        self.map_staging().map(Some)
    }

    /// Takes the next changed desktop image, if one comes within `timeout`. The caller copies
    /// it and then calls [`Self::release`].
    fn acquire(&mut self, timeout: Duration) -> Result<Option<(ID3D11Texture2D, D3D11_TEXTURE2D_DESC)>> {
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
        // LastPresentTime is the performance counter when the image went to the screen.
        let (mut now, mut frequency) = (0, 0);
        let present_to_capture = unsafe {
            QueryPerformanceCounter(&mut now).is_ok() && QueryPerformanceFrequency(&mut frequency).is_ok()
        }
        .then(|| now.saturating_sub(info.LastPresentTime))
        .filter(|&ticks| frequency > 0 && ticks >= 0)
        .map(|ticks| Duration::from_secs_f64(ticks as f64 / frequency as f64));
        self.timing = CaptureTiming { acquired: std::time::Instant::now(), present_to_capture };
        let texture = match resource.context("no desktop image").and_then(|r| Ok(r.cast::<ID3D11Texture2D>()?)) {
            Ok(texture) => texture,
            Err(e) => {
                unsafe { duplication.ReleaseFrame() }?;
                return Err(e);
            }
        };
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        Ok(Some((texture, desc)))
    }

    fn release(&mut self) -> Result<()> {
        if let Some(duplication) = &self.duplication {
            unsafe { duplication.ReleaseFrame() }?;
        }
        Ok(())
    }

    /// Maps the staging texture and hands out its image, readable until the next call.
    fn map_staging(&mut self) -> Result<Frame<'_>> {
        let (staging, w, h) = self.staging.clone().expect("created by the caller");
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe { self.context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }
            .context("reading desktop image")?;
        self.mapped = true;
        let stride = mapped.RowPitch as usize;
        // SAFETY: the texture stays mapped, and so this memory readable, until the next call
        // (or drop), which the returned frame's borrow of `self` outlives.
        let len = stride * (h as usize - 1) + w as usize * 4;
        let bgra = unsafe { std::slice::from_raw_parts(mapped.pData as *const u8, len) };
        Ok(Frame { width: w, height: h, stride, bgra })
    }

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
    fn timing(&self) -> CaptureTiming {
        self.timing
    }

    fn refresh_hz(&self) -> Option<u32> {
        self.refresh_hz
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame<'_>>> {
        // The previous frame has been read by now; its staging texture takes the next one.
        self.unmap();
        let Some((texture, desc)) = self.acquire(timeout)? else { return Ok(None) };
        let copied = (|| -> Result<()> {
            let staging = self.ensure_staging(&desc)?;
            unsafe { self.context.CopyResource(&staging, &texture) };
            self.width = desc.Width;
            self.height = desc.Height;
            Ok(())
        })();
        self.release()?;
        copied?;
        self.map_staging().map(Some)
    }

    fn as_dxgi(&mut self) -> Option<&mut DxgiCapturer> {
        Some(self)
    }
}
