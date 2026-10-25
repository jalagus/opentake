//! Arrangement view: ruler, track headers and the region timeline (audio and MIDI).

use std::sync::Arc;

use opentake_engine::{RegionData, TrackUpdate, Waveform};
use opentake_engine::project::{Clip, MidiClip, Project, Track};
use eframe::egui;
use egui::{Align, Align2, Color32, CursorIcon, FontId, Layout, Painter, Pos2, Rangef, Rect, RichText, Sense, Stroke, StrokeKind, UiBuilder, pos2, vec2};

use super::widgets::{self, Icon};
use super::{Action, DawApp, Snapshot, theme};

const HEADER_W: f32 = 260.0;
const ROW_H: f32 = 64.0;
const RULER_H: f32 = 30.0;
/// Extra ruler row for song tempo markings, shown when regions carry tempo marks.
const TEMPO_ROW_H: f32 = 15.0;
/// Height of the cycle (loop) strip at the top of the ruler.
const CYCLE_H: f32 = 12.0;
const CYCLE_COLOR: Color32 = Color32::from_rgb(232, 190, 60);
/// Pointer distance (px) from a region edge that grabs the edge for trimming.
const EDGE_GRAB: f32 = 6.0;
const MIN_REGION_SECS: f64 = 0.01;

/// Grid resolution used for snapping and grid lines (Logic's "Snap" menu).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapMode {
    Off,
    /// Follows the zoom level.
    Smart,
    Bar,
    Beat,
    Eighth,
    Sixteenth,
    ThirtySecond,
}

impl SnapMode {
    pub const ALL: [SnapMode; 7] =
        [SnapMode::Off, SnapMode::Smart, SnapMode::Bar, SnapMode::Beat, SnapMode::Eighth, SnapMode::Sixteenth, SnapMode::ThirtySecond];

    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Smart => "Smart",
            Self::Bar => "Bar",
            Self::Beat => "Beat",
            Self::Eighth => "1/8",
            Self::Sixteenth => "1/16",
            Self::ThirtySecond => "1/32",
        }
    }
}

/// Edit tool (Logic's left-click tool).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    Pointer,
    Scissors,
}

pub use opentake_engine::{RegionKind, RegionRef};

#[derive(Clone, Copy)]
enum Content<'a> {
    Audio(&'a Clip),
    Midi(&'a MidiClip),
}

/// Uniform view of an audio or MIDI region for drawing and gestures.
#[derive(Clone, Copy)]
pub(crate) struct RegionView<'a> {
    pub r: RegionRef,
    pub name: &'a str,
    pub position: f64,
    pub length: f64,
    pub mute: bool,
    content: Content<'a>,
}

impl<'a> RegionView<'a> {
    pub fn end(&self) -> f64 {
        self.position + self.length
    }

    /// Detached copy of the underlying region.
    pub fn to_data(self) -> RegionData {
        match self.content {
            Content::Audio(c) => RegionData::Audio(c.clone()),
            Content::Midi(c) => RegionData::Midi(c.clone()),
        }
    }
}

/// All regions of a track (audio first, then MIDI).
pub(crate) fn regions_of(track: &Track, ti: usize) -> impl Iterator<Item = RegionView<'_>> {
    let audio = track.clips.iter().enumerate().map(move |(i, c)| RegionView {
        r: RegionRef::audio(ti, i),
        name: &c.name,
        position: c.position_secs,
        length: c.length_secs,
        mute: c.mute,
        content: Content::Audio(c),
    });
    let midi = track.midi_clips.iter().enumerate().map(move |(i, c)| RegionView {
        r: RegionRef::midi(ti, i),
        name: &c.name,
        position: c.position_secs,
        length: c.length_secs,
        mute: c.mute,
        content: Content::Midi(c),
    });
    audio.chain(midi)
}

pub(crate) fn region_view(project: &Project, r: RegionRef) -> Option<RegionView<'_>> {
    let track = project.tracks.get(r.track)?;
    regions_of(track, r.track).find(|v| v.r == r)
}

#[derive(Clone, Copy, PartialEq)]
enum DragMode {
    Move,
    TrimStart,
    TrimEnd,
}

struct RegionDrag {
    /// The region under the pointer when the drag started.
    grabbed: RegionRef,
    mode: DragMode,
    /// ⌘ held during a move: drop copies and leave the originals.
    copy: bool,
    /// Offset applied to every selected region during a move.
    delta_t: f64,
    delta_track: isize,
    /// Seconds between the grabbed edge (region start for moves) and the press point, so
    /// the edge follows the pointer without jumping.
    grab_offset: f64,
    /// Preview of the trimmed region.
    start: f64,
    end: f64,
}

/// Rubber-band selection in the lanes.
struct Marquee {
    origin: Pos2,
    current: Pos2,
    /// Selection before the marquee (kept when Shift was held).
    base: Vec<RegionRef>,
}

#[derive(Clone, Copy, PartialEq)]
enum CycleMode {
    New(f64),
    Move,
    Start,
    End,
}

struct CycleDrag {
    mode: CycleMode,
    origin_t: f64,
    start: f64,
    end: f64,
}

/// A finished region gesture, applied after the frame is drawn.
enum Edit {
    Move { delta_t: f64, delta_track: isize, copy: bool },
    Trim { region: RegionRef, start: Option<f64>, end: Option<f64> },
}

const MIN_PX_PER_SEC: f32 = 2.0;
const MAX_PX_PER_SEC: f32 = 2000.0;
/// Zoom step for the buttons and ⌘← / ⌘→.
pub const ZOOM_STEP: f32 = 1.5;

/// Horizontal zoom: − / slider / +. Shared by the arrangement, piano roll and tablature,
/// which all draw on the same time axis. Call inside a right-to-left layout.
pub fn zoom_controls(app: &mut DawApp, ui: &mut egui::Ui, id: &str, playhead: f64) {
    ui.push_id(id, |ui| {
        if widgets::icon_button(ui, Icon::Plus, false, theme::TEXT_DIM, "Zoom In (⌘→, or ⌘ + scroll)", vec2(22.0, 20.0)).clicked() {
            app.view.zoom_by(ZOOM_STEP, playhead);
        }
        let mut pps = app.view.px_per_sec;
        ui.spacing_mut().slider_width = 90.0;
        let slider = egui::Slider::new(&mut pps, MIN_PX_PER_SEC..=MAX_PX_PER_SEC).logarithmic(true).show_value(false);
        let r = ui.add(slider).on_hover_text("Horizontal zoom (Z fits the selection or the whole song)");
        widgets::label(&r, egui::Role::Slider, "Horizontal Zoom");
        if r.changed() {
            app.view.zoom_by(pps / app.view.px_per_sec, playhead);
        }
        if widgets::icon_button(ui, Icon::Minus, false, theme::TEXT_DIM, "Zoom Out (⌘←)", vec2(22.0, 20.0)).clicked() {
            app.view.zoom_by(1.0 / ZOOM_STEP, playhead);
        }
    });
}

/// Horizontal zoom/scroll state plus geometry remembered from the last frame
/// (used to map drops and drags to tracks/times).
pub struct View {
    pub start: f64,
    pub px_per_sec: f32,
    x_range: Rangef,
    rows: Vec<(usize, Rangef)>,
    drag: Option<RegionDrag>,
    marquee: Option<Marquee>,
    cycle_drag: Option<CycleDrag>,
    /// Timeline position of the last right-click (for "Split Here").
    context_time: f64,
}

impl Default for View {
    fn default() -> Self {
        Self {
            start: 0.0,
            px_per_sec: 48.0,
            x_range: Rangef::new(0.0, 0.0),
            rows: Vec::new(),
            drag: None,
            marquee: None,
            cycle_drag: None,
            context_time: 0.0,
        }
    }
}

