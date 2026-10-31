//! First run: name this PC and generate the account (User-ID) before anything else.

use std::path::PathBuf;

use connect_host::VideoSource;
use connect_net::{self as net, iroh::Endpoint};
use eframe::egui::{self, RichText, TextEdit};
use tokio::runtime::Handle;

use super::{icons, settings::Settings, theme};

pub struct Welcome {
    runtime: Handle,
    config_dir: PathBuf,
    pub settings: Settings,
    /// Set once the account exists, for the screen that says so.
    created: Option<Endpoint>,
    /// Install the built-in gamepad driver along with the account (Windows only).
    setup_gamepads: bool,
    error: Option<String>,
}

impl Welcome {
    pub fn new(runtime: Handle, config_dir: PathBuf, settings: Settings) -> Self {
        Self {
            runtime,
            config_dir,
            settings,
            created: None,
            setup_gamepads: VideoSource::SCREEN_SUPPORTED && connect_host::gamepad_support().is_err(),
            error: None,
        }
    }

    /// Once the user carries on into the app: the new account's endpoint, the settings, and
    /// whether to set up gamepad support.
    pub fn show(&mut self, ui: &mut egui::Ui) -> Option<(Endpoint, Settings, bool)> {
        let mut done = false;
        theme::backdrop(ui.painter(), ui.max_rect());
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space((ui.available_height() * 0.1).clamp(24.0, 72.0));
                ui.set_max_width(540.0);
                match &self.created {
                    None => self.ask(ui),
                    Some(endpoint) => done = created(ui, endpoint),
                }
                ui.add_space(24.0);
            });
        });
        if done {
            let endpoint = self.created.take().expect("shown above");
            return Some((endpoint, std::mem::take(&mut self.settings), self.setup_gamepads));
        }
        None
    }

    fn ask(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("Welcome to Connect").font(theme::semibold(32.0)).extra_letter_spacing(-0.5));
        ui.add_space(2.0);
        ui.label(
            RichText::new("Play on another PC as if you were sitting at it, or let a friend play on yours.")
                .size(15.0)
                .color(theme::DIM),
        );
        ui.add_space(20.0);

        theme::card(ui, |ui| {
            theme::section_title(ui, "Name this PC");
            ui.label(theme::caption("Shown to the people you connect with, and on their Connect page."));
            ui.add(
                TextEdit::singleline(&mut self.settings.device_name)
                    .desired_width(f32::INFINITY)
                    .margin(egui::vec2(12.0, 8.0)),
            );
        });
        ui.add_space(10.0);
        theme::card(ui, |ui| {
            theme::section_title(ui, "Your account");
            ui.label(
                "Generating an account creates your User-ID: a key that lives only on this PC. \
                 There's no email or password.",
            );
            ui.add_space(4.0);
            ui.label(theme::caption(
                "Nobody can connect without your approval, and you choose what they may use: mouse, keyboard, \
                 gamepad.",
            ));
        });
        if VideoSource::SCREEN_SUPPORTED && connect_host::gamepad_support().is_err() {
            ui.add_space(10.0);
            theme::card(ui, |ui| {
                theme::switch_row(ui, "Set up gamepad support", &mut self.setup_gamepads);
                ui.label(
                    theme::caption(
                        "Lets people who connect use their controller in your games. It's built in; Windows asks \
                         for permission once.",
                    )
                    .size(11.5),
                );
            });
        }
        ui.add_space(18.0);

        let named = !self.settings.device_name.trim().is_empty();
        let button = theme::primary("Generate Account").width(240.0);
        if ui.add_enabled(named, button).clicked() {
            self.settings.device_name = self.settings.device_name.trim().to_owned();
            match self.generate() {
                Ok(endpoint) => self.created = Some(endpoint),
                Err(e) => self.error = Some(format!("Couldn't create the account: {e:#}")),
            }
        }
        if let Some(error) = &self.error {
            ui.add_space(6.0);
            ui.label(RichText::new(error).color(theme::BAD));
        }
    }

    fn generate(&self) -> anyhow::Result<Endpoint> {
        let key = net::create_identity(&self.config_dir)?;
        self.runtime.block_on(net::bind_endpoint(key))
    }
}

/// Says the account is ready and where its User-ID lives. True when the user continues.
fn created(ui: &mut egui::Ui, endpoint: &Endpoint) -> bool {
    ui.label(RichText::new("Your account is ready").font(theme::semibold(32.0)).extra_letter_spacing(-0.5));
    ui.add_space(20.0);
    theme::card(ui, |ui| {
        theme::section_title(ui, "Your User-ID");
        ui.label("It's in Settings, hidden until you click Show, so it doesn't end up on stream or in a screenshot.");
        let how = if VideoSource::SCREEN_SUPPORTED {
            "Give it to whoever should connect to this PC. To play on someone else's PC, click + Connect and paste \
             their User-ID."
        } else {
            "To play on someone else's PC, click + Connect and paste their User-ID. (This PC can connect to others; \
             hosting needs Windows.)"
        };
        ui.label(theme::caption(how));
        ui.add_space(6.0);
        if ui.add(theme::secondary("Copy User-ID").icon(icons::COPY)).clicked() {
            ui.ctx().copy_text(endpoint.id().to_string());
        }
    });
    ui.add_space(18.0);
    ui.add(theme::primary("Continue").width(240.0)).clicked()
}
