//! Custom-painted controls. Icons are drawn as vectors (no font glyphs), and every
//! control reports an accessibility label so it can be found by tests and screen readers.

use std::f32::consts::PI;

use eframe::egui;
use egui::{Align2, Color32, FontId, Pos2, Rangef, Rect, Response, Role, Sense, Shape, Stroke, StrokeKind, Ui, Vec2, WidgetInfo, pos2, vec2};

use super::theme;

pub const MIN_DB: f32 = -60.0;
pub const MAX_DB: f32 = 6.0;

pub fn label(resp: &Response, role: Role, text: &str) {
    let text = text.to_string();
    let enabled = resp.enabled();
    resp.widget_info(move || WidgetInfo::labeled(role, enabled, &text));
}

/// Small rounded toggle like Logic's M / S / R / I track buttons.
pub fn chip(ui: &mut Ui, on: bool, text: &str, accent: Color32, a11y: &str, size: Vec2) -> Response {
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    label(&resp, Role::Button, a11y);
    let p = ui.painter();
    let fill = if on {
        accent
    } else if resp.hovered() {
        theme::BG_CONTROL_HOVER
    } else {
        theme::BG_CONTROL
    };
    p.rect_filled(rect, 4.0, fill);
    if !on {
        p.rect_stroke(rect, 4.0, Stroke::new(1.0, theme::BORDER), StrokeKind::Inside);
    }
    let fg = if on { Color32::from_rgb(20, 20, 22) } else { theme::TEXT_DIM };
    p.text(rect.center(), Align2::CENTER_CENTER, text, FontId::proportional(10.5), fg);
    resp.on_hover_text(a11y)
}

#[derive(Clone, Copy)]
pub enum Icon {
    ToStart,
    Play,
    Stop,
    Record,
    Gear,
    Plus,
    Minus,
    Close,
    Power,
    Inspector,
    Pointer,
    Scissors,
    Rewind,
    Forward,
    Metronome,
    Cycle,
}

/// Rounded transport-style button with a vector icon.
pub fn icon_button(ui: &mut Ui, icon: Icon, active: bool, active_color: Color32, a11y: &str, size: Vec2) -> Response {
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    label(&resp, Role::Button, a11y);
    let p = ui.painter();
    let fill = if resp.is_pointer_button_down_on() {
        theme::shade(theme::BG_CONTROL_HOVER, 1.15)
    } else if resp.hovered() {
        theme::BG_CONTROL_HOVER
    } else {
        theme::BG_CONTROL
    };
    raised(p, rect, 6, fill);
    let color = if active { active_color } else { theme::TEXT };
    draw_icon(p, icon, rect.center(), rect.height().min(rect.width()) * 0.3, color);
    resp.on_hover_text(a11y)
}

/// A softly raised surface: drop shadow, fill and a faint top highlight.
fn raised(p: &egui::Painter, rect: Rect, radius: u8, fill: Color32) {
    p.rect_filled(rect.translate(vec2(0.0, 1.0)), radius, Color32::from_black_alpha(70));
    p.rect_filled(rect, radius, fill);
    // Lighter upper half reads as a gentle vertical gradient.
    let top = Rect::from_min_max(rect.min, pos2(rect.right(), rect.center().y));
    p.rect_filled(top, egui::CornerRadius { nw: radius, ne: radius, sw: 0, se: 0 }, Color32::from_white_alpha(5));
    p.hline(
        Rangef::new(rect.left() + radius as f32, rect.right() - radius as f32),
        rect.top() + 0.5,
        Stroke::new(1.0, Color32::from_white_alpha(22)),
    );
}

/// One button in a [`transport`] group.
pub struct TransportButton<'a> {
    pub icon: Icon,
    /// Highlights the segment (e.g. playing / recording).
    pub active: bool,
    pub color: Color32,
    /// Colour of the icon when idle (record stays red but dimmer).
    pub idle_color: Color32,
    pub a11y: &'a str,
}

