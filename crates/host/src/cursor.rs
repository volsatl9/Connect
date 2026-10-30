//! Cursor telemetry and a visual remote pointer. This never injects input.
//!
//! The native window works over windowed/borderless applications. Windows may put exclusive
//! fullscreen surfaces above desktop windows, so the remote pointer is not guaranteed there.
// Capture is Windows-only; elsewhere only the tests use most of this.
#![cfg_attr(not(windows), allow(dead_code, unused_imports))]

use std::{
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Instant,
};

use anyhow::{Context, Result};
use connect_proto::cursor::{CursorPosition, CursorShape, MAX_CURSOR_DIMENSION};
use tokio::sync::watch;

#[derive(Clone, Debug)]
pub struct CursorState {
    pub position: CursorPosition,
    pub shape: Option<Arc<CursorShape>>,
}

struct Shared {
    stop: AtomicBool,
    remote: Mutex<Option<(CursorPosition, Instant)>>,
}

/// Owns the native cursor observer and remote-pointer window until dropped.
pub struct CursorStream {
    shared: Arc<Shared>,
    receiver: watch::Receiver<CursorState>,
    thread: Option<JoinHandle<()>>,
}

impl CursorStream {
    pub fn start() -> Result<Self> {
        #[cfg(not(windows))]
        anyhow::bail!("native cursor capture is only supported on Windows hosts");

        #[cfg(windows)]
        {
            let shared = Arc::new(Shared { stop: AtomicBool::new(false), remote: Mutex::new(None) });
            let (sender, receiver) = watch::channel(CursorState {
                position: CursorPosition { seq: 0, x: 0, y: 0, visible: false },
                shape: None,
            });
            let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
            let worker_shared = shared.clone();
            let thread = thread::Builder::new()
                .name("connect-cursor".into())
                .spawn(move || {
                    if let Err(error) = native::run(worker_shared, sender, &ready_tx) {
                        let _ = ready_tx.send(Err(format!("{error:#}")));
                        tracing::warn!("native cursor thread stopped: {error:#}");
                    }
                })
                .context("starting native cursor thread")?;
            match ready_rx.recv().context("native cursor thread exited during startup")? {
                Ok(()) => Ok(Self { shared, receiver, thread: Some(thread) }),
                Err(error) => {
                    let _ = thread.join();
                    anyhow::bail!(error)
                }
            }
        }
    }

    pub fn subscribe(&self) -> watch::Receiver<CursorState> {
        self.receiver.clone()
    }

    /// Call only for the approved session. Visible pointers expire unless periodically refreshed.
    pub fn set_remote_pointer(&self, position: CursorPosition) {
        *self.shared.remote.lock().unwrap_or_else(PoisonError::into_inner) = Some((position, Instant::now()));
        if let Some(thread) = &self.thread {
            thread.thread().unpark();
        }
    }
}

