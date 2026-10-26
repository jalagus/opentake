//! Tablature and standard notation in the editor area. Both draw with the arrangement's own
//! time-to-screen mapping, so bars, notes and the playhead line up exactly with the regions
//! above and scroll and zoom along with them (including while playing or recording).
//! Notation is spaced proportionally to time, like the tab, rather than engraved.

use opentake_engine::project::{MidiClip, Note, Project, TabBar, TabLayout};
use eframe::egui;
use egui::{Align, Align2, Color32, FontId, Layout, Painter, Pos2, Rangef, Rect, RichText, Sense, Shape, Stroke, StrokeKind, pos2, vec2};

use super::{Action, DawApp, Snapshot, theme, timeline, widgets};

const NOTE_NAMES: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];
/// Row with bar numbers, time signatures and sections.
const HEADER_H: f32 = 22.0;
/// Where the system's opening line sits in the left column.
const SYSTEM_X: f32 = 30.0;
/// Extra header row for tempo markings.
const TEMPO_H: f32 = 14.0;

/// What the view shows.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum TabMode {
    Tab,
    Notes,
    #[default]
    Both,
}

impl TabMode {
    const ALL: [TabMode; 3] = [TabMode::Tab, TabMode::Notes, TabMode::Both];

    pub fn label(self) -> &'static str {
        match self {
            TabMode::Tab => "Tab",
            TabMode::Notes => "Notes",
            TabMode::Both => "Notes + Tab",
        }
    }
}

/// Tracks with at least one region that has tablature.
pub fn tab_tracks(project: &Project) -> Vec<usize> {
    project.tracks.iter().enumerate().filter(|(_, t)| t.midi_clips.iter().any(|c| c.tab.is_some())).map(|(i, _)| i).collect()
}