impl View {
    fn x_of(&self, t: f64) -> f32 {
        self.x_range.min + ((t - self.start) * self.px_per_sec as f64) as f32
    }

    fn t_of(&self, x: f32) -> f64 {
        self.start + ((x - self.x_range.min) / self.px_per_sec) as f64
    }

    /// Timeline time under screen x, if x is over the timeline.
    pub fn time_at(&self, x: f32) -> Option<f64> {
        (x >= self.x_range.min && x <= self.x_range.max).then(|| self.t_of(x).max(0.0))
    }

    pub fn track_at(&self, y: f32) -> Option<usize> {
        self.rows.iter().find(|(_, r)| r.contains(y)).map(|(i, _)| *i)
    }

    fn row_of(&self, track: usize) -> Option<Rangef> {
        self.rows.iter().find(|(i, _)| *i == track).map(|(_, r)| *r)
    }

    fn visible_secs(&self) -> f64 {
        (self.x_range.span() / self.px_per_sec) as f64
    }

    /// Screen x of a timeline time (shared with the tablature view).
    pub(crate) fn x(&self, t: f64) -> f32 {
        self.x_of(t)
    }

    /// Screen x-range of the lanes.
    pub(crate) fn lanes(&self) -> Rangef {
        self.x_range
    }

    /// Ctrl/Cmd+scroll zooms around the pointer, horizontal scroll pans, while the
    /// pointer is over `area`.
    pub(crate) fn handle_zoom_scroll(&mut self, ui: &egui::Ui, area: Rect) {
        let (zoom, scroll_x, hover) = ui.input(|i| (i.zoom_delta(), i.smooth_scroll_delta.x, i.pointer.hover_pos()));
        let Some(p) = hover.filter(|p| area.contains(*p) && p.x >= self.x_range.min) else { return };
        if zoom != 1.0 {
            self.zoom_around(zoom, p.x);
        }
        if scroll_x != 0.0 {
            self.start = (self.start - (scroll_x / self.px_per_sec) as f64).max(0.0);
        }
    }

    /// Zoom by `factor`, keeping the playhead in place when it's on screen (else the centre).
    pub(crate) fn zoom_by(&mut self, factor: f32, playhead: f64) {
        let px = self.x_of(playhead);
        let x = if self.x_range.contains(px) { px } else { self.x_range.center() };
        self.zoom_around(factor, x);
    }

    /// Fit `from..to` into the lanes.
    pub(crate) fn zoom_to_fit(&mut self, from: f64, to: f64) {
        let margin = 16.0;
        let span = (to - from).max(0.25);
        self.px_per_sec = ((self.x_range.span() - 2.0 * margin).max(50.0) / span as f32).clamp(MIN_PX_PER_SEC, MAX_PX_PER_SEC);
        self.start = (from - (margin / self.px_per_sec) as f64).max(0.0);
    }

    fn zoom_around(&mut self, factor: f32, x: f32) {
        let anchor = self.t_of(x);
        self.px_per_sec = (self.px_per_sec * factor).clamp(MIN_PX_PER_SEC, MAX_PX_PER_SEC);
        self.start = (anchor - ((x - self.x_range.min) / self.px_per_sec) as f64).max(0.0);
    }
}

/// Length of one beat in seconds (the time-signature denominator's note value).
pub fn beat_secs(project: &Project) -> f64 {
    60.0 / project.tempo_bpm * 4.0 / project.time_signature.1.max(1) as f64
}

pub fn bar_secs(project: &Project) -> f64 {
    beat_secs(project) * project.time_signature.0.max(1) as f64
}

/// Grid size in seconds for a snap mode, or `None` when snapping is off.
pub fn grid_secs(mode: SnapMode, project: &Project, px_per_sec: f32) -> Option<f64> {
    let quarter = 60.0 / project.tempo_bpm;
    Some(match mode {
        SnapMode::Off => return None,
        SnapMode::Bar => bar_secs(project),
        SnapMode::Beat => beat_secs(project),
        SnapMode::Eighth => quarter / 2.0,
        SnapMode::Sixteenth => quarter / 4.0,
        SnapMode::ThirtySecond => quarter / 8.0,
        SnapMode::Smart => {
            // Finest musical value that is still at least ~14 px wide.
            let min = 14.0 / px_per_sec as f64;
            let mut candidates = vec![quarter / 4.0, quarter / 2.0, beat_secs(project)];
            let mut bars = bar_secs(project);
            for _ in 0..12 {
                candidates.push(bars);
                bars *= 2.0;
            }
            candidates.into_iter().find(|&g| g >= min).unwrap_or(bars)
        }
    })
}

/// Snap `t` to the grid (unless snapping is off or Alt is held).
pub fn snap_to(mode: SnapMode, ui: &egui::Ui, t: f64, project: &Project, px_per_sec: f32) -> f64 {
    let bypass = ui.input(|i| i.modifiers.alt);
    match grid_secs(mode, project, px_per_sec) {
        Some(grid) if !bypass => ((t / grid).round() * grid).max(0.0),
        _ => t.max(0.0),
    }
}

pub fn snap_time(app: &DawApp, ui: &egui::Ui, t: f64, project: &Project) -> f64 {
    snap_to(app.snap_mode, ui, t, project, app.view.px_per_sec)
}

