//! Piano roll editor for the selected MIDI region.

use opentake_engine::project::{MidiClip, Note, Project};
use eframe::egui;
use egui::{Align, Align2, Color32, CursorIcon, FontId, Layout, Pos2, Rangef, Rect, RichText, Sense, Stroke, StrokeKind, UiBuilder, pos2, vec2};

use super::timeline::{self, grid_at, grid_step_secs, snap_to};
use super::widgets;
use super::{Action, DawApp, Snapshot, theme};

const KEYS_W: f32 = 58.0;
const TOOLBAR_H: f32 = 30.0;
const RULER_H: f32 = 18.0;
const EDGE_GRAB: f32 = 6.0;
const NOTE_NAMES: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];

fn is_black(pitch: u8) -> bool {
    matches!(pitch % 12, 1 | 3 | 6 | 8 | 10)
}

/// Logic's naming: MIDI 60 = C3.
pub fn note_name(pitch: u8) -> String {
    format!("{}{}", NOTE_NAMES[(pitch % 12) as usize], pitch as i32 / 12 - 2)
}

#[derive(Clone, Copy, PartialEq)]
enum DragMode {
    Move,
    Resize,
}

struct NoteDrag {
    mode: DragMode,
    /// ⌘ held while moving: drop copies and keep the originals.
    copy: bool,
    /// Pointer position at press, in (region seconds, pitch).
    origin_t: f64,
    origin_pitch: i32,
    /// Current offset applied to the selected notes.
    dt: f64,
    dpitch: i32,
}

/// Piano roll view state (lives in the app; the region itself lives in the engine).
pub struct PianoRoll {
    pub region: Option<(usize, usize)>,
    /// Highest pitch visible at the top edge.
    top_pitch: f32,
    key_h: f32,
    pub selected: Vec<Note>,
    drag: Option<NoteDrag>,
    /// Rubber-band selection: (origin, current, selection before it started).
    marquee: Option<(Pos2, Pos2, Vec<Note>)>,
    /// Note being auditioned (track, pitch).
    audition: Option<(usize, u8)>,
    /// Velocity used for new notes and shown for the selection.
    velocity: u8,
    centered_for: Option<(usize, usize)>,
    /// Last frame's geometry, so tests can locate notes on screen.
    #[cfg(test)]
    pub geo: Option<(Rect, f32, f32, f32, f64)>,
}

impl Default for PianoRoll {
    fn default() -> Self {
        Self {
            region: None,
            top_pitch: 84.0,
            key_h: 14.0,
            selected: Vec::new(),
            drag: None,
            marquee: None,
            audition: None,
            velocity: 100,
            centered_for: None,
            #[cfg(test)]
            geo: None,
        }
    }
}

impl PianoRoll {
    pub fn open(&mut self, track: usize, clip: usize) {
        if self.region != Some((track, clip)) {
            self.region = Some((track, clip));
            self.selected.clear();
            self.drag = None;
        }
    }
}

struct Geo {
    grid: Rect,
    top_pitch: f32,
    key_h: f32,
    px_per_sec: f32,
    start: f64,
}

impl Geo {
    fn y_of(&self, pitch: u8) -> f32 {
        self.grid.top() + (self.top_pitch - pitch as f32) * self.key_h
    }

    fn pitch_at(&self, y: f32) -> i32 {
        (self.top_pitch - (y - self.grid.top()) / self.key_h).ceil() as i32
    }

    fn x_of(&self, t: f64) -> f32 {
        self.grid.left() + ((t - self.start) * self.px_per_sec as f64) as f32
    }

    fn t_of(&self, x: f32) -> f64 {
        self.start + ((x - self.grid.left()) / self.px_per_sec) as f64
    }

    fn note_rect(&self, n: &Note) -> Rect {
        let y = self.y_of(n.pitch);
        Rect::from_min_max(pos2(self.x_of(n.start_secs), y + 0.5), pos2(self.x_of(n.start_secs + n.length_secs), y + self.key_h - 0.5))
    }
}

