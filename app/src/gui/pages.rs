//! The sidebar, the Connect page (this PC and the saved computers as cards), the Settings page,
//! and their dialogs.

use std::{
    sync::atomic::Ordering,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use connect_host::VideoSource;
use connect_net as net;
use connect_proto::DeviceClass;
use eframe::egui::{
    self, Align, Align2, Color32, CursorIcon, FontId, Id, Layout, Margin, Rect, RichText, Sense, Stroke, StrokeKind,
    TextEdit, TextStyle, Ui, UiBuilder, emath::easing, pos2, vec2,
};

use super::{
    App, ClientState, Dialog, Page, icons,
    prompt::permission_toggles,
    settings::{self, BITRATE_CHOICES_MBPS, KnownDevice},
    theme::{self, Glass},
};

const SIDEBAR_OPEN: f32 = 184.0;
const SIDEBAR_SHUT: f32 = 60.0;
/// Space around the sidebar, and between it and the page.
const EDGE: f32 = 12.0;
const CARD: egui::Vec2 = vec2(136.0, 178.0);
const CARD_GAP: f32 = 12.0;
/// How long a notice stays up.
const NOTICE_TIME: Duration = Duration::from_secs(6);
const ERROR_TIME: Duration = Duration::from_secs(12);

/// Draws the sidebar down the left of `area`, either open with labels or shut to a strip of
/// icons, and returns the space left for the page.
pub fn sidebar(app: &mut App, ui: &mut Ui, area: Rect) -> Rect {
    let ctx = ui.ctx().clone();
    let open = ctx.animate_bool_with_time_and_easing(
        Id::unique("sidebar"),
        !app.settings.sidebar_collapsed,
        0.24,
        easing::cubic_out,
    );
    let width = egui::lerp(SIDEBAR_SHUT..=SIDEBAR_OPEN, open);
    let bar = Rect::from_min_size(area.min + vec2(EDGE, EDGE), vec2(width, area.height() - 2.0 * EDGE));
    ui.painter().add(Glass::PANEL.shapes(bar, 20.0));
    let painter = ui.painter().with_clip_rect(bar);
    // Icons keep to a column down the left; the labels beside them fade in as the bar opens.
    let icon_x = bar.left() + SIDEBAR_SHUT / 2.0;
    let label_x = bar.left() + 50.0;
    let labels = ((open - 0.4) / 0.6).clamp(0.0, 1.0);
    let label = |y: f32, text: &str, font: FontId, color: Color32| {
        if labels > 0.0 {
            let galley = theme::truncated(ui, text, font, bar.right() - label_x - 12.0);
            painter.galley(pos2(label_x, y - galley.size().y / 2.0), galley, color.gamma_multiply(labels));
        }
    };

    // The name, with the button that shuts the bar. Shut, the logo turns into the button that
    // opens it again while the pointer is over it.
    let header_y = bar.top() + 30.0;
    let logo = Rect::from_center_size(pos2(icon_x, header_y), vec2(22.0, 22.0));
    let toggle = if open > 0.5 {
        Rect::from_center_size(pos2(bar.right() - 24.0, header_y), vec2(26.0, 26.0))
    } else {
        logo.expand(5.0)
    };
    let toggle =
        ui.interact(toggle, Id::unique("sidebar-toggle"), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
    let hover = ctx.animate_bool_with_time(Id::unique("sidebar-toggle-hover"), toggle.hovered(), 0.12);
    let logo_shown = if open > 0.5 { 1.0 } else { 1.0 - hover };
    painter.add(logo_glass(logo_shown).shapes(logo, 7.0));
    label(header_y, "Connect", theme::semibold(17.0), theme::TEXT);
    if open > 0.5 {
        let fill = Color32::from_white_alpha((14.0 + 22.0 * hover) as u8).gamma_multiply(labels);
        painter.circle_filled(toggle.rect.center(), 13.0, fill);
        let caret = theme::TEXT.gamma_multiply(labels);
        painter.text(toggle.rect.center(), Align2::CENTER_CENTER, icons::CARET_LEFT, theme::icon_font(14.0), caret);
    } else {
        let caret = Color32::WHITE.gamma_multiply(hover);
        painter.circle_filled(logo.center(), 14.0, Color32::from_white_alpha((26.0 * hover) as u8));
        painter.text(logo.center(), Align2::CENTER_CENTER, icons::CARET_RIGHT, theme::icon_font(15.0), caret);
    }
    let toggle = toggle.on_hover_text(if open > 0.5 { "Collapse the sidebar" } else { "Expand the sidebar" });
    if toggle.clicked() {
        app.settings.sidebar_collapsed = !app.settings.sidebar_collapsed;
    }

    let mut y = header_y + 44.0;
    for (page, icon, name) in
        [(Page::Connect, icons::MONITOR, "Connect"), (Page::Settings, icons::GEAR_SIX, "Settings")]
    {
        let row = Rect::from_min_max(pos2(bar.left() + 8.0, y), pos2(bar.right() - 8.0, y + 38.0));
        let response =
            ui.interact(row, Id::unique(("nav", name)), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
        let response = if open < 0.5 { response.on_hover_text(name) } else { response };
        let selected = app.page == page;
        let hover = ctx.animate_bool_with_time(response.id.with("hover"), response.hovered(), 0.12);
        if selected {
            painter.add(Glass { shadow: egui::Shadow::NONE, ..Glass::RAISED }.shapes(row, 12.0));
        } else if hover > 0.0 {
            painter.rect_filled(row, 12.0, Color32::from_white_alpha((16.0 * hover) as u8));
        }
        let color = if selected { theme::TEXT } else { theme::DIM.lerp_to_gamma(theme::TEXT, hover) };
        painter.text(pos2(icon_x, row.center().y), Align2::CENTER_CENTER, icon, theme::icon_font(18.0), color);
        label(row.center().y, name, theme::medium(14.0), color);
        if response.clicked() && !selected {
            app.page = page;
            app.page_since = Instant::now();
        }
        y += 42.0;
    }

    // Status along the bottom: whether this PC can be reached, and who is using it.
    let mut y = bar.bottom() - 26.0;
    let (color, text) = if app.online.load(Ordering::Relaxed) {
        (theme::GOOD, "Online")
    } else {
        (Color32::from_gray(140), "Going online…")
    };
    painter.circle_filled(pos2(icon_x, y), 7.0, color.gamma_multiply(0.25));
    painter.circle_filled(pos2(icon_x, y), 4.0, color);
    label(y, text, FontId::proportional(13.0), theme::DIM);
    let status = Rect::from_center_size(pos2(icon_x, y), vec2(24.0, 24.0));
    if open < 0.5 {
        ui.interact(status, Id::unique("status"), Sense::hover()).on_hover_text(text);
    }
    if let Some(hosting) = &app.hosting {
        y -= 30.0;
        let text = format!("{} is connected", hosting.client_name);
        painter.text(pos2(icon_x, y), Align2::CENTER_CENTER, icons::BROADCAST, theme::icon_font(17.0), theme::ACCENT);
        label(y, &text, FontId::proportional(13.0), theme::ACCENT);
        let row = Rect::from_min_max(pos2(bar.left() + 8.0, y - 14.0), pos2(bar.right() - 8.0, y + 14.0));
        let response =
            ui.interact(row, Id::unique("hosting-status"), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
        if response.on_hover_text(format!("{text}. Click to manage.")).clicked() {
            app.dialog = Some(Dialog::Hosting);
        }
    }

    Rect::from_min_max(pos2(bar.right() + EDGE + 14.0, area.top() + 22.0), pos2(area.right() - 10.0, area.bottom()))
}

/// The app's mark: a small square of accent-coloured glass.
fn logo_glass(shown: f32) -> Glass {
    let fade = |c: Color32| c.gamma_multiply(shown);
    Glass {
        base: Color32::TRANSPARENT,
        top: fade(Color32::from_rgb(140, 150, 255)),
        bottom: fade(Color32::from_rgb(34, 190, 238)),
        rim_top: fade(Color32::from_white_alpha(140)),
        rim_bottom: fade(Color32::from_white_alpha(20)),
        shadow: egui::Shadow { offset: [0, 2], blur: 12, spread: 0, color: fade(theme::ACCENT.gamma_multiply(0.5)) },
    }
}

/// The latest notice, as a glass bar that goes away by itself.
fn notice(app: &mut App, ui: &mut Ui) {
    let Some(notice) = &app.notice else { return };
    let lasts = if notice.is_error { ERROR_TIME } else { NOTICE_TIME };
    let age = notice.shown.elapsed();
    // A dialog shows errors itself, for as long as it stays open.
    if app.dialog.is_some() {
        return;
    }
    if age >= lasts {
        app.notice = None;
        return;
    }
    ui.ctx().request_repaint_after(lasts - age);
    let mut dismiss = false;
    theme::glass(ui, Glass::PANEL, 14.0, Margin::symmetric(14, 10), |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal_top(|ui| {
            ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                let close = egui::Button::new(RichText::new(icons::X).font(theme::icon_font(14.0))).frame(false);
                dismiss = ui.add(close).on_hover_text("Dismiss").clicked();
                ui.with_layout(Layout::left_to_right(Align::Min), |ui| {
                    theme::dot(ui, if notice.is_error { theme::BAD } else { theme::ACCENT });
                    let text = ui.add(egui::Label::new(&notice.text).wrap());
                    if let Some(detail) = &notice.detail {
                        text.on_hover_text(detail);
                    }
                });
            });
        });
    });
    ui.add_space(12.0);
    if dismiss {
        app.notice = None;
    }
}

enum Card {
    ThisPc,
    Saved(usize),
    Add,
}

/// What a click on the Connect page asks for, carried out once the cards are drawn.
enum Action {
    Connect(net::iroh::EndpointId),
    AskRemove(String),
    Remove(String),
    KeepIt,
    AddComputer,
    Manage,
}

pub fn computers(app: &mut App, ui: &mut Ui) {
    theme::page_title(ui, "Connect", "Play on one of your computers, or let someone play on this one.");
    notice(app, ui);

    let mut cards = Vec::with_capacity(app.settings.computers.len() + 2);
    if VideoSource::SCREEN_SUPPORTED {
        cards.push(Card::ThisPc);
    }
    cards.extend((0..app.settings.computers.len()).map(Card::Saved));
    cards.push(Card::Add);
    let columns = (((ui.available_width() + CARD_GAP) / (CARD.x + CARD_GAP)).floor() as usize).max(1);
    let rows = cards.len().div_ceil(columns);
    let origin = ui.cursor().min + vec2(0.0, 4.0);
    let size = vec2(
        columns.min(cards.len()) as f32 * (CARD.x + CARD_GAP) - CARD_GAP,
        rows as f32 * (CARD.y + CARD_GAP) - CARD_GAP,
    );
    ui.allocate_rect(Rect::from_min_size(origin, size + vec2(0.0, 4.0)), Sense::hover());

    let mut action = None;
    for (n, card) in cards.into_iter().enumerate() {
        let at = vec2((n % columns) as f32 * (CARD.x + CARD_GAP), (n / columns) as f32 * (CARD.y + CARD_GAP));
        let rect = Rect::from_min_size(origin + at, CARD);
        match card {
            Card::ThisPc => this_pc_card(app, ui, rect, &mut action),
            Card::Saved(i) => {
                let device = &app.settings.computers[i];
                let confirming = app.confirm_remove.as_deref() == Some(device.id.as_str());
                saved_card(ui, rect, device, app.client.is_none(), confirming, &mut action);
            }
            Card::Add => add_card(ui, rect, &mut action),
        }
    }

    match action {
        Some(Action::Connect(id)) => {
            app.confirm_remove = None;
            app.start_connect(id, ui.ctx());
        }
        Some(Action::AskRemove(id)) => app.confirm_remove = Some(id),
        Some(Action::Remove(id)) => {
            app.settings.computers.retain(|d| d.id != id);
            app.confirm_remove = None;
        }
        Some(Action::KeepIt) => app.confirm_remove = None,
        Some(Action::AddComputer) => {
            app.connect_input.clear();
            app.notice = None;
            app.focus_input = true;
            app.dialog = Some(Dialog::AddComputer);
        }
        Some(Action::Manage) => app.dialog = Some(Dialog::Hosting),
        None => {}
    }
}

/// The glass under a card, lifting a little while the pointer is over it. Returns where the card
/// is drawn now.
fn card_glass(ui: &Ui, rect: Rect, id: Id) -> Rect {
    let hover = ui.ctx().animate_bool_with_time(id.with("hover"), ui.rect_contains_pointer(rect), 0.15);
    let rect = rect.translate(vec2(0.0, -3.0 * hover));
    ui.painter().add(Glass::PANEL.lerp(&Glass::RAISED, hover).shapes(rect, 18.0));
    if hover > 0.0 {
        let ring = Stroke::new(1.5, theme::ACCENT.gamma_multiply(0.35 * hover));
        ui.painter().rect_stroke(rect.expand(2.0), 20.0, ring, StrokeKind::Outside);
    }
    rect
}

/// The computer icon on its own small pane of glass, `tint` showing through.
fn computer_tile(ui: &Ui, centre: egui::Pos2, tint: Color32) {
    let tile = Rect::from_center_size(centre, vec2(70.0, 56.0));
    let glass = Glass {
        top: tint.gamma_multiply(0.42),
        bottom: tint.gamma_multiply(0.1),
        shadow: egui::Shadow::NONE,
        ..Glass::RAISED
    };
    ui.painter().add(glass.shapes(tile, 16.0));
    let icon = tint.lerp_to_gamma(Color32::WHITE, 0.7);
    ui.painter().text(tile.center(), Align2::CENTER_CENTER, icons::MONITOR, theme::icon_font(34.0), icon);
}

fn card_text(ui: &Ui, rect: Rect, name: &str, detail: &str, detail_color: Color32) -> Rect {
    let width = rect.width() - 20.0;
    let name = theme::truncated(ui, name, theme::semibold(13.5), width);
    let name_rect = Rect::from_center_size(pos2(rect.center().x, rect.top() + 102.0), name.size());
    ui.painter().galley(name_rect.min, name, theme::TEXT);
    let detail = theme::truncated(ui, detail, FontId::proportional(11.5), width);
    ui.painter().galley(pos2(rect.center().x - detail.size().x / 2.0, rect.top() + 114.0), detail, detail_color);
    name_rect
}

/// The bottom of a card, for its button.
fn card_footer(ui: &mut Ui, rect: Rect) -> Ui {
    let footer = Rect::from_min_max(
        pos2(rect.left() + 12.0, rect.bottom() - 40.0),
        pos2(rect.right() - 12.0, rect.bottom() - 12.0),
    );
    ui.new_child(UiBuilder::new().max_rect(footer).layout(Layout::left_to_right(Align::Center)))
}

fn this_pc_card(app: &mut App, ui: &mut Ui, rect: Rect, action: &mut Option<Action>) {
    let rect = card_glass(ui, rect, Id::unique("this-pc"));
    let tint = if app.settings.accepting { theme::GOOD } else { Color32::from_gray(150) };
    computer_tile(ui, pos2(rect.center().x, rect.top() + 50.0), tint);
    let (detail, color) = match &app.hosting {
        Some(hosting) => (format!("In use · {}", elapsed(hosting.since)), theme::ACCENT),
        None if app.settings.accepting => ("This PC · sharing on".to_owned(), theme::DIM),
        None => ("This PC · sharing off".to_owned(), theme::DIM),
    };
    let name = app.settings.device_name.trim();
    let name_rect = card_text(ui, rect, if name.is_empty() { "This PC" } else { name }, &detail, color);
    let about = match &app.hosting {
        Some(hosting) => format!("{} is connected to this PC.", hosting.client_name),
        None => "This PC. Others connect to it with your User-ID, found in Settings.".to_owned(),
    };
    ui.interact(name_rect, Id::unique("this-pc-name"), Sense::hover()).on_hover_text(about);

    let mut footer = card_footer(ui, rect);
    if app.hosting.is_some() {
        let width = footer.available_width();
        if footer.add(theme::secondary("Manage").small().width(width)).clicked() {
            *action = Some(Action::Manage);
        }
    } else {
        footer.label(RichText::new("Sharing").font(theme::medium(13.0)));
        footer.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let mut accepting = app.settings.accepting;
            let response = theme::switch(ui, &mut accepting)
                .on_hover_text("Let people ask to connect to this PC. You still allow or deny each one.");
            if response.changed() {
                app.settings.accepting = accepting;
                app.host.set_accepting(accepting);
            }
        });
    }
}

