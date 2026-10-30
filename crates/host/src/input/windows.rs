//! Injects input with `SendInput` (mouse, keyboard) and the ViGEmBus driver (virtual Xbox 360
//! gamepads). Keys go in as scancodes, which games reading DirectInput or Raw Input also see.
//!
//! The host's own mouse and keyboard win: while they're in use, the remote user's presses and
//! movement on that device are dropped (releases still go through) and whatever the remote user
//! held there is let go. Low-level hooks tell the host's real input from what this module injects.
//!
//! Windows won't let a normal program send input to one running as administrator, so those
//! windows ignore the remote user unless Connect itself runs as administrator.

use std::{
    os::windows::ffi::OsStrExt,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{Receiver, RecvTimeoutError},
    },
    time::{Duration, Instant},
};

use connect_proto::{
    DeviceClass,
    input::{GamepadState, InputEvent, MAX_GAMEPADS, MouseButton},
};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, info, warn};
use vigem_client::{Client, TargetId, XButtons, XGamepad, Xbox360Wired};
use windows::{
    Win32::{
        Foundation::{CloseHandle, ERROR_CANCELLED, HINSTANCE, LPARAM, LRESULT, WPARAM},
        System::{
            LibraryLoader::GetModuleHandleW,
            SystemInformation::GetTickCount64,
            Threading::{GetCurrentThreadId, GetExitCodeProcess, WaitForSingleObject},
        },
        UI::{
            Input::KeyboardAndMouse::{
                INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
                KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
                MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
                MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
                MOUSEEVENTF_XUP, MOUSEINPUT, SendInput, VIRTUAL_KEY,
            },
            Shell::{SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW},
            WindowsAndMessaging::{
                CallNextHookEx, GetMessageW, HC_ACTION, KBDLLHOOKSTRUCT, LLKHF_INJECTED, LLKHF_LOWER_IL_INJECTED,
                LLMHF_INJECTED, LLMHF_LOWER_IL_INJECTED, MSG, MSLLHOOKSTRUCT, PM_NOREMOVE, PeekMessageW,
                PostThreadMessageW, SW_HIDE, SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL, WH_MOUSE_LL,
                WM_QUIT, WM_USER,
            },
        },
    },
    core::{HRESULT, PCWSTR, w},
};

use super::{GAMEPAD_DRIVER_URL, Held, Unavailable};

const XBUTTON1: u32 = 1;
const XBUTTON2: u32 = 2;

/// The remote user's mouse/keyboard waits this long after the host last used their own.
const HOST_PRIORITY_MS: u64 = 1000;
/// How often the injector looks at the host's activity when no remote input is arriving.
const POLL: Duration = Duration::from_millis(15);
/// Least time between "the host is using it" notes to the remote user, per device.
const NOTICE_EVERY: Duration = Duration::from_secs(10);

/// The ViGEmBus installer, built into the app so gamepads need no separate download.
static DRIVER_INSTALLER: &[u8] = include_bytes!("../../vendor/vigembus/ViGEmBus_1.22.0_x64_x86_arm64.exe");
/// Longest the driver installer may run.
const INSTALL_TIMEOUT_MS: u32 = 5 * 60 * 1000;

pub(super) fn gamepad_support() -> Result<(), String> {
    Client::connect().map(drop).map_err(describe)
}

fn describe(e: vigem_client::Error) -> String {
    match e {
        vigem_client::Error::BusNotFound => "Gamepad support isn't set up on the host PC yet.".into(),
        other => format!("The host's gamepad driver (ViGEmBus) failed: {other}. See {GAMEPAD_DRIVER_URL}"),
    }
}

/// Installs the built-in gamepad driver: Windows asks the user for permission, then the
/// installer runs silently. Blocks until it's done; the unpacked installer is deleted afterwards.
pub(super) fn install_gamepad_driver() -> Result<(), String> {
    if gamepad_support().is_ok() {
        return Ok(());
    }
    let path = std::env::temp_dir().join("connect-gamepad-driver-setup.exe");
    std::fs::write(&path, DRIVER_INSTALLER).map_err(|e| format!("Couldn't unpack the gamepad driver: {e}"))?;
    let result = run_elevated(&path, "/quiet /norestart");
    let _ = std::fs::remove_file(&path);
    let code = result?;
    info!(code, "gamepad driver installer finished");
    match gamepad_support() {
        Ok(()) => Ok(()),
        // ERROR_SUCCESS_REBOOT_REQUIRED
        Err(_) if code == 3010 => Err("Gamepad support is installed; restart Windows to finish setting it up.".into()),
        Err(e) => Err(format!("The gamepad driver didn't install (code {code}). {e}")),
    }
}

