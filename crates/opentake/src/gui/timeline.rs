//! Arrangement view: ruler, track headers, the region timeline (audio and MIDI) and the
//! automation lanes shown under each track.

use std::sync::Arc;

use opentake_engine::{AutomationParam, RegionData, TrackUpdate, Waveform};
use opentake_engine::project::automation::{envelope_at, gain_to_pos, pos_to_gain};
use opentake_engine::project::tempo::{bar_len_q, beat_len_q};
use opentake_engine::project::{AutomationLane, AutomationMode, AutomationTarget, BarLine, Clip, Curve, Grid, MidiClip, Project, Take, TempoChange, TempoMap, Track};
use eframe::egui;
use egui::{Align, Align2, Color32, CursorIcon, FontId, Layout, Painter, Pos2, Rangef, Rect, RichText, Sense, Stroke, StrokeKind, UiBuilder, pos2, vec2};

use super::widgets::{self, Icon};
use super::{Action, DawApp, MASTER_UI, Snapshot, theme};

const HEADER_W: f32 = 260.0;
const ROW_H: f32 = 64.0;
const RULER_H: f32 = 30.0;
/// Ruler row with the project's tempo changes (the tempo track).
const TEMPO_ROW_H: f32 = 17.0;
/// Height of the cycle (loop) strip at the top of the ruler.
const CYCLE_H: f32 = 12.0;
const CYCLE_COLOR: Color32 = Color32::from_rgb(232, 190, 60);
/// Pointer distance (px) from a region edge that grabs the edge for trimming.
const EDGE_GRAB: f32 = 6.0;
const MIN_REGION_SECS: f64 = 0.01;
/// Height of an automation lane, and how close (px) the pointer must be to grab a point.
const LANE_H: f32 = 46.0;
const POINT_GRAB: f32 = 6.0;

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

    /// A take folder's takes and the one playing.
    pub fn takes(&self) -> Option<(&'a [Take], usize)> {
        match self.content {
            Content::Audio(c) if !c.takes.is_empty() => Some((&c.takes, c.active_take)),
            _ => None,
        }
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
    /// ⌥-drag of an audio region's end: time-stretch it to the new length.
    StretchEnd,
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
    Stretch { region: RegionRef, length: f64 },
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
    /// Each track's region row, and its whole extent including its automation lanes.
    rows: Vec<(usize, Rangef)>,
    spans: Vec<(usize, Rangef)>,
    drag: Option<RegionDrag>,
    marquee: Option<Marquee>,
    cycle_drag: Option<CycleDrag>,
    /// Timeline position of the last right-click (for "Split Here").
    context_time: f64,
    tempo_drag: Option<TempoDrag>,
    /// Timeline position of the last right-click on the tempo track.
    tempo_context: f64,
    /// The selected automation point (⌫ deletes it), a point being dragged, and the point
    /// (if any) under the last right-click in a lane.
    pub auto_sel: Option<AutoSel>,
    auto_drag: Option<AutoDrag>,
    auto_context: Option<usize>,
}

/// An automation point: whose lanes (a track, or `None` for the master), lane (index into
/// those lanes) and point.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AutoSel {
    pub owner: Option<usize>,
    pub lane: usize,
    pub point: usize,
}

struct AutoDrag {
    sel: AutoSel,
    /// The point's time and value when grabbed.
    from: (f64, f32),
    /// Where it was last sent, so an unmoved pointer sends nothing.
    sent: (f64, f32),
}

/// A drag on the tempo track.
#[derive(Debug, Clone, Copy)]
enum TempoDrag {
    /// Moving tempo change `index` (into the sorted changes).
    Move { index: usize, from: f64 },
    /// Changing the tempo of the section governed by `index` (`None` = the opening tempo).
    Bpm { index: Option<usize>, from: f64 },
}