fn saved_card(
    ui: &mut Ui,
    rect: Rect,
    device: &KnownDevice,
    can_connect: bool,
    confirming: bool,
    action: &mut Option<Action>,
) {
    let id = Id::unique(("computer", &device.id));
    let hovered = ui.rect_contains_pointer(rect);
    let rect = card_glass(ui, rect, id);
    if confirming {
        let text = Rect::from_min_max(rect.min + vec2(12.0, 18.0), pos2(rect.right() - 12.0, rect.bottom() - 84.0));
        let mut body = ui.new_child(UiBuilder::new().max_rect(text).layout(Layout::top_down(Align::Center)));
        body.label(RichText::new(format!("Remove {}?", device.name)).font(theme::semibold(13.5)));
        body.label(theme::caption("You can add it again with its User-ID.").size(11.5));
        let buttons = Rect::from_min_max(
            pos2(rect.left() + 12.0, rect.bottom() - 76.0),
            pos2(rect.right() - 12.0, rect.bottom() - 12.0),
        );
        let mut buttons = ui.new_child(UiBuilder::new().max_rect(buttons).layout(Layout::top_down(Align::Center)));
        buttons.spacing_mut().item_spacing.y = 8.0;
        let width = buttons.available_width();
        if buttons.add(theme::danger("Remove").small().width(width)).clicked() {
            *action = Some(Action::Remove(device.id.clone()));
        }
        if buttons.add(theme::secondary("Keep").small().width(width)).clicked() {
            *action = Some(Action::KeepIt);
        }
        return;
    }

    computer_tile(ui, pos2(rect.center().x, rect.top() + 50.0), theme::ACCENT);
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let detail = device.last_used.map_or("Saved".to_owned(), |then| format!("Used {}", settings::days_ago(then, now)));
    let name_rect = card_text(ui, rect, &device.name, &detail, theme::DIM);
    ui.interact(name_rect, id.with("name"), Sense::hover()).on_hover_text(format!(
        "{}\nUser-ID {}",
        device.name,
        short_id(&device.id)
    ));

    let mut footer = card_footer(ui, rect);
    let width = footer.available_width();
    let connect = footer
        .add_enabled(can_connect, theme::primary("Connect").small().width(width))
        .on_disabled_hover_text("Already connecting to a computer.");
    if connect.clicked()
        && let Some(user_id) = device.user_id()
    {
        *action = Some(Action::Connect(user_id));
    }

    // Remove, in the corner while the pointer is over the card.
    let shown = ui.ctx().animate_bool_with_time(id.with("remove"), hovered, 0.15);
    if shown > 0.0 {
        let corner = Rect::from_center_size(pos2(rect.right() - 19.0, rect.top() + 19.0), vec2(24.0, 24.0));
        let mut corner = ui.new_child(UiBuilder::new().max_rect(corner));
        corner.multiply_opacity(shown);
        if theme::icon_button(&mut corner, icons::X, 24.0).on_hover_text("Remove").clicked() {
            *action = Some(Action::AskRemove(device.id.clone()));
        }
    }
}