pub fn show(app: &mut DawApp, ui: &mut egui::Ui, snap: &Snapshot) {
    handle_zoom_scroll(app, ui);
    let project = &snap.project;
    let playhead = snap.status.position_secs;
    ui.spacing_mut().item_spacing = vec2(0.0, 0.0);

    // Ruler row: tools on the left, bar ruler on the right (plus song tempo markings).
    let tempos = tempo_marks(project);
    let ruler_h = RULER_H + if tempos.is_empty() { 0.0 } else { TEMPO_ROW_H };
    ui.horizontal(|ui| {
        let (bar, _) = ui.allocate_exact_size(vec2(HEADER_W, ruler_h), Sense::hover());
        ui.painter().rect_filled(bar, 0.0, theme::BG_HEADER);
        ui.painter().vline(bar.right() - 0.5, bar.y_range(), Stroke::new(1.0, theme::BORDER));
        let tools = Rect::from_min_size(bar.min, vec2(bar.width(), RULER_H));
        ui.scope_builder(UiBuilder::new().max_rect(tools.shrink2(vec2(8.0, 3.0))).layout(Layout::left_to_right(Align::Center)), |ui| {
            toolbar(app, ui, snap);
        });
        if !tempos.is_empty() {
            ui.painter().text(pos2(bar.right() - 10.0, bar.bottom() - TEMPO_ROW_H / 2.0), Align2::RIGHT_CENTER, "Song Tempo", FontId::proportional(10.0), theme::TEXT_FAINT);
        }

        let (resp, painter) = ui.allocate_painter(vec2(ui.available_width(), ruler_h), Sense::hover());
        let full_rect = resp.rect;
        let rect = Rect::from_min_size(full_rect.min, vec2(full_rect.width(), RULER_H));
        app.view.x_range = rect.x_range();
        draw_ruler(&painter, rect, &app.view, project);
        if !tempos.is_empty() {
            draw_tempo_row(&painter, Rect::from_min_max(pos2(rect.left(), rect.bottom()), full_rect.max), &app.view, &tempos, playhead);
        }
        let cycle_rect = Rect::from_min_max(rect.min, pos2(rect.right(), rect.top() + CYCLE_H));
        cycle_strip(app, ui, &painter, cycle_rect, project);
        let seek_rect = Rect::from_min_max(pos2(rect.left(), cycle_rect.bottom()), full_rect.max);
        let seek = ui.interact(seek_rect, ui.make_persistent_id("ruler_seek"), Sense::click_and_drag());
        if seek.clicked() || seek.dragged() {
            if let Some(t) = seek.interact_pointer_pos().and_then(|p| app.view.time_at(p.x)) {
                app.act(Action::Seek(snap_time(app, ui, t, project)));
            }
        }
        draw_playhead(&painter, full_rect, &app.view, playhead, true);

        // Horizontal zoom, floating at the right end of the ruler.
        let pill = Rect::from_min_size(pos2(rect.right() - 178.0, rect.center().y - 12.0), vec2(170.0, 24.0));
        painter.rect(pill, 12.0, theme::BG_HEADER, Stroke::new(1.0, theme::BORDER), StrokeKind::Inside);
        let mut zoom_ui = ui.new_child(UiBuilder::new().id_salt("arrange_zoom_pill").max_rect(pill.shrink2(vec2(6.0, 2.0))).layout(Layout::right_to_left(Align::Center)));
        zoom_ui.spacing_mut().item_spacing.x = 4.0;
        zoom_controls(app, &mut zoom_ui, "arrange_zoom", playhead);
    });

    let mut rows = Vec::with_capacity(project.tracks.len());
    let mut edit: Option<Edit> = None;
    let grid = grid_secs(app.snap_mode, project, app.view.px_per_sec);

    egui::ScrollArea::vertical().id_salt("arrange").auto_shrink([false, false]).show(ui, |ui| {
        ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
        for (ti, track) in project.tracks.iter().enumerate() {
            ui.horizontal(|ui| {
                track_header(app, ui, ti, track, snap);
                let (resp, painter) = ui.allocate_painter(vec2(ui.available_width(), ROW_H), Sense::click_and_drag());
                widgets::label(&resp, egui::Role::Group, &format!("Lane {}", track.name));
                let rect = resp.rect;
                rows.push((ti, rect.y_range()));
                let painter = painter.with_clip_rect(rect);

                let selected = app.selected_track == Some(ti);
                let drop_target = app
                    .view
                    .drag
                    .as_ref()
                    .is_some_and(|d| d.mode == DragMode::Move && d.delta_track != 0 && d.grabbed.track as isize + d.delta_track == ti as isize);
                let bg = if drop_target || selected {
                    theme::BG_LANE_SELECTED
                } else if ti % 2 == 0 {
                    theme::BG_LANE
                } else {
                    theme::BG_LANE_ALT
                };
                painter.rect_filled(rect, 0.0, bg);
                draw_grid(&painter, rect, &app.view, project, grid);
                draw_loop_tint(&painter, rect, &app.view, project);
                painter.hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, theme::BORDER));

                // Dragging on empty lane space draws a selection rectangle.
                if resp.drag_started() && app.tool == Tool::Pointer {
                    if let Some(p) = ui.input(|i| i.pointer.press_origin()) {
                        let base = if ui.input(|i| i.modifiers.shift) { app.selection.clone() } else { Vec::new() };
                        app.view.marquee = Some(Marquee { origin: p, current: p, base });
                    }
                }
                if resp.dragged() {
                    if let (Some(m), Some(p)) = (app.view.marquee.as_mut(), resp.interact_pointer_pos()) {
                        m.current = p;
                    }
                }
                if resp.drag_stopped() {
                    app.view.marquee = None;
                }

                if resp.clicked() {
                    app.selected_track = Some(ti);
                    if !ui.input(|i| i.modifiers.shift) {
                        app.selection.clear();
                    }
                    if let Some(t) = resp.interact_pointer_pos().and_then(|p| app.view.time_at(p.x)) {
                        app.act(Action::Seek(snap_time(app, ui, t, project)));
                    }
                }
                // Double-click an empty instrument lane: new one-bar MIDI region.
                if resp.double_clicked() && track.is_instrument() {
                    if let Some(t) = resp.interact_pointer_pos().and_then(|p| app.view.time_at(p.x)) {
                        let bar = bar_secs(project);
                        let at = (t / bar).floor() * bar;
                        app.act(Action::AddMidiRegion { track: ti, position: at, length: bar });
                    }
                }

                for view in regions_of(track, ti) {
                    if let Some(e) = region(app, ui, &painter, rect, view, snap) {
                        edit = Some(e);
                    }
                }

                // Scissors guide line.
                if app.tool == Tool::Scissors {
                    if let Some(p) = ui.input(|i| i.pointer.hover_pos()).filter(|p| rect.contains(*p)) {
                        let x = app.view.x_of(snap_time(app, ui, app.view.t_of(p.x), project));
                        painter.vline(x, rect.y_range(), Stroke::new(1.0, theme::with_alpha(Color32::WHITE, 180)));
                    }
                }
                draw_playhead(&painter, rect, &app.view, playhead, false);
            });
        }

        // Empty area below the tracks: drop target and quick "new track".
        let height = ui.available_height().max(80.0);
        ui.horizontal(|ui| {
            let (left, _) = ui.allocate_exact_size(vec2(HEADER_W, height), Sense::hover());
            ui.painter().rect_filled(left, 0.0, theme::BG_PANEL);
            ui.painter().vline(left.right() - 0.5, left.y_range(), Stroke::new(1.0, theme::BORDER));
            let (resp, painter) = ui.allocate_painter(vec2(ui.available_width(), height), Sense::click());
            draw_grid(&painter, resp.rect, &app.view, project, grid);
            draw_loop_tint(&painter, resp.rect, &app.view, project);
            draw_playhead(&painter, resp.rect, &app.view, playhead, false);
            painter.text(
                resp.rect.center_top() + vec2(0.0, 36.0),
                Align2::CENTER_CENTER,
                "Drag audio files here, or double-click to create a track",
                FontId::proportional(12.5),
                theme::TEXT_FAINT,
            );
            if resp.clicked() {
                app.selection.clear();
            }
            if resp.double_clicked() {
                app.act(Action::AddTrack);
            }
        });
    });
    // Live rubber-band selection: regions whose box intersects the marquee.
    if let Some(m) = &app.view.marquee {
        let area = Rect::from_two_pos(m.origin, m.current);
        let mut selection = m.base.clone();
        for (ti, track) in project.tracks.iter().enumerate() {
            let Some(row) = rows.iter().find(|(i, _)| *i == ti).map(|(_, r)| *r) else { continue };
            if row.intersection(area.y_range()).span() <= 0.0 {
                continue;
            }
            for v in regions_of(track, ti) {
                let span = Rangef::new(app.view.x_of(v.position), app.view.x_of(v.end()));
                if span.intersection(area.x_range()).span() > 0.0 && !selection.contains(&v.r) {
                    selection.push(v.r);
                }
            }
        }
        app.selection = selection;
        let layer = egui::LayerId::new(egui::Order::Foreground, ui.make_persistent_id("marquee"));
        let p = ui.ctx().layer_painter(layer).with_clip_rect(Rect::from_x_y_ranges(app.view.x_range, ui.clip_rect().y_range()));
        p.rect_filled(area, 2.0, theme::with_alpha(theme::ACCENT, 36));
        p.rect_stroke(area, 2.0, Stroke::new(1.0, theme::with_alpha(theme::ACCENT, 200)), StrokeKind::Inside);
    }
    app.view.rows = rows;

    match edit {
        Some(Edit::Move { delta_t, delta_track, copy }) => {
            let regions = app.selection.clone();
            app.act(Action::MoveRegions { regions, delta_secs: delta_t, delta_track, copy });
        }
        Some(Edit::Trim { region, start, end }) => app.act(Action::TrimRegion { region, start, end }),
        None => {}
    }

    // Keep the playhead in view while playing.
    if snap.status.playing {
        let visible = app.view.visible_secs();
        if playhead > app.view.start + visible * 0.95 || playhead < app.view.start {
            app.view.start = (playhead - visible * 0.05).max(0.0);
        }
    }
}