pub fn show(app: &mut DawApp, ui: &mut egui::Ui, snap: &Snapshot) {
    let project = &snap.project;
    let tracks = tab_tracks(project);
    if tracks.is_empty() {
        ui.centered_and_justified(|ui| {
            ui.label(RichText::new("No tablature yet — import a Guitar Pro file (File → Import Guitar Pro…) to see its tabs here, in sync with the timeline.").color(theme::TEXT_FAINT));
        });
        return;
    }
    // Show the selected track's part, or the first track that has tab.
    let track = app.selected_track.filter(|t| tracks.contains(t)).unwrap_or(tracks[0]);
    let playhead = snap.status.position_secs;

    // Track picker on the left; view mode and zoom on the right.
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        for &t in &tracks {
            let name = &project.tracks[t].name;
            let on = t == track;
            let text = RichText::new(name).size(11.0).color(if on { Color32::WHITE } else { theme::TEXT_DIM });
            let b = egui::Button::new(text).fill(if on { theme::shade(theme::track_color(t), 0.6) } else { theme::BG_CONTROL });
            if ui.add(b).on_hover_text(format!("Show {name}'s part")).clicked() {
                app.selected_track = Some(t);
            }
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            timeline::zoom_controls(app, ui, "tab_zoom", playhead);
            ui.add_space(10.0);
            // Reading comfort: size and paper (white) background.
            let paper = app.tab_paper;
            let text = RichText::new("Paper").size(11.0).color(if paper { Color32::WHITE } else { theme::TEXT_DIM });
            if ui.add(egui::Button::new(text).fill(if paper { theme::BG_HEADER_SELECTED } else { theme::BG_CONTROL })).on_hover_text("Dark ink on a white page").clicked() {
                app.set_tab_view_prefs(app.tab_scale, !paper);
            }
            if ui.add_enabled(app.tab_scale < 2.5, egui::Button::new(RichText::new("A+").size(12.0))).on_hover_text("Larger notation and tab").clicked() {
                app.set_tab_view_prefs((app.tab_scale + 0.25).min(2.5), app.tab_paper);
            }
            ui.label(RichText::new(format!("{:.0}%", app.tab_scale * 100.0)).size(10.5).color(theme::TEXT_FAINT));
            if ui.add_enabled(app.tab_scale > 0.75, egui::Button::new(RichText::new("A−").size(10.5))).on_hover_text("Smaller notation and tab").clicked() {
                app.set_tab_view_prefs((app.tab_scale - 0.25).max(0.75), app.tab_paper);
            }
            ui.add_space(10.0);
            for mode in TabMode::ALL.into_iter().rev() {
                let on = app.tab_mode == mode;
                let text = RichText::new(mode.label()).size(11.0).color(if on { Color32::WHITE } else { theme::TEXT_DIM });
                let b = egui::Button::new(text).fill(if on { theme::BG_HEADER_SELECTED } else { theme::BG_CONTROL });
                if ui.add(b).on_hover_text(format!("Show {}", mode.label().to_lowercase())).clicked() {
                    app.tab_mode = mode;
                }
            }
        });
    });
    ui.add_space(4.0);

    let pal = if app.tab_paper { Palette::PAPER } else { Palette::DARK };
    let scale = app.tab_scale;
    // Height the parts need at this size; scroll when the panel is smaller.
    let clips_strings = project.tracks[track].midi_clips.iter().filter_map(|c| c.tab.as_ref()).map(|t| t.tuning.len()).max().unwrap_or(6) as f32;
    let has_tempos = project.tracks[track].midi_clips.iter().any(|c| c.tab.is_some() && !c.tempos.is_empty());
    let header = HEADER_H + if has_tempos { TEMPO_H } else { 0.0 };
    let staff_h = 30.0 * 4.0 * scale;
    let tab_h = clips_strings * 15.0 * scale + 30.0;
    let needed = header
        + match app.tab_mode {
            TabMode::Notes => staff_h,
            TabMode::Tab => tab_h,
            TabMode::Both => staff_h + tab_h,
        };
    let avail = ui.available_height();
    if needed > avail {
        egui::ScrollArea::vertical().id_salt("tab_scroll").auto_shrink([false, false]).show(ui, |ui| draw_body(app, ui, snap, track, playhead, pal, scale, needed));
    } else {
        draw_body(app, ui, snap, track, playhead, pal, scale, avail);
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_body(app: &mut DawApp, ui: &mut egui::Ui, snap: &Snapshot, track: usize, playhead: f64, pal: Palette, scale: f32, height: f32) {
    let project = &snap.project;
    let width = ui.available_width();
    let (full, resp) = ui.allocate_exact_size(vec2(width, height), Sense::click_and_drag());
    widgets::label(&resp, egui::Role::Group, "Tab staff");
    let lanes = app.view.lanes();
    let content = Rect::from_x_y_ranges(Rangef::new(lanes.min.max(full.left()), full.right()), full.y_range());
    let gutter = Rect::from_min_max(full.min, pos2(content.left(), full.bottom()));
    let painter = ui.painter().clone();
    painter.rect_filled(full, 6.0, pal.bg);

    let clips: Vec<(&MidiClip, &TabLayout)> = project.tracks[track].midi_clips.iter().filter_map(|c| c.tab.as_ref().map(|t| (c, t))).collect();
    painter.text(gutter.left_top() + vec2(10.0, 6.0), Align2::LEFT_TOP, &project.tracks[track].name, FontId::proportional(12.0), pal.ink);
    if let Some((_, layout)) = clips.first().filter(|(_, l)| l.capo > 0) {
        painter.text(gutter.left_top() + vec2(10.0, 21.0), Align2::LEFT_TOP, format!("Capo {}", layout.capo), FontId::proportional(10.0), pal.faint);
    }

    // Split the area between staff and tab.
    let has_tempos = clips.iter().any(|(c, _)| !c.tempos.is_empty());
    let header_h = HEADER_H + if has_tempos { TEMPO_H } else { 0.0 };
    let body = Rect::from_min_max(pos2(full.left(), full.top() + header_h), full.max);
    let (staff_rect, tab_rect) = match app.tab_mode {
        TabMode::Notes => (Some(body), None),
        TabMode::Tab => (None, Some(body)),
        TabMode::Both => {
            let strings = clips.iter().map(|(_, t)| t.tuning.len()).max().unwrap_or(6) as f32;
            let tab_h = (strings * 16.0 * scale + 24.0).min(body.height() * 0.55);
            let split = body.bottom() - tab_h;
            (Some(Rect::from_min_max(body.min, pos2(body.right(), split))), Some(Rect::from_min_max(pos2(body.left(), split), body.max)))
        }
    };
    let ctx = Ctx { app, content, playhead, color: theme::track_color(track), pal, scale };
    draw_bar_header(&ctx, &painter, &clips, Rect::from_min_max(pos2(content.left(), full.top()), pos2(content.right(), body.top())));
    if let Some(r) = staff_rect {
        draw_staff(&ctx, &painter, &clips, r, gutter.x_range());
    }
    if let Some(r) = tab_rect {
        draw_tab(&ctx, &painter, &clips, r, gutter.x_range());
    }

    // Playhead, exactly where the arrangement draws it.
    let x = app.view.x(playhead);
    if content.x_range().contains(x) {
        painter.with_clip_rect(content).vline(x, Rangef::new(full.top() + 2.0, full.bottom() - 2.0), Stroke::new(1.0, pal.playhead));
    }

    // Click to move the playhead; scroll/zoom moves the shared timeline view.
    if resp.clicked() {
        if let Some(t) = resp.interact_pointer_pos().filter(|p| content.contains(*p)).and_then(|p| app.view.time_at(p.x)) {
            app.act(Action::Seek(timeline::snap_time(app, ui, t, project)));
        }
    }
    app.view.handle_zoom_scroll(ui, content);
}

struct Ctx<'a> {
    app: &'a DawApp,
    content: Rect,
    playhead: f64,
    color: Color32,
    pal: Palette,
    /// Size of the notation and tab (1.0 = normal).
    scale: f32,
}

/// Colours for the notation and tab: the app's dark look, or ink on paper.
#[derive(Clone, Copy)]
struct Palette {
    bg: Color32,
    ink: Color32,
    dim: Color32,
    faint: Color32,
    line: Color32,
    section: Color32,
    playhead: Color32,
}

impl Palette {
    const DARK: Palette = Palette {
        bg: theme::BG_DEEP,
        ink: theme::TEXT,
        dim: theme::TEXT_DIM,
        faint: theme::TEXT_FAINT,
        line: theme::HAIRLINE,
        section: theme::SOLO,
        playhead: theme::PLAYHEAD,
    };
    const PAPER: Palette = Palette {
        bg: Color32::from_rgb(251, 250, 247),
        ink: Color32::from_rgb(22, 24, 28),
        dim: Color32::from_rgb(70, 75, 84),
        faint: Color32::from_rgb(128, 134, 144),
        line: Color32::from_rgb(196, 200, 207),
        section: Color32::from_rgb(170, 110, 0),
        playhead: Color32::from_rgb(220, 60, 45),
    };
}