/// Logic-style connected transport control: one rounded pill split into segments.
/// Returns one response per button. `id_salt` must be unique per group.
pub fn transport(ui: &mut Ui, id_salt: &str, buttons: &[TransportButton], segment: Vec2) -> Vec<Response> {
    let n = buttons.len();
    let (rect, _) = ui.allocate_exact_size(vec2(segment.x * n as f32, segment.y), Sense::hover());
    let radius = 8u8;
    let p = ui.painter().clone();
    raised(&p, rect, radius, Color32::from_rgb(60, 60, 65));

    let mut responses = Vec::with_capacity(n);
    for (i, b) in buttons.iter().enumerate() {
        let seg = Rect::from_min_size(pos2(rect.left() + i as f32 * segment.x, rect.top()), segment);
        let resp = ui.interact(seg, ui.make_persistent_id((id_salt, i)), Sense::click());
        label(&resp, Role::Button, b.a11y);
        let corners = egui::CornerRadius {
            nw: if i == 0 { radius } else { 0 },
            sw: if i == 0 { radius } else { 0 },
            ne: if i == n - 1 { radius } else { 0 },
            se: if i == n - 1 { radius } else { 0 },
        };
        if b.active {
            p.rect_filled(seg, corners, theme::with_alpha(b.color, 30));
        }
        if resp.is_pointer_button_down_on() {
            p.rect_filled(seg, corners, Color32::from_black_alpha(50));
        } else if resp.hovered() {
            p.rect_filled(seg, corners, Color32::from_white_alpha(12));
        }
        if i > 0 {
            p.vline(seg.left(), Rangef::new(seg.top() + 7.0, seg.bottom() - 7.0), Stroke::new(1.0, Color32::from_rgb(38, 38, 42)));
        }
        let c = seg.center();
        let r = segment.y * 0.22;
        let color = if b.active { b.color } else { b.idle_color };
        if b.active {
            // A single soft halo that stays inside the segment.
            p.circle_filled(c, (segment.y * 0.5 - 4.0).min(r * 1.9), theme::with_alpha(b.color, 34));
        }
        draw_icon(&p, b.icon, c, r, color);
        responses.push(resp.on_hover_text(b.a11y));
    }
    responses
}

/// Polygon with rounded corners (quadratic curves at each vertex) for softer icons.
fn rounded_polygon(pts: &[Pos2], radius: f32) -> Vec<Pos2> {
    let n = pts.len();
    let mut out = Vec::with_capacity(n * 6);
    for i in 0..n {
        let (prev, cur, next) = (pts[(i + n - 1) % n], pts[i], pts[(i + 1) % n]);
        let to_prev = (prev - cur).normalized() * radius.min((prev - cur).length() / 2.0);
        let to_next = (next - cur).normalized() * radius.min((next - cur).length() / 2.0);
        let (a, b) = (cur + to_prev, cur + to_next);
        for k in 0..=4 {
            let t = k as f32 / 4.0;
            // Quadratic Bézier a -> cur -> b.
            let q = a.to_vec2() * (1.0 - t) * (1.0 - t) + cur.to_vec2() * 2.0 * (1.0 - t) * t + b.to_vec2() * t * t;
            out.push(q.to_pos2());
        }
    }
    out
}

fn rounded_triangle(p: &egui::Painter, pts: [Pos2; 3], radius: f32, color: Color32) {
    p.add(Shape::convex_polygon(rounded_polygon(&pts, radius), color, Stroke::NONE));
}

/// Segmented-control style tool button; highlighted when active.
pub fn tool_button(ui: &mut Ui, icon: Icon, active: bool, a11y: &str, size: Vec2) -> Response {
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    label(&resp, Role::Button, a11y);
    let p = ui.painter();
    let fill = if active {
        theme::ACCENT
    } else if resp.hovered() {
        theme::BG_CONTROL_HOVER
    } else {
        theme::BG_CONTROL
    };
    raised(p, rect, 5, fill);
    let color = if active { Color32::WHITE } else { theme::TEXT };
    draw_icon(p, icon, rect.center(), rect.height().min(rect.width()) * 0.32, color);
    resp.on_hover_text(a11y)
}

/// Frameless icon (for compact places like slot rows).
pub fn icon_plain(ui: &mut Ui, icon: Icon, color: Color32, a11y: &str, size: f32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    label(&resp, Role::Button, a11y);
    let c = if resp.hovered() { Color32::WHITE } else { color };
    if resp.hovered() {
        ui.painter().rect_filled(rect, 3.0, theme::BG_CONTROL_HOVER);
    }
    draw_icon(ui.painter(), icon, rect.center(), size * 0.32, c);
    resp.on_hover_text(a11y)
}

