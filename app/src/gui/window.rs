//! The window's own controls, as the app draws its edges itself: somewhere to drag it, the
//! minimise, maximise and close buttons, and borders to resize it by.

use eframe::egui::{
    self, Color32, CursorIcon, Id, PointerButton, Pos2, Rect, ResizeDirection, Sense, Stroke, Ui, ViewportCommand,
    pos2, vec2,
};

use super::{icons, theme};

/// The top of the window: drag it there, and find the buttons.
pub const BAR_HEIGHT: f32 = 34.0;
const BUTTON: egui::Vec2 = vec2(42.0, 28.0);
/// How far in from the window's edges the resize borders reach, and how far along at the corners.
const BORDER: f32 = 5.0;
const CORNER: f32 = 14.0;

fn is_maximized(ui: &Ui) -> bool {
    ui.input(|i| i.viewport().maximized.unwrap_or(false))
}

/// Moves the window when `rect` is dragged, and maximises it on a double click. Goes in before
/// whatever is drawn over `rect`, so that keeps its clicks.
pub fn drag_area(ui: &Ui, id: &str, rect: Rect) {
    let response = ui.interact(rect, Id::unique(("window-drag", id)), Sense::click_and_drag());
    if response.double_clicked() {
        ui.ctx().send_viewport_cmd(ViewportCommand::Maximized(!is_maximized(ui)));
    } else if response.drag_started_by(PointerButton::Primary) {
        ui.ctx().send_viewport_cmd(ViewportCommand::StartDrag);
    }
}

/// The borders to resize by, and the buttons in the top right of `area`. Last, to go over the rest.
pub fn controls(ui: &mut Ui, area: Rect) {
    if !is_maximized(ui) {
        resize_borders(ui, area);
    }
    let close = Rect::from_min_size(pos2(area.right() - BUTTON.x - 6.0, area.top() + 3.0), BUTTON);
    let maximize = close.translate(vec2(-BUTTON.x, 0.0));
    let minimize = maximize.translate(vec2(-BUTTON.x, 0.0));
    if button(ui, "minimize", minimize, theme::TEXT, |ui, c, color| {
        ui.painter().hline((c.x - 5.0)..=(c.x + 5.0), c.y, Stroke::new(1.5, color));
    }) {
        ui.ctx().send_viewport_cmd(ViewportCommand::Minimized(true));
    }
    if button(ui, "maximize", maximize, theme::TEXT, |ui, c, color| {
        let square = Rect::from_center_size(c, vec2(10.0, 10.0));
        ui.painter().rect_stroke(square, 2.0, Stroke::new(1.5, color), egui::StrokeKind::Inside);
    }) {
        ui.ctx().send_viewport_cmd(ViewportCommand::Maximized(!is_maximized(ui)));
    }
    if button(ui, "close", close, theme::BAD, |ui, c, color| {
        ui.painter().text(c, egui::Align2::CENTER_CENTER, icons::X, theme::icon_font(14.0), color);
    }) {
        ui.ctx().send_viewport_cmd(ViewportCommand::Close);
    }
}

/// True if clicked.
fn button(ui: &mut Ui, id: &str, rect: Rect, hover_fill: Color32, draw: impl FnOnce(&Ui, Pos2, Color32)) -> bool {
    let response = ui.interact(rect, Id::unique(("window-button", id)), Sense::click());
    let hover = ui.ctx().animate_bool_with_time(response.id.with("hover"), response.hovered(), 0.1);
    if hover > 0.0 {
        let fill = if hover_fill == theme::TEXT { Color32::from_white_alpha(26) } else { hover_fill };
        ui.painter().rect_filled(rect, 8.0, fill.gamma_multiply(hover));
    }
    draw(ui, rect.center(), theme::DIM.lerp_to_gamma(theme::TEXT, hover));
    response.clicked()
}

fn resize_borders(ui: &Ui, area: Rect) {
    let Rect { min, max } = area;
    let edge = |a: Pos2, b: Pos2| Rect::from_two_pos(a, b);
    let corner =
        |x: f32, y: f32| Rect::from_center_size(pos2(x, y), vec2(2.0 * BORDER, 2.0 * BORDER)).expand(CORNER / 2.0);
    let sides = [
        (edge(pos2(min.x, min.y), pos2(max.x, min.y + BORDER)), ResizeDirection::North, CursorIcon::ResizeVertical),
        (edge(pos2(min.x, max.y - BORDER), pos2(max.x, max.y)), ResizeDirection::South, CursorIcon::ResizeVertical),
        (edge(pos2(min.x, min.y), pos2(min.x + BORDER, max.y)), ResizeDirection::West, CursorIcon::ResizeHorizontal),
        (edge(pos2(max.x - BORDER, min.y), pos2(max.x, max.y)), ResizeDirection::East, CursorIcon::ResizeHorizontal),
        (corner(min.x, min.y), ResizeDirection::NorthWest, CursorIcon::ResizeNwSe),
        (corner(max.x, min.y), ResizeDirection::NorthEast, CursorIcon::ResizeNeSw),
        (corner(min.x, max.y), ResizeDirection::SouthWest, CursorIcon::ResizeNeSw),
        (corner(max.x, max.y), ResizeDirection::SouthEast, CursorIcon::ResizeNwSe),
    ];
    for (n, (rect, direction, cursor)) in sides.into_iter().enumerate() {
        let response = ui.interact(rect, Id::unique(("window-resize", n)), Sense::drag()).on_hover_cursor(cursor);
        if response.drag_started_by(PointerButton::Primary) {
            ui.ctx().send_viewport_cmd(ViewportCommand::BeginResize(direction));
        }
    }
}

/// The bar over a session in a window, holding its name.
pub fn session_bar(ui: &mut Ui, area: Rect, name: &str) {
    let bar = Rect::from_min_size(area.min, vec2(area.width(), BAR_HEIGHT));
    ui.painter().rect_filled(bar, 0.0, Color32::from_rgb(20, 21, 31));
    drag_area(ui, "session", bar);
    let text = pos2(area.left() + 14.0, bar.center().y);
    ui.painter().text(text, egui::Align2::LEFT_CENTER, name, theme::medium(13.0), theme::DIM);
    controls(ui, area);
}