fn add_card(ui: &mut Ui, rect: Rect, action: &mut Option<Action>) {
    let response = ui
        .interact(rect, Id::unique("add-computer"), Sense::click())
        .on_hover_cursor(CursorIcon::PointingHand)
        .on_hover_text("Connect to a computer by its User-ID");
    let hover = ui.ctx().animate_bool_with_time(response.id.with("hover"), response.hovered(), 0.15);
    let rect = rect.translate(vec2(0.0, -3.0 * hover));
    ui.painter().rect_filled(rect, 18.0, Color32::from_white_alpha((6.0 + 12.0 * hover) as u8));
    let dashes = Stroke::new(1.5, Color32::from_white_alpha((60.0 + 60.0 * hover) as u8));
    ui.painter().extend(theme::dashed_outline(rect.shrink(0.75), 18.0, dashes));

    let circle = Rect::from_center_size(pos2(rect.center().x, rect.top() + 54.0), vec2(52.0, 52.0));
    ui.painter().add(Glass::PANEL.lerp(&Glass::RAISED, hover).shapes(circle, 26.0));
    ui.painter().text(circle.center(), Align2::CENTER_CENTER, icons::PLUS, theme::icon_font(24.0), theme::TEXT);
    card_text(ui, rect, "Connect", "to a new computer", theme::DIM);
    if response.clicked() {
        *action = Some(Action::AddComputer);
    }
}