impl Ctx<'_> {
    fn x(&self, t: f64) -> f32 {
        self.app.view.x(t)
    }

    fn visible(&self, x0: f32, x1: f32) -> bool {
        x1 >= self.content.left() - 20.0 && x0 <= self.content.right() + 20.0
    }

    fn sounding(&self, t: f64, end: f64) -> bool {
        self.playhead >= t && self.playhead < end
    }
}

/// Bars inside a region, with their absolute start times.
fn bar_times<'a>(clip: &MidiClip, layout: &'a TabLayout) -> impl Iterator<Item = (f64, &'a TabBar)> {
    let (start, end) = (clip.position_secs, clip.position_secs + clip.length_secs);
    layout.bars.iter().map(move |b| (start + b.start_secs, b)).take_while(move |(t, _)| *t <= end)
}

fn draw_bar_header(ctx: &Ctx, painter: &Painter, clips: &[(&MidiClip, &TabLayout)], rect: Rect) {
    let p = painter.with_clip_rect(rect);
    for (clip, layout) in clips {
        let mut prev_sig = None;
        for (t, bar) in bar_times(clip, layout) {
            let sig = (bar.numerator, bar.denominator);
            let sig_changed = prev_sig != Some(sig);
            prev_sig = Some(sig);
            let x = ctx.x(t);
            if x < rect.left() - 300.0 || x > rect.right() {
                continue;
            }
            let font = FontId::proportional(10.0);
            let r = p.text(pos2(x + 3.0, rect.top() + 5.0), Align2::LEFT_TOP, bar.number.to_string(), font.clone(), ctx.pal.faint);
            let mut label_x = r.right() + 5.0;
            if sig_changed {
                let r = p.text(pos2(label_x, rect.top() + 5.0), Align2::LEFT_TOP, format!("{}/{}", sig.0, sig.1), font, ctx.pal.dim);
                label_x = r.right() + 6.0;
            }
            if let Some(section) = &bar.section {
                p.text(pos2(label_x, rect.top() + 4.5), Align2::LEFT_TOP, section, FontId::proportional(10.5), ctx.pal.section);
            }
        }
        // Tempo markings on a second line, the one at the playhead highlighted.
        let marks: Vec<(f32, f64)> = clip.tempos.iter().filter(|m| m.start_secs < clip.length_secs).map(|m| (ctx.x(clip.position_secs + m.start_secs), m.bpm)).collect();
        let current = clip.tempos.iter().rposition(|m| clip.position_secs + m.start_secs <= ctx.playhead + 1e-9);
        // Within this region only, so the sticky label doesn't leak past its edges.
        let span = Rangef::new(ctx.x(clip.position_secs).max(rect.left()), ctx.x(clip.position_secs + clip.length_secs).min(rect.right()));
        if span.span() > 0.0 {
            let row = p.with_clip_rect(Rect::from_x_y_ranges(span, rect.y_range()));
            timeline::draw_tempo_labels(&row, span, rect.top() + HEADER_H + 4.0, &marks, current, ctx.pal.bg);
        }
    }
}