fn toolbar(app: &mut DawApp, ui: &mut egui::Ui, snap: &Snapshot) {
    ui.spacing_mut().item_spacing.x = 4.0;
    let add = widgets::icon_button(ui, Icon::Plus, false, theme::TEXT, "New Track", vec2(24.0, 20.0));
    egui::Popup::menu(&add).show(|ui| {
        ui.set_min_width(220.0);
        if ui.add(egui::Button::new("Audio Track").shortcut_text(if cfg!(target_os = "macos") { "⌥⌘N" } else { "Ctrl+Alt+N" })).clicked() {
            app.act(Action::AddTrack);
            ui.close();
        }
        ui.separator();
        widgets::section(ui, "Instrument Track");
        for (name, id) in &snap.instruments {
            if ui.button(name).clicked() {
                app.act(Action::AddInstrumentTrack(id.clone()));
                ui.close();
            }
        }
    });
    ui.add_space(4.0);
    for (tool, icon, tip) in [(Tool::Pointer, Icon::Pointer, "Pointer Tool"), (Tool::Scissors, Icon::Scissors, "Scissors Tool — click a region to split it")] {
        if widgets::tool_button(ui, icon, app.tool == tool, tip, vec2(24.0, 20.0)).clicked() {
            app.tool = tool;
        }
    }
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        let text = if app.snap_mode == SnapMode::Off { "Snap: Off".to_string() } else { format!("Snap: {}", app.snap_mode.label()) };
        egui::ComboBox::from_id_salt("snap_mode")
            .selected_text(RichText::new(text).size(11.0))
            .width(86.0)
            .show_ui(ui, |ui| {
                for mode in SnapMode::ALL {
                    ui.selectable_value(&mut app.snap_mode, mode, mode.label());
                }
            })
            .response
            .on_hover_text("Snap to grid (⌘G toggles, hold Alt to bypass)");
    });
}

fn handle_zoom_scroll(app: &mut DawApp, ui: &egui::Ui) {
    let area = ui.available_rect_before_wrap();
    app.view.handle_zoom_scroll(ui, area);
}

/// Draws one region and handles select / move / copy / trim / split.
fn region(app: &mut DawApp, ui: &mut egui::Ui, painter: &Painter, row: Rect, view: RegionView, snap: &Snapshot) -> Option<Edit> {
    let project = &snap.project;
    let r = view.r;
    let wf = match view.content {
        Content::Audio(c) => snap.waveforms.get(&c.file),
        Content::Midi(_) => None,
    };
    let selected = app.selection.contains(&r);
    // This region's own gesture (grabbed), or a group move it takes part in.
    let drag = app.view.drag.as_ref().filter(|d| d.grabbed == r || (d.mode == DragMode::Move && selected));
    let (shown_start, shown_end) = match drag {
        Some(d) if d.mode == DragMode::Move => (view.position + d.delta_t, view.end() + d.delta_t),
        Some(d) => (d.start, d.end),
        None => (view.position, view.end()),
    };

    let x0 = app.view.x_of(shown_start);
    let x1 = app.view.x_of(shown_end).max(x0 + 3.0);
    let rect = Rect::from_min_max(pos2(x0, row.top() + 1.0), pos2(x1, row.bottom() - 2.0));
    let visible = rect.intersect(row);
    if visible.width() <= 0.0 && drag.is_none() {
        return None;
    }

    let resp = ui.interact(visible, ui.make_persistent_id(("region", r)), Sense::click_and_drag());
    widgets::label(&resp, egui::Role::Button, &format!("Region {}", view.name));
    let lifted = drag.is_some();
    // MIDI regions have no source offset, so only their end can be trimmed.
    let trim_start_ok = r.kind == RegionKind::Audio;

    let hover_x = resp.hover_pos().map(|p| p.x);
    let near_edge = |x: f32| {
        let edge = EDGE_GRAB.min(rect.width() / 4.0);
        if trim_start_ok && (x - rect.left()).abs() <= edge {
            Some(DragMode::TrimStart)
        } else if (rect.right() - x).abs() <= edge {
            Some(DragMode::TrimEnd)
        } else {
            None
        }
    };
    if drag.is_some_and(|d| d.mode == DragMode::Move && d.copy) {
        ui.ctx().set_cursor_icon(CursorIcon::Copy);
    } else if app.tool == Tool::Pointer {
        if let Some(x) = hover_x {
            if near_edge(x).is_some() || drag.is_some_and(|d| d.mode != DragMode::Move) {
                ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
            }
        }
    } else if resp.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::Crosshair);
    }

    // Paint. While ⌘-dragging a copy, the original stays drawn in place.
    let color = if view.mute { Color32::from_gray(110) } else { theme::track_color(r.track) };
    let copying = drag.is_some_and(|d| d.mode == DragMode::Move && d.copy);
    let painted = |start: f64, end: f64, mode: Option<DragMode>| Painted { view, start, end, mode, wf };
    if copying {
        let orig = Rect::from_min_max(pos2(app.view.x_of(view.position), rect.top()), pos2(app.view.x_of(view.end()), rect.bottom()));
        paint_region(painter, row, orig, painted(view.position, view.end(), None), color, &app.view, RegionLook::default());
    }
    let look = RegionLook { selected, lifted, hovered: resp.hovered(), copy_badge: copying };
    let shown = painted(shown_start, shown_end, drag.map(|d| d.mode));
    match drag.filter(|d| d.mode == DragMode::Move) {
        // While moving/copying, draw the region in the lane it will land in, on a layer
        // above all lanes (lanes below this one are painted later and would cover it).
        Some(d) => {
            let target_track = (r.track as isize + d.delta_track).max(0) as usize;
            let lane = app.view.row_of(target_track).unwrap_or(row.y_range());
            let target = Rect::from_x_y_ranges(rect.x_range(), Rangef::new(lane.min + 1.0, lane.max - 2.0));
            let clip = Rect::from_x_y_ranges(row.x_range(), ui.clip_rect().y_range());
            let layer = egui::LayerId::new(egui::Order::Foreground, ui.make_persistent_id("region_drag"));
            let fg = ui.ctx().layer_painter(layer).with_clip_rect(clip);
            paint_region(&fg, row, target, shown, color, &app.view, look);
        }
        None => paint_region(painter, row, rect, shown, color, &app.view, look),
    }
    if app.tool == Tool::Pointer {
        // Trim handles on hover.
        if let Some(mode) = hover_x.and_then(near_edge) {
            let x = if mode == DragMode::TrimStart { rect.left() + 1.5 } else { rect.right() - 1.5 };
            painter.vline(x, Rangef::new(rect.top() + 3.0, rect.bottom() - 3.0), Stroke::new(3.0, Color32::WHITE));
        }
    }

    // Interaction.
    let pointer = resp.interact_pointer_pos();
    let shift = ui.input(|i| i.modifiers.shift);
    if resp.clicked() {
        app.selected_track = Some(r.track);
        match app.tool {
            Tool::Pointer if shift => {
                if let Some(i) = app.selection.iter().position(|&s| s == r) {
                    app.selection.remove(i);
                } else {
                    app.selection.push(r);
                }
            }
            Tool::Pointer => app.selection = vec![r],
            Tool::Scissors => {
                if let Some(p) = pointer {
                    let at = snap_time(app, ui, app.view.t_of(p.x), project);
                    app.act(Action::SplitRegions { regions: vec![r], at });
                }
            }
        }
    }
    if resp.double_clicked() && r.kind == RegionKind::Midi {
        app.open_piano_roll(r);
    }
    if resp.secondary_clicked() {
        if let Some(p) = pointer {
            app.view.context_time = snap_time(app, ui, app.view.t_of(p.x), project);
        }
        if !selected {
            app.selection = vec![r];
        }
        app.selected_track = Some(r.track);
    }
    if resp.drag_started() && app.tool == Tool::Pointer {
        // egui reports the drag only after the pointer passed the drag threshold, so use
        // where the button went down to decide edge-vs-body and the grab offset.
        let origin = ui.input(|i| i.pointer.press_origin()).or(pointer);
        if let Some(p) = origin {
            // Grabbing an unselected region selects it (Shift adds it to the selection).
            if !selected {
                if shift {
                    app.selection.push(r);
                } else {
                    app.selection = vec![r];
                }
            }
            let mode = near_edge(p.x).unwrap_or(DragMode::Move);
            if mode != DragMode::Move {
                app.selection = vec![r]; // trims apply to the grabbed region only
            }
            let edge = if mode == DragMode::TrimEnd { view.end() } else { view.position };
            app.view.drag = Some(RegionDrag {
                grabbed: r,
                mode,
                copy: false,
                delta_t: 0.0,
                delta_track: 0,
                grab_offset: app.view.t_of(p.x) - edge,
                start: view.position,
                end: view.end(),
            });
        }
    }
    if resp.dragged() && app.view.drag.as_ref().is_some_and(|d| d.grabbed == r) {
        if let (Some(p), Some(mode)) = (pointer, app.view.drag.as_ref().map(|d| d.mode)) {
            let grab = app.view.drag.as_ref().map_or(0.0, |d| d.grab_offset);
            let t = app.view.t_of(p.x) - grab;
            let (orig_start, orig_end) = (view.position, view.end());
            // Audio can't be extended past its source file.
            let (source_start, source_end) = match view.content {
                Content::Audio(c) => {
                    let s = c.position_secs - c.source_offset_secs;
                    (s, wf.map_or(f64::INFINITY, |w| s + w.frames as f64 / w.sample_rate as f64))
                }
                Content::Midi(_) => (0.0, f64::INFINITY),
            };
            if mode == DragMode::Move {
                // Snap the grabbed region; everything selected moves by the same offset,
                // kept on the timeline and within the track list.
                let group: Vec<RegionView> = app.selection.iter().filter_map(|&s| region_view(project, s)).collect();
                let min_pos = group.iter().map(|g| g.position).fold(f64::INFINITY, f64::min);
                let delta_t = (snap_time(app, ui, t, project) - view.position).max(-min_pos.min(view.position));
                let (lo, hi) = group.iter().fold((usize::MAX, 0), |(lo, hi), g| (lo.min(g.r.track), hi.max(g.r.track)));
                let n = project.tracks.len() as isize;
                let lane = app.view.track_at(p.y).map_or(0, |l| l as isize - r.track as isize);
                let delta_track = lane.clamp(-(lo.min(r.track) as isize), n - 1 - hi.max(r.track) as isize);
                let copy = ui.input(|i| i.modifiers.command);
                if let Some(d) = app.view.drag.as_mut() {
                    d.delta_t = delta_t;
                    d.delta_track = delta_track;
                    d.copy = copy;
                }
            }
            let (start, end) = match mode {
                DragMode::Move => (view.position, view.end()),
                DragMode::TrimStart => {
                    let s = snap_time(app, ui, t, project).max(source_start).max(0.0).min(orig_end - MIN_REGION_SECS);
                    (s, orig_end)
                }
                DragMode::TrimEnd => {
                    let e = snap_time(app, ui, t, project).min(source_end).max(orig_start + MIN_REGION_SECS);
                    (orig_start, e)
                }
            };
            if let Some(d) = app.view.drag.as_mut() {
                d.start = start;
                d.end = end;
            }
        }
    }
    resp.context_menu(|ui| region_context_menu(app, ui, view, snap));
    if resp.drag_stopped() && app.view.drag.as_ref().is_some_and(|d| d.grabbed == r) {
        if let Some(d) = app.view.drag.take() {
            let (orig_start, orig_end) = (view.position, view.end());
            return match d.mode {
                // A copy may land exactly on the original position on another track; a move must change something.
                DragMode::Move if d.copy || d.delta_t != 0.0 || d.delta_track != 0 => {
                    Some(Edit::Move { delta_t: d.delta_t, delta_track: d.delta_track, copy: d.copy })
                }
                DragMode::TrimStart if d.start != orig_start => Some(Edit::Trim { region: r, start: Some(d.start), end: None }),
                DragMode::TrimEnd if d.end != orig_end => Some(Edit::Trim { region: r, start: None, end: Some(d.end) }),
                _ => None,
            };
        }
    }
    None
}

