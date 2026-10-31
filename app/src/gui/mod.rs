//! The desktop app: first-run account setup, the Connect and Settings pages, the Allow/Deny
//! prompt, and the session view.

mod driver;
mod gamepad;
mod icons;
mod input;
mod pages;
mod prompt;
mod session;
mod settings;
mod theme;
mod updates;
mod video;
mod welcome;

use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::Result;
use connect_client::{ClientEvent, ClientHandle, Stage};
use connect_host::{ConnectionRequest, HostConfig, HostEvent, HostHandle};
use connect_net::{
    self as net,
    iroh::{Endpoint, EndpointId},
};
use connect_proto::{DeviceClass, Permissions};
use eframe::egui::{self, ViewportCommand};
use tokio::{
    runtime::{Handle, Runtime},
    sync::mpsc,
};

use settings::Settings;
use welcome::Welcome;

/// How long closing may take to end sessions cleanly before the app exits regardless.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// Set when an update has been installed: the executable to start once this one has exited.
type Restart = Arc<Mutex<Option<PathBuf>>>;

pub fn run(runtime: Runtime, config_dir: PathBuf) -> Result<()> {
    // Lets the UI thread start tasks on the runtime.
    let guard = runtime.enter();
    // An existing account goes straight in; a new install first gets the welcome screen.
    let endpoint = match net::load_identity(&config_dir)? {
        Some(key) => Some(runtime.block_on(net::bind_endpoint(key))?),
        None => None,
    };
    // wgpu (Direct3D 12 / Vulkan / Metal) normally; plain OpenGL for machines where that fails,
    // such as old GPUs and some VMs.
    let renderer = match std::env::var("CONNECT_RENDERER").as_deref() {
        Ok("glow" | "opengl") => eframe::Renderer::Glow,
        _ => eframe::Renderer::Wgpu,
    };
    // Where hardware decoding is wanted and the graphics card decodes through Vulkan, the window
    // renders with the decoder's own device, so decoded pictures go straight to the screen.
    let shared_gpu = (renderer == eframe::Renderer::Wgpu
        && Settings::saved().hardware_decoding
        && std::env::var("CONNECT_RENDERER").as_deref() != Ok("separate"))
    .then(|| match connect_client::SharedGpu::open() {
        Ok(gpu) => Some(gpu),
        Err(e) => {
            tracing::info!("the window draws on its own device: {e:#}");
            None
        }
    })
    .flatten();
    let mut wgpu_options =
        eframe::WgpuConfiguration::default().with_surface_config(eframe::egui_wgpu::SurfaceConfig::LOW_LATENCY);
    if let Some(gpu) = &shared_gpu {
        tracing::info!(gpu = gpu.name, "the window and the video decoder share the graphics card");
        let (instance, adapter, device, queue) = gpu.wgpu();
        wgpu_options.wgpu_setup = eframe::egui_wgpu::WgpuSetup::Existing(eframe::egui_wgpu::WgpuSetupExisting {
            instance,
            adapter,
            device,
            queue,
        });
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Connect")
            .with_inner_size([980.0, 640.0])
            .with_min_inner_size([760.0, 520.0]),
        renderer,
        // One frame queued for the screen, not two: a refresh less of delay.
        wgpu_options,
        ..Default::default()
    };
    let handle = runtime.handle().clone();
    let restart = Restart::default();
    let result = eframe::run_native(
        "Connect",
        options,
        Box::new({
            let restart = restart.clone();
            move |cc| {
                // The window opened on the shared card: decoders hand it textures from now on.
                if let (Some(gpu), Some(_)) = (&shared_gpu, &cc.wgpu_render_state) {
                    gpu.install();
                }
                Ok(Box::new(Root::new(cc, handle, config_dir, endpoint, restart)))
            }
        }),
    )
    .map_err(|e| anyhow::anyhow!("couldn't open the window: {e}"));

    // Sessions were ended in `on_exit`; don't let anything still running hold the process open.
    drop(guard);
    runtime.shutdown_timeout(SHUTDOWN_GRACE);
    if let Some(exe) = restart.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take() {
        std::process::Command::new(&exe).spawn().map_err(|e| anyhow::anyhow!("couldn't restart Connect: {e}"))?;
    }
    result
}

