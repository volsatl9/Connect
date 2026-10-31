//! The look: dark "liquid glass". Translucent panels with a bright top rim over a soft colour
//! backdrop, Inter for text and Phosphor for icons. Animations run only while something moves, so
//! an idle window draws nothing; the backdrop is still for the same reason, since a drifting one
//! would redraw the window all the time, even behind a game.

use std::{
    f32::consts::TAU,
    sync::Arc,
    time::{Duration, Instant},
};

use eframe::egui::{
    self, Align, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, FontTweak, Frame, Galley,
    InnerResponse, Layout, Margin, Mesh, Painter, Pos2, Rect, Response, RichText, Role, Sense, Shadow, Shape, Stroke,
    StrokeKind, TextStyle, Theme, Ui, Widget, WidgetInfo, WidgetText,
    emath::easing,
    epaint::{ColorMode, PathShape, PathStroke},
    pos2,
    text::{LayoutJob, TextFormat, TextWrapping},
    vec2,
};

pub const ACCENT: Color32 = Color32::from_rgb(124, 131, 255);
const ACCENT_DEEP: Color32 = Color32::from_rgb(88, 90, 236);
pub const GOOD: Color32 = Color32::from_rgb(52, 211, 153);
pub const BAD: Color32 = Color32::from_rgb(255, 107, 107);
pub const TEXT: Color32 = Color32::from_rgb(242, 243, 248);
/// Secondary text: captions, hints, "last used".
pub const DIM: Color32 = Color32::from_rgb(158, 163, 184);
const BACKDROP: Color32 = Color32::from_rgb(10, 12, 19);

/// White at `a` opacity, premultiplied as egui expects.
const fn white(a: u8) -> Color32 {
    Color32::from_rgba_premultiplied(a, a, a, a)
}

const MEDIUM: &str = "medium";
const SEMIBOLD: &str = "semibold";
const ICONS: &str = "icons";

pub fn medium(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(MEDIUM.into()))
}

pub fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(SEMIBOLD.into()))
}

/// For the characters in [`super::icons`]. They need a family of their own: Inter puts glyphs of
/// its own at some of the same private codepoints.
pub fn icon_font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(ICONS.into()))
}

/// An optional icon and then `text`, laid out in `font` with the colour left to the painter.
pub fn with_icon(icon: Option<&str>, text: &str, font: FontId) -> LayoutJob {
    let format =
        |font_id| TextFormat { font_id, color: Color32::PLACEHOLDER, valign: Align::Center, ..Default::default() };
    let mut job = LayoutJob::default();
    if let Some(icon) = icon {
        job.append(icon, 0.0, format(icon_font(font.size + 2.0)));
    }
    if !text.is_empty() {
        job.append(text, if icon.is_some() { 8.0 } else { 0.0 }, format(font));
    }
    job
}

pub fn apply(ctx: &egui::Context) {
    install_fonts(ctx);
    ctx.set_theme(Theme::Dark);
    ctx.style_mut_of(Theme::Dark, |style| {
        style.text_styles = [
            (TextStyle::Small, FontId::proportional(11.5)),
            (TextStyle::Body, FontId::proportional(14.0)),
            (TextStyle::Button, medium(13.5)),
            (TextStyle::Heading, semibold(22.0)),
            (TextStyle::Monospace, FontId::monospace(13.5)),
        ]
        .into();
        style.animation_time = 0.18;
        style.spacing.button_padding = vec2(14.0, 7.0);
        style.spacing.item_spacing = vec2(10.0, 8.0);
        style.spacing.interact_size.y = 30.0;

        let v = &mut style.visuals;
        v.panel_fill = BACKDROP;
        v.window_fill = Color32::from_rgb(26, 28, 42);
        v.window_stroke = Stroke::new(1.0, white(28));
        v.window_corner_radius = CornerRadius::same(16);
        v.window_shadow = Shadow { offset: [0, 12], blur: 32, spread: 0, color: Color32::from_black_alpha(110) };
        v.popup_shadow = Shadow { offset: [0, 6], blur: 18, spread: 0, color: Color32::from_black_alpha(90) };
        v.menu_corner_radius = CornerRadius::same(12);
        v.extreme_bg_color = Color32::from_black_alpha(90);
        v.text_edit_bg_color = Some(Color32::from_black_alpha(90));
        v.hyperlink_color = ACCENT;
        v.selection.bg_fill = ACCENT.gamma_multiply(0.45);
        v.selection.stroke = Stroke::new(1.0, ACCENT);
        v.weak_text_color = Some(DIM);
        v.error_fg_color = BAD;
        v.text_cursor.stroke = Stroke::new(2.0, ACCENT);
        v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
        v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, white(22));
        let w = &mut v.widgets;
        for (state, fill, stroke) in
            [(&mut w.inactive, 18, 26), (&mut w.hovered, 32, 52), (&mut w.active, 44, 70), (&mut w.open, 32, 52)]
        {
            state.bg_fill = white(fill);
            state.weak_bg_fill = white(fill);
            state.bg_stroke = Stroke::new(1.0, white(stroke));
            state.corner_radius = CornerRadius::same(10);
            state.fg_stroke.color = TEXT;
            state.expansion = 0.0;
        }
    });
}