fn draw_tab(ctx: &Ctx, painter: &Painter, clips: &[(&MidiClip, &TabLayout)], rect: Rect, gutter: Rangef) {
    let strings = clips.iter().map(|(_, t)| t.tuning.len()).max().unwrap_or(6).max(1);
    let spacing = ((rect.height() - 24.0) / strings as f32).clamp(9.0 * ctx.scale, 18.0 * ctx.scale);
    let top = rect.top() + 12.0 + spacing / 2.0;
    // Highest string on top, as tab is written.
    let y_of = |string: usize| top + (strings - 1 - string.min(strings - 1)) as f32 * spacing;
    let (y_top, y_bottom) = (y_of(strings - 1), y_of(0));

    // The left column starts the system like printed tab: string names, a bracket line,
    // "TAB" stacked, and the strings running on into the timeline.
    let system_x = gutter.min + SYSTEM_X;
    if let Some((_, layout)) = clips.first() {
        for (s, &open) in layout.tuning.iter().enumerate() {
            painter.text(pos2(system_x - 6.0, y_of(s)), Align2::RIGHT_CENTER, NOTE_NAMES[(open % 12) as usize], FontId::monospace((spacing * 0.7).max(11.0)), ctx.pal.dim);
        }
    }
    painter.vline(system_x, Rangef::new(y_top, y_bottom), Stroke::new(1.5, ctx.pal.faint));
    for s in 0..strings {
        painter.hline(Rangef::new(system_x, gutter.max), y_of(s), Stroke::new(1.0, ctx.pal.line));
    }
    let tab_letters = ["T", "A", "B"];
    let letter_h = (y_bottom - y_top) / 3.0;
    for (i, l) in tab_letters.iter().enumerate() {
        let y = y_top + letter_h * (i as f32 + 0.5);
        painter.text(pos2(system_x + 16.0, y), Align2::CENTER_CENTER, *l, FontId::proportional((letter_h * 0.9).clamp(9.0, 15.0)), ctx.pal.dim);
    }
    let content = Rect::from_x_y_ranges(ctx.content.x_range(), rect.y_range());
    let p = painter.with_clip_rect(content);
    for s in 0..strings {
        p.hline(content.x_range(), y_of(s), Stroke::new(1.0, ctx.pal.line));
    }

    // Fret numbers fill most of the gap between strings (as printed tab does), so they're
    // readable at the normal size and grow with the spacing rather than separately from it.
    let mono = FontId::monospace((spacing * 1.05).max(11.5 * ctx.scale));
    for (clip, layout) in clips {
        let (start, end) = (clip.position_secs, clip.position_secs + clip.length_secs);
        let (x0, x1) = (ctx.x(start), ctx.x(end));
        if !ctx.visible(x0, x1) {
            continue;
        }
        for x in [x0, x1] {
            p.vline(x, Rangef::new(y_top - 6.0, y_bottom + 6.0), Stroke::new(1.0, theme::with_alpha(ctx.color, 90)));
        }
        for (t, _) in bar_times(clip, layout) {
            p.vline(ctx.x(t), Rangef::new(y_top, y_bottom), Stroke::new(1.0, ctx.pal.faint));
        }

        // Fret numbers on their strings, with a faint line for how long they ring.
        for n in clip.notes.iter().filter(|n| n.start_secs < clip.content_secs()) {
            let t = start + n.start_secs;
            let note_end = (t + n.length_secs).min(end);
            let (nx, nx_end) = (ctx.x(t), ctx.x(note_end));
            if !ctx.visible(nx, nx_end) {
                continue;
            }
            let Some(pos) = layout.position(n) else { continue };
            let y = y_of(pos.string as usize);
            let sounding = ctx.sounding(t, note_end);
            let (bg, fg) = if sounding { (ctx.color, Color32::from_rgb(16, 16, 18)) } else { (ctx.pal.bg, ctx.pal.ink) };
            // Shrink a number only when a neighbouring note on its string is too close for it;
            // both sides count, so a fast run shrinks evenly.
            let gap = clip
                .notes
                .iter()
                .filter(|m| (m.start_secs - n.start_secs).abs() > 1e-9 && layout.position(m).is_some_and(|q| q.string == pos.string))
                .map(|m| (ctx.x(start + m.start_secs) - nx).abs())
                .fold(f32::INFINITY, f32::min);
            let room = gap - 3.0;
            let digits = pos.fret.to_string().len() as f32;
            let font = if room.is_finite() { FontId::monospace(mono.size.min(room / (0.62 * digits)).max(8.0)) } else { mono.clone() };
            let galley = p.layout_no_wrap(pos.fret.to_string(), font, fg);
            // The box is a string gap tall (not the font's line height), so the numbers of a
            // chord on neighbouring strings don't cover each other.
            let box_rect = Rect::from_center_size(pos2(nx + galley.size().x / 2.0 + 1.0, y), vec2(galley.size().x + 4.0, spacing - 1.0));
            if nx_end > box_rect.right() + 2.0 {
                p.hline(Rangef::new(box_rect.right(), nx_end), y, Stroke::new(2.0, theme::with_alpha(ctx.color, 70)));
            }
            p.rect_filled(box_rect, 3.0, bg);
            if sounding {
                p.rect_stroke(box_rect.expand(1.0), 3.0, Stroke::new(1.0, Color32::WHITE), StrokeKind::Outside);
            }
            p.galley(box_rect.center() - galley.size() / 2.0, galley, fg);
        }
    }
}

/// An engraved note head `h` (half a staff space) tall around `c`. Filled heads are ovals
/// tilted up to the right; a half note's hole is narrower and tilted more steeply, so its
/// outline is thick on two opposite sides and thin between; a whole note is upright with a
/// steeply tilted hole. Open heads are a ring (not a filled shape with a background-coloured
/// hole), so staff lines still show through.
fn note_head(p: &Painter, c: Pos2, h: f32, value: Value, ink: Color32) {
    // (half-axes and tilt in radians; negative tilts rise to the right on screen)
    let ellipse = |a: f32, b: f32, tilt: f32| move |phi: f32| {
        // Distance from the centre to the ellipse along direction `phi`.
        let (s, c) = (phi - tilt).sin_cos();
        1.0 / ((c / a).powi(2) + (s / b).powi(2)).sqrt()
    };
    const N: usize = 48;
    let at = |phi: f32, r: f32| c + vec2(phi.cos(), phi.sin()) * r;
    let (outer, inner) = match value {
        Value::Whole => (ellipse(h * 1.42, h * 0.9, 0.0), Some(ellipse(h * 0.82, h * 0.46, -1.0))),
        Value::Half => (ellipse(h * 1.33, h * 0.82, -0.38), Some(ellipse(h * 1.02, h * 0.4, -0.62))),
        _ => (ellipse(h * 1.33, h * 0.82, -0.38), None),
    };
    let mut mesh = egui::Mesh::default();
    match inner {
        None => {
            mesh.colored_vertex(c, ink);
            for k in 0..N {
                let phi = k as f32 / N as f32 * std::f32::consts::TAU;
                mesh.colored_vertex(at(phi, outer(phi)), ink);
                mesh.add_triangle(0, 1 + k as u32, 1 + ((k + 1) % N) as u32);
            }
        }
        Some(inner) => {
            for k in 0..N {
                let phi = k as f32 / N as f32 * std::f32::consts::TAU;
                mesh.colored_vertex(at(phi, outer(phi)), ink);
                mesh.colored_vertex(at(phi, inner(phi)), ink);
                let (o, i, o2, i2) = (2 * k as u32, 2 * k as u32 + 1, 2 * ((k + 1) % N) as u32, 2 * ((k + 1) % N) as u32 + 1);
                mesh.add_triangle(o, i, o2);
                mesh.add_triangle(i, i2, o2);
            }
        }
    }
    p.add(Shape::mesh(mesh));
}