/// Apply a drag offset to the selected notes.
fn dragged_notes(notes: &[Note], selected: &[Note], drag: &NoteDrag, region_len: f64) -> Vec<Note> {
    notes
        .iter()
        .map(|n| {
            if !selected.contains(n) {
                return *n;
            }
            match drag.mode {
                DragMode::Move => Note {
                    start_secs: (n.start_secs + drag.dt).clamp(0.0, (region_len - 0.005).max(0.0)),
                    pitch: (n.pitch as i32 + drag.dpitch).clamp(0, 127) as u8,
                    ..*n
                },
                DragMode::Resize => Note { length_secs: (n.length_secs + drag.dt).max(0.01), ..*n },
            }
        })
        .collect()
}

pub fn show(app: &mut DawApp, ui: &mut egui::Ui, snap: &Snapshot) {
    let region = app.piano.region.and_then(|(t, c)| snap.project.tracks.get(t).and_then(|tr| tr.midi_clips.get(c)).map(|clip| (t, c, clip)));
    let Some((track, ci, clip)) = region else {
        app.piano.region = None;
        ui.centered_and_justified(|ui| {
            ui.label(
                RichText::new("Double-click a MIDI region to edit it here.\nCreate one by double-clicking an instrument track's lane.")
                    .color(theme::TEXT_FAINT),
            );
        });
        return;
    };
    let project = &snap.project;
    let full = ui.available_rect_before_wrap();
    // The grid shares the arrangement's time axis: it starts where the lanes start, with
    // the keyboard right of the track-header column, so zoom and scroll stay in sync.
    let lanes = app.view.lanes();
    let grid_left = if lanes.min - KEYS_W >= full.left() { lanes.min } else { full.left() + KEYS_W };
    let toolbar = Rect::from_min_size(full.min, vec2(full.width(), TOOLBAR_H));
    let ruler = Rect::from_min_max(pos2(grid_left, toolbar.bottom()), pos2(full.right(), toolbar.bottom() + RULER_H));
    let body = Rect::from_min_max(pos2(full.left(), ruler.bottom()), full.max);
    let keys = Rect::from_min_max(pos2(grid_left - KEYS_W, body.top()), pos2(grid_left, body.bottom()));
    let grid = Rect::from_min_max(pos2(grid_left, body.top()), body.max);
    let gutter = Rect::from_min_max(pos2(full.left(), toolbar.bottom()), pos2(keys.left(), full.bottom()));

    // Center the view on the notes the first time a region is shown.
    if app.piano.centered_for != Some((track, ci)) {
        app.piano.centered_for = Some((track, ci));
        let mid = if clip.notes.is_empty() {
            60.0
        } else {
            clip.notes.iter().map(|n| n.pitch as f32).sum::<f32>() / clip.notes.len() as f32
        };
        app.piano.top_pitch = (mid + grid.height() / app.piano.key_h / 2.0).clamp(20.0, 127.0);
        // Bring the region into view if it's scrolled off (the zoom stays shared).
        let (t0, t1) = (app.view.start, app.view.start + (grid.width() / app.view.px_per_sec) as f64);
        if clip.position_secs < t0 || clip.position_secs > t1 {
            app.view.start = (clip.position_secs - (20.0 / app.view.px_per_sec) as f64).max(0.0);
        }
    }

    toolbar_ui(app, ui, toolbar, track, ci, clip, project, snap.status.position_secs);
    handle_scroll_zoom(app, ui, body);

    let geo = Geo { grid, top_pitch: app.piano.top_pitch, key_h: app.piano.key_h, px_per_sec: app.view.px_per_sec, start: app.view.start - clip.position_secs };
    #[cfg(test)]
    {
        app.piano.geo = Some((geo.grid, geo.top_pitch, geo.key_h, geo.px_per_sec, geo.start));
    }
    let color = theme::track_color(track);
    if gutter.width() > 8.0 {
        let p = ui.painter().with_clip_rect(gutter);
        p.rect_filled(gutter, 0.0, theme::BG_HEADER);
        p.rect_filled(Rect::from_min_size(gutter.min + vec2(8.0, 10.0), vec2(3.0, 28.0)), 1.5, color);
        p.text(gutter.min + vec2(18.0, 10.0), Align2::LEFT_TOP, &project.tracks[track].name, FontId::proportional(12.0), theme::TEXT);
        p.text(gutter.min + vec2(18.0, 26.0), Align2::LEFT_TOP, &clip.name, FontId::proportional(10.5), theme::TEXT_FAINT);
    }
    draw_background(ui, &geo, clip, project, app);
    draw_ruler(ui, &app.view, ruler, project);
    let grid_resp = ui.interact(grid, ui.make_persistent_id(("piano_grid", track, ci)), Sense::click_and_drag());
    widgets::label(&grid_resp, egui::Role::Group, "Piano Roll Grid");
    notes_interaction(app, ui, &geo, &grid_resp, track, ci, clip, project, color);
    keyboard(app, ui, &geo, keys, track);

    // Playhead (relative to the region).
    let rel = snap.status.position_secs - clip.position_secs;
    let x = geo.x_of(rel);
    if rel >= 0.0 && x >= grid.left() && x <= grid.right() {
        ui.painter().vline(x, grid.y_range(), Stroke::new(1.0, theme::PLAYHEAD));
    }
    ui.allocate_rect(full, Sense::hover());
}