pub fn draw_icon(p: &egui::Painter, icon: Icon, c: Pos2, r: f32, color: Color32) {
    let stroke = Stroke::new(1.6, color);
    match icon {
        Icon::Play => {
            // Optically centred: the triangle's visual weight sits left of its box centre.
            let cx = c.x + r * 0.12;
            rounded_triangle(p, [pos2(cx - r * 0.75, c.y - r), pos2(cx + r, c.y), pos2(cx - r * 0.75, c.y + r)], r * 0.22, color);
        }
        Icon::Stop => {
            p.rect_filled(Rect::from_center_size(c, Vec2::splat(r * 1.55)), r * 0.25, color);
        }
        Icon::Record => {
            p.circle_filled(c, r * 0.82, color);
        }
        Icon::ToStart => {
            let bar = Rect::from_min_size(pos2(c.x - r * 0.95, c.y - r * 0.85), vec2(r * 0.3, r * 1.7));
            p.rect_filled(bar, r * 0.15, color);
            rounded_triangle(p, [pos2(c.x + r * 0.9, c.y - r * 0.85), pos2(c.x + r * 0.9, c.y + r * 0.85), pos2(c.x - r * 0.5, c.y)], r * 0.18, color);
        }
        Icon::Cycle => {
            // Two arcs with arrowheads forming a loop.
            let s = Stroke::new(1.6, color);
            let (rx, ry) = (r * 1.0, r * 0.62);
            let ellipse = |a0: f32, a1: f32| -> Vec<Pos2> {
                (0..=12).map(|i| a0 + (a1 - a0) * i as f32 / 12.0).map(|a| pos2(c.x + a.cos() * rx, c.y + a.sin() * ry)).collect()
            };
            p.add(Shape::line(ellipse(PI * 0.15, PI * 0.95), s));
            p.add(Shape::line(ellipse(PI * 1.15, PI * 1.95), s));
            let head = |tip: Pos2, dir: f32| {
                let pts = vec![tip, pos2(tip.x - dir * r * 0.45, tip.y - r * 0.32), pos2(tip.x - dir * r * 0.45, tip.y + r * 0.32)];
                p.add(Shape::convex_polygon(pts, color, Stroke::NONE));
            };
            head(pos2(c.x + rx * (PI * 1.95).cos() + r * 0.15, c.y + ry * (PI * 1.95).sin()), 1.0);
            head(pos2(c.x + rx * (PI * 0.95).cos() - r * 0.15, c.y + ry * (PI * 0.95).sin()), -1.0);
        }
        Icon::Metronome => {
            // Pyramid body with a swinging arm.
            let s = Stroke::new(1.4, color);
            let pts = vec![pos2(c.x - r * 0.35, c.y - r), pos2(c.x + r * 0.35, c.y - r), pos2(c.x + r * 0.8, c.y + r), pos2(c.x - r * 0.8, c.y + r)];
            p.add(Shape::closed_line(rounded_polygon(&pts, r * 0.15), s));
            p.line_segment([pos2(c.x - r * 0.62, c.y + r * 0.45), pos2(c.x + r * 0.62, c.y + r * 0.45)], s);
            p.line_segment([pos2(c.x, c.y + r * 0.45), pos2(c.x + r * 0.75, c.y - r * 0.85)], s);
        }
        Icon::Rewind | Icon::Forward => {
            let dir = if matches!(icon, Icon::Forward) { 1.0 } else { -1.0 };
            for off in [-0.5, 0.5] {
                let tip = c.x + dir * (r * 0.5 + off * r * 0.95);
                let base = tip - dir * r * 0.95;
                rounded_triangle(p, [pos2(base, c.y - r * 0.8), pos2(tip, c.y), pos2(base, c.y + r * 0.8)], r * 0.18, color);
            }
        }
        Icon::Plus => {
            p.line_segment([pos2(c.x - r, c.y), pos2(c.x + r, c.y)], stroke);
            p.line_segment([pos2(c.x, c.y - r), pos2(c.x, c.y + r)], stroke);
        }
        Icon::Minus => {
            p.line_segment([pos2(c.x - r, c.y), pos2(c.x + r, c.y)], stroke);
        }
        Icon::Close => {
            let d = r * 0.75;
            p.line_segment([pos2(c.x - d, c.y - d), pos2(c.x + d, c.y + d)], stroke);
            p.line_segment([pos2(c.x - d, c.y + d), pos2(c.x + d, c.y - d)], stroke);
        }
        Icon::Power => {
            arc(p, c, r * 0.85, -PI * 0.5 + 0.7, PI * 1.5 - 0.7, stroke);
            p.line_segment([pos2(c.x, c.y - r * 1.05), pos2(c.x, c.y - r * 0.2)], stroke);
        }
        Icon::Gear => {
            p.circle_stroke(c, r * 0.45, stroke);
            for k in 0..8 {
                let a = k as f32 * PI / 4.0;
                let (s, co) = a.sin_cos();
                p.line_segment([pos2(c.x + co * r * 0.65, c.y + s * r * 0.65), pos2(c.x + co * r, c.y + s * r)], Stroke::new(2.2, color));
            }
        }
        Icon::Pointer => {
            let pts = vec![
                pos2(c.x - r * 0.55, c.y - r),
                pos2(c.x + r * 0.6, c.y + r * 0.15),
                pos2(c.x + r * 0.02, c.y + r * 0.22),
                pos2(c.x - r * 0.3, c.y + r * 0.95),
                pos2(c.x - r * 0.55, c.y + r * 0.6),
            ];
            p.add(Shape::convex_polygon(pts, color, Stroke::NONE));
        }
        Icon::Scissors => {
            let s = Stroke::new(1.4, color);
            p.circle_stroke(pos2(c.x - r * 0.5, c.y + r * 0.6), r * 0.32, s);
            p.circle_stroke(pos2(c.x + r * 0.5, c.y + r * 0.6), r * 0.32, s);
            p.line_segment([pos2(c.x - r * 0.3, c.y + r * 0.35), pos2(c.x + r * 0.45, c.y - r)], s);
            p.line_segment([pos2(c.x + r * 0.3, c.y + r * 0.35), pos2(c.x - r * 0.45, c.y - r)], s);
        }
        Icon::Inspector => {
            p.circle_stroke(c, r, stroke);
            p.circle_filled(pos2(c.x, c.y - r * 0.45), 1.3, color);
            p.line_segment([pos2(c.x, c.y - r * 0.1), pos2(c.x, c.y + r * 0.55)], stroke);
        }
    }
}