/// Inter for text in three weights, with egui's own fonts behind it for any character Inter
/// lacks, and the icons in a family of their own.
fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let font = |bytes: &'static [u8]| Arc::new(FontData::from_static(bytes));
    fonts.font_data.insert("inter".into(), font(include_bytes!("../../assets/fonts/Inter-Regular.otf")));
    fonts.font_data.insert("inter-medium".into(), font(include_bytes!("../../assets/fonts/Inter-Medium.otf")));
    fonts.font_data.insert("inter-semibold".into(), font(include_bytes!("../../assets/fonts/Inter-SemiBold.otf")));
    fonts.font_data.insert(
        "phosphor".into(),
        // Level with Inter's capitals rather than sitting on the baseline.
        Arc::new(
            FontData::from_static(include_bytes!("../../assets/fonts/Phosphor.ttf"))
                .tweak(FontTweak { y_offset_factor: 0.1, ..Default::default() }),
        ),
    );
    let fallback = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let family = |first: &str| std::iter::once(first.to_owned()).chain(fallback.clone()).collect();
    fonts.families.insert(FontFamily::Proportional, family("inter"));
    fonts.families.insert(FontFamily::Name(MEDIUM.into()), family("inter-medium"));
    fonts.families.insert(FontFamily::Name(SEMIBOLD.into()), family("inter-semibold"));
    fonts.families.insert(FontFamily::Name(ICONS.into()), family("phosphor"));
    ctx.set_fonts(fonts);
}

/// Deep blue-black with three large soft glows of colour, for the glass to sit on.
pub fn backdrop(painter: &Painter, rect: Rect) {
    painter.rect_filled(rect, 0.0, BACKDROP);
    let size = rect.width().max(rect.height());
    for (at, radius, color) in [
        (vec2(0.04, -0.06), 0.62, Color32::from_rgb(79, 70, 229)),
        (vec2(1.02, 0.55), 0.5, Color32::from_rgb(162, 28, 175)),
        (vec2(0.42, 1.12), 0.55, Color32::from_rgb(13, 148, 136)),
    ] {
        painter.add(glow(rect.min + at * rect.size(), radius * size, color, 0.42));
    }
}

/// A radial gradient: `color` at `strength` in the middle, fading out to nothing at `radius`.
fn glow(centre: Pos2, radius: f32, color: Color32, strength: f32) -> Mesh {
    const SEGMENTS: u32 = 64;
    const RINGS: u32 = 10;
    let [r, g, b, _] = color.to_array();
    let tint = |alpha: f32| Color32::from_rgba_unmultiplied(r, g, b, (alpha * 255.0).round() as u8);
    let mut mesh = Mesh::default();
    mesh.colored_vertex(centre, tint(strength));
    for ring in 1..=RINGS {
        let t = ring as f32 / RINGS as f32;
        let alpha = strength * (-3.5 * t * t).exp() * (1.0 - t);
        for s in 0..SEGMENTS {
            let angle = s as f32 / SEGMENTS as f32 * TAU;
            mesh.colored_vertex(centre + radius * t * vec2(angle.cos(), angle.sin()), tint(alpha));
        }
    }
    for s in 0..SEGMENTS {
        mesh.add_triangle(0, 1 + s, 1 + (s + 1) % SEGMENTS);
    }
    for ring in 0..RINGS - 1 {
        let (inner, outer) = (1 + ring * SEGMENTS, 1 + (ring + 1) * SEGMENTS);
        for s in 0..SEGMENTS {
            let next = (s + 1) % SEGMENTS;
            mesh.add_triangle(inner + s, outer + s, outer + next);
            mesh.add_triangle(inner + s, outer + next, inner + next);
        }
    }
    mesh
}

