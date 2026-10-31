//! Setting up gamepad support on a Windows host: the driver ships inside the app and installs
//! with one click (and Windows' permission prompt).

use std::sync::mpsc::{Receiver, TryRecvError};

use connect_host::VideoSource;
use eframe::egui::{self, RichText};

use super::theme;

pub struct GamepadDriver {
    /// `Err` says why the host can't give a remote user a gamepad yet.
    status: Result<(), String>,
    installing: Option<Receiver<Result<(), String>>>,
    /// Why the last install attempt failed.
    error: Option<String>,
}

impl GamepadDriver {
    pub fn new() -> Self {
        let status = if VideoSource::SCREEN_SUPPORTED { connect_host::gamepad_support() } else { Ok(()) };
        Self { status, installing: None, error: None }
    }

    pub fn ready(&self) -> bool {
        self.status.is_ok()
    }

    /// Installs in the background; Windows asks for permission first.
    pub fn install(&mut self, ctx: &egui::Context) {
        if self.ready() || self.installing.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(connect_host::install_gamepad_driver());
            ctx.request_repaint();
        });
        self.installing = Some(rx);
        self.error = None;
    }

    pub fn poll(&mut self) {
        let Some(rx) = &self.installing else { return };
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err("The gamepad setup stopped unexpectedly.".into()),
        };
        self.installing = None;
        self.status = connect_host::gamepad_support();
        self.error = result.err();
    }

    /// A line saying gamepads aren't set up yet, with the button that does it. Nothing once ready.
    pub fn setup_row(&mut self, ui: &mut egui::Ui) {
        if self.ready() {
            return;
        }
        let mut install = false;
        ui.horizontal_wrapped(|ui| {
            if self.installing.is_some() {
                ui.spinner();
                ui.label(RichText::new("Setting up gamepad support… approve Windows' prompt if it asks.").small());
            } else {
                ui.label(RichText::new("Gamepads need a one-time setup on this PC.").color(theme::BAD).small());
                install = ui.add(theme::secondary("Set up gamepad support").small()).clicked();
            }
        });
        if let Some(error) = &self.error {
            ui.label(RichText::new(error).color(theme::BAD).small());
        }
        if install {
            self.install(ui.ctx());
        }
    }
}