/// Diatonic step (C-1 = 0) and whether the note needs a sharp.
fn diatonic(midi: u8) -> (i32, bool) {
    const STEP: [i32; 12] = [0, 0, 1, 1, 2, 3, 3, 4, 4, 5, 5, 6];
    let pc = (midi % 12) as usize;
    ((midi as i32 / 12 - 1) * 7 + STEP[pc], matches!(pc, 1 | 3 | 6 | 8 | 10))
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Value {
    Whole,
    Half,
    Quarter,
    Eighth,
    Sixteenth,
    ThirtySecond,
}

impl Value {
    /// Flags (or beams) on the stem.
    fn beams(self) -> usize {
        match self {
            Value::Eighth => 1,
            Value::Sixteenth => 2,
            Value::ThirtySecond => 3,
            _ => 0,
        }
    }
}

/// How a length (in quarter notes) is written: the nearest plain, dotted or triplet value.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Written {
    value: Value,
    dotted: bool,
    triplet: bool,
}

fn written(quarters: f64) -> Written {
    use Value::*;
    const T: f64 = 1.0 / 3.0;
    // (length in quarters, value, dotted, triplet)
    let table = [
        (4.0, Whole, false, false),
        (3.0, Half, true, false),
        (2.0, Half, false, false),
        (1.5, Quarter, true, false),
        (1.0, Quarter, false, false),
        (2.0 * T, Quarter, false, true),
        (0.75, Eighth, true, false),
        (0.5, Eighth, false, false),
        (T, Eighth, false, true),
        (0.375, Sixteenth, true, false),
        (0.25, Sixteenth, false, false),
        (T / 2.0, Sixteenth, false, true),
        (0.125, ThirtySecond, false, false),
    ];
    let q = quarters.clamp(0.1, 4.0);
    let (_, value, dotted, triplet) =
        table.iter().copied().min_by(|a, b| (a.0 / q).ln().abs().total_cmp(&(b.0 / q).ln().abs())).unwrap_or((1.0, Quarter, false, false));
    Written { value, dotted, triplet }
}

/// The bar a region-relative time falls in: (index, start, seconds per quarter, beat length in quarters).
fn bar_info(layout: &TabLayout, clip_len: f64, t: f64) -> (usize, f64, f64, f64) {
    let i = layout.bars.partition_point(|b| b.start_secs <= t + 1e-6).saturating_sub(1);
    let Some(bar) = layout.bars.get(i) else { return (0, 0.0, 0.5, 1.0) };
    let end = layout.bars.get(i + 1).map_or(clip_len, |b| b.start_secs);
    let quarters = bar.numerator as f64 * 4.0 / bar.denominator.max(1) as f64;
    let quarter = ((end - bar.start_secs) / quarters.max(0.25)).max(0.01);
    // Beams group by beat: dotted quarters in compound meters (6/8, 9/8, 12/8), else quarters.
    let beat = if bar.denominator == 8 && bar.numerator % 3 == 0 && bar.numerator > 3 { 1.5 } else { 1.0 };
    (i, bar.start_secs, quarter, beat)
}

/// A chord laid out on the staff.
struct Chord {
    x: f32,
    /// Region-relative onset and written end.
    onset: f64,
    end: f64,
    /// (diatonic step, sharp), sorted low to high.
    heads: Vec<(i32, bool)>,
    written: Written,
    /// (bar index, beat index) for beaming.
    beat: (usize, i64),
    quarter: f64,
    ink: Color32,
}

/// Staff geometry shared by the drawing helpers.
struct Staff<'a> {
    p: &'a Painter,
    pal: Palette,
    h: f32,
    bottom_step: i32,
    bottom_y: f32,
}

