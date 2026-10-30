//! Remote desktop, independent cursor updates, and an opaque left-side shelf.
use super::{
    App, ClientSession, ClientState,
    input::{Hotkey, normalise},
    theme,
};
use crate::stats_line;
use connect_proto::DeviceClass;
use eframe::egui::{self, Color32, CursorGrab, Rect, RichText, Sense, TextureOptions, ViewportCommand};
use std::time::{Duration, Instant};

const SHELF_WIDTH: f32 = 180.0;
const SHELF_HEIGHT: f32 = 362.0;
const RECENTRE_MARGIN: f32 = 80.0;
/// An unmoved pointer is reported again this often; the host hides it after a second without.
const POINTER_REPEAT: Duration = Duration::from_millis(250);

pub fn show(app: &mut App, ui: &mut egui::Ui) {
    let ctx = ui.ctx().clone();
    let Some(client) = &mut app.client else { return };
    let ClientState::Connected { host_name, permissions } = &client.state else { return };
    let (host_name, permissions) = (host_name.clone(), *permissions);
    let area = ui.max_rect();
    let frame = client.handle.take_frame();
    let image_size =
        frame.as_ref().map(|f| egui::vec2(f.width as f32, f.height as f32)).or_else(|| client.video.size());
    let video_rect = image_size.map_or(area, |size| fit(size, area));
    let fullscreen = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
    let (events, focused, pointer) = ctx.input(|i| (i.events.clone(), i.focused, i.pointer.hover_pos()));
    let shelf = shelf_rect(area);
    let handle = Rect::from_min_size(egui::pos2(area.left() + 6.0, area.center().y - 24.0), egui::vec2(24.0, 48.0));
    let blocked = (!client.input.captured).then_some(if client.shelf_open { shelf } else { handle });
    // Send input before color conversion and texture uploads can delay this GUI frame.
    for hotkey in client.input.process(&events, video_rect, blocked, pointer, permissions, &client.handle) {
        match hotkey {
            Hotkey::Fullscreen => ctx.send_viewport_cmd(ViewportCommand::Fullscreen(!fullscreen)),
            Hotkey::Stats => client.show_stats = !client.show_stats,
            Hotkey::MouseCapture => {
                let capture = !client.input.captured && permissions.allows(DeviceClass::Mouse);
                set_captured(&ctx, client, capture);
            }
        }
    }
    if client.input.captured {
        if !focused {
            set_captured(&ctx, client, false);
        } else if pointer.is_some_and(|p| !area.shrink(RECENTRE_MARGIN).contains(p)) {
            ctx.send_viewport_cmd(ViewportCommand::CursorPosition(area.center()));
        }
    }
    if let Some(gamepads) = &client.gamepads {
        gamepads.set_active(focused && permissions.allows(DeviceClass::Gamepad));
    }
    let remote_pointer = pointer.filter(|p| {
        focused && !client.input.captured && video_rect.contains(*p) && !blocked.is_some_and(|rect| rect.contains(*p))
    });
    let (x, y) = remote_pointer.map_or((0, 0), |p| normalise(p, video_rect));
    send_pointer(client, (x, y, remote_pointer.is_some()));
    ctx.request_repaint_after(POINTER_REPEAT);
    if let Some(frame) = frame {
        client.video.set_frame(&ctx, frame);
    }
    client.video.recycle(&client.handle);
    ui.painter().rect_filled(area, 0.0, Color32::BLACK);
    if client.video.size().is_some() {
        client.video.paint(ui.painter(), video_rect);
    } else {
        ui.painter().text(
            area.center(),
            egui::Align2::CENTER_CENTER,
            "Waiting for video…",
            egui::FontId::proportional(18.0),
            Color32::from_gray(160),
        );
    }
    draw_host_cursor(client, ui, video_rect, image_size);
    ui.allocate_rect(area, Sense::hover());
    if client.show_stats {
        let galley = ui.painter().layout(
            stats_line(&client.handle.stats()),
            egui::FontId::monospace(12.0),
            Color32::WHITE,
            area.width() - 36.0,
        );
        let rect =
            Rect::from_min_size(egui::pos2(area.left() + 12.0, area.bottom() - galley.size().y - 20.0), galley.size())
                .expand(6.0);
        ui.painter().rect_filled(rect, 6.0, Color32::from_rgb(24, 26, 34));
        ui.painter().galley(rect.min + egui::vec2(6.0, 6.0), galley, Color32::WHITE);
    }
    if let Some((text, until)) = &client.note {
        if Instant::now() < *until {
            let galley = ui.painter().layout(
                text.clone(),
                egui::FontId::proportional(14.0),
                Color32::WHITE,
                (area.width() - 48.0).min(560.0),
            );
            let rect = Rect::from_center_size(
                egui::pos2(area.center().x, area.top() + 20.0 + galley.size().y / 2.0),
                galley.size() + egui::vec2(24.0, 16.0),
            );
            ui.painter().rect_filled(rect, 8.0, Color32::from_rgb(24, 26, 34));
            ui.painter().galley(rect.min + egui::vec2(12.0, 8.0), galley, Color32::WHITE);
        } else {
            client.note = None;
        }
    }
    if client.input.captured {
        return;
    }
    if !client.shelf_open {
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(handle));
        if child
            .add_sized(handle.size(), egui::Button::new("›").fill(Color32::from_rgb(24, 26, 34)))
            .on_hover_text("Open session controls")
            .clicked()
        {
            client.shelf_open = true;
        }
        return;
    }
    ui.painter().rect_filled(shelf, 12.0, Color32::from_rgb(24, 26, 34));
    let mut controls = ui.new_child(egui::UiBuilder::new().max_rect(shelf.shrink(12.0)));
    controls.spacing_mut().item_spacing.y = 6.0;
    controls.horizontal(|ui| {
        ui.label(RichText::new("Session").strong());
        if ui.button("‹").on_hover_text("Collapse controls").clicked() {
            client.shelf_open = false;
        }
    });
    controls.add(egui::Label::new(RichText::new(&host_name).strong().size(14.0)).truncate()).on_hover_text(&host_name);
    controls.label(RichText::new(client.handle.stats().path.label()).color(theme::ACCENT).size(12.0));
    controls
        .add(egui::Label::new(RichText::new(&client.encoder).weak().size(11.0)).truncate())
        .on_hover_text(&client.encoder);
    controls.separator();
    let size = egui::vec2(SHELF_WIDTH - 24.0, 29.0);
    let label = if fullscreen { "Exit fullscreen" } else { "Enter fullscreen" };
    if controls.add_sized(size, egui::Button::new(label)).on_hover_text("Ctrl+Alt+F").clicked() {
        ctx.send_viewport_cmd(ViewportCommand::Fullscreen(!fullscreen));
    }
    let capture = controls
        .add_enabled_ui(permissions.allows(DeviceClass::Mouse), |ui| {
            ui.add_sized(size, egui::Button::new("Capture mouse")).on_hover_text("Ctrl+Alt+M releases the mouse")
        })
        .inner
        .clicked();
    let audio_label = if client.audio_muted { "Unmute audio" } else { "Mute audio" };
    if controls
        .add_enabled_ui(client.audio_active, |ui| ui.add_sized(size, egui::Button::new(audio_label)))
        .inner
        .clicked()
    {
        client.audio_muted = !client.audio_muted;
        client.handle.set_audio_muted(client.audio_muted);
    }
    if controls
        .add_sized(size, egui::Button::new(if client.show_stats { "Hide stats" } else { "Show stats" }))
        .on_hover_text("Ctrl+Alt+S")
        .clicked()
    {
        client.show_stats = !client.show_stats;
    }
    controls.separator();
    if controls.add_sized(size, theme::primary("Disconnect")).clicked() {
        client.handle.disconnect();
    }
    if !permissions.allows(DeviceClass::Audio) {
        controls.label(RichText::new("Host audio is off").weak().size(11.0));
    }
    if capture {
        set_captured(&ctx, client, true);
    }
}