fn region_context_menu(app: &mut DawApp, ui: &mut egui::Ui, view: RegionView, snap: &Snapshot) {
    let r = view.r;
    let group = app.selection.clone();
    let many = group.len() > 1;
    if many {
        ui.label(RichText::new(format!("{} regions", group.len())).strong());
    } else {
        ui.label(RichText::new(view.name).strong());
        ui.label(RichText::new(format!("{:.2}s – {:.2}s", view.position, view.end())).color(theme::TEXT_DIM));
    }
    ui.separator();
    if r.kind == RegionKind::Midi && !many && ui.button("Open in Piano Roll").clicked() {
        app.open_piano_roll(r);
        ui.close();
    }
    let playhead = snap.status.position_secs;
    let spans = |t: f64| {
        group.iter().filter_map(|&g| region_view(&snap.project, g)).any(|v| t > v.position + MIN_REGION_SECS && t < v.end() - MIN_REGION_SECS)
    };
    if ui.add_enabled(spans(playhead), egui::Button::new("Split at Playhead").shortcut_text("⌘T")).clicked() {
        app.act(Action::SplitRegions { regions: group.clone(), at: playhead });
        ui.close();
    }
    let here = app.view.context_time;
    if ui.add_enabled(spans(here), egui::Button::new("Split Here")).clicked() {
        app.act(Action::SplitRegions { regions: group.clone(), at: here });
        ui.close();
    }
    ui.separator();
    if ui.add(egui::Button::new("Copy").shortcut_text("⌘C")).clicked() {
        app.copy_selection(&snap.project);
        ui.close();
    }
    if ui.add_enabled(app.clipboard.is_some(), egui::Button::new("Paste at Playhead").shortcut_text("⌘V")).clicked() {
        app.act(Action::Paste { track: r.track, position: playhead });
        ui.close();
    }
    if ui.add(egui::Button::new("Duplicate").shortcut_text("⌘D")).clicked() {
        app.duplicate_selection(&snap.project);
        ui.close();
    }
    ui.separator();
    let label = if many { "Delete Regions" } else { "Delete Region" };
    if ui.add(egui::Button::new(label).shortcut_text("⌫")).clicked() {
        app.act(Action::RemoveRegions(group));
        app.selection.clear();
        ui.close();
    }
}

#[derive(Default, Clone, Copy)]
struct RegionLook {
    selected: bool,
    lifted: bool,
    hovered: bool,
    /// Show a "+" badge (region is being copied).
    copy_badge: bool,
}

/// A region as it should be drawn (possibly mid-gesture).
#[derive(Clone, Copy)]
struct Painted<'a> {
    view: RegionView<'a>,
    start: f64,
    end: f64,
    mode: Option<DragMode>,
    wf: Option<&'a Arc<Waveform>>,
}