#[allow(clippy::too_many_arguments)]
fn toolbar_ui(app: &mut DawApp, ui: &mut egui::Ui, rect: Rect, track: usize, ci: usize, clip: &MidiClip, project: &Project, playhead: f64) {
    ui.painter().rect_filled(rect, 0.0, theme::BG_HEADER);
    ui.scope_builder(UiBuilder::new().max_rect(rect.shrink2(vec2(8.0, 4.0))).layout(Layout::left_to_right(Align::Center)), |ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        ui.label(RichText::new(&clip.name).strong());
        ui.label(RichText::new(format!("{} notes", clip.notes.len())).size(10.5).color(theme::TEXT_FAINT));
        ui.separator();

        // Velocity of the selection (or of new notes when nothing is selected).
        let sel: Vec<&Note> = clip.notes.iter().filter(|n| app.piano.selected.contains(n)).collect();
        let mut vel = if sel.is_empty() {
            app.piano.velocity as f32
        } else {
            sel.iter().map(|n| n.velocity as f32).sum::<f32>() / sel.len() as f32
        };
        ui.label(RichText::new("Velocity").size(11.0).color(theme::TEXT_DIM));
        let r = ui.add(egui::Slider::new(&mut vel, 1.0..=127.0).show_value(true).fixed_decimals(0).clamping(egui::SliderClamping::Always));
        if r.changed() {
            app.piano.velocity = vel.round() as u8;
        }
        if (r.drag_stopped() || (r.changed() && !r.dragged())) && !sel.is_empty() {
            let v = vel.round().clamp(1.0, 127.0) as u8;
            let notes: Vec<Note> = clip.notes.iter().map(|n| if app.piano.selected.contains(n) { Note { velocity: v, ..*n } } else { *n }).collect();
            app.piano.selected = notes.iter().filter(|n| n.velocity == v && sel.iter().any(|s| s.start_secs == n.start_secs && s.pitch == n.pitch)).copied().collect();
            app.act(Action::SetNotes { track, clip: ci, notes });
        }

        let map = project.tempo_map();
        let grid = grid_at(app.snap_mode, &map, clip.position_secs, app.view.px_per_sec);
        let quantize = ui.add_enabled(grid.is_some(), egui::Button::new("Quantize")).on_hover_text("Snap note starts to the grid (selected notes, or all)");
        if quantize.clicked() {
            if let Some(g) = grid {
                let all = app.piano.selected.is_empty();
                let q = |n: &Note| {
                    let abs = clip.position_secs + n.start_secs;
                    Note { start_secs: (map.snap(abs, g) - clip.position_secs).max(0.0), ..*n }
                };
                let notes: Vec<Note> = clip.notes.iter().map(|n| if all || app.piano.selected.contains(n) { q(n) } else { *n }).collect();
                app.piano.selected.clear();
                app.act(Action::SetNotes { track, clip: ci, notes });
            }
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            timeline::zoom_controls(app, ui, "piano_zoom", playhead);
            ui.label(RichText::new("Double-click: add note · Drag: move · Edge: resize · ⌫: delete").size(10.0).color(theme::TEXT_FAINT));
        });
    });
}