fn draw_host_cursor(client: &mut ClientSession, ui: &egui::Ui, video: Rect, image_size: Option<egui::Vec2>) {
    let cursor = client.handle.cursor();
    let Some(shape) = cursor.shape else { return };
    if client.host_cursor_texture.as_ref().map(|(id, _)| *id) != Some(shape.id) {
        let image =
            egui::ColorImage::from_rgba_unmultiplied([shape.width as usize, shape.height as usize], &shape.rgba);
        client.host_cursor_texture =
            Some((shape.id, ui.ctx().load_texture("host-cursor", image, TextureOptions::LINEAR)));
    }
    if !cursor.position.visible {
        return;
    }
    let scale = image_size.map_or(1.0, |size| video.width() / size.x);
    let position = video.min
        + egui::vec2(
            cursor.position.x as f32 / 65535.0 * video.width(),
            cursor.position.y as f32 / 65535.0 * video.height(),
        );
    let rect = Rect::from_min_size(
        position - egui::vec2(shape.hotspot_x as f32, shape.hotspot_y as f32) * scale,
        egui::vec2(shape.width as f32, shape.height as f32) * scale,
    );
    if let Some((_, texture)) = &client.host_cursor_texture {
        ui.painter().with_clip_rect(video).image(
            texture.id(),
            rect,
            Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            Color32::WHITE,
        );
    }
}