impl Staff<'_> {
    fn y(&self, step: i32) -> f32 {
        self.bottom_y - (step - self.bottom_step) as f32 * self.h
    }

    fn rx(&self) -> f32 {
        self.h * 1.3
    }

    /// Note heads, ledger lines, accidentals and dots. Returns the stem x.
    fn heads(&self, c: &Chord, stem_up: bool) -> f32 {
        let (h, rx) = (self.h, self.rx());
        let cx = c.x + rx + 1.0;
        let mut prev: Option<(i32, bool)> = None; // (step, shifted)
        for &(step, sharp) in &c.heads {
            let y = self.y(step);
            // Seconds in a chord: the second head goes on the other side of the stem.
            let shifted = matches!(prev, Some((s, false)) if step - s == 1);
            prev = Some((step, shifted));
            let hx = match (shifted, stem_up) {
                (false, _) => cx,
                (true, true) => cx + 2.0 * rx,
                (true, false) => cx - 2.0 * rx,
            };
            let ledger = |s: i32| self.p.hline(Rangef::new(hx - rx * 1.7, hx + rx * 1.7), self.y(s), Stroke::new(1.0, self.pal.faint));
            for s in (step..self.bottom_step).filter(|s| (self.bottom_step - s) % 2 == 0) {
                ledger(s);
            }
            for s in (self.bottom_step + 10..=step).filter(|s| (s - self.bottom_step) % 2 == 0) {
                ledger(s);
            }
            note_head(self.p, pos2(hx, y), h, c.written.value, c.ink);
            if sharp {
                draw_sharp(self.p, pos2(cx - rx - h * 1.6, y), h, c.ink);
            }
            if c.written.dotted {
                // Dots sit in a space: a note on a line gets its dot just above.
                let on_line = (step - self.bottom_step) % 2 == 0;
                let dy = if on_line { -h } else { 0.0 };
                self.p.circle_filled(pos2(hx + rx + h * 0.9, y + dy), h * 0.32, c.ink);
            }
        }
        if stem_up { cx + rx - 0.5 } else { cx - rx + 0.5 }
    }

    fn stem_up(&self, chords: &[&Chord]) -> bool {
        let (sum, n) = chords.iter().flat_map(|c| &c.heads).fold((0i32, 0i32), |(s, n), h| (s + h.0, n + 1));
        (sum as f32 / n.max(1) as f32) < (self.bottom_step + 4) as f32
    }

    /// A chord on its own: stem and flags.
    fn single(&self, c: &Chord) {
        let up = self.stem_up(&[c]);
        let sx = self.heads(c, up);
        if c.written.value == Value::Whole {
            return;
        }
        let h = self.h;
        let (lo, hi) = (self.y(c.heads[0].0), self.y(c.heads[c.heads.len() - 1].0));
        let (y0, y1) = if up { (lo, hi - 7.0 * h) } else { (hi, lo + 7.0 * h) };
        self.p.vline(sx, Rangef::new(y0.min(y1), y0.max(y1)), Stroke::new(1.0, c.ink));
        // Flags curl back toward the heads.
        let dir = if up { 1.0 } else { -1.0 };
        for f in 0..c.written.value.beams() {
            let from = pos2(sx, y1 + dir * f as f32 * 1.6 * h);
            self.p.line_segment([from, from + vec2(h * 1.8, dir * h * 2.6)], Stroke::new(1.6, c.ink));
        }
    }

    /// Beamed group: shared stem direction, a sloped primary beam, and secondary beams (or
    /// stubs) for the shorter values.
    fn beamed(&self, group: &[&Chord]) {
        let h = self.h;
        let up = self.stem_up(group);
        let dir = if up { -1.0 } else { 1.0 }; // from the heads toward the beam (screen y)
        let stems: Vec<f32> = group.iter().map(|c| self.heads(c, up)).collect();
        // Per chord: the head nearest the beam, and the farthest one (where the stem starts).
        let near = |c: &Chord| if up { self.y(c.heads[c.heads.len() - 1].0) } else { self.y(c.heads[0].0) };
        let far = |c: &Chord| if up { self.y(c.heads[0].0) } else { self.y(c.heads[c.heads.len() - 1].0) };
        let natural = |c: &Chord| near(c) + dir * 7.0 * h;
        let (first, last) = (group[0], group[group.len() - 1]);
        let (x0, x1) = (stems[0], stems[stems.len() - 1]);
        // Gentle slope following the contour, capped at one staff space.
        let rise = (natural(last) - natural(first)).clamp(-2.0 * h, 2.0 * h);
        let slope = if x1 > x0 { rise / (x1 - x0) } else { 0.0 };
        let mut base = natural(first);
        // Move the beam away from the heads until every stem is at least 5½ half-spaces long.
        for (c, &sx) in group.iter().zip(&stems) {
            let y = base + slope * (sx - x0);
            let min_end = near(c) + dir * 5.5 * h;
            if (y - min_end) * dir < 0.0 {
                base += min_end - y;
            }
        }
        let line = |x: f32| base + slope * (x - x0);
        for (c, &sx) in group.iter().zip(&stems) {
            let (ya, yb) = (far(c), line(sx));
            self.p.vline(sx, Rangef::new(ya.min(yb), ya.max(yb)), Stroke::new(1.0, c.ink));
        }
        let thick = h * 0.95;
        let beam = |xa: f32, xb: f32, level: usize| {
            // Primary beam at the stem ends; further beams step back toward the heads.
            let shift = -dir * level as f32 * h * 1.55;
            let (ya, yb) = (line(xa) + shift, line(xb) + shift);
            let t = -dir * thick;
            self.p.add(Shape::convex_polygon(vec![pos2(xa, ya), pos2(xb, yb), pos2(xb, yb + t), pos2(xa, ya + t)], self.pal.ink, Stroke::NONE));
        };
        beam(x0 - 0.5, x1 + 0.5, 0);
        // Secondary beams join neighbours that both need them; a lone short note gets a stub
        // pointing at its neighbour.
        let stub = self.rx() * 1.6;
        for level in 1..3 {
            let mut i = 0;
            while i < group.len() {
                if group[i].written.value.beams() <= level {
                    i += 1;
                    continue;
                }
                let mut j = i;
                while j + 1 < group.len() && group[j + 1].written.value.beams() > level {
                    j += 1;
                }
                if j > i {
                    beam(stems[i] - 0.5, stems[j] + 0.5, level);
                } else if i + 1 < group.len() {
                    beam(stems[i], stems[i] + stub, level);
                } else {
                    beam(stems[i] - stub, stems[i], level);
                }
                i = j + 1;
            }
        }
        // Triplet number beyond the beam.
        if group.iter().all(|c| c.written.triplet) && (group.len() == 3 || group.len() == 6) {
            let mx = (x0 + x1) / 2.0;
            self.p.text(pos2(mx, line(mx) + dir * h * 2.2), Align2::CENTER_CENTER, group.len().to_string(), FontId::proportional(h * 2.2), self.pal.dim);
        }
    }
}