fn arc(p: &egui::Painter, c: Pos2, r: f32, a0: f32, a1: f32, stroke: Stroke) {
    let n = (((a1 - a0).abs() * r) / 2.0).ceil().max(4.0) as usize;
    let pts: Vec<Pos2> = (0..=n)
        .map(|i| {
            let a = a0 + (a1 - a0) * i as f32 / n as f32;
            pos2(c.x + a.cos() * r, c.y + a.sin() * r)
        })
        .collect();
    p.add(Shape::line(pts, stroke));
}

/// Rotary knob. Drag vertically (Shift for fine), double-click to reset.
/// `bipolar` draws the value arc from the centre (for pan).
pub fn knob(ui: &mut Ui, value: &mut f32, min: f32, max: f32, default: f32, bipolar: bool, diameter: f32, a11y: &str) -> Response {
    let (rect, mut resp) = ui.allocate_exact_size(Vec2::splat(diameter), Sense::click_and_drag());
    label(&resp, Role::Slider, a11y);
    let range = max - min;
    if resp.dragged() {
        let fine = ui.input(|i| i.modifiers.shift);
        let delta = -resp.drag_delta().y / if fine { 600.0 } else { 150.0 } * range;
        *value = (*value + delta).clamp(min, max);
        resp.mark_changed();
    }
    if resp.double_clicked() {
        *value = default;
        resp.mark_changed();
    }

    let p = ui.painter();
    let c = rect.center();
    let r = diameter * 0.5 - 1.0;
    let (start, end) = (PI * 0.75, PI * 2.25); // 270° sweep, gap at the bottom
    let t = ((*value - min) / range).clamp(0.0, 1.0);
    let angle = start + (end - start) * t;

    p.circle_filled(c, r, theme::shade(theme::BG_CONTROL, 0.8));
    p.circle_stroke(c, r, Stroke::new(1.0, theme::BORDER));
    arc(p, c, r - 2.5, start, end, Stroke::new(2.0, theme::shade(theme::BG_CONTROL, 1.35)));
    let from = if bipolar { (start + end) * 0.5 } else { start };
    if (angle - from).abs() > 0.01 {
        arc(p, c, r - 2.5, from.min(angle), from.max(angle), Stroke::new(2.0, theme::ACCENT));
    }
    let (s, co) = angle.sin_cos();
    p.line_segment([pos2(c.x + co * r * 0.25, c.y + s * r * 0.25), pos2(c.x + co * (r - 4.0), c.y + s * (r - 4.0))], Stroke::new(1.8, theme::TEXT));
    resp
}