fn paint_region(painter: &Painter, row: Rect, rect: Rect, p: Painted, color: Color32, view: &View, look: RegionLook) {
    if look.lifted {
        painter.rect_filled(rect.translate(vec2(0.0, 3.0)), 5.0, Color32::from_black_alpha(100));
    }
    let body_shade = if look.selected { 0.78 } else { 0.62 };
    painter.rect_filled(rect, 5.0, theme::shade(color, body_shade));
    let name_bar = Rect::from_min_max(rect.min, pos2(rect.right(), rect.top() + 15.0));
    painter.rect_filled(name_bar, egui::CornerRadius { nw: 5, ne: 5, sw: 0, se: 0 }, theme::shade(color, if look.selected { 1.0 } else { 0.85 }));
    painter.text(
        pos2(name_bar.left().max(row.left()) + 5.0, name_bar.center().y),
        Align2::LEFT_CENTER,
        p.view.name,
        FontId::proportional(10.5),
        Color32::from_rgb(16, 16, 18),
    );
    let body = Rect::from_min_max(pos2(rect.left(), name_bar.bottom() + 1.0), pos2(rect.right(), rect.bottom() - 1.0));
    let ink = theme::shade(color, 0.22);
    match p.view.content {
        Content::Audio(c) => {
            if let Some(wf) = p.wf {
                // Trimming the start reveals/hides audio: shift the source offset to match.
                let mut shown = c.clone();
                shown.position_secs = p.start;
                shown.length_secs = p.end - p.start;
                if p.mode == Some(DragMode::TrimStart) {
                    shown.source_offset_secs = c.source_offset_secs + (p.start - c.position_secs);
                }
                draw_waveform(painter, body, row, view, &shown, wf, ink);
            }
        }
        Content::Midi(c) => draw_note_preview(painter, body, row, view, c, p.start, p.end - p.start, ink),
    }
    let outline = if look.selected || look.lifted {
        Stroke::new(1.5, Color32::WHITE)
    } else if look.hovered {
        Stroke::new(1.0, Color32::from_gray(200))
    } else {
        Stroke::new(1.0, theme::shade(color, 0.4))
    };
    painter.rect_stroke(rect, 5.0, outline, StrokeKind::Inside);
    if look.copy_badge {
        let c = pos2(rect.right().min(row.right()) - 10.0, rect.top() + 7.5);
        painter.circle_filled(c, 6.5, Color32::WHITE);
        painter.line_segment([c - vec2(3.0, 0.0), c + vec2(3.0, 0.0)], Stroke::new(1.6, Color32::from_rgb(20, 20, 22)));
        painter.line_segment([c - vec2(0.0, 3.0), c + vec2(0.0, 3.0)], Stroke::new(1.6, Color32::from_rgb(20, 20, 22)));
    }
}

/// Miniature piano roll inside a MIDI region.
#[allow(clippy::too_many_arguments)]
fn draw_note_preview(painter: &Painter, rect: Rect, clip_to: Rect, view: &View, clip: &MidiClip, start: f64, length: f64, color: Color32) {
    if clip.notes.is_empty() || rect.height() < 6.0 {
        return;
    }
    let (lo, hi) = clip.notes.iter().fold((127u8, 0u8), |(lo, hi), n| (lo.min(n.pitch), hi.max(n.pitch)));
    // At least an octave of range so single notes don't fill the whole height.
    let span = (hi.saturating_sub(lo) as f32).max(12.0);
    let base = lo as f32 - (span - (hi - lo) as f32) / 2.0;
    let row_h = (rect.height() - 4.0) / (span + 1.0);
    let painter = painter.with_clip_rect(rect.intersect(clip_to));
    for n in clip.notes.iter().filter(|n| n.start_secs < length) {
        let x0 = view.x_of(start + n.start_secs);
        let x1 = view.x_of(start + (n.start_secs + n.length_secs).min(length)).max(x0 + 2.0);
        let y = rect.bottom() - 2.0 - (n.pitch as f32 - base + 1.0) * row_h;
        painter.rect_filled(Rect::from_min_max(pos2(x0, y), pos2(x1, y + row_h.max(2.0))), 1.0, color);
    }
}

fn draw_waveform(painter: &Painter, rect: Rect, clip_to: Rect, view: &View, clip: &Clip, wf: &Waveform, color: Color32) {
    if wf.peaks.is_empty() || rect.height() < 4.0 {
        return;
    }
    let mid = rect.center().y;
    let half = rect.height() * 0.47;
    let buckets_per_sec = wf.sample_rate as f64 / wf.frames_per_peak as f64;
    let sec_per_px = 1.0 / view.px_per_sec as f64;
    let stroke = Stroke::new(1.0, color);
    let (from, to) = (rect.left().max(clip_to.left()) + 1.0, rect.right().min(clip_to.right()) - 1.0);
    let mut x = from.floor();
    while x < to {
        let t = view.t_of(x) - clip.position_secs + clip.source_offset_secs;
        let b0 = (t * buckets_per_sec).floor().max(0.0) as usize;
        let b1 = (((t + sec_per_px) * buckets_per_sec).ceil() as usize).max(b0 + 1).min(wf.peaks.len());
        if b0 < b1 {
            let (lo, hi) = wf.peaks[b0..b1].iter().fold((0.0f32, 0.0f32), |(lo, hi), &(a, b)| (lo.min(a), hi.max(b)));
            let (lo, hi) = ((lo * clip.gain).max(-1.0), (hi * clip.gain).min(1.0));
            painter.line_segment([pos2(x, mid - hi * half), pos2(x, mid - lo * half + 0.5)], stroke);
        }
        x += 1.0;
    }
}

/// Cycle (loop) range at the top of the ruler. Drag on empty space to create a range,
/// drag the bar to move it, drag an edge to resize, click it to toggle cycling.
fn cycle_strip(app: &mut DawApp, ui: &egui::Ui, painter: &Painter, rect: Rect, project: &Project) {
    let resp = ui.interact(rect, ui.make_persistent_id("cycle_strip"), Sense::click_and_drag());
    widgets::label(&resp, egui::Role::Group, "Cycle Area");
    let (start, end) = match &app.view.cycle_drag {
        Some(d) => (d.start, d.end),
        None => (project.loop_start_secs, project.loop_end_secs),
    };
    let has_range = end > start;
    let bar = Rect::from_min_max(pos2(app.view.x_of(start), rect.top() + 1.0), pos2(app.view.x_of(end), rect.bottom() - 1.0));
    let near = |x: f32| -> Option<CycleMode> {
        if !has_range {
            return None;
        }
        let edge = 6.0f32.min(bar.width() / 4.0);
        if (x - bar.left()).abs() <= edge {
            Some(CycleMode::Start)
        } else if (x - bar.right()).abs() <= edge {
            Some(CycleMode::End)
        } else if bar.x_range().contains(x) {
            Some(CycleMode::Move)
        } else {
            None
        }
    };
    if let Some(x) = resp.hover_pos().map(|p| p.x) {
        if matches!(near(x), Some(CycleMode::Start | CycleMode::End)) {
            ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
        }
    }

    if resp.drag_started() {
        if let Some(p) = ui.input(|i| i.pointer.press_origin()) {
            let t = snap_time(app, ui, app.view.t_of(p.x), project);
            let mode = near(p.x).unwrap_or(CycleMode::New(t));
            app.view.cycle_drag = Some(CycleDrag { mode, origin_t: app.view.t_of(p.x), start, end });
        }
    }
    if resp.dragged() {
        if let (Some(p), Some(d)) = (resp.interact_pointer_pos(), app.view.cycle_drag.as_ref()) {
            let t = snap_time(app, ui, app.view.t_of(p.x), project);
            let (s0, e0) = (project.loop_start_secs, project.loop_end_secs);
            let (s1, e1) = match d.mode {
                CycleMode::New(anchor) => (anchor.min(t), anchor.max(t)),
                CycleMode::Start => (t.min(e0 - MIN_REGION_SECS), e0),
                CycleMode::End => (s0, t.max(s0 + MIN_REGION_SECS)),
                CycleMode::Move => {
                    let dt = (snap_time(app, ui, s0 + app.view.t_of(p.x) - d.origin_t, project) - s0).max(-s0);
                    (s0 + dt, e0 + dt)
                }
            };
            if let Some(d) = app.view.cycle_drag.as_mut() {
                (d.start, d.end) = (s1.max(0.0), e1.max(0.0));
            }
        }
    }
    if resp.drag_stopped() {
        if let Some(d) = app.view.cycle_drag.take() {
            if d.end - d.start > MIN_REGION_SECS {
                // Drawing a new range turns cycling on, like Logic.
                let enabled = matches!(d.mode, CycleMode::New(_)).then_some(true);
                app.act(Action::SetLoop { enabled, start: Some(d.start), end: Some(d.end) });
            }
        }
    }
    if resp.clicked() && resp.interact_pointer_pos().is_some_and(|p| bar.x_range().contains(p.x)) {
        app.act(Action::SetLoop { enabled: Some(!project.loop_enabled), start: None, end: None });
    }

    if has_range {
        let fill = if project.loop_enabled || app.view.cycle_drag.is_some() { CYCLE_COLOR } else { theme::shade(CYCLE_COLOR, 0.45) };
        painter.rect_filled(bar, 3.0, fill);
    } else if resp.hovered() {
        painter.text(rect.left_center() + vec2(8.0, 0.0), Align2::LEFT_CENTER, "Drag here to set a cycle range", FontId::proportional(9.0), theme::TEXT_FAINT);
    }
}