fn handle_scroll_zoom(app: &mut DawApp, ui: &egui::Ui, body: Rect) {
    let (zoom, scroll, hover) = ui.input(|i| (i.zoom_delta(), i.smooth_scroll_delta, i.pointer.hover_pos()));
    if !hover.is_some_and(|p| body.contains(p)) {
        return;
    }
    if scroll.y != 0.0 && zoom == 1.0 {
        app.piano.top_pitch = (app.piano.top_pitch + scroll.y / app.piano.key_h).clamp(10.0, 127.0);
    }
    // Horizontal zoom and scroll move the shared timeline view.
    app.view.handle_zoom_scroll(ui, body);
}

/// Bar numbers and beat ticks (project bars, so they match the arrangement) above the grid.
/// The grid shares the arrangement's time axis, so the timeline's tempo-map helpers apply.
fn draw_ruler(ui: &egui::Ui, view: &timeline::View, rect: Rect, project: &Project) {
    let p = ui.painter().with_clip_rect(rect);
    p.rect_filled(rect, 0.0, theme::BG_HEADER);
    let map = project.tempo_map();
    let (bars, step) = timeline::visible_bars(view, &map);
    if step == 1 {
        timeline::lines_in_bars(&bars, &map, view, 12.0, |b| opentake_engine::project::tempo::beat_len_q(b.sig), |x| {
            p.vline(x, Rangef::new(rect.bottom() - 4.0, rect.bottom()), Stroke::new(1.0, theme::HAIRLINE));
        });
    }
    for b in bars.iter().filter(|b| b.index % step == 0 || b.sig_change) {
        let x = view.x(b.secs);
        p.vline(x, Rangef::new(rect.top() + 4.0, rect.bottom()), Stroke::new(1.0, theme::TEXT_FAINT));
        p.text(pos2(x + 3.0, rect.top() + 2.0), Align2::LEFT_TOP, (b.index + 1).to_string(), FontId::proportional(10.0), theme::TEXT_DIM);
    }
    p.hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, theme::BORDER));
}

fn draw_background(ui: &egui::Ui, geo: &Geo, clip: &MidiClip, project: &Project, app: &DawApp) {
    let p = ui.painter().with_clip_rect(geo.grid);
    let g = geo.grid;
    p.rect_filled(g, 0.0, theme::BG_LANE);
    // Rows: black keys darker, a line under every C.
    let lowest = geo.pitch_at(g.bottom()).max(0);
    let highest = (geo.top_pitch.ceil() as i32).min(127);
    for pitch in lowest..=highest {
        let pitch = pitch as u8;
        let y = geo.y_of(pitch);
        if is_black(pitch) {
            p.rect_filled(Rect::from_min_max(pos2(g.left(), y), pos2(g.right(), y + geo.key_h)), 0.0, theme::BG_DEEP);
        }
        if pitch % 12 == 0 {
            p.hline(g.x_range(), y + geo.key_h, Stroke::new(1.0, theme::GRID_BAR));
        }
    }
    // Vertical grid: the project's bars and beats (tempo map), as in the arrangement.
    let map = project.tempo_map();
    let mid = app.view.start + ((g.center().x - g.left()) / app.view.px_per_sec) as f64;
    let grid = grid_at(app.snap_mode, &map, mid, geo.px_per_sec);
    timeline::draw_grid(&p, g, &app.view, &map, grid);
    // Shade outside the region.
    let end_x = geo.x_of(clip.length_secs);
    if end_x < g.right() {
        p.rect_filled(Rect::from_min_max(pos2(end_x, g.top()), g.max), 0.0, Color32::from_black_alpha(90));
        p.vline(end_x, g.y_range(), Stroke::new(1.0, theme::TEXT_FAINT));
    }
    let start_x = geo.x_of(0.0);
    if start_x > g.left() {
        p.rect_filled(Rect::from_min_max(g.min, pos2(start_x, g.bottom())), 0.0, Color32::from_black_alpha(90));
    }
}