/// How a pane of glass is tinted.
#[derive(Clone, Copy)]
pub struct Glass {
    /// Painted under the sheen, for glass over busy content (dialogs, video).
    pub base: Color32,
    pub top: Color32,
    pub bottom: Color32,
    pub rim_top: Color32,
    pub rim_bottom: Color32,
    pub shadow: Shadow,
}

impl Glass {
    /// Cards and the sidebar.
    pub const PANEL: Glass = Glass {
        base: Color32::TRANSPARENT,
        top: white(24),
        bottom: white(9),
        rim_top: white(72),
        rim_bottom: white(18),
        shadow: Shadow { offset: [0, 12], blur: 30, spread: 0, color: Color32::from_black_alpha(90) },
    };
    /// A card under the pointer.
    pub const RAISED: Glass = Glass {
        base: Color32::TRANSPARENT,
        top: white(36),
        bottom: white(15),
        rim_top: white(110),
        rim_bottom: white(34),
        shadow: Shadow { offset: [0, 18], blur: 38, spread: 0, color: Color32::from_black_alpha(120) },
    };
    /// Dialogs, over a dimmed window.
    pub const DIALOG: Glass = Glass {
        base: Color32::from_rgba_premultiplied(22, 24, 37, 236),
        top: white(20),
        bottom: white(4),
        rim_top: white(90),
        rim_bottom: white(20),
        shadow: Shadow { offset: [0, 20], blur: 48, spread: 0, color: Color32::from_black_alpha(150) },
    };
    /// Controls over the video.
    pub const OVERLAY: Glass = Glass {
        base: Color32::from_rgba_premultiplied(14, 16, 26, 214),
        top: white(18),
        bottom: white(4),
        rim_top: white(80),
        rim_bottom: white(16),
        shadow: Shadow { offset: [0, 8], blur: 24, spread: 0, color: Color32::from_black_alpha(110) },
    };

    pub fn lerp(&self, to: &Glass, t: f32) -> Glass {
        let mix = |a: Color32, b: Color32| a.lerp_to_gamma(b, t);
        Glass {
            base: mix(self.base, to.base),
            top: mix(self.top, to.top),
            bottom: mix(self.bottom, to.bottom),
            rim_top: mix(self.rim_top, to.rim_top),
            rim_bottom: mix(self.rim_bottom, to.rim_bottom),
            shadow: Shadow {
                offset: [0, egui::lerp(self.shadow.offset[1] as f32..=to.shadow.offset[1] as f32, t) as i8],
                blur: egui::lerp(self.shadow.blur as f32..=to.shadow.blur as f32, t) as u8,
                spread: 0,
                color: mix(self.shadow.color, to.shadow.color),
            },
        }
    }

    /// The shapes for a pane covering `rect`: shadow, tint, a sheen that fades downwards, and a
    /// rim that catches the light along the top.
    pub fn shapes(&self, rect: Rect, radius: f32) -> Shape {
        let corner = CornerRadius::same(radius.min(255.0) as u8);
        let mut shapes = Vec::with_capacity(4);
        if self.shadow.color.a() > 0 {
            shapes.push(self.shadow.as_shape(rect, corner).into());
        }
        if self.base.a() > 0 {
            shapes.push(Shape::rect_filled(rect, corner, self.base));
        }
        let outline = rounded_outline(rect, radius);
        shapes.push(Shape::mesh(vertical_gradient(&outline, rect, self.top, self.bottom)));
        shapes.push(rim(outline, self.rim_top, self.rim_bottom));
        Shape::Vec(shapes)
    }
}

/// Points around a rounded rectangle, clockwise from the top of the top-right corner.
fn rounded_outline(rect: Rect, radius: f32) -> Vec<Pos2> {
    let r = radius.min(rect.width() / 2.0).min(rect.height() / 2.0).max(0.0);
    let steps = (r / 2.0).clamp(2.0, 12.0) as usize;
    let corners = [
        (pos2(rect.right() - r, rect.top() + r), -90.0f32),
        (pos2(rect.right() - r, rect.bottom() - r), 0.0),
        (pos2(rect.left() + r, rect.bottom() - r), 90.0),
        (pos2(rect.left() + r, rect.top() + r), 180.0),
    ];
    let mut points = Vec::with_capacity(4 * (steps + 1));
    for (centre, start) in corners {
        for i in 0..=steps {
            let angle = (start + 90.0 * i as f32 / steps as f32).to_radians();
            points.push(centre + r * vec2(angle.cos(), angle.sin()));
        }
    }
    points
}