impl Default for View {
    fn default() -> Self {
        Self {
            start: 0.0,
            px_per_sec: 48.0,
            x_range: Rangef::new(0.0, 0.0),
            rows: Vec::new(),
            spans: Vec::new(),
            drag: None,
            marquee: None,
            cycle_drag: None,
            context_time: 0.0,
            tempo_drag: None,
            tempo_context: 0.0,
            auto_sel: None,
            auto_drag: None,
            auto_context: None,
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

    /// The track at screen y (its region row or one of its automation lanes).
    pub fn track_at(&self, y: f32) -> Option<usize> {
        self.spans.iter().find(|(_, r)| r.contains(y)).map(|(i, _)| *i)
    }

    /// A track's extent: its region row and automation lanes.
    fn span_of(&self, track: usize) -> Option<Rangef> {
        self.spans.iter().find(|(i, _)| *i == track).map(|(_, r)| *r)
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

/// The snap grid at time `t` (it follows the tempo map), or `None` when snapping is off.
pub fn grid_at(mode: SnapMode, map: &TempoMap, t: f64, px_per_sec: f32) -> Option<Grid> {
    let seg = map.segment_at(t.max(0.0));
    Some(match mode {
        SnapMode::Off => return None,
        SnapMode::Bar => Grid::Bar,
        SnapMode::Beat => Grid::Quarters(seg.beat_len_q()),
        SnapMode::Eighth => Grid::Quarters(0.5),
        SnapMode::Sixteenth => Grid::Quarters(0.25),
        SnapMode::ThirtySecond => Grid::Quarters(0.125),
        SnapMode::Smart => {
            // Finest musical value that is still at least ~14 px wide.
            let min_q = 14.0 / px_per_sec as f64 * seg.bpm / 60.0;
            [0.25, 0.5, seg.beat_len_q()].into_iter().find(|&q| q >= min_q).map_or(Grid::Bar, Grid::Quarters)
        }
    })
}

/// Length in seconds of one grid step at `t`.
pub fn grid_step_secs(grid: Grid, map: &TempoMap, t: f64) -> f64 {
    match grid {
        Grid::Bar => map.bar_secs_at(t.max(0.0)),
        Grid::Quarters(q) => q * 60.0 / map.tempo_at(t.max(0.0)),
    }
}

/// Snap `t` to the grid (unless snapping is off or Alt is held).
pub fn snap_to(mode: SnapMode, ui: &egui::Ui, t: f64, project: &Project, px_per_sec: f32) -> f64 {
    let bypass = ui.input(|i| i.modifiers.alt);
    let map = project.tempo_map();
    match grid_at(mode, &map, t, px_per_sec) {
        Some(grid) if !bypass => map.snap(t, grid),
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

    // Ruler row: tools on the left, bar ruler and tempo track on the right.
    let map = project.tempo_map();
    let ruler_h = RULER_H + TEMPO_ROW_H;
    ui.horizontal(|ui| {
        let (bar, _) = ui.allocate_exact_size(vec2(HEADER_W, ruler_h), Sense::hover());
        ui.painter().rect_filled(bar, 0.0, theme::BG_HEADER);
        ui.painter().vline(bar.right() - 0.5, bar.y_range(), Stroke::new(1.0, theme::BORDER));
        let tools = Rect::from_min_size(bar.min, vec2(bar.width(), RULER_H));
        ui.scope_builder(UiBuilder::new().max_rect(tools.shrink2(vec2(8.0, 3.0))).layout(Layout::left_to_right(Align::Center)), |ui| {
            toolbar(app, ui, snap);
        });
        ui.painter().text(pos2(bar.right() - 10.0, bar.bottom() - TEMPO_ROW_H / 2.0), Align2::RIGHT_CENTER, "Tempo", FontId::proportional(10.0), theme::TEXT_FAINT);
        // An imported song with its own tempo the project doesn't follow: offer to adopt it.
        if snap.song_tempo_differs {
            let at = Rect::from_min_size(pos2(bar.left() + 8.0, bar.bottom() - TEMPO_ROW_H + 1.0), vec2(118.0, TEMPO_ROW_H - 2.0));
            let mut child = ui.new_child(UiBuilder::new().id_salt("use_song_tempo").max_rect(at));
            let text = RichText::new("Use song tempo").size(10.0).color(theme::TEMPO);
            let b = child.add(egui::Button::new(text).frame(false)).on_hover_text(
                "The imported song has its own tempo changes that the project (bars, grid, metronome) doesn't follow yet. Click to adopt the song's tempo map.",
            );
            if b.clicked() {
                app.act(Action::AdoptSongTempo);
            }
        }

        let (resp, painter) = ui.allocate_painter(vec2(ui.available_width(), ruler_h), Sense::hover());
        let full_rect = resp.rect;
        let rect = Rect::from_min_size(full_rect.min, vec2(full_rect.width(), RULER_H));
        app.view.x_range = rect.x_range();
        draw_ruler(&painter, rect, &app.view, &map);
        tempo_track(app, ui, &painter, Rect::from_min_max(pos2(rect.left(), rect.bottom()), full_rect.max), project, &map, playhead);
        let cycle_rect = Rect::from_min_max(rect.min, pos2(rect.right(), rect.top() + CYCLE_H));
        cycle_strip(app, ui, &painter, cycle_rect, project);
        let seek_rect = Rect::from_min_max(pos2(rect.left(), cycle_rect.bottom()), rect.max);
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
    let mut spans = Vec::with_capacity(project.tracks.len());
    let mut edit: Option<Edit> = None;
    let grid = grid_at(app.snap_mode, &map, app.view.start + app.view.visible_secs() / 2.0, app.view.px_per_sec);

    egui::ScrollArea::vertical().id_salt("arrange").auto_shrink([false, false]).show(ui, |ui| {
        ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
        for (ti, track) in project.tracks.iter().enumerate() {
            let row = ui.horizontal(|ui| {
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
                draw_grid(&painter, rect, &app.view, &map, grid);
                draw_loop_tint(&painter, rect, &app.view, project);
                painter.hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, theme::BORDER));
                // A bus has no regions: a tinted lane that names what it sums.
                if track.bus {
                    painter.rect_filled(rect.shrink2(vec2(0.0, 1.0)), 0.0, theme::with_alpha(theme::BUS, 14));
                    let sources: Vec<&str> = project.sources_of(ti).iter().map(|&s| project.tracks[s].name.as_str()).collect();
                    let text = if sources.is_empty() { "Bus — route tracks here with their Output".to_string() } else { format!("Bus — {}", sources.join(", ")) };
                    painter.text(pos2(rect.left() + 10.0, rect.center().y), Align2::LEFT_CENTER, text, FontId::proportional(11.5), theme::shade(theme::BUS, 0.8));
                }

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
                        let map = project.tempo_map();
                        let bar = map.bar_at(t);
                        app.act(Action::AddMidiRegion { track: ti, position: bar.secs, length: map.bar_secs_at(bar.secs) });
                    }
                }

                if track.armed {
                    if let Some(start) = snap.status.record_start_secs {
                        recording_region(&painter, rect, &app.view, start, playhead, project, ui);
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
            let mut span = row.response.rect.y_range();
            if track.show_automation {
                span.max = automation_rows(app, ui, LaneHost::track(ti, track), snap, &map, grid);
            }
            spans.push((ti, span));
        }

        // The master bus's automation, below the tracks.
        if project.show_master_automation {
            master_automation_header(app, ui, &map, grid);
            automation_rows(app, ui, LaneHost::master(project), snap, &map, grid);
        }

        // Empty area below the tracks: drop target and quick "new track".
        let height = ui.available_height().max(80.0);
        ui.horizontal(|ui| {
            let (left, _) = ui.allocate_exact_size(vec2(HEADER_W, height), Sense::hover());
            ui.painter().rect_filled(left, 0.0, theme::BG_PANEL);
            ui.painter().vline(left.right() - 0.5, left.y_range(), Stroke::new(1.0, theme::BORDER));
            let (resp, painter) = ui.allocate_painter(vec2(ui.available_width(), height), Sense::click());
            draw_grid(&painter, resp.rect, &app.view, &map, grid);
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
    app.view.spans = spans;

    match edit {
        Some(Edit::Move { delta_t, delta_track, copy }) => {
            let regions = app.selection.clone();
            app.act(Action::MoveRegions { regions, delta_secs: delta_t, delta_track, copy });
        }
        Some(Edit::Trim { region, start, end }) => app.act(Action::TrimRegion { region, start, end }),
        Some(Edit::Stretch { region, length }) => {
            let ps = opentake_engine::PitchStretch { length_secs: Some(length), ..Default::default() };
            app.act(Action::PitchStretch { track: region.track, clip: region.index, ps });
        }
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
/// What an armed track is recording right now: a red region growing with the playhead (a
/// cycle recording restarts it at the cycle start on every pass).
fn recording_region(painter: &Painter, row: Rect, view: &View, start: f64, playhead: f64, project: &Project, ui: &egui::Ui) {
    let start = if playhead < start && project.loop_enabled { project.loop_start_secs } else { start };
    if playhead <= start {
        return;
    }
    let rect = Rect::from_min_max(pos2(view.x_of(start), row.top() + 1.0), pos2(view.x_of(playhead), row.bottom() - 2.0)).intersect(row);
    if rect.width() <= 0.0 {
        return;
    }
    painter.rect_filled(rect, 5.0, theme::with_alpha(theme::RECORD, 60));
    painter.rect_stroke(rect, 5.0, Stroke::new(1.0, theme::with_alpha(theme::RECORD, 200)), StrokeKind::Inside);
    let bar = Rect::from_min_max(rect.min, pos2(rect.right(), rect.top() + 15.0));
    painter.rect_filled(bar, egui::CornerRadius { nw: 5, ne: 5, sw: 0, se: 0 }, theme::with_alpha(theme::RECORD, 170));
    // A slowly pulsing dot so it reads as live.
    let pulse = (ui.input(|i| i.time) * 3.0).sin() as f32 * 0.5 + 0.5;
    painter.circle_filled(pos2(bar.left() + 9.0, bar.center().y), 3.5, Color32::from_white_alpha((120.0 + 135.0 * pulse) as u8));
    painter.with_clip_rect(bar).text(pos2(bar.left() + 17.0, bar.center().y), Align2::LEFT_CENTER, "Recording", FontId::proportional(10.5), Color32::WHITE);
    ui.ctx().request_repaint();
}

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
        // Trim handles on hover (accent: ⌥ stretches the end instead).
        if let Some(mode) = hover_x.and_then(near_edge) {
            let x = if mode == DragMode::TrimStart { rect.left() + 1.5 } else { rect.right() - 1.5 };
            let stretch = mode == DragMode::TrimEnd && r.kind == RegionKind::Audio && ui.input(|i| i.modifiers.alt);
            painter.vline(x, Rangef::new(rect.top() + 3.0, rect.bottom() - 3.0), Stroke::new(3.0, if stretch { theme::ACCENT } else { Color32::WHITE }));
        }
    }

    // A take folder's badge picks the take that plays.
    if let (Some((takes, active)), Some(badge)) = (view.takes(), take_badge(rect, row)) {
        if drag.is_none() {
            let b = ui.interact(badge, ui.make_persistent_id(("takes", r)), Sense::click());
            widgets::label(&b, egui::Role::Button, &format!("Takes {}", view.name));
            if b.hovered() {
                painter.rect_stroke(badge, 4.0, Stroke::new(1.0, Color32::from_rgb(16, 16, 18)), StrokeKind::Inside);
            }
            egui::Popup::menu(&b).show(|ui| take_items(app, ui, r, takes, active));
        }
    }

    // Interaction.
    let pointer = resp.interact_pointer_pos();
    let shift = ui.input(|i| i.modifiers.shift);
    if resp.clicked() {
        app.selected_track = Some(r.track);
        app.view.auto_sel = None;
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
            let mut mode = near_edge(p.x).unwrap_or(DragMode::Move);
            if mode == DragMode::TrimEnd && r.kind == RegionKind::Audio && ui.input(|i| i.modifiers.alt) {
                mode = DragMode::StretchEnd;
            }
            if mode != DragMode::Move {
                app.selection = vec![r]; // trims apply to the grabbed region only
            }
            let edge = if matches!(mode, DragMode::TrimEnd | DragMode::StretchEnd) { view.end() } else { view.position };
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
            // Audio can't be extended past its source file (played `stretch` times as long),
            // nor stretched beyond the stretch range.
            let ((source_start, source_end), (min_len, max_len)) = match view.content {
                Content::Audio(c) => {
                    let s = c.position_secs - c.source_offset_secs * c.stretch;
                    let (lo, hi) = opentake_engine::STRETCH_RANGE;
                    let source = wf.map_or(f64::INFINITY, |w| s + w.frames as f64 / w.sample_rate as f64 * c.stretch);
                    ((s, source), (c.length_secs * lo / c.stretch, c.length_secs * hi / c.stretch))
                }
                Content::Midi(_) => ((0.0, f64::INFINITY), (0.0f64, f64::INFINITY)),
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
                DragMode::StretchEnd => {
                    let e = snap_time(app, ui, t, project).clamp(orig_start + min_len.max(MIN_REGION_SECS), orig_start + max_len);
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
                DragMode::StretchEnd if d.end != orig_end => Some(Edit::Stretch { region: r, length: d.end - d.start }),
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
    if let (Some((takes, active)), false) = (view.takes(), many) {
        widgets::section(ui, "Takes");
        take_items(app, ui, r, takes, active);
        if ui.button(format!("Delete {}", takes[active].name)).clicked() {
            app.act(Action::DeleteTake { track: r.track, clip: r.index, take: active });
            ui.close();
        }
        ui.separator();
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

/// One entry per take; picking one makes it play.
fn take_items(app: &mut DawApp, ui: &mut egui::Ui, r: RegionRef, takes: &[Take], active: usize) {
    for (i, take) in takes.iter().enumerate() {
        if ui.add(egui::Button::selectable(i == active, &take.name)).clicked() {
            if i != active {
                app.act(Action::SelectTake { track: r.track, clip: r.index, take: i });
            }
            ui.close();
        }
    }
}

/// Where a take folder shows its take, in the name bar's right end (if there's room).
fn take_badge(rect: Rect, row: Rect) -> Option<Rect> {
    let right = rect.right().min(row.right()) - 3.0;
    let left = right - 44.0;
    (left > rect.left().max(row.left()) + 24.0).then(|| Rect::from_min_max(pos2(left, rect.top() + 2.0), pos2(right, rect.top() + 13.0)))
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
    let badge = p.view.takes().and_then(|t| Some((t, take_badge(rect, row)?)));
    let name_clip = Rect::from_min_max(name_bar.min, pos2(badge.map_or(name_bar.right(), |(_, b)| b.left() - 2.0), name_bar.bottom()));
    // Pitch-shifted / stretched audio says so after its name.
    let name = match p.view.content {
        Content::Audio(c) if c.is_flexed() => {
            let pitch = (c.transpose() != 0.0).then(|| format!("{:+} st", (c.transpose() * 100.0).round() / 100.0));
            let stretch = (c.stretch != 1.0).then(|| format!("{:.0}%", c.stretch * 100.0));
            format!("{}  ·  {}", p.view.name, pitch.into_iter().chain(stretch).collect::<Vec<_>>().join(" "))
        }
        _ => p.view.name.to_string(),
    };
    painter.with_clip_rect(name_clip.intersect(painter.clip_rect())).text(
        pos2(name_bar.left().max(row.left()) + 5.0, name_bar.center().y),
        Align2::LEFT_CENTER,
        name,
        FontId::proportional(10.5),
        Color32::from_rgb(16, 16, 18),
    );
    if let Some(((takes, active), b)) = badge {
        painter.rect_filled(b, 4.0, theme::with_alpha(Color32::from_rgb(16, 16, 18), 70));
        let ink = Color32::from_rgb(16, 16, 18);
        let text = format!("{}/{}", active + 1, takes.len());
        painter.text(b.center() - vec2(4.0, 0.0), Align2::CENTER_CENTER, text, FontId::proportional(9.5), ink);
        let c = pos2(b.right() - 8.0, b.center().y + 1.0);
        painter.add(egui::Shape::convex_polygon(vec![c + vec2(-3.0, -2.0), c + vec2(3.0, -2.0), c + vec2(0.0, 1.5)], ink, Stroke::NONE));
    }
    let body = Rect::from_min_max(pos2(rect.left(), name_bar.bottom() + 1.0), pos2(rect.right(), rect.bottom() - 1.0));
    let ink = theme::shade(color, 0.22);
    match p.view.content {
        Content::Audio(c) => {
            if let Some(wf) = p.wf {
                // Trimming the start reveals/hides audio: shift the source offset to match.
                let mut shown = c.clone();
                shown.position_secs = p.start;
                shown.length_secs = p.end - p.start;
                match p.mode {
                    Some(DragMode::TrimStart) => shown.source_offset_secs = c.source_offset_secs + (p.start - c.position_secs) / c.stretch,
                    Some(DragMode::StretchEnd) => shown.stretch = c.stretch * shown.length_secs / c.length_secs,
                    _ => {}
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
    // In source seconds: a stretched region plays its source `stretch` times as long.
    let sec_per_px = 1.0 / view.px_per_sec as f64 / clip.stretch;
    let stroke = Stroke::new(1.0, color);
    let (from, to) = (rect.left().max(clip_to.left()) + 1.0, rect.right().min(clip_to.right()) - 1.0);
    let mut x = from.floor();
    while x < to {
        let t = (view.t_of(x) - clip.position_secs) / clip.stretch + clip.source_offset_secs;
        if t + sec_per_px <= 0.0 {
            x += 1.0; // before the file starts (a take that begins later than its folder)
            continue;
        }
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

/// Visible bar lines and the bar-number step that keeps numbers at least ~70 px apart
/// (based on the shortest bar on screen).
pub(crate) fn visible_bars(view: &View, map: &TempoMap) -> (Vec<BarLine>, i64) {
    let (t0, t1) = (view.start, view.start + view.visible_secs());
    let bars = map.bars(t0, t1);
    let shortest = bars.iter().map(|b| map.bar_secs_at(b.secs)).fold(f64::INFINITY, f64::min).min(map.bar_secs_at(t0));
    let mut step = 1i64;
    while shortest * step as f64 * (view.px_per_sec as f64) < 70.0 && step < 4096 {
        step *= 2;
    }
    (bars, step)
}

/// Lines inside each visible bar every `step_q` quarter notes (bar lines excluded), where
/// they're at least `min_px` apart.
pub(crate) fn lines_in_bars(bars: &[BarLine], map: &TempoMap, view: &View, min_px: f32, step_q: impl Fn(&BarLine) -> f64, mut f: impl FnMut(f32)) {
    for (i, bar) in bars.iter().enumerate() {
        let step = step_q(bar);
        if step <= 0.0 || (step * 60.0 / map.tempo_at(bar.secs)) as f32 * view.px_per_sec < min_px {
            continue;
        }
        let end_q = bars.get(i + 1).map_or_else(|| bar.q + bar_len_q(bar.sig), |b| b.q);
        let mut k = 1;
        while bar.q + k as f64 * step < end_q - 1e-6 && k < 2048 {
            f(view.x_of(map.secs_at(bar.q + k as f64 * step)));
            k += 1;
        }
    }
}

/// Bar lines, beat lines and (when snapping finer than a beat) subdivision lines, all
/// following the tempo map.
pub(crate) fn draw_grid(painter: &Painter, rect: Rect, view: &View, map: &TempoMap, snap_grid: Option<Grid>) {
    let (bars, step) = visible_bars(view, map);
    if step == 1 {
        if let Some(Grid::Quarters(q)) = snap_grid {
            lines_in_bars(&bars, map, view, 8.0, |b| if q < beat_len_q(b.sig) - 1e-9 { q } else { 0.0 }, |x| {
                painter.vline(x, rect.y_range(), Stroke::new(1.0, theme::GRID_SUB));
            });
        }
        lines_in_bars(&bars, map, view, 12.0, |b| beat_len_q(b.sig), |x| {
            painter.vline(x, rect.y_range(), Stroke::new(1.0, theme::GRID_BEAT));
        });
    }
    for b in bars.iter().filter(|b| b.index % step == 0 || b.sig_change) {
        painter.vline(view.x_of(b.secs), rect.y_range(), Stroke::new(1.0, theme::GRID_BAR));
    }
}

fn draw_ruler(painter: &Painter, rect: Rect, view: &View, map: &TempoMap) {
    painter.rect_filled(rect, 0.0, theme::BG_HEADER);
    painter.rect_filled(Rect::from_min_max(rect.min, pos2(rect.right(), rect.top() + CYCLE_H)), 0.0, theme::shade(theme::BG_HEADER, 0.85));
    let (bars, step) = visible_bars(view, map);
    if step == 1 {
        lines_in_bars(&bars, map, view, 12.0, |b| beat_len_q(b.sig), |x| {
            painter.vline(x, Rangef::new(rect.bottom() - 5.0, rect.bottom()), Stroke::new(1.0, theme::HAIRLINE));
        });
    }
    let mut last_right = f32::NEG_INFINITY;
    for b in bars.iter().filter(|b| b.index % step == 0 || b.sig_change) {
        let x = view.x_of(b.secs);
        painter.vline(x, Rangef::new(rect.bottom() - 11.0, rect.bottom()), Stroke::new(1.0, theme::TEXT_FAINT));
        if x + 4.0 < last_right {
            continue;
        }
        let r = painter.text(pos2(x + 4.0, rect.top() + CYCLE_H + 1.0), Align2::LEFT_TOP, (b.index + 1).to_string(), FontId::proportional(11.0), theme::TEXT_DIM);
        last_right = r.right() + 6.0;
        // Time signature where it changes (and at the start).
        if b.sig_change || b.index == 0 {
            let r = painter.text(pos2(r.right() + 5.0, rect.top() + CYCLE_H + 2.0), Align2::LEFT_TOP, format!("{}/{}", b.sig.0, b.sig.1), FontId::proportional(9.5), theme::TEXT_FAINT);
            last_right = r.right() + 6.0;
        }
    }
    painter.hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, theme::BORDER));
}

/// Imported songs' tempo marks (absolute seconds, bpm) from unmuted regions, merged.
fn song_tempo_marks(project: &Project) -> Vec<(f64, f64)> {
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

/// Common time signatures offered in menus.
const SIGNATURES: [(u32, u32); 9] = [(2, 4), (3, 4), (4, 4), (5, 4), (6, 4), (6, 8), (7, 8), (9, 8), (12, 8)];

/// The tempo track: the project's tempo changes along the timeline, editable. Drag a
/// section up/down to change its tempo, drag a marker sideways to move it, double-click to
/// add a change, right-click for exact values, time signatures and deleting.
fn tempo_track(app: &mut DawApp, ui: &egui::Ui, painter: &Painter, rect: Rect, project: &Project, map: &TempoMap, playhead: f64) {
    let p = painter.with_clip_rect(rect);
    let bg = theme::shade(theme::BG_HEADER, 0.9);
    p.rect_filled(rect, 0.0, bg);
    let mut sorted = project.tempo_changes.clone();
    sorted.sort_by(|a, b| a.at_secs.total_cmp(&b.at_secs));
    let changes = &sorted;
    let governing = |t: f64| changes.iter().rposition(|c| c.at_secs <= t + 1e-9);
    let tempo_of = |i: Option<usize>| i.map_or(project.tempo_bpm, |i| changes[i].bpm);

    // Markings: the opening tempo, then each change.
    let mut marks: Vec<(f32, f64)> = vec![(app.view.x_of(0.0), project.tempo_bpm)];
    marks.extend(changes.iter().map(|c| (app.view.x_of(c.at_secs), c.bpm)));
    let current = Some(governing(playhead).map_or(0, |i| i + 1));
    for (i, &(x, _)) in marks.iter().enumerate().skip(1).filter(|(_, m)| rect.x_range().contains(m.0)) {
        let color = if Some(i) == current { theme::TEMPO } else { theme::with_alpha(theme::TEMPO, 150) };
        p.vline(x, Rangef::new(rect.top() + 2.0, rect.bottom()), Stroke::new(1.0, color));
    }
    draw_tempo_labels(&p, rect.x_range(), rect.center().y, &marks, current, bg);
    // The song's own tempo marks where the project doesn't follow them: faint ticks.
    for (t, bpm) in song_tempo_marks(project) {
        if (map.tempo_at(t + 1e-6) - bpm).abs() > 0.05 {
            let x = app.view.x_of(t);
            p.vline(x, Rangef::new(rect.top() + 1.0, rect.top() + 6.0), Stroke::new(1.0, theme::with_alpha(theme::TEMPO, 110)));
        }
    }
    p.hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, theme::BORDER));

    let resp = ui.interact(rect, ui.make_persistent_id("tempo_track"), Sense::click_and_drag());
    widgets::label(&resp, egui::Role::Group, "Tempo Track");
    let hover = resp.hover_pos().or(resp.interact_pointer_pos());
    let near_marker = |x: f32| changes.iter().position(|c| (app.view.x_of(c.at_secs) - x).abs() <= 5.0);
    if let Some(pos) = hover {
        ui.ctx().set_cursor_icon(if near_marker(pos.x).is_some() { CursorIcon::ResizeHorizontal } else { CursorIcon::ResizeVertical });
    }
    let resp = resp.on_hover_text("Tempo track: drag up/down to change a tempo (Shift = fine), drag a marker to move it, double-click to add a change, right-click for more");

    if resp.drag_started() {
        if let Some(origin) = ui.input(|i| i.pointer.press_origin()) {
            app.view.tempo_drag = Some(match near_marker(origin.x) {
                Some(index) => TempoDrag::Move { index, from: changes[index].at_secs },
                None => {
                    let index = app.view.time_at(origin.x).and_then(governing);
                    TempoDrag::Bpm { index, from: tempo_of(index) }
                }
            });
        }
    }
    if resp.dragged() {
        let total = ui.input(|i| i.pointer.press_origin().zip(i.pointer.latest_pos()).map(|(a, b)| b - a)).unwrap_or_default();
        match app.view.tempo_drag {
            Some(TempoDrag::Move { index, from }) => {
                let lo = if index > 0 { changes[index - 1].at_secs } else { 0.0 } + 0.01;
                let hi = changes.get(index + 1).map_or(f64::INFINITY, |c| c.at_secs - 0.01);
                let t = snap_time(app, ui, from + (total.x / app.view.px_per_sec) as f64, project).clamp(lo, hi.max(lo));
                if (t - changes[index].at_secs).abs() > 1e-9 {
                    let mut c = changes.clone();
                    c[index].at_secs = t;
                    app.act(Action::SetTempoChanges { changes: c, merge: Some(format!("tempo-move:{index}")) });
                }
            }
            Some(TempoDrag::Bpm { index, from }) => {
                let per_px = if ui.input(|i| i.modifiers.shift) { 0.05 } else { 0.5 };
                let bpm = ((from - total.y as f64 * per_px) * 10.0).round().clamp(200.0, 9990.0) / 10.0;
                if (bpm - tempo_of(index)).abs() > 1e-9 {
                    match index {
                        None => app.act(Action::SetTempo(bpm)),
                        Some(i) => {
                            let mut c = changes.clone();
                            c[i].bpm = bpm;
                            app.act(Action::SetTempoChanges { changes: c, merge: Some(format!("tempo-bpm:{i}")) });
                        }
                    }
                }
            }
            None => {}
        }
    }
    if resp.drag_stopped() {
        app.view.tempo_drag = None;
    }
    if resp.double_clicked() {
        if let Some(t) = resp.interact_pointer_pos().and_then(|p| app.view.time_at(p.x)) {
            let t = snap_time(app, ui, t, project);
            if t > 0.01 && !changes.iter().any(|c| (c.at_secs - t).abs() < 0.01) {
                let mut c = changes.clone();
                c.push(TempoChange { at_secs: t, bpm: map.tempo_at(t), time_signature: None });
                c.sort_by(|a, b| a.at_secs.total_cmp(&b.at_secs));
                app.act(Action::SetTempoChanges { changes: c, merge: None });
            }
        }
    }
    if resp.secondary_clicked() {
        if let Some(t) = resp.interact_pointer_pos().and_then(|p| app.view.time_at(p.x)) {
            app.view.tempo_context = t;
        }
    }
    resp.context_menu(|ui| {
        let t = app.view.tempo_context;
        let index = governing(t);
        let pos = map.position(t);
        widgets::section(ui, &format!("Tempo at bar {}", pos.bar));
        ui.horizontal(|ui| {
            ui.label("Tempo");
            let mut bpm = tempo_of(index);
            if ui.add(egui::DragValue::new(&mut bpm).range(20.0..=999.0).speed(0.5).fixed_decimals(1).suffix(" BPM")).changed() {
                match index {
                    None => app.act(Action::SetTempo(bpm)),
                    Some(i) => {
                        let mut c = changes.clone();
                        c[i].bpm = bpm;
                        app.act(Action::SetTempoChanges { changes: c, merge: Some(format!("tempo-bpm:{i}")) });
                    }
                }
            }
        });
        ui.menu_button("Time Signature", |ui| {
            let current = map.signature_at(t);
            for sig in SIGNATURES {
                if ui.add(egui::Button::new(format!("{}/{}", sig.0, sig.1)).selected(sig == current)).clicked() {
                    match index {
                        None => app.act(Action::SetTimeSignature(sig)),
                        Some(i) => {
                            let mut c = changes.clone();
                            c[i].time_signature = Some(sig);
                            app.act(Action::SetTempoChanges { changes: c, merge: None });
                        }
                    }
                    ui.close();
                }
            }
        });
        ui.separator();
        if ui.button("Add Tempo Change Here").clicked() {
            let at = snap_time(app, ui, t, project);
            if at > 0.01 && !changes.iter().any(|c| (c.at_secs - at).abs() < 0.01) {
                let mut c = changes.clone();
                c.push(TempoChange { at_secs: at, bpm: map.tempo_at(at), time_signature: None });
                c.sort_by(|a, b| a.at_secs.total_cmp(&b.at_secs));
                app.act(Action::SetTempoChanges { changes: c, merge: None });
            }
            ui.close();
        }
        if ui.button("Add Time Signature Change at Bar").clicked() {
            // At the bar line nearest the click, keeping the signature (pick one after).
            let bar = map.bar_at(t);
            let at = if t - bar.secs > map.bar_secs_at(t) / 2.0 { map.secs_of(bar.index + 2, 1.0) } else { bar.secs };
            if at > 0.01 && !changes.iter().any(|c| (c.at_secs - at).abs() < 0.01) {
                let mut c = changes.clone();
                c.push(TempoChange { at_secs: at, bpm: map.tempo_at(at), time_signature: Some(map.signature_at(at)) });
                c.sort_by(|a, b| a.at_secs.total_cmp(&b.at_secs));
                app.act(Action::SetTempoChanges { changes: c, merge: None });
            }
            ui.close();
        }
        if let Some(i) = index {
            if ui.button("Delete Tempo Change").clicked() {
                let mut c = changes.clone();
                c.remove(i);
                app.act(Action::SetTempoChanges { changes: c, merge: None });
                ui.close();
            }
        }
        if !song_tempo_marks(project).is_empty() && ui.button("Use Song's Tempo Map").clicked() {
            app.act(Action::AdoptSongTempo);
            ui.close();
        }
        if !changes.is_empty() && ui.button("Remove All Tempo Changes").clicked() {
            app.act(Action::SetTempoChanges { changes: Vec::new(), merge: None });
            ui.close();
        }
    });
}

/// Where tempo labels go along a row spanning `span`: (mark index, label left x).
/// The marking in effect at the left edge sticks there until the next one pushes it out,
/// earlier ones aren't shown, and where labels would overlap the one at the playhead wins,
/// then later ones.
pub(crate) fn place_tempo_labels(span: Rangef, marks: &[(f32, f32)], current: Option<usize>) -> Vec<(usize, f32)> {
    const PAD: f32 = 3.0;
    let first_visible = marks.iter().position(|m| m.0 >= span.min).unwrap_or(marks.len());
    let sticky = first_visible.checked_sub(1);
    let mut candidates: Vec<(usize, f32, f32)> = Vec::new(); // (index, left, width)
    for (i, &(x, w)) in marks.iter().enumerate() {
        if x > span.max {
            break;
        }
        if i + 1 < first_visible {
            continue; // superseded before the left edge
        }
        let left = if Some(i) == sticky {
            // Pushed out to the left by the next marking as it arrives.
            let next = marks.get(first_visible).map_or(f32::INFINITY, |m| m.0 + PAD);
            (span.min + PAD).min(next - w - 2.0 * PAD)
        } else {
            x + PAD
        };
        candidates.push((i, left, w));
    }
    let mut order: Vec<usize> = (0..candidates.len()).collect();
    order.sort_by_key(|&k| (Some(candidates[k].0) != current, std::cmp::Reverse(candidates[k].0)));
    let mut taken: Vec<Rangef> = Vec::new();
    let mut placed = Vec::new();
    for k in order {
        let (i, left, w) = candidates[k];
        let r = Rangef::new(left - 2.0, left + w + 2.0);
        if taken.iter().all(|t| !t.intersects(r)) {
            taken.push(r);
            placed.push((i, left));
        }
    }
    placed.sort_by_key(|p| p.0);
    placed
}

/// Draw tempo labels (screen x, bpm) centred on `y`, each on a `bg` backing so lines behind
/// them don't strike through. The painter should clip to the row.
pub(crate) fn draw_tempo_labels(p: &Painter, span: Rangef, y: f32, marks: &[(f32, f64)], current: Option<usize>, bg: Color32) {
    let font = FontId::proportional(10.0);
    let galleys: Vec<_> = marks
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let color = if Some(i) == current { theme::TEMPO } else { theme::with_alpha(theme::TEMPO, 150) };
            p.layout_no_wrap(theme::tempo_label(m.1), font.clone(), color)
        })
        .collect();
    let widths: Vec<(f32, f32)> = marks.iter().zip(&galleys).map(|(m, g)| (m.0, g.size().x)).collect();
    for (i, left) in place_tempo_labels(span, &widths, current) {
        let g = galleys[i].clone();
        let r = Rect::from_min_size(pos2(left, y - g.size().y / 2.0), g.size());
        p.rect_filled(r.expand2(vec2(2.0, 0.0)), 2.0, bg);
        p.galley(r.min, g, theme::TEMPO);
    }
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
    let background = ui.interact(rect, ui.make_persistent_id(("track_header", ti)), Sense::click_and_drag());
    if background.clicked() {
        app.selected_track = Some(ti);
    }
    background.context_menu(|ui| track_context_menu(app, ui, ti, track));
    // Dragging the header moves the track; a line shows where it will land.
    if background.drag_started() {
        app.track_drag = Some(ti);
        app.selected_track = Some(ti);
    }
    if app.track_drag == Some(ti) {
        ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
        let last = snap.project.tracks.len().saturating_sub(1);
        let target = ui.ctx().pointer_latest_pos().map_or(ti, |p| match app.view.track_at(p.y) {
            Some(t) => t,
            None if app.view.span_of(0).is_some_and(|r| p.y < r.min) => 0,
            None => last,
        });
        if background.drag_stopped() {
            app.track_drag = None;
            if target != ti {
                app.act(Action::MoveTrack { from: ti, to: target });
            }
        } else if let Some(row) = app.view.span_of(target).filter(|_| target != ti) {
            let y = if target > ti { row.max } else { row.min };
            let layer = egui::LayerId::new(egui::Order::Foreground, ui.make_persistent_id("track_drop"));
            let x = Rangef::new(rect.left(), ui.ctx().content_rect().right());
            ui.ctx().layer_painter(layer).hline(x, y, Stroke::new(2.0, theme::ACCENT));
        }
    }

    let selected = app.selected_track == Some(ti);
    let color = theme::color_of(track, ti);
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
            name.context_menu(|ui| track_context_menu(app, ui, ti, track));
            if let Some(inst) = &track.instrument {
                let short = super::short_plugin_name(&inst.name);
                ui.label(RichText::new(short).size(10.0).color(theme::TEXT_FAINT));
            }
            if track.bus {
                ui.label(RichText::new("BUS").size(9.5).strong().color(theme::BUS));
            } else if let Some(b) = snap.project.output_of(ti) {
                ui.label(RichText::new(format!("→ {}", snap.project.tracks[b].name)).size(10.0).color(theme::shade(theme::BUS, 0.85)));
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
            // Record/input apply to audio input; instrument tracks don't record MIDI yet, and
            // buses only mix.
            let records = !track.is_instrument() && !track.bus;
            if records {
                chips.push(("R", track.armed, theme::RECORD, "Record Enable", |v| TrackUpdate { armed: Some(v), ..Default::default() }));
                chips.push(("I", track.monitor_input, theme::INPUT, "Input Monitoring", |v| TrackUpdate {
                    monitor_input: Some(v),
                    ..Default::default()
                }));
            }
            // Automated controls show (and while stopped, edit) their lane's value at the playhead.
            let playhead = snap.status.position_secs;
            let auto = |target| track.automated(target, playhead);
            let has_lane = |target| track.automation.iter().any(|l| l.target == target && l.mode != AutomationMode::Off);
            chips[0].1 = auto(AutomationTarget::Mute).map_or(track.mute, |v| v >= 0.5);
            for (text, on, accent, a11y, update) in chips {
                if widgets::chip(ui, on, text, accent, a11y, size).clicked() {
                    let plain = Action::UpdateTrack(ti, update(!on));
                    if text == "M" {
                        app.control(&track.automation, Some(ti), AutomationTarget::Mute, if on { 0.0 } else { 1.0 }, plain);
                    } else {
                        app.act(plain);
                    }
                }
            }
            if !records {
                ui.add_space(2.0 * 23.0);
            }
            let a = widgets::chip(ui, track.show_automation, "A", theme::AUTOMATION, "Show Automation", size);
            if a.clicked() {
                app.act(Action::ShowAutomation { track: Some(ti), show: !track.show_automation });
            }
            ui.add_space(6.0);
            let mut gain = auto(AutomationTarget::Volume).map_or(track.volume, pos_to_gain);
            let fader_w = ui.available_width() - 26.0;
            let r = widgets::hfader(ui, &mut gain, fader_w, "Volume");
            // Held still, an automated fader keeps writing (Touch/Latch).
            if r.changed() || (r.dragged() && has_lane(AutomationTarget::Volume)) {
                app.control(&track.automation, Some(ti), AutomationTarget::Volume, gain_to_pos(gain), Action::UpdateTrack(ti, TrackUpdate { volume: Some(gain), ..Default::default() }));
            }
            if r.drag_stopped() && has_lane(AutomationTarget::Volume) {
                app.act(Action::ReleaseControl { owner: Some(ti), target: AutomationTarget::Volume });
            }
            ui.add_space(2.0);
            let mut pan = auto(AutomationTarget::Pan).map_or(track.pan, |v| v * 2.0 - 1.0);
            let r = widgets::knob(ui, &mut pan, -1.0, 1.0, 0.0, true, 20.0, "Pan");
            if r.changed() || (r.dragged() && has_lane(AutomationTarget::Pan)) {
                app.control(&track.automation, Some(ti), AutomationTarget::Pan, (pan + 1.0) / 2.0, Action::UpdateTrack(ti, TrackUpdate { pan: Some(pan), ..Default::default() }));
            }
            if r.drag_stopped() && has_lane(AutomationTarget::Pan) {
                app.act(Action::ReleaseControl { owner: Some(ti), target: AutomationTarget::Pan });
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

fn track_context_menu(app: &mut DawApp, ui: &mut egui::Ui, ti: usize, track: &Track) {
    app.selected_track = Some(ti);
    let show = !track.show_automation;
    if ui.add(egui::Button::new(if show { "Show Automation" } else { "Hide Automation" }).shortcut_text("A")).clicked() {
        app.act(Action::ShowAutomation { track: Some(ti), show });
        ui.close();
    }
    if ui.button("Add Automation Lane").clicked() {
        app.act(Action::AddLane { owner: Some(ti), target: None });
        ui.close();
    }
    if track.automation.iter().any(|l| !l.shown) && ui.button("Show All Automation Lanes").clicked() {
        for (lane, _) in track.automation.iter().enumerate().filter(|(_, l)| !l.shown) {
            app.act(Action::SetLaneShown { owner: Some(ti), lane, shown: true });
        }
        app.act(Action::ShowAutomation { track: Some(ti), show: true });
        ui.close();
    }
    ui.separator();
    if ui.button("Delete Track").clicked() {
        app.act(Action::RemoveTrack(ti));
        ui.close();
    }
}

/// The row above the master's automation lanes: its name and a button to hide them.
fn master_automation_header(app: &mut DawApp, ui: &mut egui::Ui, map: &TempoMap, grid: Option<Grid>) {
    const H: f32 = 28.0;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
        let w = (app.view.lanes().min - ui.cursor().left()).clamp(0.0, HEADER_W);
        let (rect, _) = ui.allocate_exact_size(vec2(w, H), Sense::hover());
        let rect = Rect::from_min_size(rect.min, vec2(HEADER_W, H));
        let p = ui.painter();
        p.rect_filled(rect, 0.0, theme::BG_HEADER);
        p.rect_filled(Rect::from_min_size(rect.min, vec2(4.0, H)), 0.0, theme::TEXT_DIM);
        p.hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, theme::BORDER));
        p.vline(rect.right() - 0.5, rect.y_range(), Stroke::new(1.0, theme::BORDER));
        p.text(pos2(rect.left() + 32.0, rect.center().y), Align2::LEFT_CENTER, "Stereo Out", FontId::proportional(12.5), theme::TEXT);
        p.text(pos2(rect.left() + 108.0, rect.center().y), Align2::LEFT_CENTER, "MASTER", FontId::proportional(9.5), theme::TEXT_FAINT);
        let chip = Rect::from_min_size(pos2(rect.right() - 40.0, rect.center().y - 9.0), vec2(22.0, 18.0));
        let mut child = ui.new_child(UiBuilder::new().id_salt("master_auto_chip").max_rect(chip));
        if widgets::chip(&mut child, true, "A", theme::AUTOMATION, "Hide Master Automation", vec2(22.0, 18.0)).clicked() {
            app.act(Action::ShowMasterAutomation(false));
        }
        let (resp, painter) = ui.allocate_painter(vec2(ui.available_width(), H), Sense::hover());
        painter.rect_filled(resp.rect, 0.0, theme::shade(theme::BG_LANE, 0.7));
        draw_grid(&painter, resp.rect, &app.view, map, grid);
        painter.hline(resp.rect.x_range(), resp.rect.bottom() - 0.5, Stroke::new(1.0, theme::BORDER));
    });
}

/// Whose automation lanes a set of rows shows: a track, or the master bus.
#[derive(Clone, Copy)]
pub(crate) struct LaneHost<'a> {
    pub owner: Option<usize>,
    /// Key for ids and `Snapshot::auto_params` (the track index, or `MASTER_UI`).
    pub key: usize,
    /// Stable id for undo merge keys.
    pub id: u64,
    pub name: &'a str,
    pub color: Color32,
    pub lanes: &'a [AutomationLane],
    /// Current volume, pan and mute, drawn as a guide in empty lanes.
    pub volume: f32,
    pub pan: f32,
    pub mute: bool,
}

impl<'a> LaneHost<'a> {
    pub fn track(ti: usize, track: &'a Track) -> Self {
        Self { owner: Some(ti), key: ti, id: track.id, name: &track.name, color: theme::color_of(track, ti), lanes: &track.automation, volume: track.volume, pan: track.pan, mute: track.mute }
    }

    pub fn master(project: &'a Project) -> Self {
        let lanes = &project.master_automation;
        Self { owner: None, key: MASTER_UI, id: u64::MAX, name: "Stereo Out", color: theme::TEXT_DIM, lanes, volume: project.master_volume, pan: 0.0, mute: false }
    }
}

/// The automation lanes under a track or the master (or a row offering to add one). Returns
/// the bottom of the last row.
fn automation_rows(app: &mut DawApp, ui: &mut egui::Ui, host: LaneHost, snap: &Snapshot, map: &TempoMap, grid: Option<Grid>) -> f32 {
    let params = snap.auto_params.get(&host.key).map_or(&[][..], |p| p.as_slice());
    let lanes: Vec<(usize, &AutomationLane)> = host.lanes.iter().enumerate().filter(|(_, l)| l.shown).collect();
    let mut bottom = ui.cursor().top();
    if lanes.is_empty() {
        let r = ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
            let rect = lane_header_rect(app, ui);
            lane_header_frame(ui, rect, host.color);
            let at = Rect::from_min_size(pos2(rect.left() + 32.0, rect.center().y - 11.0), vec2(150.0, 22.0));
            let mut child = ui.new_child(UiBuilder::new().id_salt(("add_lane", host.key)).max_rect(at));
            if child.button(RichText::new("+ Add Automation Lane").size(11.0)).clicked() {
                app.act(Action::AddLane { owner: host.owner, target: None });
            }
            let (resp, painter) = ui.allocate_painter(vec2(ui.available_width(), LANE_H), Sense::hover());
            painter.rect_filled(resp.rect, 0.0, theme::shade(theme::BG_LANE, 0.85));
            painter.hline(resp.rect.x_range(), resp.rect.bottom() - 0.5, Stroke::new(1.0, theme::BORDER));
        });
        return r.response.rect.bottom();
    }
    for (li, lane) in lanes {
        let r = ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
            lane_header(app, ui, host, li, lane, params, snap.status.position_secs);
            lane_area(app, ui, host, li, lane, snap, map, grid);
        });
        bottom = r.response.rect.bottom();
    }
    bottom
}

/// Room for a lane header: the lane area starts where the timeline does (as the region
/// rows do), so lanes line up with the ruler.
fn lane_header_rect(app: &DawApp, ui: &mut egui::Ui) -> Rect {
    let w = (app.view.lanes().min - ui.cursor().left()).clamp(0.0, HEADER_W);
    let (rect, _) = ui.allocate_exact_size(vec2(w, LANE_H), Sense::hover());
    Rect::from_min_size(rect.min, vec2(HEADER_W, LANE_H))
}

fn lane_header_frame(ui: &egui::Ui, rect: Rect, color: Color32) {
    let p = ui.painter();
    p.rect_filled(rect, 0.0, theme::shade(theme::BG_HEADER, 0.9));
    p.rect_filled(Rect::from_min_size(rect.min, vec2(4.0, rect.height())), 0.0, theme::with_alpha(color, 110));
    p.hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, theme::BORDER));
    p.vline(rect.right() - 0.5, rect.y_range(), Stroke::new(1.0, theme::BORDER));
}

/// A lane target's name, from the track's automatable parameters.
fn lane_name(params: &[AutomationParam], target: AutomationTarget) -> String {
    match params.iter().find(|p| p.target == target) {
        Some(p) if p.slot.is_some() => format!("{}: {}", p.group.trim_start_matches("OpenTake "), p.name),
        Some(p) => p.name.clone(),
        None => format!("{target:?}"),
    }
}

fn mode_color(mode: AutomationMode) -> Color32 {
    match mode {
        AutomationMode::Off => theme::TEXT_FAINT,
        AutomationMode::Read => theme::AUTOMATION,
        AutomationMode::Touch | AutomationMode::Latch => theme::AUTOMATION_WRITE,
    }
}

/// Lane header: parameter picker, mode, the value at the playhead, add and remove.
#[allow(clippy::too_many_arguments)]
fn lane_header(app: &mut DawApp, ui: &mut egui::Ui, host: LaneHost, li: usize, lane: &AutomationLane, params: &[AutomationParam], playhead: f64) {
    let rect = lane_header_rect(app, ui);
    lane_header_frame(ui, rect, host.color);
    let (owner, ti) = (host.owner, host.key);
    let content = Rect::from_min_max(pos2(rect.left() + 32.0, rect.top() + 4.0), pos2(rect.right() - 10.0, rect.bottom() - 4.0));
    // A child Ui, so nothing in the header can push the lane area aside.
    let mut child = ui.new_child(UiBuilder::new().id_salt(("lane_header", ti, li)).max_rect(content).layout(Layout::top_down(Align::Min)));
    {
        let ui = &mut child;
        ui.spacing_mut().item_spacing = vec2(4.0, 2.0);
        ui.horizontal(|ui| {
            let name = lane_name(params, lane.target);
            // Long plug-in parameter names are cut so the mode stays in view.
            let short: String = if name.chars().count() > 22 { name.chars().take(21).chain(['…']).collect() } else { name.clone() };
            let picker = egui::ComboBox::from_id_salt(("lane_target", ti, li)).selected_text(RichText::new(short).size(11.0)).width(128.0).truncate().height(360.0).show_ui(ui, |ui| {
                let mut group = "";
                for p in params {
                    if p.group != group {
                        widgets::section(ui, &p.group);
                        group = &p.group;
                    }
                    let taken = host.lanes.iter().any(|l| l.target == p.target);
                    let b = ui.add_enabled(!taken || p.target == lane.target, egui::Button::selectable(p.target == lane.target, &p.name));
                    if b.clicked() && p.target != lane.target {
                        app.act(Action::SetLaneTarget { owner, lane: li, target: p.target });
                    }
                }
            });
            widgets::label(&picker.response, egui::Role::ComboBox, &format!("Automation Parameter {name}"));
            let mut mode = lane.mode;
            let picker = egui::ComboBox::from_id_salt(("lane_mode", ti, li)).selected_text(RichText::new(lane.mode.label()).size(11.0).color(mode_color(lane.mode))).width(46.0).show_ui(ui, |ui| {
                for m in AutomationMode::ALL {
                    ui.selectable_value(&mut mode, m, RichText::new(m.label()).color(mode_color(m)));
                }
            });
            widgets::label(&picker.response, egui::Role::ComboBox, &format!("Automation Mode {}", lane.mode.label()));
            if mode != lane.mode {
                app.act(Action::SetLaneMode { owner, lane: li, mode });
            }
        });
        ui.horizontal(|ui| {
            let value = lane.value_at(playhead).map_or("—".to_string(), |v| lane.target.format(v));
            ui.label(RichText::new(value).size(10.5).monospace().color(theme::TEXT_DIM));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if widgets::icon_plain(ui, Icon::Close, theme::TEXT_DIM, "Remove Automation Lane", 14.0).on_hover_text("Remove this lane").clicked() {
                    app.act(Action::RemoveLane { owner, lane: li });
                }
                if widgets::icon_plain(ui, Icon::Plus, theme::TEXT_DIM, "Add Automation Lane", 14.0).on_hover_text("Add another lane").clicked() {
                    app.act(Action::AddLane { owner, target: None });
                }
            });
        });
    }
}

/// The envelope over the timeline. Click to add a point (snapped), drag points (Shift: fine),
/// double-click or ⌫ to delete, right-click for curves and more.
#[allow(clippy::too_many_arguments)]
fn lane_area(app: &mut DawApp, ui: &mut egui::Ui, host: LaneHost, li: usize, lane: &AutomationLane, snap: &Snapshot, map: &TempoMap, grid: Option<Grid>) {
    let project = &snap.project;
    let owner = host.owner;
    let (resp, painter) = ui.allocate_painter(vec2(ui.available_width(), LANE_H), Sense::click_and_drag());
    let name = lane_name(snap.auto_params.get(&host.key).map_or(&[][..], |p| p.as_slice()), lane.target);
    widgets::label(&resp, egui::Role::Group, &format!("Automation {} {name}", host.name));
    let rect = resp.rect;
    let painter = painter.with_clip_rect(rect);
    painter.rect_filled(rect, 0.0, theme::shade(theme::BG_LANE, 0.85));
    draw_grid(&painter, rect, &app.view, map, grid);
    draw_loop_tint(&painter, rect, &app.view, project);
    painter.hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, theme::BORDER));

    let plot = rect.shrink2(vec2(0.0, 6.0));
    let y_of = |v: f32| plot.bottom() - v.clamp(0.0, 1.0) * plot.height();
    let v_of = |y: f32| ((plot.bottom() - y) / plot.height()).clamp(0.0, 1.0);
    let pts: Vec<Pos2> = lane.points.iter().map(|p| pos2(app.view.x_of(p.time_secs), y_of(p.value))).collect();
    let at = |i: usize| pts[i];
    let color = host.color;
    let ink = if lane.mode == AutomationMode::Off { theme::shade(color, 0.45) } else { color };
    let sel = |i: usize| app.view.auto_sel == Some(AutoSel { owner, lane: li, point: i });

    // The curve (and a soft fill below it), sampled per pixel so every shape draws right.
    let hover = resp.hover_pos();
    if lane.points.is_empty() {
        let v = match lane.target {
            AutomationTarget::Volume => gain_to_pos(host.volume),
            AutomationTarget::Pan => (host.pan + 1.0) / 2.0,
            AutomationTarget::Mute => host.mute as u8 as f32,
            AutomationTarget::Param { .. } => 0.5,
        };
        painter.hline(rect.x_range(), y_of(v), Stroke::new(1.0, theme::with_alpha(ink, 90)));
        painter.text(rect.left_center() + vec2(10.0, 0.0), Align2::LEFT_CENTER, "Click to add automation points", FontId::proportional(11.0), theme::TEXT_FAINT);
    } else {
        let stepped = lane.target.stepped();
        let line: Vec<Pos2> = (rect.left() as i32..=rect.right() as i32)
            .map(|x| pos2(x as f32, y_of(envelope_at(&lane.points, app.view.t_of(x as f32), stepped).unwrap_or(0.0))))
            .collect();
        let mut mesh = egui::Mesh::default();
        let fill = theme::with_alpha(ink, 34);
        for (k, p) in line.iter().enumerate() {
            mesh.colored_vertex(*p, fill);
            mesh.colored_vertex(pos2(p.x, plot.bottom()), fill);
            if k > 0 {
                let i = (k as u32) * 2;
                mesh.add_triangle(i - 2, i - 1, i);
                mesh.add_triangle(i - 1, i, i + 1);
            }
        }
        painter.add(egui::Shape::mesh(mesh));
        painter.add(egui::Shape::line(line, Stroke::new(1.6, ink)));
    }
    let near = |p: Pos2| (0..lane.points.len()).filter(|&i| at(i).distance(p) <= POINT_GRAB).min_by(|&a, &b| at(a).distance(p).total_cmp(&at(b).distance(p)));
    let hovered = hover.and_then(near);
    for i in 0..lane.points.len() {
        let c = at(i);
        if !rect.expand(4.0).contains(c) {
            continue;
        }
        let big = sel(i) || hovered == Some(i);
        painter.circle_filled(c, if big { 4.5 } else { 3.2 }, if sel(i) { Color32::WHITE } else { ink });
        painter.circle_stroke(c, if big { 4.5 } else { 3.2 }, Stroke::new(1.0, theme::shade(ink, 0.35)));
    }
    if hovered.is_some() {
        ui.ctx().set_cursor_icon(CursorIcon::Grab);
    }

    // Value (and time) at the point being dragged or hovered.
    let dragging = app.view.auto_drag.as_ref().filter(|d| d.sel.owner == owner && d.sel.lane == li).map(|d| d.sel.point);
    if let Some(i) = dragging.or(hovered).filter(|&i| i < lane.points.len()) {
        let p = lane.points[i];
        let text = format!("{}  ·  {:.2}s", lane.target.format(p.value), p.time_secs);
        let c = at(i);
        let galley = painter.layout_no_wrap(text, FontId::proportional(10.5), theme::TEXT);
        let left = (c.x + 8.0).min(rect.right() - galley.size().x - 6.0);
        let top = if c.y - rect.top() > galley.size().y + 6.0 { c.y - galley.size().y - 6.0 } else { c.y + 6.0 };
        let r = Rect::from_min_size(pos2(left, top), galley.size()).expand2(vec2(4.0, 2.0));
        painter.rect_filled(r, 3.0, theme::with_alpha(theme::BG_DEEP, 230));
        painter.galley(pos2(left, top), galley, theme::TEXT);
    }
    draw_playhead(&painter, rect, &app.view, snap.status.position_secs, false);

    // Interaction.
    let press = ui.input(|i| i.pointer.press_origin());
    if resp.drag_started() {
        if let Some(i) = press.and_then(near) {
            let p = lane.points[i];
            let sel = AutoSel { owner, lane: li, point: i };
            app.view.auto_sel = Some(sel);
            app.selection.clear();
            app.view.auto_drag = Some(AutoDrag { sel, from: (p.time_secs, p.value), sent: (p.time_secs, p.value) });
        }
    }
    if resp.dragged() {
        let total = ui.input(|i| i.pointer.press_origin().zip(i.pointer.latest_pos()).map(|(a, b)| b - a)).unwrap_or_default();
        let fine = ui.input(|i| i.modifiers.shift);
        let scale = if fine { 0.2 } else { 1.0 };
        if let Some(d) = app.view.auto_drag.as_mut().filter(|d| d.sel.owner == owner && d.sel.lane == li) {
            let t = d.from.0 + (total.x * scale / app.view.px_per_sec) as f64;
            let t = if fine { t.max(0.0) } else { snap_to(app.snap_mode, ui, t, project, app.view.px_per_sec) };
            let v = (d.from.1 - total.y * scale / plot.height()).clamp(0.0, 1.0);
            if (t, v) != d.sent {
                d.sent = (t, v);
                let merge = format!("autodrag:{}:{li}:{}", host.id, d.sel.point);
                let (lane, index) = (d.sel.lane, d.sel.point);
                app.act(Action::MovePoint { owner, lane, index, time: t, value: v, merge });
            }
        }
    }
    if resp.drag_stopped() {
        app.view.auto_drag = None;
    }
    let pointer = resp.interact_pointer_pos();
    if resp.double_clicked() {
        if let Some(i) = pointer.and_then(near) {
            app.act(Action::DeletePoints { owner, lane: li, indices: vec![i] });
        }
    } else if resp.clicked() {
        if owner.is_some() {
            app.selected_track = owner;
        }
        app.selection.clear();
        if let Some(p) = pointer {
            match near(p) {
                Some(i) => app.view.auto_sel = Some(AutoSel { owner, lane: li, point: i }),
                None => {
                    let t = snap_time(app, ui, app.view.t_of(p.x), project);
                    // On (or near) the line, the point goes on the line; elsewhere where clicked.
                    let on_line = envelope_at(&lane.points, t, lane.target.stepped()).filter(|&v| (y_of(v) - p.y).abs() <= 5.0);
                    app.act(Action::AddPoint { owner, lane: li, time: t, value: on_line.unwrap_or(v_of(p.y)) });
                }
            }
        }
    }
    if resp.secondary_clicked() {
        app.view.auto_context = pointer.and_then(near);
    }
    resp.context_menu(|ui| {
        widgets::section(ui, &name);
        if let Some(i) = app.view.auto_context.filter(|&i| i < lane.points.len()) {
            ui.menu_button("Curve", |ui| {
                for c in Curve::ALL {
                    if ui.add(egui::Button::selectable(lane.points[i].curve == c, c.label())).clicked() {
                        app.act(Action::SetPointCurve { owner, lane: li, index: i, curve: c });
                        ui.close();
                    }
                }
            });
            if ui.add(egui::Button::new("Delete Point").shortcut_text("⌫")).clicked() {
                app.act(Action::DeletePoints { owner, lane: li, indices: vec![i] });
                ui.close();
            }
            ui.separator();
        }
        if ui.add_enabled(!lane.points.is_empty(), egui::Button::new("Clear Lane")).clicked() {
            app.act(Action::ClearLane { owner, lane: li });
            ui.close();
        }
        if ui.button("Hide Lane").clicked() {
            app.act(Action::SetLaneShown { owner, lane: li, shown: false });
            ui.close();
        }
        if ui.button("Remove Lane").clicked() {
            app.act(Action::RemoveLane { owner, lane: li });
            ui.close();
        }
    });
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
    fn tempo_labels_stick_at_the_left_and_never_overlap() {
        let span = Rangef::new(100.0, 1000.0);
        // Three markings scrolled off to the left, then two visible ones; labels 50 px wide.
        let marks = [(-300.0, 50.0), (-100.0, 50.0), (40.0, 50.0), (400.0, 50.0), (430.0, 50.0)];
        let placed = place_tempo_labels(span, &marks, Some(3));
        // Only the marking in effect at the edge (index 2) sticks; the earlier ones are gone.
        assert_eq!(placed.first(), Some(&(2, 103.0)));
        assert!(placed.iter().all(|p| p.0 >= 2));
        // 3 and 4 overlap: the one at the playhead (3) wins.
        assert!(placed.contains(&(3, 403.0)) && !placed.iter().any(|p| p.0 == 4));

        // As the next marking reaches the edge it pushes the sticky label out.
        let marks = [(40.0, 50.0), (130.0, 50.0)];
        let placed = place_tempo_labels(span, &marks, Some(1));
        let sticky = placed.iter().find(|p| p.0 == 0).map(|p| p.1);
        assert!(sticky.is_none_or(|x| x + 50.0 <= 130.0), "{placed:?}");
        assert!(placed.contains(&(1, 133.0)));
    }

    #[test]
    fn grids_follow_the_tempo_map() {
        let p = Project { tempo_bpm: 120.0, tempo_changes: vec![TempoChange { at_secs: 4.0, bpm: 60.0, time_signature: Some((6, 8)) }], ..Project::default() };
        let map = p.tempo_map();
        assert_eq!(grid_at(SnapMode::Off, &map, 1.0, 50.0), None);
        assert_eq!(grid_at(SnapMode::Beat, &map, 1.0, 50.0), Some(Grid::Quarters(1.0)));
        assert_eq!(grid_at(SnapMode::Beat, &map, 5.0, 50.0), Some(Grid::Quarters(0.5)), "eighth-note beats in 6/8");
        assert_eq!(grid_step_secs(Grid::Bar, &map, 1.0), 2.0);
        assert_eq!(grid_step_secs(Grid::Bar, &map, 5.0), 3.0, "6/8 at 60 bpm");
        assert_eq!(grid_step_secs(Grid::Quarters(0.25), &map, 5.0), 0.25);
        assert_eq!(map.snap(5.6, Grid::Quarters(0.5)), 5.5);
        // Smart picks coarser grids when zoomed out.
        assert_eq!(grid_at(SnapMode::Smart, &map, 1.0, 2.0), Some(Grid::Bar));
        assert_eq!(grid_at(SnapMode::Smart, &map, 1.0, 400.0), Some(Grid::Quarters(0.25)));
    }
}