/// Whichever dialog is up: connection progress first, then the one the user opened.
pub fn dialogs(app: &mut App, ctx: &egui::Context) {
    if app.client.as_ref().is_some_and(|c| !matches!(c.state, ClientState::Connected { .. })) {
        connecting_dialog(app, ctx);
        return;
    }
    match app.dialog {
        Some(Dialog::AddComputer) => add_dialog(app, ctx),
        Some(Dialog::Hosting) => hosting_dialog(app, ctx),
        None => {}
    }
}

fn connecting_dialog(app: &mut App, ctx: &egui::Context) {
    let Some(client) = &app.client else { return };
    let name =
        app.settings.computer_name(client.host_id).map_or_else(|| short_id(&client.host_id.to_string()), str::to_owned);
    let (title, detail) = match client.state {
        ClientState::WaitingForApproval => {
            (format!("Waiting for {name} to let you in…"), "They'll see a request to allow you in on their screen.")
        }
        _ => (format!("Connecting to {name}…"), "Finding the computer and the fastest way to reach it."),
    };
    let (cancel, _) = theme::dialog(ctx, "connecting", 360.0, |ui| {
        ui.horizontal(|ui| {
            ui.add(egui::Spinner::new().size(18.0).color(theme::ACCENT));
            ui.add(egui::Label::new(RichText::new(title).font(theme::semibold(15.0))).wrap());
        });
        ui.label(theme::caption(detail));
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.label(theme::caption(elapsed(client.started)));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| ui.add(theme::secondary("Cancel")).clicked())
                .inner
        })
        .inner
    });
    if cancel {
        client.handle.disconnect();
    }
}