/// Fills a convex `outline` with `top` fading to `bottom` across `rect`.
fn vertical_gradient(outline: &[Pos2], rect: Rect, top: Color32, bottom: Color32) -> Mesh {
    let color_at = |y: f32| top.lerp_to_gamma(bottom, ((y - rect.top()) / rect.height().max(1.0)).clamp(0.0, 1.0));
    let mut mesh = Mesh::default();
    mesh.colored_vertex(rect.center(), color_at(rect.center().y));
    for point in outline {
        mesh.colored_vertex(*point, color_at(point.y));
    }
    let n = outline.len() as u32;
    for i in 0..n {
        mesh.add_triangle(0, 1 + i, 1 + (i + 1) % n);
    }
    mesh
}

fn rim(outline: Vec<Pos2>, top: Color32, bottom: Color32) -> Shape {
    let color = ColorMode::UV(Arc::new(move |bounds: Rect, p: Pos2| {
        let t = ((p.y - bounds.top()) / bounds.height().max(1.0)).clamp(0.0, 1.0);
        top.lerp_to_gamma(bottom, easing::cubic_out(t))
    }));
    Shape::Path(PathShape {
        points: outline,
        closed: true,
        fill: Color32::TRANSPARENT,
        stroke: PathStroke { width: 1.0, color, kind: StrokeKind::Middle },
    })
}

/// A dashed line around a rounded rectangle, for empty slots.
pub fn dashed_outline(rect: Rect, radius: f32, stroke: Stroke) -> Vec<Shape> {
    let mut points = rounded_outline(rect, radius);
    points.push(points[0]);
    Shape::dashed_line(&points, stroke, 6.0, 5.0)
}

/// Lays `add` out on a pane of `glass`.
pub fn glass<R>(
    ui: &mut Ui,
    glass: Glass,
    radius: f32,
    margin: impl Into<Margin>,
    add: impl FnOnce(&mut Ui) -> R,
) -> InnerResponse<R> {
    let slot = ui.painter().add(Shape::Noop);
    let inner = Frame::new().inner_margin(margin).show(ui, add);
    ui.painter().set(slot, glass.shapes(inner.response.rect, radius));
    inner
}

/// A full-width card of glass.
pub fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R> {
    glass(ui, Glass::PANEL, 18.0, Margin::same(18), |ui| {
        ui.set_width(ui.available_width());
        add(ui)
    })
}

/// A dialog over the dimmed window: what `add` returned, and whether the user clicked outside it
/// or pressed Escape.
pub fn dialog<R>(ctx: &egui::Context, id: &str, width: f32, add: impl FnOnce(&mut Ui) -> R) -> (R, bool) {
    let response = egui::Modal::new(egui::Id::unique(("dialog", id)))
        .frame(Frame::new())
        .backdrop_color(Color32::from_black_alpha(140))
        .show(ctx, |ui| {
            glass(ui, Glass::DIALOG, 22.0, Margin::same(22), |ui| {
                ui.set_width(width);
                add(ui)
            })
            .inner
        });
    let close = response.should_close();
    (response.inner, close)
}

pub fn page_title(ui: &mut Ui, title: &str, subtitle: &str) {
    ui.label(RichText::new(title).font(semibold(28.0)).color(TEXT).extra_letter_spacing(-0.4));
    if !subtitle.is_empty() {
        ui.label(RichText::new(subtitle).color(DIM));
    }
    ui.add_space(14.0);
}

pub fn section_title(ui: &mut Ui, title: &str) {
    ui.label(RichText::new(title).font(semibold(15.0)).color(TEXT));
}

/// Small grey explanatory text.
pub fn caption(text: impl Into<String>) -> RichText {
    RichText::new(text).color(DIM).size(12.5)
}

/// `text` on one line, cut short with "…" if wider than `width`. The painter gives the colour.
pub fn truncated(ui: &Ui, text: &str, font: FontId, width: f32) -> Arc<Galley> {
    let mut job = LayoutJob::simple_singleline(text.to_owned(), font, Color32::PLACEHOLDER);
    job.wrap = TextWrapping::truncate_at_width(width);
    ui.painter().layout_job(job)
}

