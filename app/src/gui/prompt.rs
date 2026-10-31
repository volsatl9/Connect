//! The Allow / Deny prompt for incoming connections, where the host also picks what the other
//! person may use.

use connect_host::Decision;
use connect_net as net;
use connect_proto::{DeviceClass, Permissions};
use eframe::egui::{self, Align, Layout, RichText};

use super::{App, icons, theme};

pub fn show(app: &mut App, ctx: &egui::Context) {
    let timeout = connect_proto::APPROVAL_TIMEOUT;
    let driver = &mut app.gamepad_driver;
    let Some(request) = app.requests.front_mut() else { return };
    let remaining = timeout.saturating_sub(request.arrived.elapsed()).as_secs();

    let (decision, _) = theme::dialog(ctx, "connection-request", 460.0, |ui| {
        ui.label(RichText::new("Connection request").font(theme::semibold(20.0)));
        ui.add_space(2.0);
        ui.label(
            RichText::new(format!("{} wants to connect to this PC and see your screen.", request.inner.client_name))
                .size(15.0),
        );
        ui.add_space(6.0);
        theme::glass(ui, theme::Glass::PANEL, 12.0, egui::Margin::symmetric(12, 8), |ui| {
            ui.set_width(ui.available_width());
            ui.label(theme::caption("Their User-ID").size(11.5));
            ui.label(RichText::new(net::format_user_id(&request.inner.peer)).monospace().color(theme::TEXT));
        });
        ui.label(
            theme::caption("The User-ID is verified. The name is whatever they chose, so check the ID if unsure.")
                .size(11.5),
        );

        ui.add_space(10.0);
        theme::section_title(ui, "Let them use");
        permission_toggles(ui, &mut request.permissions);
        if request.permissions.allows(DeviceClass::Gamepad) {
            driver.setup_row(ui);
        }
        ui.label(
            theme::caption("Desktop audio is shared only if selected. You can change all of these during the session.")
                .size(11.5),
        );

        ui.add_space(6.0);
        theme::switch_row(ui, "Always allow this device", &mut request.always_allow);
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.label(theme::caption(format!("Denying automatically in {remaining} s")));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add(theme::primary("Allow")).clicked() {
                    return Some(Decision::Allow(request.permissions));
                }
                ui.add(theme::secondary("Deny")).clicked().then_some(Decision::Deny)
            })
            .inner
        })
        .inner
    });

    if let Some(decision) = decision {
        let request = app.requests.pop_front().expect("shown above");
        if matches!(decision, Decision::Allow(_)) && request.always_allow {
            app.settings.trust(request.inner.peer, &request.inner.client_name);
            app.host.set_trusted(request.inner.peer, true);
        }
        request.inner.respond(decision);
    }
}

/// A chip per input device, plus a "View only" shortcut. True if anything changed.
pub fn permission_toggles(ui: &mut egui::Ui, permissions: &mut Permissions) -> bool {
    let before = *permissions;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
        for device in DeviceClass::ALL {
            let allowed = permissions.allows(device);
            if theme::chip(ui, Some(icon(device)), device.label(), allowed).clicked() {
                *permissions = permissions.with(device, !allowed);
            }
        }
        if ui.add_enabled(!permissions.is_view_only(), theme::secondary("View only").small()).clicked() {
            *permissions = Permissions::VIEW_ONLY;
        }
    });
    *permissions != before
}

fn icon(device: DeviceClass) -> &'static str {
    match device {
        DeviceClass::Mouse => icons::MOUSE,
        DeviceClass::Keyboard => icons::KEYBOARD,
        DeviceClass::Gamepad => icons::GAME_CONTROLLER,
        DeviceClass::Audio => icons::SPEAKER_HIGH,
    }
}
