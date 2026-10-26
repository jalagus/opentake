//! Take lanes (Logic's quick swipe comping). An open take folder shows one lane per take under
//! its track: each take's waveform over the folder, lit where it plays. Swiping across a lane
//! plays that take over the range (snapped); clicking a lane's header plays it all through.

use std::collections::HashMap;
use std::path::PathBuf;

use opentake_engine::project::CompSegment;

use super::*;

/// Height of a take lane.
const TAKE_H: f32 = 30.0;

/// A swipe across a take lane. The comp is recomputed from the folder as it was when the
/// swipe began, so dragging back gives the range back.
pub(super) struct Swipe {
    region: RegionRef,
    take: usize,
    /// Where it began (seconds from the region start).
    from: f64,
    base: Clip,
    /// The comp last sent, so an unmoved pointer sends nothing.
    sent: Vec<CompSegment>,
    /// Undo merge key, one per swipe.
    key: String,
}

/// The open/close triangle left of a take folder's badge.
pub(super) fn toggle_rect(badge: Rect) -> Rect {
    Rect::from_min_max(pos2(badge.left() - 15.0, badge.top()), pos2(badge.left() - 2.0, badge.bottom()))
}

/// A disclosure triangle: pointing right when closed, down when open.
pub(super) fn paint_toggle(painter: &Painter, rect: Rect, open: bool, ink: Color32) {
    let c = rect.center();
    let points = if open { vec![c + vec2(-3.5, -2.0), c + vec2(3.5, -2.0), c + vec2(0.0, 2.5)] } else { vec![c + vec2(-2.0, -3.5), c + vec2(2.5, 0.0), c + vec2(-2.0, 3.5)] };
    painter.add(egui::Shape::convex_polygon(points, ink, Stroke::NONE));
}

/// A subtle tint per take, so a comp's segments tell apart.
fn take_tint(take: usize) -> Color32 {
    Color32::from_white_alpha([0, 26, 12, 38, 6, 20][take % 6])
}

/// A comped folder's body: each segment's take waveform over a tint of its own, the joins
/// marked and each segment numbered by its take.
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_comp(painter: &Painter, body: Rect, row: Rect, view: &View, c: &Clip, waveforms: &HashMap<PathBuf, Arc<Waveform>>, color: Color32, ink: Color32) {
    let painter = painter.with_clip_rect(body.intersect(row).intersect(painter.clip_rect()));
    for (s, (part, _)) in c.comp_segments().iter().zip(c.played_parts(Default::default())) {
        let (x0, x1) = (view.x_of(c.position_secs + s.start_secs), view.x_of(c.position_secs + s.end_secs));
        let seg = Rect::from_x_y_ranges(Rangef::new(x0, x1), body.y_range());
        painter.rect_filled(seg, 0.0, take_tint(s.take));
        if let Some(wf) = waveforms.get(&part.file) {
            draw_waveform(&painter, seg, row, view, &part, wf, ink);
        }
        if s.start_secs > 0.0 {
            painter.vline(x0, body.y_range(), Stroke::new(1.0, theme::shade(color, 0.25)));
        }
        if x1 - x0 > 14.0 {
            painter.text(pos2(x0 + 3.0, body.top() + 1.0), Align2::LEFT_TOP, (s.take + 1).to_string(), FontId::proportional(9.0), theme::shade(color, 0.3));
        }
    }
}

/// The take lanes of a track's open folders. Returns the bottom of the last one, if any.
pub(super) fn take_lanes(app: &mut DawApp, ui: &mut egui::Ui, ti: usize, track: &Track, snap: &Snapshot, map: &TempoMap, grid: Option<Grid>) -> Option<f32> {
    let mut bottom = None;
    for (ci, c) in track.clips.iter().enumerate().filter(|(_, c)| c.show_takes && !c.takes.is_empty()) {
        for k in 0..c.takes.len() {
            let r = ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
                take_header(app, ui, ti, ci, c, k);
                take_area(app, ui, ti, ci, c, k, snap, map, grid);
            });
            bottom = Some(r.response.rect.bottom());
        }
    }
    bottom
}

/// A take lane's header: the take's number and name; a click plays the take all through.
fn take_header(app: &mut DawApp, ui: &mut egui::Ui, ti: usize, ci: usize, c: &Clip, k: usize) {
    let rect = lane_header_rect(app, ui, TAKE_H);
    let resp = ui.interact(rect, ui.make_persistent_id(("take_header", ti, ci, k)), Sense::click());
    widgets::label(&resp, egui::Role::Button, &format!("Take Lane {} {}", c.name, c.takes[k].name));
    let color = theme::track_color(ti);
    let plays = c.comp_segments().iter().any(|s| s.take == k);
    let whole = k == c.active_take && !c.is_comped();
    let p = ui.painter();
    p.rect_filled(rect, 0.0, if resp.hovered() { theme::BG_HEADER } else { theme::shade(theme::BG_HEADER, 0.9) });
    p.rect_filled(Rect::from_min_size(rect.min, vec2(4.0, rect.height())), 0.0, theme::with_alpha(color, 110));
    p.hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, theme::BORDER));
    p.vline(rect.right() - 0.5, rect.y_range(), Stroke::new(1.0, theme::BORDER));
    // The number on a chip, lit while the take plays somewhere.
    let chip = Rect::from_center_size(pos2(rect.left() + 44.0, rect.center().y), vec2(22.0, 16.0));
    p.rect_filled(chip, 4.0, if plays { color } else { theme::BG_CONTROL });
    p.text(chip.center(), Align2::CENTER_CENTER, (k + 1).to_string(), FontId::proportional(10.5), if plays { Color32::from_rgb(16, 16, 18) } else { theme::TEXT_DIM });
    p.text(pos2(chip.right() + 8.0, rect.center().y), Align2::LEFT_CENTER, &c.takes[k].name, FontId::proportional(11.5), if plays { theme::TEXT } else { theme::TEXT_DIM });
    if whole {
        p.text(pos2(rect.right() - 12.0, rect.center().y), Align2::RIGHT_CENTER, "whole take", FontId::proportional(9.5), theme::TEXT_FAINT);
    }
    if resp.on_hover_text(format!("Play {} all through (clears the comp)", c.takes[k].name)).clicked() && !whole {
        app.act(Action::SelectTake { track: ti, clip: ci, take: k });
    }
}