/// Faint highlight of the active cycle range across a lane.
fn draw_loop_tint(painter: &Painter, rect: Rect, view: &View, project: &Project) {
    if !project.loop_enabled || project.loop_end_secs <= project.loop_start_secs {
        return;
    }
    let span = Rect::from_x_y_ranges(Rangef::new(view.x_of(project.loop_start_secs), view.x_of(project.loop_end_secs)), rect.y_range());
    painter.rect_filled(span.intersect(rect), 0.0, theme::with_alpha(CYCLE_COLOR, 10));
    for x in [span.left(), span.right()] {
        painter.vline(x, rect.y_range(), Stroke::new(1.0, theme::with_alpha(CYCLE_COLOR, 70)));
    }
}

/// Bar spacing that keeps bar numbers at least ~70 px apart.
fn bar_step(view: &View, project: &Project) -> (f64, u64) {
    let bar = bar_secs(project);
    let mut step = 1u64;
    while bar * step as f64 * (view.px_per_sec as f64) < 70.0 && step < 4096 {
        step *= 2;
    }
    (bar, step)
}

/// Bar lines, beat lines and (when snapping finer than a beat) subdivision lines.
fn draw_grid(painter: &Painter, rect: Rect, view: &View, project: &Project, snap_grid: Option<f64>) {
    let (bar, step) = bar_step(view, project);
    let beat = beat_secs(project);
    let pps = view.px_per_sec as f64;
    let (t0, t1) = (view.start, view.start + view.visible_secs());

    // Finest visible subdivision: the snap grid if it's finer than a beat and not too dense.
    if let Some(g) = snap_grid.filter(|&g| g < beat - 1e-9 && g * pps >= 8.0) {
        let mut k = (t0 / g).floor() as i64;
        while (k as f64) * g <= t1 {
            painter.vline(view.x_of(k as f64 * g), rect.y_range(), Stroke::new(1.0, theme::GRID_SUB));
            k += 1;
        }
    }
    if step == 1 && beat * pps >= 12.0 {
        let mut k = (t0 / beat).floor() as i64;
        while (k as f64) * beat <= t1 {
            painter.vline(view.x_of(k as f64 * beat), rect.y_range(), Stroke::new(1.0, theme::GRID_BEAT));
            k += 1;
        }
    }
    let first = (t0 / bar).floor() as u64;
    let last = (t1 / bar).ceil() as u64;
    for b in (first..=last).filter(|b| b % step == 0) {
        painter.vline(view.x_of(b as f64 * bar), rect.y_range(), Stroke::new(1.0, theme::GRID_BAR));
    }
}

fn draw_ruler(painter: &Painter, rect: Rect, view: &View, project: &Project) {
    painter.rect_filled(rect, 0.0, theme::BG_HEADER);
    painter.rect_filled(Rect::from_min_max(rect.min, pos2(rect.right(), rect.top() + CYCLE_H)), 0.0, theme::shade(theme::BG_HEADER, 0.85));
    let (bar, step) = bar_step(view, project);
    let beat = beat_secs(project);
    let first = (view.start / bar).floor() as u64;
    let last = ((view.start + view.visible_secs()) / bar).ceil() as u64;
    let show_beats = step == 1 && beat * (view.px_per_sec as f64) >= 12.0;
    for b in first..=last {
        if show_beats {
            for k in 1..project.time_signature.0 {
                let xb = view.x_of(b as f64 * bar + k as f64 * beat);
                painter.vline(xb, Rangef::new(rect.bottom() - 5.0, rect.bottom()), Stroke::new(1.0, theme::HAIRLINE));
            }
        }
        if b % step == 0 {
            let x = view.x_of(b as f64 * bar);
            painter.vline(x, Rangef::new(rect.bottom() - 11.0, rect.bottom()), Stroke::new(1.0, theme::TEXT_FAINT));
            painter.text(pos2(x + 4.0, rect.top() + CYCLE_H + 1.0), Align2::LEFT_TOP, (b + 1).to_string(), FontId::proportional(11.0), theme::TEXT_DIM);
        }
    }
    painter.hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, theme::BORDER));
}

/// Song tempo changes on the timeline (absolute seconds, bpm), from unmuted regions with
/// tempo marks; duplicates (several tracks of one song) are merged.
pub(crate) fn tempo_marks(project: &Project) -> Vec<(f64, f64)> {
    let mut marks: Vec<(f64, f64)> = project
        .tracks
        .iter()
        .filter(|t| !t.mute)
        .flat_map(|t| &t.midi_clips)
        .filter(|c| !c.mute)
        .flat_map(|c| c.tempos.iter().filter(|m| m.start_secs < c.length_secs).map(|m| (c.position_secs + m.start_secs, m.bpm)))
        .collect();
    marks.sort_by(|a, b| a.0.total_cmp(&b.0));
    marks.dedup_by(|b, a| (b.0 - a.0).abs() < 1e-3 && (b.1 - a.1).abs() < 1e-6);
    marks
}

fn draw_tempo_row(painter: &Painter, rect: Rect, view: &View, marks: &[(f64, f64)], playhead: f64) {
    let p = painter.with_clip_rect(rect);
    p.rect_filled(rect, 0.0, theme::shade(theme::BG_HEADER, 0.9));
    let current = marks.iter().rposition(|m| m.0 <= playhead + 1e-9);
    let mut last_right = f32::NEG_INFINITY;
    for (i, &(t, bpm)) in marks.iter().enumerate() {
        let x = view.x_of(t);
        if x > rect.right() {
            break;
        }
        // The marking in effect at the playhead is highlighted.
        let color = if Some(i) == current { theme::TEMPO } else { theme::with_alpha(theme::TEMPO, 150) };
        p.vline(x, Rangef::new(rect.top() + 2.0, rect.bottom()), Stroke::new(1.0, color));
        let label_x = x.max(rect.left() + 2.0) + 3.0;
        // Skip labels that would overlap the previous one (zoomed far out).
        if label_x > last_right + 4.0 || Some(i) == current {
            let r = p.text(pos2(label_x, rect.center().y), Align2::LEFT_CENTER, theme::tempo_label(bpm), FontId::proportional(10.0), color);
            last_right = r.right();
        }
    }
    p.hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, theme::BORDER));
}