/// The welcome screen until an account exists, then the app proper.
struct Root {
    runtime: Handle,
    restart: Restart,
    screen: Screen,
}

enum Screen {
    Welcome(Welcome),
    Main(Box<App>),
}

impl Root {
    fn new(
        cc: &eframe::CreationContext<'_>,
        runtime: Handle,
        config_dir: PathBuf,
        endpoint: Option<Endpoint>,
        restart: Restart,
    ) -> Self {
        theme::apply(&cc.egui_ctx);
        video::init(cc);
        let settings: Settings = cc.storage.and_then(|s| eframe::get_value(s, eframe::APP_KEY)).unwrap_or_default();
        let screen = match endpoint {
            Some(endpoint) => Screen::Main(Box::new(App::new(&cc.egui_ctx, endpoint, settings, restart.clone()))),
            None => Screen::Welcome(Welcome::new(runtime.clone(), config_dir, settings)),
        };
        Self { runtime, restart, screen }
    }
}

impl eframe::App for Root {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if let Screen::Main(app) = &mut self.screen {
            app.logic(ctx);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        match &mut self.screen {
            Screen::Welcome(welcome) => {
                if let Some((endpoint, settings, setup_gamepads)) = welcome.show(ui) {
                    let mut app = App::new(ui.ctx(), endpoint, settings, self.restart.clone());
                    if setup_gamepads {
                        app.gamepad_driver.install(ui.ctx());
                    }
                    self.screen = Screen::Main(Box::new(app));
                }
            }
            Screen::Main(app) => app.ui(ui),
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        let settings = match &self.screen {
            Screen::Welcome(welcome) => &welcome.settings,
            Screen::Main(app) => &app.settings,
        };
        eframe::set_value(storage, eframe::APP_KEY, settings);
    }