/// A take lane: the take's waveform over the folder's span, bright where it plays and dim
/// elsewhere. Swipe to play it over a range.
#[allow(clippy::too_many_arguments)]
fn take_area(app: &mut DawApp, ui: &mut egui::Ui, ti: usize, ci: usize, c: &Clip, k: usize, snap: &Snapshot, map: &TempoMap, grid: Option<Grid>) {
    let project = &snap.project;
    let r = RegionRef::audio(ti, ci);
    let (resp, painter) = ui.allocate_painter(vec2(ui.available_width(), TAKE_H), Sense::click_and_drag());
    widgets::label(&resp, egui::Role::Group, &format!("Take {} {}", c.takes[k].name, c.name));
    let rect = resp.rect;
    let painter = painter.with_clip_rect(rect);
    painter.rect_filled(rect, 0.0, theme::shade(theme::BG_LANE, 0.85));
    draw_grid(&painter, rect, &app.view, map, grid);
    painter.hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, theme::BORDER));

    // The take as a plain region over the folder.
    let take = &c.takes[k];
    let clip = Clip { file: take.file.clone(), source_offset_secs: c.source_offset_secs + take.offset_secs, takes: Vec::new(), comp: Vec::new(), ..c.clone() };
    let color = if c.mute { Color32::from_gray(110) } else { theme::track_color(ti) };
    let x_of = |rel: f64| app.view.x_of(c.position_secs + rel);
    let span = Rect::from_min_max(pos2(x_of(0.0), rect.top() + 2.0), pos2(x_of(c.length_secs), rect.bottom() - 2.0));
    let wf = snap.waveforms.get(&take.file);
    painter.rect_filled(span, 3.0, theme::shade(color, 0.2));
    if let Some(wf) = wf {
        draw_waveform(&painter, span, rect, &app.view, &clip, wf, theme::shade(color, 0.45));
    }
    for s in c.comp_segments().iter().filter(|s| s.take == k) {
        let seg = Rect::from_x_y_ranges(Rangef::new(x_of(s.start_secs), x_of(s.end_secs)), span.y_range());
        painter.rect_filled(seg, 3.0, theme::shade(color, 0.7));
        if let Some(wf) = wf {
            draw_waveform(&painter, seg, rect, &app.view, &clip, wf, theme::shade(color, 0.2));
        }
    }
    painter.rect_stroke(span, 3.0, Stroke::new(1.0, theme::shade(color, 0.4)), StrokeKind::Inside);
    draw_playhead(&painter, rect, &app.view, snap.status.position_secs, false);
    if resp.hover_pos().is_some_and(|p| span.x_range().contains(p.x)) {
        ui.ctx().set_cursor_icon(CursorIcon::Crosshair);
    }

    // Swipe: from where the button went down to the pointer, snapped.
    let at = |app: &DawApp, ui: &egui::Ui, x: f32| snap_time(app, ui, app.view.t_of(x), project) - c.position_secs;
    if resp.drag_started() {
        if let Some(p) = ui.input(|i| i.pointer.press_origin()) {
            app.view.swipes += 1;
            let key = format!("swipe:{}:{ci}:{}", project.tracks[ti].id, app.view.swipes);
            app.view.swipe = Some(Swipe { region: r, take: k, from: at(app, ui, p.x), base: c.clone(), sent: c.comp.clone(), key });
            app.selection = vec![r];
            app.selected_track = Some(ti);
        }
    }
    if resp.dragged() {
        let to = resp.interact_pointer_pos().map(|p| at(app, ui, p.x));
        if let (Some(to), Some(s)) = (to, app.view.swipe.as_mut().filter(|s| s.region == r && s.take == k)) {
            let mut next = s.base.clone();
            next.set_comp_range(s.from, to, k);
            if next.comp != s.sent {
                s.sent = next.comp.clone();
                let (comp, merge) = (next.comp, s.key.clone());
                app.act(Action::SetComp { track: ti, clip: ci, comp, merge });
            }
        }
    }
    if resp.drag_stopped() {
        app.view.swipe = None;
    }
    if resp.clicked() {
        app.selection = vec![r];
        app.selected_track = Some(ti);
    }
}