#[allow(clippy::too_many_arguments)]
fn notes_interaction(
    app: &mut DawApp,
    ui: &egui::Ui,
    geo: &Geo,
    resp: &egui::Response,
    track: usize,
    ci: usize,
    clip: &MidiClip,
    project: &Project,
    color: Color32,
) {
    let snap = |app: &DawApp, ui: &egui::Ui, rel: f64| snap_to(app.snap_mode, ui, clip.position_secs + rel, project, geo.px_per_sec) - clip.position_secs;
    let hit = |pos: Pos2| -> Option<(Note, bool)> {
        clip.notes.iter().rev().find_map(|n| {
            let r = geo.note_rect(n);
            r.expand2(vec2(0.0, 0.5)).contains(pos).then(|| (*n, pos.x >= r.right() - EDGE_GRAB.min(r.width() / 3.0)))
        })
    };

    // Cursor feedback.
    if let Some(p) = resp.hover_pos() {
        match (app.piano.drag.as_ref().map(|d| (d.mode, d.copy)), hit(p)) {
            (Some((DragMode::Resize, _)), _) | (None, Some((_, true))) => ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal),
            (Some((DragMode::Move, true)), _) => ui.ctx().set_cursor_icon(CursorIcon::Copy),
            (Some((DragMode::Move, false)), _) => ui.ctx().set_cursor_icon(CursorIcon::Grabbing),
            _ => {}
        }
    }

    let shift = ui.input(|i| i.modifiers.shift);
    let pointer = resp.interact_pointer_pos();
    if resp.double_clicked() {
        if let Some(p) = pointer.filter(|p| hit(*p).is_none()) {
            let t = snap(app, ui, geo.t_of(p.x)).max(0.0);
            if t < clip.length_secs {
                let map = project.tempo_map();
                let at = clip.position_secs + t;
                let len = grid_at(app.snap_mode, &map, at, geo.px_per_sec).map_or(map.beat_secs_at(at) / 4.0, |g| grid_step_secs(g, &map, at));
                let pitch = geo.pitch_at(p.y).clamp(0, 127) as u8;
                let note = Note { start_secs: t, length_secs: len.min(clip.length_secs - t).max(0.01), pitch, velocity: app.piano.velocity, tab: None };
                let mut notes = clip.notes.clone();
                notes.push(note);
                app.piano.selected = vec![note];
                app.act(Action::SetNotes { track, clip: ci, notes });
            }
        }
    } else if resp.clicked() {
        match pointer.and_then(hit) {
            Some((n, _)) if shift => {
                if let Some(i) = app.piano.selected.iter().position(|s| *s == n) {
                    app.piano.selected.remove(i);
                } else {
                    app.piano.selected.push(n);
                }
            }
            Some((n, _)) => app.piano.selected = vec![n],
            None if !shift => app.piano.selected.clear(),
            None => {}
        }
    }

    if resp.drag_started() {
        let origin = ui.input(|i| i.pointer.press_origin()).or(pointer);
        if let Some(p) = origin {
            if let Some((n, edge)) = hit(p) {
                if !app.piano.selected.contains(&n) {
                    app.piano.selected = vec![n];
                }
                app.piano.drag = Some(NoteDrag {
                    mode: if edge { DragMode::Resize } else { DragMode::Move },
                    copy: false,
                    origin_t: geo.t_of(p.x),
                    origin_pitch: geo.pitch_at(p.y),
                    dt: 0.0,
                    dpitch: 0,
                });
                audition(app, track, Some(n.pitch));
            } else {
                // Empty space: rubber-band selection (Shift adds to the current selection).
                let base = if shift { app.piano.selected.clone() } else { Vec::new() };
                app.piano.marquee = Some((p, p, base));
            }
        }
    }
    if resp.dragged() {
        if let (Some(p), Some(m)) = (pointer, app.piano.marquee.as_mut()) {
            m.1 = p;
            let area = Rect::from_two_pos(m.0, m.1);
            let mut selected = m.2.clone();
            for n in clip.notes.iter().filter(|n| geo.note_rect(n).intersects(area)) {
                if !selected.contains(n) {
                    selected.push(*n);
                }
            }
            app.piano.selected = selected;
        }
    }
    if resp.dragged() {
        if let (Some(p), Some(origin_t)) = (pointer, app.piano.drag.as_ref().map(|d| d.origin_t)) {
            // Snap the moved edge of the grabbed note, so notes land on the grid.
            let anchor = app.piano.selected.first().copied();
            let mode = app.piano.drag.as_ref().map(|d| d.mode);
            let raw = geo.t_of(p.x) - origin_t;
            let dt = match (anchor, mode) {
                (Some(a), Some(DragMode::Move)) => snap(app, ui, a.start_secs + raw) - a.start_secs,
                (Some(a), Some(DragMode::Resize)) => snap(app, ui, a.start_secs + a.length_secs + raw) - (a.start_secs + a.length_secs),
                _ => raw,
            };
            let pitch = geo.pitch_at(p.y);
            let copy = ui.input(|i| i.modifiers.command);
            if let Some(d) = app.piano.drag.as_mut() {
                d.dt = dt;
                d.copy = copy && d.mode == DragMode::Move;
                if d.mode == DragMode::Move {
                    let new = pitch - d.origin_pitch;
                    if new != d.dpitch {
                        d.dpitch = new;
                        let a = anchor.map(|a| (a.pitch as i32 + new).clamp(0, 127) as u8);
                        audition(app, track, a);
                    }
                }
            }
        }
    }
    if resp.drag_stopped() {
        audition(app, track, None);
        app.piano.marquee = None;
        if let Some(d) = app.piano.drag.take() {
            if d.dt != 0.0 || d.dpitch != 0 {
                let moved: Vec<Note> = dragged_notes(&app.piano.selected, &app.piano.selected, &d, clip.length_secs);
                let notes = if d.copy {
                    clip.notes.iter().copied().chain(moved.iter().copied()).collect()
                } else {
                    dragged_notes(&clip.notes, &app.piano.selected, &d, clip.length_secs)
                };
                app.piano.selected = moved;
                app.act(Action::SetNotes { track, clip: ci, notes });
            }
        }
    }

    // Draw notes (with the drag preview applied).
    let shown = match &app.piano.drag {
        // Copying: originals stay, copies follow the pointer.
        Some(d) if d.copy => clip.notes.iter().copied().chain(dragged_notes(&app.piano.selected, &app.piano.selected, d, clip.length_secs)).collect(),
        Some(d) => dragged_notes(&clip.notes, &app.piano.selected, d, clip.length_secs),
        None => clip.notes.clone(),
    };
    let preview_selected = match &app.piano.drag {
        Some(d) => dragged_notes(&app.piano.selected, &app.piano.selected, d, clip.length_secs),
        None => app.piano.selected.clone(),
    };
    let p = ui.painter().with_clip_rect(geo.grid);
    for n in &shown {
        let r = geo.note_rect(n);
        let selected = preview_selected.contains(n);
        let brightness = 0.55 + 0.45 * (n.velocity as f32 / 127.0);
        p.rect_filled(r, 2.0, theme::shade(color, if selected { 1.15 } else { brightness }));
        p.rect_stroke(r, 2.0, Stroke::new(1.0, if selected { Color32::WHITE } else { theme::shade(color, 0.35) }), StrokeKind::Inside);
        if r.width() > 28.0 && geo.key_h >= 11.0 {
            p.text(r.left_center() + vec2(3.0, 0.0), Align2::LEFT_CENTER, note_name(n.pitch), FontId::proportional(9.0), Color32::from_rgb(20, 20, 22));
        }
    }
    if let Some((a, b, _)) = app.piano.marquee {
        let area = Rect::from_two_pos(a, b);
        p.rect_filled(area, 2.0, theme::with_alpha(theme::ACCENT, 36));
        p.rect_stroke(area, 2.0, Stroke::new(1.0, theme::with_alpha(theme::ACCENT, 200)), StrokeKind::Inside);
    }
}