/// Lay out a region's chords (notes starting together share a stem) around the visible area.
fn chords_of(ctx: &Ctx, clip: &MidiClip, layout: &TabLayout, content: Rect) -> Vec<Chord> {
    let notes: Vec<&Note> = clip.notes.iter().filter(|n| n.start_secs < clip.content_secs()).collect();
    let start = clip.position_secs;
    let mut out = Vec::new();
    let mut i = 0;
    while i < notes.len() {
        let onset = notes[i].start_secs;
        let mut j = i;
        while j < notes.len() && notes[j].start_secs - onset < 0.004 {
            j += 1;
        }
        let chord = &notes[i..j];
        let next_onset = notes.get(j).map(|n| n.start_secs);
        i = j;
        let x = ctx.x(start + onset);
        // Some margin so beams reaching in from off-screen still draw.
        if x < content.left() - 400.0 || x > content.right() + 400.0 {
            continue;
        }
        // Written length: up to the next onset, so let-ring notes don't all read as whole notes.
        let len = chord.iter().map(|n| n.length_secs).fold(0.0, f64::max);
        let len = next_onset.map_or(len, |next| len.min(next - onset));
        let (bar, bar_start, quarter, beat_q) = bar_info(layout, clip.length_secs, onset);
        let beat = ((onset - bar_start) / (quarter * beat_q) + 1e-3).floor() as i64;
        let mut heads: Vec<(i32, bool)> = chord.iter().map(|n| diatonic(n.pitch.saturating_add(12))).collect();
        heads.sort();
        heads.dedup();
        let sounding = chord.iter().any(|n| ctx.sounding(start + n.start_secs, start + n.start_secs + n.length_secs));
        out.push(Chord {
            x,
            onset,
            end: onset + len,
            heads,
            written: written(len / quarter),
            beat: (bar, beat),
            quarter,
            ink: if sounding { ctx.color } else { ctx.pal.ink },
        });
    }
    out
}

/// Split chords into beam groups: consecutive eighths (and shorter) within one beat with no
/// rest between them. Everything else comes back as a group of one.
fn beam_groups(chords: &[Chord]) -> Vec<Vec<usize>> {
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for (i, c) in chords.iter().enumerate() {
        let joins = c.written.value.beams() > 0
            && groups.last().and_then(|g| g.last()).is_some_and(|&k| {
                let prev = &chords[k];
                prev.written.value.beams() > 0 && prev.beat == c.beat && (c.onset - prev.end).abs() < 0.1 * c.quarter
            });
        match groups.last_mut() {
            Some(g) if joins => g.push(i),
            _ => groups.push(vec![i]),
        }
    }
    groups
}

fn draw_staff(ctx: &Ctx, painter: &Painter, clips: &[(&MidiClip, &TabLayout)], rect: Rect, gutter: Rangef) {
    // Guitar is written an octave above where it sounds (treble 8vb); bass likewise in bass clef.
    let bass = clips.first().and_then(|(_, l)| l.tuning.first()).is_some_and(|&low| low < 36);
    let bottom_step = if bass { diatonic(43).0 } else { diatonic(64).0 }; // G2 / E4 on the bottom line
    let h = (rect.height() / 26.0).clamp(3.0 * ctx.scale, 5.0 * ctx.scale); // half a staff space
    let middle = rect.center().y;
    let bottom_y = middle + 4.0 * h;
    let y_of = |step: i32| bottom_y - (step - bottom_step) as f32 * h;
    let (y_top, y_bottom) = (y_of(bottom_step + 8), bottom_y);

    // The left column starts the system like a printed score: a bracket line, the clef,
    // and the staff running on into the timeline.
    let system_x = gutter.min + SYSTEM_X;
    let clef_x = system_x + 18.0;
    painter.vline(system_x, Rangef::new(y_top, y_bottom), Stroke::new(1.5, ctx.pal.faint));
    for k in 0..5 {
        painter.hline(Rangef::new(system_x, gutter.max), y_of(bottom_step + 2 * k), Stroke::new(1.0, ctx.pal.faint));
    }
    // Clef extents relative to their reference line, in staff spaces (SMuFL metrics):
    // the G clef curls around the second line, the F clef's dots straddle the fourth.
    let space = 2.0 * h;
    let (glyph, line, above, below) = if bass { ("\u{1D122}", y_of(bottom_step + 6), 1.05, 2.54) } else { ("\u{1D11E}", y_of(bottom_step + 2), 4.39, 2.63) };
    let (clef_top, clef_bottom) = (line - above * space, line + below * space);
    if !(theme::has_music_font() && draw_glyph_fitted(painter, glyph, clef_x, clef_top, clef_bottom, ctx.pal.dim)) {
        painter.text(pos2(clef_x, middle), Align2::CENTER_CENTER, if bass { "F" } else { "G" }, FontId::proportional(h * 4.0), ctx.pal.dim);
    }
    // "8" under the clef: written an octave above where it sounds.
    let eight_y = clef_bottom.max(y_bottom) + h * 1.8;
    painter.text(pos2(clef_x, eight_y), Align2::CENTER_CENTER, "8", FontId::proportional(h * 2.2), ctx.pal.faint);

    let content = Rect::from_x_y_ranges(ctx.content.x_range(), rect.y_range());
    let p = painter.with_clip_rect(content);
    for k in 0..5 {
        p.hline(content.x_range(), y_of(bottom_step + 2 * k), Stroke::new(1.0, ctx.pal.faint));
    }

    for (clip, layout) in clips {
        let (start, end) = (clip.position_secs, clip.position_secs + clip.length_secs);
        if !ctx.visible(ctx.x(start), ctx.x(end)) {
            continue;
        }
        for (t, _) in bar_times(clip, layout) {
            p.vline(ctx.x(t), Rangef::new(y_top, y_bottom), Stroke::new(1.0, ctx.pal.faint));
        }

        let staff = Staff { p: &p, pal: ctx.pal, h, bottom_step, bottom_y };
        let chords = chords_of(ctx, clip, layout, content);
        for group in beam_groups(&chords) {
            let members: Vec<&Chord> = group.iter().map(|&i| &chords[i]).collect();
            if members.len() > 1 {
                staff.beamed(&members);
            } else {
                staff.single(members[0]);
            }
        }
    }
}

