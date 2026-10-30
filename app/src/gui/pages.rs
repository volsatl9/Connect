//! The sidebar and the Share, Connect and Settings pages.

use std::sync::atomic::Ordering;

use connect_host::VideoSource;
use connect_net as net;
use connect_proto::DeviceClass;
use eframe::egui::{self, Align, Color32, Layout, RichText, TextEdit};

use super::{App, ClientState, Page, prompt::permission_toggles, settings::BITRATE_CHOICES_MBPS, theme};

pub fn nav(app: &mut App, ui: &mut egui::Ui) {
    ui.label(RichText::new("Connect").size(22.0).strong().color(theme::ACCENT));
    ui.add_space(18.0);
    for (page, label) in [(Page::Share, "Share this PC"), (Page::Connect, "Connect"), (Page::Settings, "Settings")] {
        if page == Page::Share && !VideoSource::SCREEN_SUPPORTED {
            continue;
        }
        let button = egui::Button::selectable(app.page == page, RichText::new(label).size(15.0))
            .min_size(egui::vec2(ui.available_width(), 36.0));
        if ui.add(button).clicked() {
            app.page = page;
        }
    }

    ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
        let (color, text) = if app.online.load(Ordering::Relaxed) {
            (theme::GOOD, "Online")
        } else {
            (Color32::from_gray(140), "Going online…")
        };
        ui.label(RichText::new(format!("● {text}")).color(color));
        if let Some(hosting) = &app.hosting {
            ui.label(RichText::new(format!("{} is viewing this PC", hosting.client_name)).color(theme::ACCENT));
        }
    });
}

fn notice(app: &App, ui: &mut egui::Ui) {
    if let Some(n) = &app.notice {
        let color = if n.is_error { theme::BAD } else { Color32::from_gray(200) };
        ui.label(RichText::new(&n.text).color(color));
    }
}

pub fn share(app: &mut App, ui: &mut egui::Ui) {
    theme::page_title(
        ui,
        "Share this PC",
        "Give your User-ID to the person who should connect. You'll be asked to allow them.",
    );

    theme::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        theme::section_title(ui, "Your User-ID");
        let id = net::format_user_id(&app.endpoint.id());
        // Two rows of four groups reads better than one long line.
        let groups: Vec<&str> = id.split(' ').collect();
        for row in groups.chunks(4) {
            ui.label(RichText::new(row.join(" ")).monospace().size(20.0));
        }
        ui.horizontal(|ui| {
            if ui.add(theme::primary("Copy User-ID")).clicked() {
                ui.ctx().copy_text(app.endpoint.id().to_string());
                app.notify("User-ID copied.", false);
            }
            notice(app, ui);
        });
    });
    ui.add_space(6.0);

    theme::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        if let Some(hosting) = &mut app.hosting {
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{} is connected", hosting.client_name)).strong().color(theme::GOOD));
                ui.label(RichText::new(format!("for {}", elapsed(hosting.since))).weak());
                ui.label(RichText::new(short_id(&hosting.peer.to_string())).monospace().weak());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("Disconnect").clicked() {
                        app.host.disconnect();
                    }
                });
            });
            ui.label("They can use:");
            if permission_toggles(ui, &mut hosting.permissions) {
                app.host.set_permissions(hosting.permissions);
            }
            if hosting.permissions.allows(DeviceClass::Gamepad) {
                app.gamepad_driver.setup_row(ui);
            }
            ui.separator();
        }

        let mut accepting = app.settings.accepting;
        if ui.checkbox(&mut accepting, "Allow incoming connections").changed() {
            app.settings.accepting = accepting;
            app.host.set_accepting(accepting);
        }
    });
    ui.add_space(6.0);

    theme::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        theme::section_title(ui, "Allowed by default");
        ui.label(
            RichText::new("Pre-selected when someone asks to connect, and what \"always allowed\" devices get.").weak(),
        );
        if permission_toggles(ui, &mut app.settings.permissions) {
            app.host.set_default_permissions(app.settings.permissions);
        }
        if app.settings.permissions.allows(DeviceClass::Gamepad) {
            app.gamepad_driver.setup_row(ui);
        }
        ui.label(
            RichText::new(
                "Windows keeps input away from programs running as administrator unless Connect runs as \
                 administrator too.",
            )
            .weak()
            .small(),
        );
    });
    ui.add_space(6.0);

    theme::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        theme::section_title(ui, "Always allowed");
        if app.settings.trusted.is_empty() {
            ui.label(RichText::new("Devices you tick \"Always allow\" for connect without asking. None yet.").weak());
        }
        let mut remove = None;
        for (i, device) in app.settings.trusted.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.label(RichText::new(&device.name).strong());
                ui.label(RichText::new(short_id(&device.id)).monospace().weak());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("Remove").clicked() {
                        remove = Some(i);
                    }
                });
            });
        }
        if let Some(i) = remove {
            let device = app.settings.trusted.remove(i);
            if let Some(id) = device.user_id() {
                app.host.set_trusted(id, false);
            }
        }
    });
}