    /// The window is closing: end sessions so nothing is left held down or hanging.
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Screen::Main(app) = &mut self.screen {
            app.shutdown(&self.runtime);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    /// This PC and the computers it can connect to.
    Connect,
    Settings,
}

/// A dialog over the pages.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Dialog {
    /// Typing in a new computer's User-ID.
    AddComputer,
    /// Who is using this PC, and what they may use.
    Hosting,
}

pub struct App {
    endpoint: Endpoint,
    online: Arc<AtomicBool>,
    settings: Settings,
    host: HostHandle,
    host_events: mpsc::UnboundedReceiver<HostEvent>,
    /// Whether this PC can give a remote user a gamepad, and setting that up.
    gamepad_driver: driver::GamepadDriver,
    /// Connection requests waiting for Allow/Deny, oldest first.
    requests: VecDeque<Request>,
    /// Who is using this PC right now.
    hosting: Option<Hosting>,
    page: Page,
    /// When the current page was opened, for fading it in.
    page_since: Instant,
    dialog: Option<Dialog>,
    /// The User-ID being typed into the Connect dialog.
    connect_input: String,
    /// Put the cursor in that field on the next frame.
    focus_input: bool,
    /// The saved computer (by User-ID) whose card asks "Remove?".
    confirm_remove: Option<String>,
    /// Whether Settings shows this PC's User-ID rather than dots.
    show_user_id: bool,
    client: Option<ClientSession>,
    /// The latest thing worth telling the user, such as why a session ended.
    notice: Option<Notice>,
    updater: updates::Updater,
    restart: Restart,
}

struct Request {
    inner: ConnectionRequest,
    arrived: Instant,
    always_allow: bool,
    /// What they'll be able to use if allowed; starts from the defaults.
    permissions: Permissions,
}

struct Hosting {
    peer: EndpointId,
    client_name: String,
    since: Instant,
    permissions: Permissions,
}

struct Notice {
    text: String,
    /// The technical side of an error, shown only on request.
    detail: Option<String>,
    is_error: bool,
    shown: Instant,
}

/// This PC connecting to another one.
struct ClientSession {
    handle: ClientHandle,
    events: mpsc::Receiver<ClientEvent>,
    host_id: EndpointId,
    started: Instant,
    state: ClientState,
    video: video::VideoView,
    /// The pointer position last sent to the host, and when.
    pointer_sent: Option<((u16, u16, bool), Instant)>,
    host_cursor_texture: Option<(u64, egui::TextureHandle)>,
    encoder: String,
    audio_active: bool,
    audio_muted: bool,
    shelf_open: bool,
    show_stats: bool,
    input: input::InputCapture,
    gamepads: Option<gamepad::Forwarder>,
    /// A short message over the video, e.g. that the host changed permissions.
    note: Option<(String, Instant)>,
}

impl ClientSession {
    fn note(&mut self, text: impl Into<String>, for_secs: u64) {
        self.note = Some((text.into(), Instant::now() + Duration::from_secs(for_secs)));
    }
}

enum ClientState {
    Connecting,
    WaitingForApproval,
    Connected { host_name: String, permissions: Permissions },
}

impl App {
    fn new(ctx: &egui::Context, endpoint: Endpoint, settings: Settings, restart: Restart) -> Self {
        connect_client::set_hardware_decoding(settings.hardware_decoding);
        connect_client::set_smooth_pacing(settings.smooth_pacing);
        connect_client::set_allow_h265(settings.allow_h265);
        let online = Arc::new(AtomicBool::new(false));
        tokio::spawn({
            let (endpoint, online, ctx) = (endpoint.clone(), online.clone(), ctx.clone());
            async move {
                endpoint.online().await;
                online.store(true, Ordering::Relaxed);
                ctx.request_repaint();
            }
        });

        let (host, events) = connect_host::start(
            endpoint.clone(),
            HostConfig {
                accepting: settings.accepting,
                default_permissions: settings.permissions,
                trusted: settings.trusted_ids(),
                bitrate_mbps: settings.bitrate_mbps,
                ..HostConfig::new(settings.device_name.clone())
            },
        );

        Self {
            endpoint,
            online,
            settings,
            host,
            host_events: forward_host_events(events, ctx.clone()),
            gamepad_driver: driver::GamepadDriver::new(),
            requests: VecDeque::new(),
            hosting: None,
            page: Page::Connect,
            page_since: Instant::now(),
            dialog: None,
            connect_input: String::new(),
            focus_input: false,
            confirm_remove: None,
            show_user_id: false,
            client: None,
            notice: None,
            updater: updates::Updater::new(ctx),
            restart,
        }
    }

    /// Hangs up as the app closes: the remote user's held input is released on this PC, and the
    /// other side is told at once rather than waiting to time out.
    fn shutdown(&mut self, runtime: &Handle) {
        if let Some(client) = &self.client {
            client.handle.disconnect();
        }
        let (host, endpoint) = (self.host.clone(), self.endpoint.clone());
        runtime.block_on(async move {
            let _ = tokio::time::timeout(SHUTDOWN_GRACE, host.shutdown()).await;
            let _ = tokio::time::timeout(SHUTDOWN_GRACE, endpoint.close()).await;
        });
    }

    fn notify(&mut self, text: impl Into<String>, is_error: bool) {
        self.notice = Some(Notice { text: text.into(), detail: None, is_error, shown: Instant::now() });
    }

    fn on_host_event(&mut self, event: HostEvent) {
        match event {
            HostEvent::Request(inner) => self.requests.push_back(Request {
                inner,
                arrived: Instant::now(),
                always_allow: false,
                permissions: self.settings.permissions,
            }),
            HostEvent::SessionStarted { peer, client_name, permissions } => {
                self.hosting = Some(Hosting { peer, client_name, since: Instant::now(), permissions });
            }
            HostEvent::SessionEnded { reason, .. } => {
                self.hosting = None;
                self.notify(reason, false);
            }
        }
    }