fn add_dialog(app: &mut App, ctx: &egui::Context) {
    let (mut submit, mut close) = (false, false);
    let (parsed, dismissed) = theme::dialog(ctx, "add-computer", 400.0, |ui| {
        ui.label(RichText::new("Connect to a computer").font(theme::semibold(19.0)));
        ui.label(theme::caption(
            "Paste the User-ID from the other PC. Once they let you in, it's saved on your Connect page.",
        ));
        ui.add_space(8.0);
        let field = TextEdit::singleline(&mut app.connect_input)
            .hint_text("Paste their User-ID")
            .font(TextStyle::Monospace)
            .margin(vec2(12.0, 9.0))
            .desired_width(f32::INFINITY);
        let response = ui.add(field);
        if std::mem::take(&mut app.focus_input) {
            response.request_focus();
        }
        // Checked after the field has taken this frame's typing, which can end with Enter.
        let parsed = net::parse_user_id(&app.connect_input);
        // Enter in the field, whichever frame the field gives up focus in.
        let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
        submit |= enter && ui.memory(|m| m.focused().is_none_or(|id| id == response.id));
        match (&parsed, &app.notice) {
            (Err(e), _) if !app.connect_input.trim().is_empty() => {
                ui.label(RichText::new(e.to_string()).color(theme::BAD));
            }
            (_, Some(notice)) if notice.is_error => {
                ui.label(RichText::new(&notice.text).color(theme::BAD));
                if let Some(detail) = &notice.detail {
                    egui::CollapsingHeader::new(theme::caption("Details")).id_salt("error-detail").show(ui, |ui| {
                        ui.label(theme::caption(detail).size(11.5));
                    });
                }
            }
            _ => {}
        }
        ui.add_space(10.0);
        theme::right_row(ui, |ui| {
            submit |= ui.add_enabled(parsed.is_ok(), theme::primary("Connect").icon(icons::PLUS)).clicked();
            close |= ui.add(theme::secondary("Cancel")).clicked();
        });
        parsed
    });
    if dismissed || close {
        app.dialog = None;
        app.connect_input.clear();
        app.notice = None;
    } else if submit && let Ok(id) = parsed {
        app.start_connect(id, ctx);
    }
}