/// Fader position (0..1) <-> linear gain, with 0 dB at ~78% travel like hardware faders.
pub fn gain_to_pos(gain: f32) -> f32 {
    let db = gain_to_db(gain);
    if db <= MIN_DB { 0.0 } else { ((db - MIN_DB) / (MAX_DB - MIN_DB)).powf(1.6) }
}

pub fn pos_to_gain(pos: f32) -> f32 {
    if pos <= 0.0 {
        return 0.0;
    }
    db_to_gain(MIN_DB + pos.clamp(0.0, 1.0).powf(1.0 / 1.6) * (MAX_DB - MIN_DB))
}

pub fn gain_to_db(gain: f32) -> f32 {
    if gain <= 0.0 { f32::NEG_INFINITY } else { 20.0 * gain.log10() }
}

pub fn db_to_gain(db: f32) -> f32 {
    if db <= MIN_DB { 0.0 } else { 10f32.powf(db / 20.0) }
}

pub fn format_db(gain: f32) -> String {
    let db = gain_to_db(gain);
    if db <= MIN_DB { "-∞".into() } else { format!("{db:+.1}") }
}

fn fader_interact(ui: &Ui, resp: &mut Response, gain: &mut f32, travel: f32, vertical: bool) {
    if resp.dragged() {
        let fine = ui.input(|i| i.modifiers.shift);
        let d = if vertical { -resp.drag_delta().y } else { resp.drag_delta().x };
        let pos = (gain_to_pos(*gain) + d / travel * if fine { 0.2 } else { 1.0 }).clamp(0.0, 1.0);
        *gain = pos_to_gain(pos);
        resp.mark_changed();
    }
    if resp.double_clicked() {
        *gain = 1.0;
        resp.mark_changed();
    }
}

/// Vertical channel-strip fader with dB scale. Double-click for 0 dB.
pub fn vfader(ui: &mut Ui, gain: &mut f32, size: Vec2, a11y: &str) -> Response {
    let (rect, mut resp) = ui.allocate_exact_size(size, Sense::click_and_drag());
    label(&resp, Role::Slider, a11y);
    let cap_h = 26.0;
    let travel = rect.height() - cap_h;
    fader_interact(ui, &mut resp, gain, travel, true);

    let p = ui.painter();
    let groove_x = rect.center().x + 6.0;
    let top = rect.top() + cap_h * 0.5;
    let bottom = rect.bottom() - cap_h * 0.5;
    let y_of = |gain: f32| bottom - gain_to_pos(gain) * travel;

    for db in [6.0, 0.0, -6.0, -12.0, -24.0, -40.0] {
        let y = y_of(db_to_gain(db));
        let strong = db == 0.0;
        p.line_segment([pos2(rect.left() + 18.0, y), pos2(groove_x - 6.0, y)], Stroke::new(1.0, if strong { theme::TEXT_DIM } else { theme::HAIRLINE }));
        p.text(pos2(rect.left() + 15.0, y), Align2::RIGHT_CENTER, format!("{}", db.abs() as i32), FontId::proportional(9.0), theme::TEXT_FAINT);
    }
    let groove = Rect::from_min_max(pos2(groove_x - 2.0, top), pos2(groove_x + 2.0, bottom));
    p.rect_filled(groove, 2.0, theme::BG_DEEP);
    let y = y_of(*gain);
    p.rect_filled(Rect::from_min_max(pos2(groove_x - 2.0, y), pos2(groove_x + 2.0, bottom)), 2.0, theme::with_alpha(theme::ACCENT, 160));

    let cap = Rect::from_center_size(pos2(groove_x, y), vec2(22.0, cap_h));
    p.rect_filled(cap.translate(vec2(0.0, 2.0)), 4.0, Color32::from_black_alpha(90));
    p.rect_filled(cap, 4.0, if resp.hovered() || resp.dragged() { Color32::from_gray(200) } else { Color32::from_gray(172) });
    p.line_segment([pos2(cap.left() + 3.0, cap.center().y), pos2(cap.right() - 3.0, cap.center().y)], Stroke::new(1.5, Color32::from_gray(60)));
    resp
}