pub fn connect(app: &mut App, ui: &mut egui::Ui) {
    theme::page_title(ui, "Connect to a PC", "Enter the User-ID the host gave you. They'll be asked to allow you in.");

    theme::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        if let Some(client) = &app.client {
            let host = short_id(&client.host_id.to_string());
            ui.horizontal(|ui| {
                ui.spinner();
                let text = match client.state {
                    ClientState::Connecting => format!("Looking up {host}…"),
                    ClientState::WaitingForApproval => format!("Waiting for {host} to allow the connection…"),
                    ClientState::Connected { .. } => "Connected.".into(),
                };
                ui.label(RichText::new(text).strong());
                ui.label(RichText::new(elapsed(client.started)).weak());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("Cancel").clicked() {
                        client.handle.disconnect();
                    }
                });
            });
            return;
        }

        theme::section_title(ui, "Host's User-ID");
        let parsed = net::parse_user_id(&app.connect_input);
        let mut submit = false;
        ui.horizontal(|ui| {
            let field = TextEdit::singleline(&mut app.connect_input)
                .hint_text("Paste the host's User-ID")
                .font(egui::TextStyle::Monospace)
                .desired_width(ui.available_width() - 120.0);
            let response = ui.add(field);
            submit |= response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            submit |= ui.add_enabled(parsed.is_ok(), theme::primary("Connect")).clicked();
        });
        match &parsed {
            Err(e) if !app.connect_input.trim().is_empty() => {
                ui.label(RichText::new(e.to_string()).color(theme::BAD));
            }
            _ => notice(app, ui),
        }
        if submit && let Ok(id) = parsed {
            app.start_connect(id, ui.ctx());
        }
    });
    ui.add_space(6.0);

    theme::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        theme::section_title(ui, "Recent");
        if app.settings.recent.is_empty() {
            ui.label(RichText::new("Hosts you connect to show up here for one-click reconnects.").weak());
        }
        let (mut connect_to, mut remove) = (None, None);
        for (i, host) in app.settings.recent.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.label(RichText::new(&host.name).strong());
                ui.label(RichText::new(short_id(&host.id)).monospace().weak());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("✕").on_hover_text("Forget").clicked() {
                        remove = Some(i);
                    }
                    if ui.add_enabled(app.client.is_none(), theme::primary("Connect")).clicked() {
                        connect_to = host.user_id();
                    }
                });
            });
        }
        if let Some(i) = remove {
            app.settings.recent.remove(i);
        }
        if let Some(id) = connect_to {
            app.start_connect(id, ui.ctx());
        }
    });
}

pub fn settings(app: &mut App, ui: &mut egui::Ui) {
    theme::page_title(ui, "Settings", "");

    theme::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        theme::section_title(ui, "Device name");
        ui.label(RichText::new("Shown to the other person when you connect or they connect to you.").weak());
        let response = ui.add(TextEdit::singleline(&mut app.settings.device_name).desired_width(320.0));
        if response.changed() {
            app.host.set_host_name(app.settings.device_name.trim());
        }
    });
    ui.add_space(6.0);

    if VideoSource::SCREEN_SUPPORTED {
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            theme::section_title(ui, "Stream quality (when sharing this PC)");
            ui.label(
                RichText::new(
                    "The most video data per second this PC sends. Higher looks sharper, especially at 120+ fps, \
                     but needs a faster upload here. Applies from the next connection.",
                )
                .weak(),
            );
            ui.horizontal(|ui| {
                for mbps in BITRATE_CHOICES_MBPS {
                    if ui.selectable_label(app.settings.bitrate_mbps == mbps, format!("{mbps} Mb/s")).clicked() {
                        app.settings.bitrate_mbps = mbps;
                        app.host.set_bitrate_mbps(mbps);
                    }
                }
            });
        });
        ui.add_space(6.0);
    }

    app.updater.settings_card(ui);
    ui.add_space(6.0);

    theme::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        theme::section_title(ui, "Account");
        ui.label(RichText::new(net::format_user_id(&app.endpoint.id())).monospace());
        if ui.button("Copy User-ID").clicked() {
            ui.ctx().copy_text(app.endpoint.id().to_string());
        }
    });
}

/// The first and last groups of a User-ID, enough to tell devices apart at a glance.
fn short_id(id: &str) -> String {
    if id.len() <= 16 { id.to_owned() } else { format!("{}…{}", &id[..8], &id[id.len() - 8..]) }
}

fn elapsed(since: std::time::Instant) -> String {
    let s = since.elapsed().as_secs();
    if s < 60 { format!("{s} s") } else { format!("{}:{:02}", s / 60, s % 60) }
}