fn hosting_dialog(app: &mut App, ctx: &egui::Context) {
    let Some(hosting) = &mut app.hosting else {
        app.dialog = None;
        return;
    };
    let (host, driver) = (&app.host, &mut app.gamepad_driver);
    let ((disconnect, done), dismissed) = theme::dialog(ctx, "hosting", 440.0, |ui| {
        ui.label(RichText::new(format!("{} is connected", hosting.client_name)).font(theme::semibold(19.0)));
        ui.label(theme::caption(format!(
            "For {} · User-ID {}",
            elapsed(hosting.since),
            short_id(&hosting.peer.to_string())
        )));
        ui.add_space(10.0);
        theme::section_title(ui, "They can use");
        if permission_toggles(ui, &mut hosting.permissions) {
            host.set_permissions(hosting.permissions);
        }
        if hosting.permissions.allows(DeviceClass::Gamepad) {
            driver.setup_row(ui);
        }
        ui.add_space(12.0);
        theme::right_row(ui, |ui| {
            let done = ui.add(theme::primary("Done")).clicked();
            let disconnect = ui.add(theme::danger("Disconnect").icon(icons::SIGN_OUT)).clicked();
            (disconnect, done)
        })
    });
    if disconnect {
        app.host.disconnect();
    }
    if disconnect || done || dismissed {
        app.dialog = None;
    }
}