impl Drop for CursorStream {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

fn normalize_axis(value: i32, origin: i32, size: i32) -> u16 {
    let extent = i64::from(size.saturating_sub(1).max(1));
    let offset = (i64::from(value) - i64::from(origin)).clamp(0, extent);
    ((offset * u16::MAX as i64 + extent / 2) / extent) as u16
}

fn denormalize_axis(value: u16, origin: i32, size: i32) -> i32 {
    let extent = i64::from(size.saturating_sub(1).max(0));
    (i64::from(origin) + (i64::from(value) * extent + 32767) / 65535) as i32
}

fn cursor_dimensions(width: u32, height: u32) -> Result<(u16, u16)> {
    anyhow::ensure!(
        width > 0 && height > 0 && width <= 4096 && height <= 4096,
        "invalid native cursor dimensions {width}x{height}"
    );
    let longest = width.max(height);
    let max = u32::from(MAX_CURSOR_DIMENSION);
    if longest <= max {
        return Ok((width as u16, height as u16));
    }
    let scaled = |v: u32| ((v * max + longest / 2) / longest).max(1) as u16;
    Ok((scaled(width), scaled(height)))
}

/// Reconstruct straight-alpha RGBA from the same cursor drawn over black and white.
fn composite_to_rgba(black: &[u8], white: &[u8]) -> Vec<u8> {
    black
        .as_chunks::<4>()
        .0
        .iter()
        .zip(white.as_chunks::<4>().0.iter())
        .flat_map(|(b, w)| {
            let transparency = (0..3).map(|c| w[c].saturating_sub(b[c])).max().unwrap_or(255);
            let alpha = 255 - transparency;
            let channel = |c: usize| {
                if alpha == 0 {
                    0
                } else {
                    ((u32::from(b[c]) * 255 + u32::from(alpha) / 2) / u32::from(alpha)).min(255) as u8
                }
            };
            // XOR inversion cannot be represented by RGBA; inverted monochrome pixels become white.
            [channel(2), channel(1), channel(0), alpha]
        })
        .collect()
}

#[cfg(windows)]
mod native {
    use super::*;
    use std::{
        collections::hash_map::DefaultHasher,
        hash::{Hash, Hasher},
        mem::size_of,
        time::Duration,
    };
    use windows::{
        Win32::{
            Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM},
            Graphics::Gdi::{
                AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
                CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, GdiFlush,
                GetMonitorInfoW, GetObjectW, HBITMAP, HDC, HGDIOBJ, MONITOR_DEFAULTTOPRIMARY, MONITORINFO,
                MonitorFromPoint, SelectObject,
            },
            System::LibraryLoader::GetModuleHandleW,
            UI::{
                HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext},
                WindowsAndMessaging::{
                    CURSOR_SHOWING, CURSOR_SUPPRESSED, CURSORINFO, CreateWindowExW, DI_NORMAL, DefWindowProcW,
                    DestroyWindow, DispatchMessageW, DrawIconEx, GetCursorInfo, GetIconInfo, HCURSOR, HICON,
                    HTTRANSPARENT, HWND_TOPMOST, ICONINFO, MA_NOACTIVATE, MSG, PM_REMOVE, PeekMessageW, RegisterClassW,
                    SW_HIDE, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOSIZE, SetWindowDisplayAffinity, SetWindowPos,
                    ShowWindow, TranslateMessage, ULW_ALPHA, UpdateLayeredWindow, WDA_EXCLUDEFROMCAPTURE,
                    WM_MOUSEACTIVATE, WM_NCHITTEST, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
                    WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
                },
            },
        },
        core::w,
    };

    const POLL: Duration = Duration::from_millis(8);
    const SHAPE_REFRESH: Duration = Duration::from_millis(100);
    const REMOTE_TIMEOUT: Duration = Duration::from_secs(1);

    pub(super) fn run(
        shared: Arc<Shared>,
        sender: watch::Sender<CursorState>,
        ready: &std::sync::mpsc::SyncSender<std::result::Result<(), String>>,
    ) -> Result<()> {
        unsafe {
            let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
        let mut bounds = primary_bounds()?;
        let mut bounds_at = Instant::now();
        let mut overlay = match Overlay::new() {
            Ok(overlay) => Some(overlay),
            Err(error) => {
                tracing::warn!("remote pointer overlay disabled; native cursor capture continues: {error:#}");
                None
            }
        };
        let mut capture = CursorCapture::new();
        ready.send(Ok(())).context("cursor startup receiver closed")?;
        let mut cursor_failed = false;
        let mut overlay_failed = false;
        while !shared.stop.load(Ordering::Acquire) {
            let mut message = MSG::default();
            unsafe {
                while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
            if bounds_at.elapsed() >= Duration::from_secs(1) {
                if let Ok(updated) = primary_bounds() {
                    bounds = updated;
                }
                bounds_at = Instant::now();
            }
            match capture.sample(bounds) {
                Ok(sample) => {
                    cursor_failed = false;
                    publish(&sender, sample);
                }
                Err(error) => {
                    if !cursor_failed {
                        tracing::debug!("cursor unavailable (for example on a secure desktop): {error:#}");
                    }
                    cursor_failed = true;
                    let mut hidden = sender.borrow().clone();
                    hidden.position.visible = false;
                    publish(&sender, hidden);
                }
            }
            if let Some(overlay) = &mut overlay {
                let remote = *shared.remote.lock().unwrap_or_else(PoisonError::into_inner);
                let remote = remote
                    .filter(|(position, at)| position.visible && at.elapsed() < REMOTE_TIMEOUT)
                    .map(|(position, _)| position);
                if let Err(error) = overlay.update(remote, bounds) {
                    overlay.hide();
                    if !overlay_failed {
                        tracing::warn!("remote pointer overlay unavailable: {error:#}");
                    }
                    overlay_failed = true;
                } else {
                    overlay_failed = false;
                }
            }
            thread::park_timeout(POLL);
        }
        Ok(())
    }

    fn publish(sender: &watch::Sender<CursorState>, mut sample: CursorState) {
        let old = sender.borrow();
        if old.position.x == sample.position.x
            && old.position.y == sample.position.y
            && old.position.visible == sample.position.visible
            && old.shape.as_ref().map(|s| s.id) == sample.shape.as_ref().map(|s| s.id)
        {
            return;
        }
        sample.position.seq = old.position.seq.wrapping_add(1);
        drop(old);
        sender.send_replace(sample);
    }

    fn primary_bounds() -> Result<RECT> {
        let monitor = unsafe { MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY) };
        let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        unsafe { GetMonitorInfoW(monitor, &mut info) }.ok().context("reading shared monitor bounds")?;
        anyhow::ensure!(
            info.rcMonitor.right > info.rcMonitor.left && info.rcMonitor.bottom > info.rcMonitor.top,
            "shared monitor has empty bounds"
        );
        Ok(info.rcMonitor)
    }

    struct CursorCapture {
        handle: HCURSOR,
        checked: Instant,
        shape: Option<Arc<CursorShape>>,
    }

    impl CursorCapture {
        fn new() -> Self {
            Self { handle: HCURSOR::default(), checked: Instant::now() - SHAPE_REFRESH, shape: None }
        }

        fn sample(&mut self, bounds: RECT) -> Result<CursorState> {
            let mut info = CURSORINFO { cbSize: size_of::<CURSORINFO>() as u32, ..Default::default() };
            unsafe { GetCursorInfo(&mut info) }.context("reading Windows cursor")?;
            let point = info.ptScreenPos;
            let visible = info.flags.0 & CURSOR_SHOWING.0 != 0
                && info.flags.0 & CURSOR_SUPPRESSED.0 == 0
                && !info.hCursor.0.is_null()
                && point.x >= bounds.left
                && point.x < bounds.right
                && point.y >= bounds.top
                && point.y < bounds.bottom;
            if visible && (info.hCursor != self.handle || self.checked.elapsed() >= SHAPE_REFRESH) {
                let shape = cursor_shape(info.hCursor)?;
                if self.shape.as_ref().map(|s| s.id) != Some(shape.id) {
                    self.shape = Some(Arc::new(shape));
                }
                self.handle = info.hCursor;
                self.checked = Instant::now();
            }
            Ok(CursorState {
                position: CursorPosition {
                    seq: 0,
                    x: normalize_axis(point.x, bounds.left, bounds.right - bounds.left),
                    y: normalize_axis(point.y, bounds.top, bounds.bottom - bounds.top),
                    visible,
                },
                shape: self.shape.clone(),
            })
        }
    }

    struct IconBitmaps(ICONINFO);

    impl Drop for IconBitmaps {
        fn drop(&mut self) {
            unsafe {
                if !self.0.hbmColor.0.is_null() {
                    let _ = DeleteObject(self.0.hbmColor.into());
                }
                if !self.0.hbmMask.0.is_null() {
                    let _ = DeleteObject(self.0.hbmMask.into());
                }
            }
        }
    }

    fn cursor_shape(cursor: HCURSOR) -> Result<CursorShape> {
        let mut icon = IconBitmaps(ICONINFO::default());
        unsafe { GetIconInfo(HICON(cursor.0), &mut icon.0) }.context("reading native cursor shape")?;
        let color = !icon.0.hbmColor.0.is_null();
        let bitmap = if color { icon.0.hbmColor } else { icon.0.hbmMask };
        let mut description = BITMAP::default();
        anyhow::ensure!(
            unsafe {
                GetObjectW(bitmap.into(), size_of::<BITMAP>() as i32, Some((&mut description as *mut BITMAP).cast()))
            } != 0,
            "reading native cursor bitmap"
        );
        let width = description.bmWidth.unsigned_abs();
        let height = description.bmHeight.unsigned_abs() / if color { 1 } else { 2 };
        let (out_w, out_h) = cursor_dimensions(width, height)?;
        let mut dib = Dib::new(i32::from(out_w), i32::from(out_h))?;
        let black = dib.draw_cursor(cursor, 0)?;
        let white = dib.draw_cursor(cursor, 255)?;
        let rgba = composite_to_rgba(&black, &white);
        let hotspot_x =
            ((u64::from(icon.0.xHotspot) * u64::from(out_w) / u64::from(width)).min(u64::from(out_w - 1))) as u16;
        let hotspot_y =
            ((u64::from(icon.0.yHotspot) * u64::from(out_h) / u64::from(height)).min(u64::from(out_h - 1))) as u16;
        let mut hash = DefaultHasher::new();
        (out_w, out_h, hotspot_x, hotspot_y).hash(&mut hash);
        rgba.hash(&mut hash);
        Ok(CursorShape { id: hash.finish(), width: out_w, height: out_h, hotspot_x, hotspot_y, rgba })
    }

    struct Dib {
        dc: HDC,
        bitmap: HBITMAP,
        previous: HGDIOBJ,
        bits: *mut u8,
        width: i32,
        height: i32,
    }

    impl Dib {
        fn new(width: i32, height: i32) -> Result<Self> {
            let mut dib = Self {
                dc: unsafe { CreateCompatibleDC(None) },
                bitmap: HBITMAP::default(),
                previous: HGDIOBJ::default(),
                bits: std::ptr::null_mut(),
                width,
                height,
            };
            anyhow::ensure!(!dib.dc.0.is_null(), "creating cursor bitmap DC: {}", std::io::Error::last_os_error());
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    biHeight: -height,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = std::ptr::null_mut();
            dib.bitmap = unsafe { CreateDIBSection(Some(dib.dc), &info, DIB_RGB_COLORS, &mut bits, None, 0) }
                .context("creating cursor bitmap")?;
            dib.bits = bits.cast();
            dib.previous = unsafe { SelectObject(dib.dc, dib.bitmap.into()) };
            anyhow::ensure!(
                !dib.previous.0.is_null() && dib.previous.0 as isize != -1 && !dib.bits.is_null(),
                "selecting cursor bitmap"
            );
            Ok(dib)
        }

        fn bytes(&mut self) -> &mut [u8] {
            unsafe { std::slice::from_raw_parts_mut(self.bits, (self.width * self.height * 4) as usize) }
        }

        fn draw_cursor(&mut self, cursor: HCURSOR, background: u8) -> Result<Vec<u8>> {
            self.bytes().fill(background);
            unsafe {
                DrawIconEx(self.dc, 0, 0, HICON(cursor.0), self.width, self.height, 0, None, DI_NORMAL)
                    .context("drawing native cursor")?;
                let _ = GdiFlush();
            }
            Ok(self.bytes().to_vec())
        }
    }

    impl Drop for Dib {
        fn drop(&mut self) {
            unsafe {
                if !self.previous.0.is_null() && self.previous.0 as isize != -1 {
                    let _ = SelectObject(self.dc, self.previous);
                }
                if !self.bitmap.0.is_null() {
                    let _ = DeleteObject(self.bitmap.into());
                }
                if !self.dc.0.is_null() {
                    let _ = DeleteDC(self.dc);
                }
            }
        }
    }

    const OVERLAY_WIDTH: i32 = 24;
    const OVERLAY_HEIGHT: i32 = 32;

    struct Overlay {
        window: HWND,
        dib: Dib,
        shown: bool,
        position: Option<POINT>,
    }

    impl Overlay {
        fn new() -> Result<Self> {
            static CLASS: std::sync::OnceLock<std::result::Result<(), String>> = std::sync::OnceLock::new();
            let instance = HINSTANCE(unsafe { GetModuleHandleW(None)? }.0);
            let class = CLASS.get_or_init(|| {
                let class = WNDCLASSW {
                    hInstance: instance,
                    lpfnWndProc: Some(window_proc),
                    lpszClassName: w!("ConnectRemotePointer"),
                    ..Default::default()
                };
                if unsafe { RegisterClassW(&class) } == 0 {
                    Err(format!("registering remote pointer window: {}", std::io::Error::last_os_error()))
                } else {
                    Ok(())
                }
            });
            if let Err(error) = class {
                anyhow::bail!(error.clone());
            }
            let mut dib = Dib::new(OVERLAY_WIDTH, OVERLAY_HEIGHT)?;
            draw_remote_arrow(dib.bytes());
            let window = unsafe {
                CreateWindowExW(
                    WS_EX_LAYERED | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_TRANSPARENT,
                    w!("ConnectRemotePointer"),
                    w!("Connect remote pointer"),
                    WS_POPUP,
                    0,
                    0,
                    OVERLAY_WIDTH,
                    OVERLAY_HEIGHT,
                    None,
                    None,
                    Some(instance),
                    None,
                )
            }
            .context("creating remote pointer window")?;
            let overlay = Self { window, dib, shown: false, position: None };
            // If capture exclusion is unavailable, keep the overlay off instead of duplicating it.
            unsafe { SetWindowDisplayAffinity(window, WDA_EXCLUDEFROMCAPTURE) }
                .context("excluding remote pointer overlay from desktop capture (requires Windows 10 2004+)")?;
            Ok(overlay)
        }

        fn update(&mut self, remote: Option<CursorPosition>, bounds: RECT) -> Result<()> {
            let Some(remote) = remote else {
                self.hide();
                return Ok(());
            };
            let point = POINT {
                x: denormalize_axis(remote.x, bounds.left, bounds.right - bounds.left) - 1,
                y: denormalize_axis(remote.y, bounds.top, bounds.bottom - bounds.top) - 1,
            };
            if self.shown && self.position == Some(point) {
                return Ok(());
            }
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            unsafe {
                UpdateLayeredWindow(
                    self.window,
                    None,
                    Some(&point),
                    Some(&SIZE { cx: OVERLAY_WIDTH, cy: OVERLAY_HEIGHT }),
                    Some(self.dib.dc),
                    Some(&POINT::default()),
                    COLORREF(0),
                    Some(&blend),
                    ULW_ALPHA,
                )
                .context("updating remote pointer window")?;
                SetWindowPos(self.window, Some(HWND_TOPMOST), point.x, point.y, 0, 0, SWP_NOACTIVATE | SWP_NOSIZE)
                    .context("positioning remote pointer window")?;
                let _ = ShowWindow(self.window, SW_SHOWNOACTIVATE);
            }
            self.shown = true;
            self.position = Some(point);
            Ok(())
        }

        fn hide(&mut self) {
            if self.shown {
                unsafe {
                    let _ = ShowWindow(self.window, SW_HIDE);
                }
                self.shown = false;
            }
        }
    }

    impl Drop for Overlay {
        fn drop(&mut self) {
            unsafe {
                let _ = DestroyWindow(self.window);
            }
        }
    }

    unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        match message {
            WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
            WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
            _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
        }
    }

    fn draw_remote_arrow(bytes: &mut [u8]) {
        const POINTS: [(f32, f32); 7] =
            [(1., 1.), (1., 22.), (6., 17.), (11., 28.), (16., 26.), (11., 15.), (20., 15.)];
        for y in 0..OVERLAY_HEIGHT {
            for x in 0..OVERLAY_WIDTH {
                let mut sum = [0u32; 4];
                for sy in 0..4 {
                    for sx in 0..4 {
                        let (px, py) = (x as f32 + (sx as f32 + 0.5) / 4., y as f32 + (sy as f32 + 0.5) / 4.);
                        let mut inside = false;
                        let mut distance = f32::MAX;
                        for i in 0..POINTS.len() {
                            let (ax, ay) = POINTS[i];
                            let (bx, by) = POINTS[(i + 1) % POINTS.len()];
                            if (ay > py) != (by > py) && px < (bx - ax) * (py - ay) / (by - ay) + ax {
                                inside = !inside;
                            }
                            let t = (((px - ax) * (bx - ax) + (py - ay) * (by - ay))
                                / ((bx - ax).powi(2) + (by - ay).powi(2)))
                            .clamp(0., 1.);
                            distance = distance
                                .min(((px - ax - t * (bx - ax)).powi(2) + (py - ay - t * (by - ay)).powi(2)).sqrt());
                        }
                        if inside {
                            let color = if distance < 1.0 { [35, 28, 15, 255] } else { [255, 224, 45, 255] };
                            for c in 0..4 {
                                sum[c] += color[c];
                            }
                        }
                    }
                }
                let offset = ((y * OVERLAY_WIDTH + x) * 4) as usize;
                for c in 0..4 {
                    bytes[offset + c] = ((sum[c] + 8) / 16) as u8;
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use windows::Win32::{
            System::Threading::{GR_GDIOBJECTS, GetCurrentProcess, GetGuiResources},
            UI::WindowsAndMessaging::{
                FindWindowW, GWL_EXSTYLE, GetForegroundWindow, GetWindowDisplayAffinity, GetWindowLongPtrW, IDC_ARROW,
                IsWindow, IsWindowVisible, LoadCursorW,
            },
        };

        #[test]
        fn native_system_cursor_keeps_transparency_and_releases_gdi_objects() {
            let cursor = unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap();
            let _ = cursor_shape(cursor).unwrap();
            let before = unsafe { GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS) };
            for _ in 0..32 {
                let shape = cursor_shape(cursor).unwrap();
                assert!(shape.width <= 64 && shape.height <= 64);
                assert!(shape.hotspot_x < shape.width && shape.hotspot_y < shape.height);
                assert_eq!(shape.rgba.len(), usize::from(shape.width) * usize::from(shape.height) * 4);
                assert!(shape.rgba.as_chunks::<4>().0.iter().any(|p| p[3] == 0));
                assert!(shape.rgba.as_chunks::<4>().0.iter().any(|p| p[3] > 0));
            }
            let after = unsafe { GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS) };
            assert_eq!(before, after, "cursor capture leaked GDI objects");
        }

        /// Shows only a local visual pointer; never moves the OS cursor or injects input.
        #[test]
        #[ignore = "briefly shows a local remote-pointer overlay on the desktop"]
        fn native_cursor_overlay_probe() {
            let foreground = unsafe { GetForegroundWindow() };
            let stream = CursorStream::start().unwrap();
            stream.set_remote_pointer(CursorPosition { seq: 1, x: 32767, y: 32767, visible: true });
            thread::sleep(Duration::from_millis(60));
            let window = unsafe { FindWindowW(w!("ConnectRemotePointer"), None) }.unwrap();
            let styles = unsafe { GetWindowLongPtrW(window, GWL_EXSTYLE) } as u32;
            for required in [WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT] {
                assert_eq!(styles & required.0, required.0);
            }
            let mut affinity = 0;
            unsafe { GetWindowDisplayAffinity(window, &mut affinity) }.unwrap();
            assert_eq!(affinity, WDA_EXCLUDEFROMCAPTURE.0);
            assert!(unsafe { IsWindowVisible(window) }.as_bool());
            assert_eq!(unsafe { GetForegroundWindow() }, foreground, "overlay took focus");
            assert_eq!(
                unsafe { window_proc(window, WM_NCHITTEST, WPARAM(0), LPARAM(0)) },
                LRESULT(HTTRANSPARENT as isize)
            );
            assert_eq!(
                unsafe { window_proc(window, WM_MOUSEACTIVATE, WPARAM(0), LPARAM(0)) },
                LRESULT(MA_NOACTIVATE as isize)
            );
            let mut arrow = vec![0; (OVERLAY_WIDTH * OVERLAY_HEIGHT * 4) as usize];
            draw_remote_arrow(&mut arrow);
            save_probe_preview(&arrow);
            thread::sleep(Duration::from_millis(1100));
            assert!(!unsafe { IsWindowVisible(window) }.as_bool(), "stale remote pointer remained visible");
            let start = Instant::now();
            drop(stream);
            assert!(start.elapsed() < Duration::from_millis(250), "cursor thread did not stop promptly");
            assert!(!unsafe { IsWindow(Some(window)) }.as_bool(), "overlay window survived stream shutdown");
            println!(
                "native pointer probe: no focus, transparent hit test, capture excluded, stale hide, clean shutdown"
            );
        }

        fn save_probe_preview(arrow: &[u8]) {
            const SCALE: usize = 6;
            let (width, height) = (OVERLAY_WIDTH as usize * SCALE, OVERLAY_HEIGHT as usize * SCALE);
            let data_len = width * height * 4;
            let mut bitmap = vec![0u8; 54 + data_len];
            bitmap[0..2].copy_from_slice(b"BM");
            bitmap[2..6].copy_from_slice(&((54 + data_len) as u32).to_le_bytes());
            bitmap[10..14].copy_from_slice(&54u32.to_le_bytes());
            bitmap[14..18].copy_from_slice(&40u32.to_le_bytes());
            bitmap[18..22].copy_from_slice(&(width as i32).to_le_bytes());
            bitmap[22..26].copy_from_slice(&(-(height as i32)).to_le_bytes());
            bitmap[26..28].copy_from_slice(&1u16.to_le_bytes());
            bitmap[28..30].copy_from_slice(&32u16.to_le_bytes());
            for y in 0..height {
                for x in 0..width {
                    let source = ((y / SCALE) * OVERLAY_WIDTH as usize + x / SCALE) * 4;
                    let target = 54 + (y * width + x) * 4;
                    let background = if (x / 24 + y / 24) % 2 == 0 { 55u16 } else { 75 };
                    for c in 0..3 {
                        bitmap[target + c] = (u16::from(arrow[source + c])
                            + (255 - u16::from(arrow[source + 3])) * background / 255)
                            .min(255) as u8;
                    }
                    bitmap[target + 3] = 255;
                }
            }
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.tools/cursor-overlay-preview.bmp");
            std::fs::write(&path, bitmap).unwrap();
            println!("overlay bitmap preview: {}", path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalized_coordinates_include_both_edges_and_negative_monitor_origins() {
        assert_eq!(normalize_axis(-1920, -1920, 1920), 0);
        assert_eq!(normalize_axis(-1, -1920, 1920), u16::MAX);
        assert_eq!(normalize_axis(100, -1920, 1920), u16::MAX);
        assert_eq!(denormalize_axis(0, -1920, 1920), -1920);
        assert_eq!(denormalize_axis(u16::MAX, -1920, 1920), -1);
        for pixel in 0..3840 {
            assert_eq!(denormalize_axis(normalize_axis(pixel, 0, 3840), 0, 3840), pixel);
        }
        assert_eq!(normalize_axis(0, 0, 1), 0);
        assert_eq!(denormalize_axis(u16::MAX, 42, 1), 42);
    }

    #[test]
    fn oversized_native_cursor_is_scaled_inside_wire_bound() {
        assert_eq!(cursor_dimensions(32, 48).unwrap(), (32, 48));
        assert_eq!(cursor_dimensions(256, 128).unwrap(), (64, 32));
        assert_eq!(cursor_dimensions(1, 4096).unwrap(), (1, 64));
        assert!(cursor_dimensions(0, 32).is_err());
        assert!(cursor_dimensions(4097, 32).is_err());
    }

    #[test]
    fn native_composites_recover_transparency_color_and_inverted_mask() {
        let black = [0, 0, 0, 0, 30, 20, 10, 0, 128, 64, 32, 0, 255, 255, 255, 0];
        let white = [255, 255, 255, 0, 30, 20, 10, 0, 255, 191, 159, 0, 0, 0, 0, 0];
        assert_eq!(
            composite_to_rgba(&black, &white),
            [0, 0, 0, 0, 10, 20, 30, 255, 64, 128, 255, 128, 255, 255, 255, 255,]
        );
    }
}