/// Fades a page in over a moment after `since`, sliding it up a few points.
pub fn fade_in(ui: &mut Ui, since: Instant) {
    const TIME: Duration = Duration::from_millis(220);
    let t = (since.elapsed().as_secs_f32() / TIME.as_secs_f32()).min(1.0);
    if t < 1.0 {
        ui.ctx().request_repaint();
    }
    let t = easing::cubic_out(t);
    ui.multiply_opacity(t);
    ui.add_space((1.0 - t) * 10.0);
}

/// How a [`Pill`] button is coloured.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// The one thing to do here, in the accent colour.
    Primary,
    Secondary,
    Danger,
}

/// A rounded button: an optional icon and a label.
pub struct Pill<'a> {
    text: &'a str,
    icon: Option<&'static str>,
    tone: Tone,
    width: Option<f32>,
    small: bool,
}

pub fn primary(text: &str) -> Pill<'_> {
    Pill { text, icon: None, tone: Tone::Primary, width: None, small: false }
}

pub fn secondary(text: &str) -> Pill<'_> {
    Pill { tone: Tone::Secondary, ..primary(text) }
}

pub fn danger(text: &str) -> Pill<'_> {
    Pill { tone: Tone::Danger, ..primary(text) }
}

impl Pill<'_> {
    pub fn icon(mut self, icon: &'static str) -> Self {
        self.icon = Some(icon);
        self
    }

    pub fn width(mut self, width: f32) -> Self {
        self.width = Some(width);
        self
    }

    pub fn small(mut self) -> Self {
        self.small = true;
        self
    }
}

impl Widget for Pill<'_> {
    fn ui(self, ui: &mut Ui) -> Response {
        let (height, font, padding) = if self.small { (28.0, 12.5, 24.0) } else { (34.0, 13.5, 34.0) };
        let galley = ui.painter().layout_job(with_icon(self.icon, self.text, medium(font)));
        let width = self.width.unwrap_or(galley.size().x + padding).max(height);
        let (rect, response) = ui.allocate_exact_size(vec2(width, height), Sense::click());
        response.widget_info(|| WidgetInfo::labeled(Role::Button, ui.is_enabled(), self.text));
        if !ui.is_rect_visible(rect) {
            return response;
        }
        let enabled = response.enabled();
        let hover = ui.ctx().animate_bool_with_time(response.id.with("hover"), enabled && response.hovered(), 0.12);
        let pressed = enabled && response.is_pointer_button_down_on();
        let rect = if pressed { rect.shrink(1.0) } else { rect };

        let (glass, text) = match self.tone {
            Tone::Primary => {
                let lift = 0.12 * hover;
                let top = Color32::from_rgb(134, 140, 255).lerp_to_gamma(Color32::WHITE, lift);
                let bottom = ACCENT_DEEP.lerp_to_gamma(Color32::WHITE, lift * 0.6);
                let glow = ACCENT_DEEP.gamma_multiply(0.35 + 0.25 * hover);
                let glass = Glass {
                    base: Color32::TRANSPARENT,
                    top,
                    bottom,
                    rim_top: white(120),
                    rim_bottom: white(10),
                    shadow: Shadow { offset: [0, 6], blur: 16, spread: 0, color: glow },
                };
                (glass, Color32::WHITE)
            }
            Tone::Secondary => {
                let quiet = Glass { shadow: Shadow::NONE, ..Glass::PANEL };
                let bright = Glass { top: white(46), bottom: white(26), rim_top: white(120), ..quiet };
                (quiet.lerp(&bright, hover), TEXT)
            }
            Tone::Danger => {
                let red = |a: u8| BAD.gamma_multiply(a as f32 / 255.0);
                let glass = Glass {
                    base: Color32::TRANSPARENT,
                    top: red(70 + (40.0 * hover) as u8),
                    bottom: red(40 + (30.0 * hover) as u8),
                    rim_top: white(90),
                    rim_bottom: red(60),
                    shadow: Shadow::NONE,
                };
                (glass, Color32::WHITE)
            }
        };
        let mut painter = ui.painter().clone();
        if !enabled {
            painter.multiply_opacity(0.4);
        }
        painter.add(glass.shapes(rect, height / 2.0));
        painter.galley(rect.center() - galley.size() / 2.0, galley, text);
        response
    }
}

