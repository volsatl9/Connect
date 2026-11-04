//! The Gamepad page: an Xbox controller that shows what's being pressed, and what each button
//! does. Click a button to change it.

use std::time::Duration;

use eframe::egui::{
    self, Align2, Color32, CursorIcon, Id, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, Ui, pos2, vec2,
};

use super::{
    App,
    gamepad::{Tester, XboxButton},
    theme,
};

/// How often to look at the controllers: a controller's own rate, near enough, while one is
/// plugged in, and slowly for one to appear.
const POLL_CONNECTED: Duration = Duration::from_millis(16);
const POLL_WAITING: Duration = Duration::from_millis(500);

const DRAWING: egui::Vec2 = vec2(460.0, 290.0);
const BODY: Color32 = Color32::from_rgb(44, 47, 68);
const BODY_LIGHT: Color32 = Color32::from_rgb(58, 62, 88);

/// Where a button is in the drawing.
#[derive(Clone, Copy)]
enum Place {
    Round(Pos2, f32),
    Wide(Rect),
}

fn place(button: XboxButton) -> Place {
    let wide = |x: f32, y: f32, w: f32, h: f32| Place::Wide(Rect::from_min_size(pos2(x, y), vec2(w, h)));
    match button {
        XboxButton::LeftTrigger => wide(92.0, 4.0, 76.0, 22.0),
        XboxButton::RightTrigger => wide(292.0, 4.0, 76.0, 22.0),
        XboxButton::LeftBumper => wide(82.0, 30.0, 92.0, 20.0),
        XboxButton::RightBumper => wide(286.0, 30.0, 92.0, 20.0),
        XboxButton::LeftStick => Place::Round(pos2(128.0, 112.0), 25.0),
        XboxButton::RightStick => Place::Round(pos2(280.0, 172.0), 25.0),
        XboxButton::DPadUp => wide(167.0, 144.0, 26.0, 26.0),
        XboxButton::DPadDown => wide(167.0, 196.0, 26.0, 26.0),
        XboxButton::DPadLeft => wide(141.0, 170.0, 26.0, 26.0),
        XboxButton::DPadRight => wide(193.0, 170.0, 26.0, 26.0),
        XboxButton::View => Place::Round(pos2(194.0, 112.0), 16.0),
        XboxButton::Menu => Place::Round(pos2(266.0, 112.0), 16.0),
        XboxButton::Guide => Place::Round(pos2(230.0, 80.0), 17.0),
        XboxButton::Y => Place::Round(pos2(336.0, 80.0), 16.0),
        XboxButton::A => Place::Round(pos2(336.0, 144.0), 16.0),
        XboxButton::X => Place::Round(pos2(304.0, 112.0), 16.0),
        XboxButton::B => Place::Round(pos2(368.0, 112.0), 16.0),
    }
}

/// The face buttons in their usual colours, so the drawing reads as the real thing at a glance.
fn colour(button: XboxButton) -> Option<Color32> {
    match button {
        XboxButton::A => Some(Color32::from_rgb(92, 190, 112)),
        XboxButton::B => Some(Color32::from_rgb(226, 96, 96)),
        XboxButton::X => Some(Color32::from_rgb(92, 142, 230)),
        XboxButton::Y => Some(Color32::from_rgb(232, 196, 84)),
        _ => None,
    }
}