fn draw_playhead(painter: &Painter, rect: Rect, view: &View, t: f64, head: bool) {
    let x = view.x_of(t);
    if x < rect.left() || x > rect.right() {
        return;
    }
    painter.vline(x, rect.y_range(), Stroke::new(1.0, theme::PLAYHEAD));
    if head {
        let b = rect.bottom();
        painter.add(egui::Shape::convex_polygon(
            vec![pos2(x - 6.0, b - 11.0), pos2(x + 6.0, b - 11.0), pos2(x + 6.0, b - 6.0), pos2(x, b), pos2(x - 6.0, b - 6.0)],
            theme::PLAYHEAD,
            Stroke::NONE,
        ));
    }
}

fn track_header(app: &mut DawApp, ui: &mut egui::Ui, ti: usize, track: &Track, snap: &Snapshot) {
    let (rect, _) = ui.allocate_exact_size(vec2(HEADER_W, ROW_H), Sense::hover());
    // Registered before the header's widgets so the buttons/fader on top of it get their
    // clicks; only clicks on empty header space select the track.
    let background = ui.interact(rect, ui.make_persistent_id(("track_header", ti)), Sense::click());
    if background.clicked() {
        app.selected_track = Some(ti);
    }
    background.context_menu(|ui| track_context_menu(app, ui, ti));

    let selected = app.selected_track == Some(ti);
    let color = theme::track_color(ti);
    let p = ui.painter();
    p.rect_filled(rect, 0.0, if selected { theme::BG_HEADER_SELECTED } else { theme::BG_HEADER });
    p.rect_filled(Rect::from_min_size(rect.min, vec2(4.0, rect.height())), 0.0, color);
    p.hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, theme::BORDER));
    p.vline(rect.right() - 0.5, rect.y_range(), Stroke::new(1.0, theme::BORDER));
    p.text(pos2(rect.left() + 18.0, rect.center().y), Align2::CENTER_CENTER, (ti + 1).to_string(), FontId::proportional(11.0), theme::TEXT_FAINT);

    // Thin level meter along the right edge.
    let db = snap.status.track_peak_db.get(ti).copied().unwrap_or(-120.0);
    let meter = Rect::from_min_max(pos2(rect.right() - 7.0, rect.top() + 8.0), pos2(rect.right() - 4.0, rect.bottom() - 8.0));
    ui.scope_builder(UiBuilder::new().max_rect(meter), |ui| widgets::vmeter(ui, meter.size(), [db, db]));

    let content = Rect::from_min_max(pos2(rect.left() + 32.0, rect.top() + 7.0), pos2(rect.right() - 14.0, rect.bottom() - 7.0));
    ui.scope_builder(UiBuilder::new().max_rect(content).layout(Layout::top_down(Align::Min)), |ui| {
        ui.spacing_mut().item_spacing = vec2(4.0, 7.0);
        ui.horizontal(|ui| {
            let name = RichText::new(&track.name).size(12.5).color(if selected { Color32::WHITE } else { theme::TEXT });
            let name = ui.add(egui::Label::new(name).truncate().sense(Sense::click()));
            if name.clicked() {
                app.selected_track = Some(ti);
            }
            name.context_menu(|ui| track_context_menu(app, ui, ti));
            if let Some(inst) = &track.instrument {
                let short = super::short_plugin_name(&inst.name);
                ui.label(RichText::new(short).size(10.0).color(theme::TEXT_FAINT));
            }
        });
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 3.0;
            let size = vec2(20.0, 18.0);
            type Chip = (&'static str, bool, Color32, &'static str, fn(bool) -> TrackUpdate);
            let mut chips: Vec<Chip> = vec![
                ("M", track.mute, theme::MUTE, "Mute", |v| TrackUpdate { mute: Some(v), ..Default::default() }),
                ("S", track.solo, theme::SOLO, "Solo", |v| TrackUpdate { solo: Some(v), ..Default::default() }),
            ];
            // Record/input apply to audio input; instrument tracks don't record MIDI yet.
            if !track.is_instrument() {
                chips.push(("R", track.armed, theme::RECORD, "Record Enable", |v| TrackUpdate { armed: Some(v), ..Default::default() }));
                chips.push(("I", track.monitor_input, theme::INPUT, "Input Monitoring", |v| TrackUpdate {
                    monitor_input: Some(v),
                    ..Default::default()
                }));
            }
            for (text, on, accent, a11y, update) in chips {
                if widgets::chip(ui, on, text, accent, a11y, size).clicked() {
                    app.act(Action::UpdateTrack(ti, update(!on)));
                }
            }
            if track.is_instrument() {
                ui.add_space(2.0 * 23.0);
            }
            ui.add_space(6.0);
            let mut gain = track.volume;
            let fader_w = ui.available_width() - 26.0;
            if widgets::hfader(ui, &mut gain, fader_w, "Volume").changed() {
                app.act(Action::UpdateTrack(ti, TrackUpdate { volume: Some(gain), ..Default::default() }));
            }
            ui.add_space(2.0);
            let mut pan = track.pan;
            let r = widgets::knob(ui, &mut pan, -1.0, 1.0, 0.0, true, 20.0, "Pan");
            if r.changed() {
                app.act(Action::UpdateTrack(ti, TrackUpdate { pan: Some(pan), ..Default::default() }));
            }
            r.on_hover_text(format!("Pan {}", pan_label(pan)));
        });
    });

    // Delete button, top right, while hovering the header or when it's selected.
    // A child Ui so the button doesn't move the row's layout cursor (and the lane). It's
    // created every frame, shown or not, so the auto ids of the widgets after it (the lane)
    // stay stable — otherwise a drag starting on the lane would be lost.
    let at = Rect::from_min_size(pos2(rect.right() - 30.0, rect.top() + 5.0), vec2(18.0, 18.0));
    let mut child = ui.new_child(UiBuilder::new().id_salt(("delete_track", ti)).max_rect(at));
    if selected || ui.rect_contains_pointer(rect) {
        let r = widgets::icon_plain(&mut child, Icon::Close, theme::TEXT_DIM, "Delete Track", 18.0);
        if r.on_hover_text("Delete track (⌘⌫)").clicked() {
            app.act(Action::RemoveTrack(ti));
        }
    }
}

fn track_context_menu(app: &mut DawApp, ui: &mut egui::Ui, ti: usize) {
    app.selected_track = Some(ti);
    if ui.button("Delete Track").clicked() {
        app.act(Action::RemoveTrack(ti));
        ui.close();
    }
}

pub fn pan_label(v: f32) -> String {
    let pct = (v * 100.0).round() as i32;
    match pct {
        0 => "C".into(),
        p if p < 0 => format!("L{}", -p),
        p => format!("R{p}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_sizes_follow_tempo_and_signature() {
        let mut p = Project { tempo_bpm: 120.0, ..Project::default() };
        assert_eq!(grid_secs(SnapMode::Bar, &p, 50.0), Some(2.0));
        assert_eq!(grid_secs(SnapMode::Beat, &p, 50.0), Some(0.5));
        assert_eq!(grid_secs(SnapMode::Sixteenth, &p, 50.0), Some(0.125));
        assert_eq!(grid_secs(SnapMode::Off, &p, 50.0), None);
        p.time_signature = (6, 8);
        assert_eq!(grid_secs(SnapMode::Beat, &p, 50.0), Some(0.25));
        assert_eq!(grid_secs(SnapMode::Bar, &p, 50.0), Some(1.5));
        // Smart picks coarser grids when zoomed out.
        assert!(grid_secs(SnapMode::Smart, &p, 2.0).unwrap() > grid_secs(SnapMode::Smart, &p, 400.0).unwrap());
    }
}