/// Runs `exe` as administrator (Windows shows its permission prompt) and returns its exit code.
fn run_elevated(exe: &Path, args: &str) -> Result<u32, String> {
    let file: Vec<u16> = exe.as_os_str().encode_wide().chain([0]).collect();
    let params: Vec<u16> = args.encode_utf16().chain([0]).collect();
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
        lpVerb: w!("runas"),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(params.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    if let Err(e) = unsafe { ShellExecuteExW(&mut info) } {
        return Err(if e.code() == HRESULT::from_win32(ERROR_CANCELLED.0) {
            "Windows' permission prompt was declined, so gamepad support wasn't set up.".into()
        } else {
            format!("Couldn't start the gamepad driver setup: {e}")
        });
    }
    if info.hProcess.is_invalid() {
        return Err("The gamepad driver setup didn't start.".into());
    }
    let mut code = 0;
    unsafe {
        WaitForSingleObject(info.hProcess, INSTALL_TIMEOUT_MS);
        let _ = GetExitCodeProcess(info.hProcess, &mut code);
        let _ = CloseHandle(info.hProcess);
    }
    Ok(code)
}

/// Applies events until the session's sender is dropped, then unplugs its gamepads.
pub(super) fn run(events: Receiver<InputEvent>, unavailable: UnboundedSender<Unavailable>) {
    let local = LocalInput::start();
    let mut gamepads = Gamepads::default();
    let mut held = Held::default();
    let mut host_using = [false; 2];
    let mut last_notice: [Option<Instant>; 2] = [None; 2];
    let mut blocked_reported = false;

    loop {
        let event = match events.recv_timeout(POLL) {
            Ok(event) => Some(event),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break,
        };

        // The host just took their mouse or keyboard back: let go of what the remote user held.
        for (i, device) in [DeviceClass::Mouse, DeviceClass::Keyboard].into_iter().enumerate() {
            let using = local.as_ref().is_some_and(|l| l.recently_used(device));
            if using && !host_using[i] {
                debug!(?device, "host took over");
                for release in held.release(device) {
                    send(release);
                }
                if last_notice[i].is_none_or(|t| t.elapsed() >= NOTICE_EVERY) {
                    last_notice[i] = Some(Instant::now());
                    let what = if device == DeviceClass::Mouse { "mouse" } else { "keyboard" };
                    let reason = format!("The host is using their {what}. Yours works again a moment after they stop.");
                    let _ = unavailable.send(Unavailable { device, reason });
                }
            }
            host_using[i] = using;
        }

        let Some(event) = event else { continue };
        match event {
            InputEvent::Gamepad { pad, state } => {
                if let Err(reason) = gamepads.update(pad, state) {
                    let _ = unavailable.send(Unavailable { device: DeviceClass::Gamepad, reason });
                }
            }
            InputEvent::GamepadRemoved { pad } => gamepads.remove(pad),
            other => {
                let host_has_it = local.as_ref().is_some_and(|l| l.recently_used(other.device()));
                if host_has_it && !is_release(&other) {
                    continue;
                }
                held.note(&other);
                if !send(other) && !blocked_reported {
                    // Typically the foreground window belongs to an elevated program.
                    debug!("Windows blocked injected input");
                    blocked_reported = true;
                }
            }
        }
    }
}

/// Key and button releases always go through, so nothing can stay stuck down.
fn is_release(event: &InputEvent) -> bool {
    matches!(event, InputEvent::Key { down: false, .. } | InputEvent::MouseButton { down: false, .. })
}

/// `false` if Windows refused the input.
fn send(event: InputEvent) -> bool {
    let input = match event {
        InputEvent::MouseMoveTo { x, y } => {
            // Absolute coordinates without MOUSEEVENTF_VIRTUALDESK span the primary monitor,
            // which is the one being captured.
            mouse(x.into(), y.into(), 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE)
        }
        InputEvent::MouseMoveBy { dx, dy } => mouse(dx, dy, 0, MOUSEEVENTF_MOVE),
        InputEvent::MouseButton { button, down } => {
            let (flags, data) = match (button, down) {
                (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
                (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
                (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
                (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
                (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
                (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
                (MouseButton::Back, true) => (MOUSEEVENTF_XDOWN, XBUTTON1),
                (MouseButton::Back, false) => (MOUSEEVENTF_XUP, XBUTTON1),
                (MouseButton::Forward, true) => (MOUSEEVENTF_XDOWN, XBUTTON2),
                (MouseButton::Forward, false) => (MOUSEEVENTF_XUP, XBUTTON2),
            };
            mouse(0, 0, data, flags)
        }
        InputEvent::MouseWheel { dx, dy } => {
            let mut ok = true;
            if dy != 0 {
                ok &= submit(&[mouse(0, 0, dy as u32, MOUSEEVENTF_WHEEL)]);
            }
            if dx != 0 {
                ok &= submit(&[mouse(0, 0, dx as u32, MOUSEEVENTF_HWHEEL)]);
            }
            return ok;
        }
        InputEvent::Key { scancode, down } => {
            let mut flags = KEYEVENTF_SCANCODE;
            if scancode & 0xFF00 == 0xE000 {
                flags |= KEYEVENTF_EXTENDEDKEY;
            }
            if !down {
                flags |= KEYEVENTF_KEYUP;
            }
            key(scancode & 0xFF, flags)
        }
        InputEvent::Gamepad { .. } | InputEvent::GamepadRemoved { .. } => return true,
    };
    submit(&[input])
}

fn mouse(dx: i32, dy: i32, data: u32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy, mouseData: data, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    }
}

fn key(scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: VIRTUAL_KEY(0), wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    }
}

fn submit(inputs: &[INPUT]) -> bool {
    let sent = unsafe { SendInput(inputs, size_of::<INPUT>() as i32) };
    sent as usize == inputs.len()
}

/// When the host last used their own (physical, not injected) mouse and keyboard, in
/// `GetTickCount64` milliseconds. Written by the hooks, which can only reach statics.
static LOCAL_MOUSE_AT: AtomicU64 = AtomicU64::new(0);
static LOCAL_KEYBOARD_AT: AtomicU64 = AtomicU64::new(0);

/// Low-level mouse and keyboard hooks on their own thread, for the length of a session.
struct LocalInput {
    thread_id: u32,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl LocalInput {
    fn start() -> Option<Self> {
        let (tx, rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("connect-host-input".into())
            .spawn(move || unsafe {
                let mut msg = MSG::default();
                // Create this thread's message queue before anyone can post to it.
                let _ = PeekMessageW(&mut msg, None, WM_USER, WM_USER, PM_NOREMOVE);
                let module = GetModuleHandleW(PCWSTR::null()).ok().map(|m| HINSTANCE(m.0));
                let hooks = [
                    SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), module, 0),
                    SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), module, 0),
                ];
                if let Some(Err(e)) = hooks.iter().find(|h| h.is_err()) {
                    warn!("can't watch the host's own input, so it won't take priority: {e}");
                }
                let _ = tx.send(GetCurrentThreadId());
                // Hooks are called from inside this loop; it ends with WM_QUIT.
                while GetMessageW(&mut msg, None, 0, 0).0 > 0 {}
                for hook in hooks.into_iter().flatten() {
                    let _ = UnhookWindowsHookEx(hook);
                }
            })
            .map_err(|e| warn!("couldn't start watching the host's input: {e}"))
            .ok()?;
        let thread_id = rx.recv().ok()?;
        Some(Self { thread_id, thread: Some(thread) })
    }

    fn recently_used(&self, device: DeviceClass) -> bool {
        let at = match device {
            DeviceClass::Mouse => &LOCAL_MOUSE_AT,
            DeviceClass::Keyboard => &LOCAL_KEYBOARD_AT,
            DeviceClass::Gamepad | DeviceClass::Audio => return false,
        };
        unsafe { GetTickCount64() }.saturating_sub(at.load(Ordering::Relaxed)) < HOST_PRIORITY_MS
    }
}

impl Drop for LocalInput {
    fn drop(&mut self) {
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let info = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
        if info.flags & (LLMHF_INJECTED | LLMHF_LOWER_IL_INJECTED) == 0 {
            LOCAL_MOUSE_AT.store(unsafe { GetTickCount64() }, Ordering::Relaxed);
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let info = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        if info.flags.0 & (LLKHF_INJECTED.0 | LLKHF_LOWER_IL_INJECTED.0) == 0 {
            LOCAL_KEYBOARD_AT.store(unsafe { GetTickCount64() }, Ordering::Relaxed);
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// Virtual Xbox 360 pads, plugged in on first use and unplugged when dropped. Windows gives each
/// the next free controller slot, so with the host's own controller connected the remote user's
/// becomes player 2.
#[derive(Default)]
struct Gamepads {
    /// `None` until the first gamepad event; `Err` once connecting to the driver failed.
    client: Option<Result<Arc<Client>, String>>,
    pads: [Option<Xbox360Wired<Arc<Client>>>; MAX_GAMEPADS as usize],
}

impl Gamepads {
    /// Applies `state` to slot `pad`. `Err` (once per session) if gamepads can't work here.
    fn update(&mut self, pad: u8, state: GamepadState) -> Result<(), String> {
        let first_try = self.client.is_none();
        let client = match self.client.get_or_insert_with(|| Client::connect().map(Arc::new).map_err(describe)) {
            Ok(client) => client.clone(),
            Err(reason) if first_try => {
                warn!("gamepad input unavailable: {reason}");
                return Err(reason.clone());
            }
            Err(_) => return Ok(()),
        };
        let slot = &mut self.pads[pad as usize];
        if slot.is_none() {
            let mut target = Xbox360Wired::new(client, TargetId::XBOX360_WIRED);
            if let Err(e) = target.plugin().and_then(|()| target.wait_ready()) {
                warn!(pad, "couldn't plug in a virtual gamepad: {e}");
                return Ok(());
            }
            debug!(pad, "virtual gamepad plugged in");
            *slot = Some(target);
        }
        let report = XGamepad {
            buttons: XButtons(state.buttons),
            left_trigger: state.left_trigger,
            right_trigger: state.right_trigger,
            thumb_lx: state.left_x,
            thumb_ly: state.left_y,
            thumb_rx: state.right_x,
            thumb_ry: state.right_y,
        };
        if let Some(target) = slot
            && let Err(e) = target.update(&report)
        {
            debug!(pad, "gamepad update failed: {e}");
        }
        Ok(())
    }

    fn remove(&mut self, pad: u8) {
        if let Some(slot) = self.pads.get_mut(pad as usize) {
            *slot = None; // dropping unplugs it
        }
    }
}