    fn start_connect(&mut self, host_id: EndpointId, ctx: &egui::Context) {
        if self.client.is_some() {
            return;
        }
        let ctx = ctx.clone();
        let (handle, events) =
            connect_client::connect(self.endpoint.clone(), host_id, self.settings.device_name.clone(), move || {
                ctx.request_repaint()
            });
        self.client = Some(ClientSession {
            handle,
            events,
            host_id,
            started: Instant::now(),
            state: ClientState::Connecting,
            video: Default::default(),
            pointer_sent: None,
            host_cursor_texture: None,
            encoder: "Starting encoder…".into(),
            audio_active: false,
            audio_muted: false,
            shelf_open: true,
            show_stats: false,
            input: input::InputCapture::default(),
            gamepads: None,
            note: None,
        });
        self.notice = None;
    }

    fn poll_client(&mut self, ctx: &egui::Context) {
        let Some(client) = &mut self.client else { return };
        let mut ended = None;
        while let Ok(event) = client.events.try_recv() {
            match event {
                ClientEvent::Stage(Stage::Connecting) => client.state = ClientState::Connecting,
                ClientEvent::Stage(Stage::WaitingForApproval) => client.state = ClientState::WaitingForApproval,
                ClientEvent::Accepted { host_name, permissions, .. } => {
                    self.settings.remember_computer(client.host_id, &host_name);
                    self.dialog = None;
                    self.connect_input.clear();
                    client.note(format!("Connected. You can use: {}.", describe(permissions)), 5);
                    client.state = ClientState::Connected { host_name, permissions };
                    client.gamepads = Some(gamepad::Forwarder::start(client.handle.clone()));
                }
                ClientEvent::PermissionsChanged(now) => {
                    if let ClientState::Connected { permissions, .. } = &mut client.state {
                        let before = std::mem::replace(permissions, now);
                        client.input.permissions_changed(now);
                        if !now.allows(DeviceClass::Mouse) {
                            session::set_captured(ctx, client, false);
                        }
                        if before != now {
                            client.note(permission_change(before, now), 5);
                        }
                    }
                }
                ClientEvent::InputUnavailable { reason, .. } => client.note(reason, 10),
                ClientEvent::VideoEncoder { name, codec } => client.encoder = format!("{codec} · {name}"),
                ClientEvent::AudioStatus { active, reason } => {
                    client.audio_active = active;
                    if let Some(reason) = reason {
                        client.note(reason, 10);
                    }
                }
                ClientEvent::VideoFormat { .. } => {}
                ClientEvent::Ended(reason) => ended = Some(reason),
            }
        }
        if let Some(reason) = ended {
            session::set_captured(ctx, client, false);
            let is_error = !matches!(reason, connect_client::EndReason::Disconnected);
            self.client = None;
            self.page = Page::Connect;
            let (text, detail) = explain(&reason);
            self.notice = Some(Notice { text, detail, is_error, shown: Instant::now() });
            if ctx.input(|i| i.viewport().fullscreen.unwrap_or(false)) {
                ctx.send_viewport_cmd(ViewportCommand::Fullscreen(false));
            }
        }
    }