/// A round glass button with just an icon.
pub fn icon_button(ui: &mut Ui, icon: &str, size: f32) -> Response {
    let (rect, response) = ui.allocate_exact_size(vec2(size, size), Sense::click());
    response.widget_info(|| WidgetInfo::labeled(Role::Button, ui.is_enabled(), icon));
    if ui.is_rect_visible(rect) {
        let hover = ui.ctx().animate_bool_with_time(response.id.with("hover"), response.hovered(), 0.12);
        let glass = Glass { shadow: Shadow::NONE, ..Glass::PANEL }
            .lerp(&Glass { shadow: Shadow::NONE, ..Glass::RAISED }, hover);
        ui.painter().add(glass.shapes(rect, size / 2.0));
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            icon,
            icon_font(size * 0.5),
            TEXT.gamma_multiply(0.8 + 0.2 * hover),
        );
    }
    response
}

/// An on/off switch, green when on.
pub fn switch(ui: &mut Ui, on: &mut bool) -> Response {
    let size = vec2(40.0, 24.0);
    let (rect, mut response) = ui.allocate_exact_size(size, Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    response.widget_info(|| WidgetInfo::selected(Role::CheckBox, ui.is_enabled(), *on, ""));
    if ui.is_rect_visible(rect) {
        let t = ui.ctx().animate_bool_with_time_and_easing(response.id, *on, 0.18, easing::cubic_out);
        let hover = response.hovered() as u8 as f32;
        let mut painter = ui.painter().clone();
        if !response.enabled() {
            painter.multiply_opacity(0.4);
        }
        let track = white(40 + (14.0 * hover) as u8).lerp_to_gamma(GOOD, t);
        painter.rect_filled(rect, 12.0, track);
        painter.rect_stroke(rect, 12.0, Stroke::new(1.0, white(30)), StrokeKind::Inside);
        let knob = pos2(egui::lerp(rect.left() + 12.0..=rect.right() - 12.0, t), rect.center().y);
        painter.circle_filled(knob + vec2(0.0, 1.5), 10.0, Color32::from_black_alpha(60));
        painter.circle_filled(knob, 10.0, Color32::WHITE);
    }
    response
}

/// A row of buttons against the right edge, as tall as the buttons. (`with_layout` on its own
/// would take all the height left and centre them in it.)
pub fn right_row<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    ui.horizontal(|ui| ui.with_layout(Layout::right_to_left(Align::Center), add).inner).inner
}

/// `label` on the left and a [`switch`] on the right edge.
pub fn switch_row(ui: &mut Ui, label: impl Into<WidgetText>, on: &mut bool) -> Response {
    ui.horizontal(|ui| {
        ui.label(label);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| switch(ui, on)).inner
    })
    .inner
}

/// A toggle in the shape of a pill, filled with the accent while selected.
pub fn chip(ui: &mut Ui, icon: Option<&str>, label: &str, selected: bool) -> Response {
    let galley = ui.painter().layout_job(with_icon(icon, label, medium(13.0)));
    let size = vec2(galley.size().x + 28.0, 32.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    response.widget_info(|| WidgetInfo::selected(Role::CheckBox, ui.is_enabled(), selected, label));
    if ui.is_rect_visible(rect) {
        let t = ui.ctx().animate_bool_with_time(response.id.with("on"), selected, 0.15);
        let hover = ui.ctx().animate_bool_with_time(response.id.with("hover"), response.hovered(), 0.12);
        let off = white(16 + (16.0 * hover) as u8);
        let on = ACCENT.gamma_multiply(0.38 + 0.12 * hover);
        let mut painter = ui.painter().clone();
        if !response.enabled() {
            painter.multiply_opacity(0.4);
        }
        painter.rect_filled(rect, 16.0, off.lerp_to_gamma(on, t));
        painter.rect_stroke(rect, 16.0, Stroke::new(1.0, white(30).lerp_to_gamma(ACCENT, t)), StrokeKind::Inside);
        painter.galley(
            rect.center() - galley.size() / 2.0,
            galley,
            DIM.lerp_to_gamma(Color32::WHITE, t.max(hover * 0.5)),
        );
    }
    response
}

/// A small coloured dot, for status lines.
pub fn dot(ui: &mut Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 6.0, color.gamma_multiply(0.25));
    ui.painter().circle_filled(rect.center(), 3.5, color);
}