/// Tells the host where this user's pointer is over the picture, when it moved, and now and then
/// regardless so the host knows it's still current.
fn send_pointer(client: &mut ClientSession, position: (u16, u16, bool)) {
    if client.pointer_sent.is_some_and(|(sent, at)| sent == position && at.elapsed() < POINTER_REPEAT) {
        return;
    }
    client.pointer_sent = Some((position, Instant::now()));
    client.handle.send_cursor(position.0, position.1, position.2);
}

pub(super) fn set_captured(ctx: &egui::Context, client: &mut ClientSession, captured: bool) {
    if client.input.captured == captured {
        return;
    }
    client.input.captured = captured;
    ctx.send_viewport_cmd(ViewportCommand::CursorGrab(if captured { CursorGrab::Confined } else { CursorGrab::None }));
    ctx.send_viewport_cmd(ViewportCommand::CursorVisible(!captured));
    if captured {
        send_pointer(client, (0, 0, false));
        client.note("Mouse captured. Ctrl+Alt+M releases it.", 4);
    }
}

fn shelf_rect(area: Rect) -> Rect {
    Rect::from_min_size(
        egui::pos2(area.left() + 8.0, (area.center().y - SHELF_HEIGHT / 2.0).max(area.top() + 8.0)),
        egui::vec2(SHELF_WIDTH, SHELF_HEIGHT.min(area.height() - 16.0)),
    )
}

