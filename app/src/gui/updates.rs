//! "Update available" banner, the Settings card, and the background checks behind them.

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};

use eframe::egui::{self, Align, Color32, Layout, RichText};
use tokio::sync::mpsc;
use tracing::warn;

use super::theme;
use crate::update::{self, Update};

/// First check shortly after launch, then this often while the app stays open.
const FIRST_CHECK: Duration = Duration::from_secs(3);
const CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);

enum State {
    Idle,
    Checking,
    UpToDate,
    Available(Update),
    Downloading { update: Update, progress: Arc<AtomicU32> },
    Failed { message: String, update: Option<Update> },
}

enum Msg {
    Checked { result: Result<Option<Update>, String>, manual: bool },
    Downloaded { result: Result<PathBuf, String>, update: Update },
}

pub struct Updater {
    state: State,
    /// "Later" was clicked for this version; don't show the banner again this run.
    dismissed: Option<String>,
    next_check: Instant,
    tx: mpsc::UnboundedSender<Msg>,
    rx: mpsc::UnboundedReceiver<Msg>,
    ctx: egui::Context,
}

impl Updater {
    pub fn new(ctx: &egui::Context) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self { state: State::Idle, dismissed: None, next_check: Instant::now() + FIRST_CHECK, tx, rx, ctx: ctx.clone() }
    }

    /// Runs scheduled checks and takes in results. Returns the executable to restart into once
    /// an update has been installed.
    pub fn poll(&mut self) -> Option<PathBuf> {
        if Instant::now() >= self.next_check && !self.busy() {
            self.check(false);
        }
        self.ctx.request_repaint_after(self.next_check.saturating_duration_since(Instant::now()));

        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Checked { result, manual } => {
                    self.state = match result {
                        Ok(Some(update)) => State::Available(update),
                        Ok(None) => State::UpToDate,
                        Err(message) if manual => State::Failed { message, update: None },
                        Err(message) => {
                            warn!("update check failed: {message}");
                            State::Idle
                        }
                    }
                }
                Msg::Downloaded { result: Ok(staged), update } => match update::install(&staged) {
                    Ok(exe) => return Some(exe),
                    Err(e) => self.state = State::Failed { message: format!("{e:#}"), update: Some(update) },
                },
                Msg::Downloaded { result: Err(message), update } => {
                    self.state = State::Failed { message, update: Some(update) }
                }
            }
        }
        if matches!(self.state, State::Downloading { .. }) {
            self.ctx.request_repaint_after(Duration::from_millis(100));
        }
        None
    }

    fn busy(&self) -> bool {
        matches!(self.state, State::Checking | State::Downloading { .. })
    }

    fn check(&mut self, manual: bool) {
        self.next_check = Instant::now() + CHECK_EVERY;
        if matches!(self.state, State::Downloading { .. }) {
            return;
        }
        self.state = State::Checking;
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        tokio::spawn(async move {
            let result = update::check().await.map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Checked { result, manual });
            ctx.request_repaint();
        });
    }

    fn start_download(&mut self, update: Update) {
        if let Err(e) = update::check_writable() {
            self.state = State::Failed { message: format!("{e:#}"), update: Some(update) };
            return;
        }
        let progress = Arc::new(AtomicU32::new(0));
        self.state = State::Downloading { update: update.clone(), progress: progress.clone() };
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        tokio::spawn(async move {
            let result = update::download(&update, progress).await.map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Downloaded { result, update });
            ctx.request_repaint();
        });
    }

    /// The banner over the pages, when there's something to say. `in_session` holds the
    /// update back, since restarting would cut the session off.
    pub fn banner(&mut self, ui: &mut egui::Ui, in_session: bool) {
        let mut action = None;
        match &self.state {
            State::Available(update) if self.dismissed.as_ref() != Some(&update.version) => {
                banner(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Update available").font(theme::semibold(14.0)).color(theme::ACCENT));
                        ui.label(format!("Connect {} (you have {})", update.version, update::current_version()));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            let now = ui
                                .add_enabled(!in_session, theme::primary("Update now").small())
                                .on_disabled_hover_text("Available once the current session ends.");
                            if now.clicked() {
                                action = Some(Action::Download(update.clone()));
                            }
                            if ui.add(theme::secondary("Later").small()).clicked() {
                                action = Some(Action::Dismiss(update.version.clone()));
                            }
                            ui.hyperlink_to("What's new", &update.notes_url);
                        });
                    });
                });
                ui.add_space(6.0);
            }
            State::Downloading { update, progress } => {
                let fraction = progress.load(Ordering::Relaxed) as f32 / 1000.0;
                banner(ui, |ui| {
                    ui.label(format!("Downloading Connect {}… Connect restarts when it's done.", update.version));
                    ui.add(egui::ProgressBar::new(fraction).show_percentage());
                });
                ui.add_space(6.0);
            }
            State::Failed { message, update: Some(update) } => {
                banner(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new("Couldn't update:").font(theme::semibold(14.0)).color(theme::BAD));
                        ui.label(message.as_str());
                    });
                    ui.horizontal(|ui| {
                        if ui.add_enabled(!in_session, theme::primary("Try again").small()).clicked() {
                            action = Some(Action::Download(update.clone()));
                        }
                        if ui.add(theme::secondary("Later").small()).clicked() {
                            action = Some(Action::Dismiss(update.version.clone()));
                        }
                        ui.hyperlink_to("Download it yourself", &update.notes_url);
                    });
                });
                ui.add_space(6.0);
            }
            _ => {}
        }
        match action {
            Some(Action::Download(update)) => self.start_download(update),
            Some(Action::Dismiss(version)) => self.dismissed = Some(version),
            None => {}
        }
    }

    /// The "Updates" card on the Settings page.
    pub fn settings_card(&mut self, ui: &mut egui::Ui) {
        theme::card(ui, |ui| {
            theme::section_title(ui, "Updates");
            ui.horizontal(|ui| {
                ui.label(format!("Connect {}", env!("CARGO_PKG_VERSION")));
                let checking = self.busy();
                if ui.add_enabled(!checking, theme::secondary("Check for updates").small()).clicked() {
                    self.dismissed = None;
                    self.check(true);
                }
                let (text, color) = match &self.state {
                    State::Checking => ("Checking…".to_owned(), Color32::from_gray(170)),
                    State::UpToDate => ("You have the latest version.".to_owned(), theme::GOOD),
                    State::Available(u) => (format!("Version {} is available.", u.version), theme::ACCENT),
                    State::Downloading { update, .. } => (format!("Downloading {}…", update.version), theme::ACCENT),
                    State::Failed { message, .. } => (message.clone(), theme::BAD),
                    State::Idle => (String::new(), Color32::from_gray(170)),
                };
                ui.label(RichText::new(text).color(color));
            });
            ui.label(theme::caption("Connect checks for updates when it starts and every few hours."));
        });
    }
}

enum Action {
    Download(Update),
    Dismiss(String),
}

/// A full-width pane of glass with an accent-coloured rim.
fn banner(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    let glass =
        theme::Glass { rim_top: theme::ACCENT, rim_bottom: theme::ACCENT.gamma_multiply(0.4), ..theme::Glass::PANEL };
    theme::glass(ui, glass, 16.0, egui::Margin::symmetric(16, 12), |ui| {
        ui.set_width(ui.available_width());
        add(ui);
    });
}