pub fn settings(app: &mut App, ui: &mut Ui) {
    theme::page_title(ui, "Settings", "");
    notice(app, ui);
    let gap = 12.0;

    theme::card(ui, |ui| {
        theme::section_title(ui, "Your User-ID");
        ui.label(theme::caption(
            "Anyone with it can ask to connect to this PC. It's hidden so it doesn't end up on stream or in a screenshot.",
        ));
        ui.add_space(4.0);
        let id = net::format_user_id(&app.endpoint.id());
        let groups: Vec<String> = if app.show_user_id {
            id.split(' ').map(str::to_owned).collect()
        } else {
            id.split(' ').map(|g| "•".repeat(g.len())).collect()
        };
        for row in groups.chunks(4) {
            ui.label(RichText::new(row.join(" ")).monospace().size(17.0).color(theme::TEXT));
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let (icon, label) = if app.show_user_id { (icons::EYE_SLASH, "Hide") } else { (icons::EYE, "Show") };
            if ui.add(theme::secondary(label).icon(icon)).clicked() {
                app.show_user_id = !app.show_user_id;
            }
            if ui.add(theme::secondary("Copy").icon(icons::COPY)).clicked() {
                ui.ctx().copy_text(app.endpoint.id().to_string());
                app.notify("User-ID copied.", false);
            }
        });
    });
    ui.add_space(gap);

    theme::card(ui, |ui| {
        theme::section_title(ui, "This PC's name");
        ui.label(theme::caption("Shown to the people you connect with, and on their Connect page."));
        let field = TextEdit::singleline(&mut app.settings.device_name).desired_width(320.0).margin(vec2(12.0, 8.0));
        if ui.add(field).changed() {
            app.host.set_host_name(app.settings.device_name.trim());
        }
    });
    ui.add_space(gap);

    theme::card(ui, |ui| {
        theme::section_title(ui, "Viewing");
        let mut hardware = app.settings.hardware_decoding;
        if theme::switch_row(ui, "Hardware video decoding", &mut hardware).changed() {
            app.settings.hardware_decoding = hardware;
            connect_client::set_hardware_decoding(hardware);
        }
        ui.label(theme::caption(
            "Decodes the other PC's screen on your graphics card, which leaves the processor free and keeps up at \
             high resolutions and frame rates. Connect uses the processor instead where the card can't. Turn this \
             off if the picture shows glitches. Applies from the next connection.",
        ));
        ui.add_space(8.0);
        ui.label(RichText::new("Frame pacing").font(theme::medium(14.0)));
        ui.label(theme::caption(
            "Lowest latency shows each frame the moment it arrives. Smooth evens out motion when frames arrive \
             unevenly, by showing them a steady few milliseconds later. Takes effect at once.",
        ));
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            for (smooth, label) in [(false, "Lowest latency"), (true, "Smooth")] {
                if theme::chip(ui, None, label, app.settings.smooth_pacing == smooth).clicked() {
                    app.settings.smooth_pacing = smooth;
                    connect_client::set_smooth_pacing(smooth);
                }
            }
        });
    });
    ui.add_space(gap);

    if VideoSource::SCREEN_SUPPORTED {
        sharing_card(app, ui);
        ui.add_space(gap);

        theme::card(ui, |ui| {
            theme::section_title(ui, "Stream quality");
            ui.label(theme::caption(
                "The most video data per second this PC sends when sharing. Higher looks sharper, especially at \
                 120+ fps, but needs a faster upload. Applies from the next connection.",
            ));
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                for mbps in BITRATE_CHOICES_MBPS {
                    if theme::chip(ui, None, &format!("{mbps} Mb/s"), app.settings.bitrate_mbps == mbps).clicked() {
                        app.settings.bitrate_mbps = mbps;
                        app.host.set_bitrate_mbps(mbps);
                    }
                }
            });
        });
        ui.add_space(gap);
    }

    app.updater.settings_card(ui);
}