/// Play `pitch` (releasing any previous audition note), or just release with `None`.
fn audition(app: &mut DawApp, track: usize, pitch: Option<u8>) {
    if let Some((t, p)) = app.piano.audition.take() {
        app.act(Action::LiveNote { track: t, pitch: p, velocity: 0 });
    }
    if let Some(p) = pitch {
        app.act(Action::LiveNote { track, pitch: p, velocity: app.piano.velocity });
        app.piano.audition = Some((track, p));
    }
}

fn keyboard(app: &mut DawApp, ui: &egui::Ui, geo: &Geo, rect: Rect, track: usize) {
    let resp = ui.interact(rect, ui.make_persistent_id(("piano_keys", track)), Sense::click_and_drag());
    widgets::label(&resp, egui::Role::Group, "Piano Keyboard");
    let p = ui.painter().with_clip_rect(rect);
    p.rect_filled(rect, 0.0, Color32::from_gray(225));
    let lowest = geo.pitch_at(rect.bottom()).max(0);
    let highest = (geo.top_pitch.ceil() as i32).min(127);
    let pressed = app.piano.audition.filter(|(t, _)| *t == track).map(|(_, p)| p);
    for pitch in lowest..=highest {
        let pitch = pitch as u8;
        let y = geo.y_of(pitch);
        let row = Rect::from_min_max(pos2(rect.left(), y), pos2(rect.right(), y + geo.key_h));
        if is_black(pitch) {
            let key = Rect::from_min_max(row.min, pos2(rect.left() + rect.width() * 0.62, row.bottom()));
            p.rect_filled(key, egui::CornerRadius { ne: 2, se: 2, nw: 0, sw: 0 }, if pressed == Some(pitch) { theme::ACCENT } else { Color32::from_gray(30) });
        } else {
            if pressed == Some(pitch) {
                p.rect_filled(row, 0.0, theme::with_alpha(theme::ACCENT, 140));
            }
            if matches!(pitch % 12, 0 | 5) {
                p.hline(rect.x_range(), row.bottom(), Stroke::new(1.0, Color32::from_gray(150)));
            }
            if pitch % 12 == 0 {
                p.text(pos2(rect.right() - 4.0, row.center().y), Align2::RIGHT_CENTER, note_name(pitch), FontId::proportional(9.5), Color32::from_gray(80));
            }
        }
    }
    p.vline(rect.right() - 0.5, rect.y_range(), Stroke::new(1.0, theme::BORDER));

    // Click and slide along the keys to audition.
    if resp.is_pointer_button_down_on() {
        if let Some(pos) = ui.input(|i| i.pointer.interact_pos()) {
            let pitch = geo.pitch_at(pos.y).clamp(0, 127) as u8;
            if app.piano.audition != Some((track, pitch)) {
                audition(app, track, Some(pitch));
            }
        }
    } else if app.piano.audition.is_some() && app.piano.drag.is_none() {
        audition(app, track, None);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn note_names_follow_logic() {
        assert_eq!(note_name(60), "C3");
        assert_eq!(note_name(69), "A3");
        assert_eq!(note_name(0), "C-2");
    }

    #[test]
    fn drag_moves_only_selected_notes() {
        let a = Note { start_secs: 0.0, length_secs: 0.5, pitch: 60, velocity: 100, tab: None };
        let b = Note { start_secs: 1.0, length_secs: 0.5, pitch: 62, velocity: 100, tab: None };
        let d = NoteDrag { mode: DragMode::Move, copy: false, origin_t: 0.0, origin_pitch: 0, dt: 0.25, dpitch: 2 };
        let out = dragged_notes(&[a, b], &[b], &d, 4.0);
        assert_eq!(out[0], a);
        assert_eq!((out[1].start_secs, out[1].pitch), (1.25, 64));
        let r = NoteDrag { mode: DragMode::Resize, ..d };
        assert_eq!(dragged_notes(&[a], &[a], &r, 4.0)[0].length_secs, 0.75);
    }

    /// Screen centre of a note, from the geometry the piano roll last drew with.
    pub(crate) fn note_center(app: &DawApp, n: &Note) -> egui::Pos2 {
        let (grid, top_pitch, key_h, pps, start) = app.piano.geo.expect("piano roll drawn");
        let geo = Geo { grid, top_pitch, key_h, px_per_sec: pps, start };
        geo.note_rect(n).center()
    }
}