/// Ink bounds of a galley's first visible glyph, relative to the galley's top-left.
fn ink_rect(galley: &egui::Galley) -> Option<Rect> {
    let row = galley.rows.first()?;
    let glyph = row.glyphs.iter().find(|g| !g.uv_rect.is_nothing())?;
    Some(Rect::from_min_size(row.pos + glyph.pos.to_vec2() + glyph.uv_rect.offset, glyph.uv_rect.size))
}

/// Draw a single glyph scaled so its visible shape spans `top..bottom`, centred on `cx`.
/// Positions by the glyph's actual ink, not the font's line box, so it sits exactly on the
/// staff whatever the font's metrics. Returns false if the font can't draw it.
fn draw_glyph_fitted(p: &Painter, glyph: &str, cx: f32, top: f32, bottom: f32, color: Color32) -> bool {
    const PROBE: f32 = 64.0;
    let probe = p.layout_no_wrap(glyph.to_owned(), FontId::proportional(PROBE), color);
    let Some(ink) = ink_rect(&probe).filter(|r| r.height() > 1.0) else { return false };
    let size = PROBE * (bottom - top) / ink.height();
    let galley = p.layout_no_wrap(glyph.to_owned(), FontId::proportional(size), color);
    let Some(ink) = ink_rect(&galley) else { return false };
    p.galley(pos2(cx - ink.center().x, top - ink.top()), galley, color);
    true
}

/// A sharp sign centred on `at`, sized for half-space `h`.
fn draw_sharp(p: &Painter, at: egui::Pos2, h: f32, ink: Color32) {
    for dx in [-0.45 * h, 0.45 * h] {
        p.vline(at.x + dx, Rangef::new(at.y - 1.6 * h, at.y + 1.6 * h), Stroke::new(1.0, ink));
    }
    for dy in [-0.55 * h, 0.55 * h] {
        p.line_segment([pos2(at.x - 0.9 * h, at.y + dy + 0.25 * h), pos2(at.x + 0.9 * h, at.y + dy - 0.25 * h)], Stroke::new(1.6, ink));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diatonic_steps_and_sharps() {
        assert_eq!(diatonic(60), (28, false)); // C4
        assert_eq!(diatonic(64), (30, false)); // E4, treble bottom line
        assert_eq!(diatonic(61), (28, true)); // C#4
        assert_eq!(diatonic(43), (18, false)); // G2, bass bottom line
    }

    #[test]
    fn note_values_from_quarter_lengths() {
        assert_eq!(written(4.0).value, Value::Whole);
        assert_eq!(written(2.0).value, Value::Half);
        assert_eq!(written(1.0).value, Value::Quarter);
        assert_eq!(written(0.5).value, Value::Eighth);
        assert_eq!(written(0.25).value, Value::Sixteenth);
        assert_eq!(written(1.0 / 3.0), Written { value: Value::Eighth, dotted: false, triplet: true });
        assert_eq!(written(0.75), Written { value: Value::Eighth, dotted: true, triplet: false });
        assert_eq!(written(0.48).value, Value::Eighth, "slightly short eighths stay eighths");
    }

    fn chord(onset: f64, len: f64, beat: (usize, i64)) -> Chord {
        Chord { x: 0.0, onset, end: onset + len, heads: vec![(30, false)], written: written(len / 0.5), beat, quarter: 0.5, ink: theme::TEXT }
    }

    #[test]
    fn eighths_beam_within_a_beat_and_break_at_rests_and_longer_notes() {
        // Beat 0: two eighths; beat 1: an eighth then a rest; beat 2: a quarter; beat 3: four sixteenths.
        let chords = vec![
            chord(0.0, 0.25, (0, 0)),
            chord(0.25, 0.25, (0, 0)),
            chord(0.5, 0.25, (0, 1)),
            chord(1.0, 0.5, (0, 2)),
            chord(1.5, 0.125, (0, 3)),
            chord(1.625, 0.125, (0, 3)),
            chord(1.75, 0.125, (0, 3)),
            chord(1.875, 0.125, (0, 3)),
        ];
        assert_eq!(beam_groups(&chords), vec![vec![0, 1], vec![2], vec![3], vec![4, 5, 6, 7]]);
    }
}