fn sharing_card(app: &mut App, ui: &mut Ui) {
    theme::card(ui, |ui| {
        theme::section_title(ui, "Sharing");
        let mut accepting = app.settings.accepting;
        if theme::switch_row(ui, "Let people ask to connect to this PC", &mut accepting).changed() {
            app.settings.accepting = accepting;
            app.host.set_accepting(accepting);
        }
        ui.add_space(8.0);
        ui.label(RichText::new("Allowed by default").font(theme::medium(14.0)));
        ui.label(theme::caption("Pre-selected when someone asks to connect, and what \"always allowed\" devices get."));
        if permission_toggles(ui, &mut app.settings.permissions) {
            app.host.set_default_permissions(app.settings.permissions);
        }
        if app.settings.permissions.allows(DeviceClass::Gamepad) {
            app.gamepad_driver.setup_row(ui);
        }
        ui.label(
            theme::caption(
                "Windows keeps input away from programs running as administrator unless Connect runs as \
                 administrator too.",
            )
            .size(11.5),
        );

        ui.add_space(8.0);
        ui.label(RichText::new("Always allowed").font(theme::medium(14.0)));
        if app.settings.trusted.is_empty() {
            ui.label(theme::caption("Devices you tick \"Always allow\" for connect without asking. None yet."));
        }
        let mut remove = None;
        for (i, device) in app.settings.trusted.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.label(RichText::new(&device.name).font(theme::medium(13.5)));
                ui.label(RichText::new(short_id(&device.id)).monospace().color(theme::DIM));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.add(theme::secondary("Remove").small()).clicked() {
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

/// The first and last groups of a User-ID, enough to tell devices apart at a glance.
pub(super) fn short_id(id: &str) -> String {
    if id.len() <= 16 { id.to_owned() } else { format!("{}…{}", &id[..8], &id[id.len() - 8..]) }
}

pub(super) fn elapsed(since: Instant) -> String {
    let s = since.elapsed().as_secs();
    if s < 60 { format!("{s} s") } else { format!("{}:{:02}", s / 60, s % 60) }
}