fn fit(size: egui::Vec2, area: Rect) -> Rect {
    Rect::from_center_size(area.center(), size * (area.width() / size.x).min(area.height() / size.y))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shelf_is_compact_and_inside_minimum_viewport() {
        let area = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(760.0, 520.0));
        let shelf = shelf_rect(area);
        assert!(area.contains_rect(shelf));
        assert!(shelf.width() < area.width() / 3.0);
        assert!(shelf.height() < area.height());
        assert_eq!(shelf.left(), 8.0);
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "opens a native localhost session and saves a rendered shelf preview"]
    fn native_session_shelf_probe() {
        use connect_host::{HostConfig, InputSink, VideoSource};
        use connect_net::{self as net, iroh::SecretKey};
        use connect_proto::{Permissions, input::InputEvent};
        use winit::platform::windows::EventLoopBuilderExtWindows;

        struct Probe {
            app: App,
            injected: tokio::sync::mpsc::UnboundedReceiver<InputEvent>,
            deadline: Instant,
            screenshot_requested: bool,
            screenshot: std::sync::Arc<std::sync::Mutex<Option<std::sync::Arc<egui::ColorImage>>>>,
        }
        impl eframe::App for Probe {
            fn logic(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
                self.app.poll_client(ctx);
                while let Ok(event) = self.injected.try_recv() {
                    assert!(!matches!(event, InputEvent::MouseButton { .. }), "shelf click reached host: {event:?}");
                }
                if let Some(client) = &mut self.app.client {
                    client.note = None;
                    if client.video.size().is_some()
                        && !self.screenshot_requested
                        && client.started.elapsed() > Duration::from_secs(1)
                    {
                        // Exercise the actual input path with shelf clicks. These must remain local.
                        let video = ctx.viewport_rect();
                        let shelf = shelf_rect(video);
                        for pos in [shelf.center(), shelf.min + egui::vec2(2.0, 2.0), shelf.max - egui::vec2(2.0, 2.0)]
                        {
                            let events = [true, false].map(|pressed| egui::Event::PointerButton {
                                pos,
                                button: egui::PointerButton::Primary,
                                pressed,
                                modifiers: Default::default(),
                            });
                            client.input.process(
                                &events,
                                video,
                                Some(shelf),
                                Some(pos),
                                Permissions::ALL,
                                &client.handle,
                            );
                        }
                        self.screenshot_requested = true;
                        let screenshot = self.screenshot.clone();
                        ctx.request_screenshot(move |image| {
                            *screenshot.lock().unwrap() = Some(image);
                        });
                    }
                }
                if Instant::now() >= self.deadline || self.screenshot.lock().unwrap().is_some() {
                    ctx.send_viewport_cmd(ViewportCommand::Close);
                }
            }
            fn ui(&mut self, ui: &mut egui::Ui, _: &mut eframe::Frame) {
                self.app.ui(ui);
            }
            fn on_exit(&mut self, _: Option<&eframe::glow::Context>) {
                if let Some(client) = &self.app.client {
                    client.handle.disconnect();
                }
            }
        }
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _guard = runtime.enter();
        let host_ep = runtime.block_on(net::bind_local_endpoint(SecretKey::generate())).unwrap();
        let client_ep = runtime.block_on(net::bind_local_endpoint(SecretKey::generate())).unwrap();
        let (sink, injected) = tokio::sync::mpsc::unbounded_channel();
        let (host, _events) = connect_host::start(
            host_ep.clone(),
            HostConfig {
                video: VideoSource::TestPattern { width: 1280, height: 720, fps: 60 },
                input: InputSink::Channel(sink),
                default_permissions: Permissions::ALL.with(DeviceClass::Audio, false),
                trusted: [client_ep.id()].into_iter().collect(),
                ..HostConfig::new("Session controls preview")
            },
        );
        let options = eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_title("Connect session UI probe")
                .with_inner_size([980.0, 640.0])
                .with_min_inner_size([760.0, 520.0]),
            event_loop_builder: Some(Box::new(|builder| {
                builder.with_any_thread(true);
            })),
            ..Default::default()
        };
        let screenshot = std::sync::Arc::new(std::sync::Mutex::new(None));
        let captured = screenshot.clone();
        eframe::run_native(
            "Connect session UI probe",
            options,
            Box::new(|cc| {
                theme::apply(&cc.egui_ctx);
                let mut app = App::new(
                    &cc.egui_ctx,
                    client_ep.clone(),
                    super::super::settings::Settings { accepting: false, ..Default::default() },
                    Default::default(),
                );
                let ctx = cc.egui_ctx.clone();
                let (handle, events) =
                    connect_client::connect(client_ep.clone(), net::local_addr(&host_ep), "Shelf probe", move || {
                        ctx.request_repaint()
                    });
                app.client = Some(ClientSession {
                    handle,
                    events,
                    host_id: host_ep.id(),
                    started: Instant::now(),
                    state: ClientState::Connecting,
                    video: Default::default(),
                    pointer_sent: None,
                    host_cursor_texture: None,
                    encoder: "Starting encoder…".into(),
                    audio_active: false,
                    audio_muted: true,
                    shelf_open: true,
                    show_stats: false,
                    input: Default::default(),
                    gamepads: None,
                    note: None,
                });
                Ok(Box::new(Probe {
                    app,
                    injected,
                    deadline: Instant::now() + Duration::from_secs(20),
                    screenshot_requested: false,
                    screenshot: captured,
                }))
            }),
        )
        .unwrap();
        runtime.block_on(host.shutdown());
        runtime.block_on(host_ep.close());
        runtime.block_on(client_ep.close());
        let image = screenshot.lock().unwrap().take().expect("native renderer returned a screenshot");
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../.tools/session-shelf.png");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let rgba: Vec<u8> = image.pixels.iter().flat_map(|pixel| pixel.to_array()).collect();
        image::save_buffer(&path, &rgba, image.width() as u32, image.height() as u32, image::ColorType::Rgba8).unwrap();
        println!("Rendered native shelf saved to {}; shelf clicks stayed local", path.display());
    }
}