pub fn show(app: &mut App, ui: &mut Ui) {
    theme::page_title(ui, "Gamepad", "Change what your controller's buttons do when you play on another PC.");
    let tester = app.tester.get_or_insert_with(Tester::new);
    let tested = tester.poll(&app.settings.button_map);
    let error = tester.error().map(str::to_owned);
    ui.ctx().request_repaint_after(if tested.connected { POLL_CONNECTED } else { POLL_WAITING });

    let (rect, _) = ui.allocate_exact_size(DRAWING, Sense::hover());
    let at = |p: Pos2| rect.min + p.to_vec2();
    draw_body(ui, at);
    for button in XboxButton::ALL {
        let target = app.settings.button_map.target(button);
        let held = tested.held.contains(&button);
        if draw_button(ui, button, target, held, app.selected_button == button, at) {
            app.selected_button = button;
        }
    }

    theme::card(ui, |ui| {
        let button = app.selected_button;
        let mut target = app.settings.button_map.target(button);
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{} does", button.label())).font(theme::semibold(15.0)));
            let options = XboxButton::ALL.map(|b| (b, b.label().to_owned()));
            if theme::dropdown(ui, "button-target", &mut target, &options) {
                app.settings.button_map.set(button, target);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let reset = theme::secondary("Reset all").small();
                if ui.add_enabled(!app.settings.button_map.is_default(), reset).clicked() {
                    app.settings.button_map.reset();
                }
            });
        });
        ui.label(theme::caption("Click any button on the controller above to change it."));
        let unassigned = app.settings.button_map.unassigned();
        if !unassigned.is_empty() {
            let names: Vec<&str> = unassigned.iter().map(|b| b.label()).collect();
            ui.label(theme::caption(format!("Nothing does {} now.", names.join(", "))));
        }
    });
    ui.add_space(12.0);

    let status = if let Some(error) = error {
        format!("Controllers aren't available: {error}")
    } else if !tested.connected {
        "No controller found. Plug one in to try your buttons here.".to_owned()
    } else if tested.held.is_empty() {
        "Controller connected. Press a button to try it.".to_owned()
    } else if tested.sends.is_empty() {
        "The other PC would get nothing.".to_owned()
    } else {
        let names: Vec<&str> = tested.sends.iter().map(|b| b.label()).collect();
        format!("The other PC would get: {}", names.join(", "))
    };
    ui.label(RichText::new(status).color(theme::DIM));
    ui.label(theme::caption("The other PC sees an ordinary Xbox controller. Sticks stay as they are."));
}

/// A capsule: a thick line with round ends.
fn capsule(painter: &egui::Painter, a: Pos2, b: Pos2, radius: f32, color: Color32) {
    painter.line_segment([a, b], Stroke::new(radius * 2.0, color));
    painter.circle_filled(a, radius, color);
    painter.circle_filled(b, radius, color);
}

fn draw_body(ui: &Ui, at: impl Fn(Pos2) -> Pos2) {
    let painter = ui.painter();
    capsule(painter, at(pos2(118.0, 92.0)), at(pos2(70.0, 232.0)), 50.0, BODY);
    capsule(painter, at(pos2(342.0, 92.0)), at(pos2(390.0, 232.0)), 50.0, BODY);
    painter.rect_filled(Rect::from_min_max(at(pos2(100.0, 42.0)), at(pos2(360.0, 236.0))), 44.0, BODY);
    capsule(painter, at(pos2(130.0, 56.0)), at(pos2(330.0, 56.0)), 14.0, BODY_LIGHT);
}

/// True if clicked.
fn draw_button(
    ui: &mut Ui,
    button: XboxButton,
    target: XboxButton,
    held: bool,
    selected: bool,
    at: impl Fn(Pos2) -> Pos2,
) -> bool {
    let shape = place(button);
    let rect = match shape {
        Place::Round(centre, radius) => Rect::from_center_size(at(centre), egui::Vec2::splat(radius * 2.0)),
        Place::Wide(rect) => rect.translate(at(Pos2::ZERO).to_vec2()),
    };
    let response = ui
        .interact(rect, Id::unique(("gamepad-button", button)), Sense::click())
        .on_hover_cursor(CursorIcon::PointingHand)
        .on_hover_text(if target == button {
            button.label().to_owned()
        } else {
            format!("{} does {}", button.label(), target.label())
        });
    let hover = ui.ctx().animate_bool_with_time(response.id.with("hover"), response.hovered(), 0.12);
    let changed = target != button;
    let base = colour(target).unwrap_or(Color32::from_rgb(26, 28, 42));
    let fill = if held { theme::ACCENT } else { base.lerp_to_gamma(Color32::WHITE, 0.14 * hover) };
    let text = if held || colour(target).is_some() { Color32::from_rgb(16, 17, 26) } else { theme::TEXT };
    let ring = match (selected, changed) {
        (true, _) => Stroke::new(2.5, Color32::WHITE),
        (false, true) => Stroke::new(2.0, theme::ACCENT),
        (false, false) => Stroke::new(1.0, Color32::from_white_alpha(38)),
    };
    let painter = ui.painter();
    match shape {
        Place::Round(_, radius) => {
            painter.circle(rect.center(), radius, fill, ring);
        }
        Place::Wide(_) => {
            painter.rect(rect, 7.0, fill, ring, StrokeKind::Inside);
        }
    }
    let size = match target.short().chars().count() {
        4.. => 9.5,
        _ if rect.height() < 24.0 => 11.0,
        _ => 12.5,
    };
    painter.text(rect.center(), Align2::CENTER_CENTER, target.short(), theme::semibold(size), text);
    response.clicked()
}
