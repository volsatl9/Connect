//! The Allow / Deny prompt for incoming connections, where the host also picks what the other
//! person may use.

use connect_host::Decision;
use connect_net as net;
use connect_proto::{DeviceClass, Permissions};
use eframe::egui::{self, Align, Layout, RichText};

use super::{App, theme};

pub fn show(app: &mut App, ctx: &egui::Context) {
    let timeout = connect_proto::APPROVAL_TIMEOUT;
    let driver = &mut app.gamepad_driver;
    let Some(request) = app.requests.front_mut() else { return };
    let remaining = timeout.saturating_sub(request.arrived.elapsed()).as_secs();

    let mut decision = None;
    egui::Modal::new(egui::Id::unique("connection-request")).frame(theme::card()).show(ctx, |ui| {
        ui.set_width(460.0);
        ui.label(RichText::new("Connection request").size(20.0).strong());
        ui.add_space(4.0);
        ui.label(
            RichText::new(format!(
                "{} wants to connect to this PC and see your screen. Desktop sound is shared only when enabled below.",
                request.inner.client_name
            ))
            .size(15.0),
        );
        ui.add_space(4.0);
        ui.label(RichText::new("User-ID").weak().small());
        ui.label(RichText::new(net::format_user_id(&request.inner.peer)).monospace());
        ui.label(
            RichText::new("The User-ID is verified. The name is whatever they chose, so check the ID if unsure.")
                .weak()
                .small(),
        );

        ui.add_space(8.0);
        ui.label(RichText::new("Share and allow").strong());
        permission_toggles(ui, &mut request.permissions);
        if request.permissions.allows(DeviceClass::Gamepad) {
            driver.setup_row(ui);
        }
        ui.label(RichText::new("You can change these at any time during the session.").weak().small());

        ui.add_space(6.0);
        ui.checkbox(&mut request.always_allow, "Always allow this device");
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("Denying automatically in {remaining} s")).weak());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add(theme::primary("Allow")).clicked() {
                    decision = Some(Decision::Allow(request.permissions));
                }
                if ui.button("Deny").clicked() {
                    decision = Some(Decision::Deny);
                }
            });
        });
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

/// A checkbox per input device, plus a "View only" shortcut. True if anything changed.
pub fn permission_toggles(ui: &mut egui::Ui, permissions: &mut Permissions) -> bool {
    let before = *permissions;
    ui.horizontal_wrapped(|ui| {
        for device in DeviceClass::ALL {
            let mut allowed = permissions.allows(device);
            if ui.checkbox(&mut allowed, device.label()).changed() {
                *permissions = permissions.with(device, allowed);
            }
        }
        ui.add_space(12.0);
        if ui.add_enabled(!permissions.is_view_only(), egui::Button::new("View only")).clicked() {
            *permissions = Permissions::VIEW_ONLY;
        }
    });
    *permissions != before
}