    fn logic(&mut self, ctx: &egui::Context) {
        while let Ok(event) = self.host_events.try_recv() {
            self.on_host_event(event);
        }
        let before = self.requests.len();
        self.requests.retain(|r| r.inner.is_pending());
        if self.requests.len() < before {
            self.notify("A connection request expired or was withdrawn.", false);
        }
        self.poll_client(ctx);
        self.gamepad_driver.poll();
        if let Some(exe) = self.updater.poll() {
            *self.restart.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(exe);
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }

        // Countdown on the prompt, the elapsed timers, and notes fading out over the video.
        let waiting = self.client.as_ref().is_some_and(|c| !matches!(c.state, ClientState::Connected { .. }));
        if !self.requests.is_empty() || waiting {
            ctx.request_repaint_after(Duration::from_millis(250));
        } else if self.hosting.is_some() {
            ctx.request_repaint_after(Duration::from_secs(1));
        }
        if let Some((_, until)) = self.client.as_ref().and_then(|c| c.note.as_ref()) {
            ctx.request_repaint_after(until.saturating_duration_since(Instant::now()));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui) {
        if self.client.as_ref().is_some_and(|c| matches!(c.state, ClientState::Connected { .. })) {
            session::show(self, ui);
        } else {
            let area = ui.max_rect();
            theme::backdrop(ui.painter(), area);
            let content = pages::sidebar(self, ui, area);
            let mut ui = ui.new_child(egui::UiBuilder::new().max_rect(content));
            let in_session = self.hosting.is_some() || self.client.is_some();
            self.updater.banner(&mut ui, in_session);
            egui::ScrollArea::vertical().auto_shrink(false).show(&mut ui, |ui| {
                // Room on the right for the scroll bar, which floats over the content.
                egui::Frame::new().inner_margin(egui::Margin { right: 14, bottom: 24, ..Default::default() }).show(
                    ui,
                    |ui| {
                        theme::fade_in(ui, self.page_since);
                        match self.page {
                            Page::Connect => pages::computers(self, ui),
                            Page::Settings => pages::settings(self, ui),
                        }
                    },
                );
            });
            pages::dialogs(self, ui.ctx());
        }
        prompt::show(self, ui.ctx());
    }
}

/// Why a connection ended, in words for people: for a failure, its outermost reason up front and
/// the error chain beneath it as detail.
fn explain(reason: &connect_client::EndReason) -> (String, Option<String>) {
    let connect_client::EndReason::Failed(error) = reason else { return (reason.to_string(), None) };
    let (head, detail) = match error.split_once(": ") {
        Some((head, rest)) => (head, Some(rest.to_owned())),
        None => (error.as_str(), None),
    };
    let mut text: String = head.chars().take(1).flat_map(char::to_uppercase).chain(head.chars().skip(1)).collect();
    if !text.ends_with(['.', '?', '!']) {
        text.push('.');
    }
    (text, detail)
}

/// "mouse, keyboard and gamepad", or "only the screen (view only)".
fn describe(permissions: Permissions) -> String {
    let names: Vec<String> =
        DeviceClass::ALL.into_iter().filter(|d| permissions.allows(*d)).map(|d| d.label().to_lowercase()).collect();
    match names.as_slice() {
        [] => "nothing, view only".into(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

fn permission_change(before: Permissions, now: Permissions) -> String {
    let changed: Vec<String> = DeviceClass::ALL
        .into_iter()
        .filter(|d| before.allows(*d) != now.allows(*d))
        .map(|d| format!("{} {}", d.label(), if now.allows(d) { "on" } else { "off" }))
        .collect();
    format!("The host turned {}.", changed.join(", "))
}

/// Passes host events to the UI, waking it and bringing the window forward for a request,
/// even when it's minimised.
fn forward_host_events(
    mut events: mpsc::Receiver<HostEvent>,
    ctx: egui::Context,
) -> mpsc::UnboundedReceiver<HostEvent> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            if matches!(event, HostEvent::Request(_)) {
                ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
                ctx.send_viewport_cmd(ViewportCommand::Focus);
                ctx.send_viewport_cmd(ViewportCommand::RequestUserAttention(egui::UserAttentionType::Critical));
            }
            if tx.send(event).is_err() {
                return;
            }
            ctx.request_repaint();
        }
    });
    rx
}

#[cfg(test)]
mod tests {
    use connect_client::EndReason;

    use super::explain;

    #[test]
    fn failures_lead_with_the_reason_and_keep_the_chain_as_detail() {
        let failed = EndReason::Failed(
            "couldn't reach that User-ID. Is Connect running on the host?: No addressing information available: \
             lookup failed"
                .into(),
        );
        let (text, detail) = explain(&failed);
        assert_eq!(text, "Couldn't reach that User-ID. Is Connect running on the host?");
        assert_eq!(detail.as_deref(), Some("No addressing information available: lookup failed"));

        let (text, detail) = explain(&EndReason::Failed("the host didn't answer".into()));
        assert_eq!((text.as_str(), detail), ("The host didn't answer.", None));

        let (text, detail) = explain(&EndReason::Rejected("The host denied the connection.".into()));
        assert_eq!((text.as_str(), detail), ("The host denied the connection.", None));
    }
}