/// Compact horizontal fader for track headers. Double-click for 0 dB.
pub fn hfader(ui: &mut Ui, gain: &mut f32, width: f32, a11y: &str) -> Response {
    let (rect, mut resp) = ui.allocate_exact_size(vec2(width, 14.0), Sense::click_and_drag());
    label(&resp, Role::Slider, a11y);
    let travel = rect.width() - 10.0;
    fader_interact(ui, &mut resp, gain, travel, false);

    let p = ui.painter();
    let y = rect.center().y;
    let (l, r) = (rect.left() + 5.0, rect.right() - 5.0);
    p.rect_filled(Rect::from_min_max(pos2(l, y - 1.5), pos2(r, y + 1.5)), 1.5, theme::BG_DEEP);
    let x = l + gain_to_pos(*gain) * travel;
    p.rect_filled(Rect::from_min_max(pos2(l, y - 1.5), pos2(x, y + 1.5)), 1.5, theme::with_alpha(theme::ACCENT, 170));
    let unity = l + gain_to_pos(1.0) * travel;
    p.line_segment([pos2(unity, y - 5.0), pos2(unity, y - 3.0)], Stroke::new(1.0, theme::TEXT_FAINT));
    let thumb = if resp.hovered() || resp.dragged() { Color32::from_gray(225) } else { Color32::from_gray(185) };
    p.circle_filled(pos2(x, y), 5.0, thumb);
    p.circle_stroke(pos2(x, y), 5.0, Stroke::new(1.0, theme::BORDER));
    resp.on_hover_text(format!("{a11y}: {} dB", format_db(*gain)))
}

fn meter_color(db: f32) -> Color32 {
    if db > -1.0 {
        theme::METER_HIGH
    } else if db > -12.0 {
        theme::METER_MID
    } else {
        theme::METER_LOW
    }
}

fn meter_frac(db: f32) -> f32 {
    ((db - MIN_DB) / (MAX_DB - MIN_DB)).clamp(0.0, 1.0)
}

/// Two stacked horizontal meters (L/R) in one allocation.
pub fn stereo_hmeter(ui: &mut Ui, size: Vec2, db: [f32; 2]) -> Response {
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    let p = ui.painter();
    let h = (rect.height() - 2.0) / 2.0;
    for (i, &d) in db.iter().enumerate() {
        let row = Rect::from_min_size(pos2(rect.left(), rect.top() + i as f32 * (h + 2.0)), vec2(rect.width(), h));
        p.rect_filled(row, 1.5, theme::BG_DEEP);
        let f = meter_frac(d);
        if f > 0.0 {
            p.rect_filled(Rect::from_min_size(row.min, vec2(row.width() * f, h)), 1.5, meter_color(d));
        }
    }
    resp
}

/// Stereo vertical meter with segment gaps.
pub fn vmeter(ui: &mut Ui, size: Vec2, db: [f32; 2]) {
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 2.0, theme::BG_DEEP);
    let w = (rect.width() - 3.0) / 2.0;
    for (i, &d) in db.iter().enumerate() {
        let x = rect.left() + 1.0 + i as f32 * (w + 1.0);
        let h = (rect.height() - 2.0) * meter_frac(d);
        let bar = Rect::from_min_max(pos2(x, rect.bottom() - 1.0 - h), pos2(x + w, rect.bottom() - 1.0));
        if h > 0.0 {
            p.rect_filled(bar, 1.0, meter_color(d));
        }
    }
    // Segment gaps for an LED look.
    let mut y = rect.bottom() - 4.0;
    while y > rect.top() {
        p.hline(rect.x_range(), y, Stroke::new(1.0, theme::BG_DEEP));
        y -= 4.0;
    }
}

/// Small caps section label.
pub fn section(ui: &mut Ui, text: &str) {
    ui.label(egui::RichText::new(text.to_uppercase()).size(9.5).color(theme::TEXT_FAINT).strong());
}
