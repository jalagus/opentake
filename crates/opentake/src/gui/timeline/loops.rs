//! Region looping in the arrangement (Logic's loop handle). The upper part of a region's right
//! edge, at the end of its name bar, is the loop handle: dragging it loops the region up to
//! there (snapped). Repeats are drawn as fainter copies of the content with notches at the
//! loop boundaries. Loop toggles from the context menu, the inspector and L (filling up to
//! the next region); Convert Loops to Regions makes the repeats real regions.

use std::collections::HashMap;
use std::path::PathBuf;

use opentake_engine::LoopTo;
use opentake_engine::project::{LoopPass, MAX_LOOP_PASSES, RegionLoop};

use super::*;

/// Width of the loop handle at the name bar's right end (the take badge stays left of it).
pub(super) const HANDLE_W: f32 = 8.0;
const NAME_BAR_H: f32 = 15.0;

impl RegionView<'_> {
    pub fn looped(&self) -> Option<RegionLoop> {
        match self.content {
            Content::Audio(c) => c.looped,
            Content::Midi(c) => c.looped,
        }
    }
}

/// Whether `p` is on a region's loop handle.
pub(super) fn in_handle(rect: Rect, p: Pos2) -> bool {
    let w = HANDLE_W.min(rect.width() / 4.0);
    Rect::from_min_max(pos2(rect.right() - w, rect.top()), pos2(rect.right() + 1.0, rect.top() + NAME_BAR_H)).contains(p)
}

/// The handle while hovered or dragged: a loop arrow in the corner.
pub(super) fn paint_handle(painter: &Painter, rect: Rect, row: Rect) {
    let x = rect.right().min(row.right());
    let r = Rect::from_min_max(pos2(x - 14.0, rect.top() + 1.0), pos2(x - 1.0, rect.top() + NAME_BAR_H - 1.0));
    painter.rect_filled(r, 3.0, theme::ACCENT);
    let c = r.center();
    let stroke = Stroke::new(1.3, Color32::WHITE);
    painter.add(egui::Shape::Path(egui::epaint::PathShape::line(
        (0..=10).map(|k| c + 3.5 * vec2((0.4 + k as f32 * 0.52).cos(), (0.4 + k as f32 * 0.52).sin())).collect(),
        stroke,
    )));
    let tip = c + 3.5 * vec2(0.4f32.cos(), 0.4f32.sin());
    painter.add(egui::Shape::convex_polygon(vec![tip + vec2(-2.2, -0.6), tip + vec2(2.2, -0.6), tip + vec2(0.0, 2.4)], Color32::WHITE, Stroke::NONE));
}

/// Where a loop-handle drag to `t` (snapped) ends the region: at least its first pass.
pub(super) fn drag_end(view: RegionView, t: f64, map: &TempoMap) -> f64 {
    // In the region's units: seconds, or a MIDI region's quarter notes.
    let (rel, length, back): (f64, f64, &dyn Fn(f64) -> f64) = match view.content {
        Content::Audio(_) => (t - view.position, view.length, &|rel| view.position + rel),
        Content::Midi(c) => (c.beats_at(map, t), c.length_beats, &|rel| c.secs_of(map, rel)),
    };
    let l = view.looped().unwrap_or(RegionLoop::new(length));
    let first = (l.len - l.phase.rem_euclid(l.len)).min(length);
    back(rel.clamp(first, first + (MAX_LOOP_PASSES - 1) as f64 * l.len))
}

/// The audio region as drawn mid-gesture: trimmed, stretched, or looped by the handle.
pub(super) fn shown_audio(c: &Clip, p: &Painted) -> Clip {
    let mut shown = c.clone();
    match p.mode {
        // Trimming the start reveals/hides audio (and shortens a loop).
        Some(DragMode::TrimStart) => shown.move_start(p.start - c.position_secs),
        Some(DragMode::StretchEnd) => {
            let k = (p.end - p.start) / c.length_secs;
            shown.stretch = c.stretch * k;
            if let Some(l) = &mut shown.looped {
                (l.len, l.phase) = (l.len * k, l.phase * k);
            }
            shown.scale_comp(k);
        }
        Some(DragMode::LoopEnd) if c.looped.is_none() => shown.looped = Some(RegionLoop::new(c.length_secs)),
        _ => {}
    }
    shown.position_secs = p.start;
    shown.length_secs = p.end - p.start;
    shown.crop_comp(0.0);
    shown
}

/// A MIDI region's passes as drawn (`p.start..p.end`), in quarter notes from where it's
/// drawn starting (returned first).
pub(super) fn shown_midi_passes(c: &MidiClip, p: &Painted) -> (f64, Vec<LoopPass>) {
    let looped = c.looped.or((p.mode == Some(DragMode::LoopEnd)).then(|| RegionLoop::new(c.length_beats)));
    let start = p.map.quarters_at(p.start);
    let length = p.map.quarters_at(p.end) - start;
    (start, looped.map_or_else(|| vec![LoopPass { index: 0, at: 0.0, from: 0.0, len: length }], |l| l.passes(length)))
}

/// Passes in quarter notes from `start` as timeline seconds from where that is (to draw the
/// tint and notches; `from` stays in quarter notes).
pub(super) fn passes_in_secs(passes: &[LoopPass], start: f64, map: &TempoMap) -> Vec<LoopPass> {
    let s0 = map.secs_at(start);
    passes.iter().map(|p| LoopPass { at: map.secs_at(start + p.at) - s0, len: map.secs_at(start + p.at + p.len) - map.secs_at(start + p.at), ..*p }).collect()
}

/// Repeats look fainter than the content: tint them towards the lane.
pub(super) fn tint_repeats(painter: &Painter, rect: Rect, view: &View, start: f64, passes: &[LoopPass]) {
    let Some(first) = passes.get(1) else { return };
    let x = view.x_of(start + first.at).max(rect.left());
    let r = Rect::from_min_max(pos2(x, rect.top()), rect.max).intersect(painter.clip_rect());
    if r.width() > 0.0 {
        painter.rect_filled(r, egui::CornerRadius { nw: 0, sw: 0, ne: 5, se: 5 }, theme::with_alpha(theme::BG_LANE, 120));
    }
}

/// A notch at every loop boundary: a line through the region with a cut at top and bottom.
pub(super) fn notches(painter: &Painter, rect: Rect, view: &View, start: f64, passes: &[LoopPass], color: Color32) {
    let ink = theme::shade(color, 0.3);
    for p in passes.iter().skip(1) {
        let x = view.x_of(start + p.at);
        if x < painter.clip_rect().left() - 4.0 || x > painter.clip_rect().right() + 4.0 {
            continue;
        }
        painter.vline(x, Rangef::new(rect.top() + 3.0, rect.bottom() - 3.0), Stroke::new(1.0, ink));
        for (y, d) in [(rect.top(), 4.0), (rect.bottom(), -4.0)] {
            painter.add(egui::Shape::convex_polygon(vec![pos2(x - 3.5, y), pos2(x + 3.5, y), pos2(x, y + d)], theme::BG_LANE, Stroke::NONE));
        }
    }
}

/// An audio region's content once per pass (repeats fainter); a comp shows its takes.
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_audio(painter: &Painter, body: Rect, row: Rect, view: &View, shown: &Clip, waveforms: &HashMap<PathBuf, Arc<Waveform>>, color: Color32, ink: Color32) {
    let clip_x = painter.clip_rect().x_range();
    for p in shown.loop_passes() {
        let (x0, x1) = (view.x_of(shown.position_secs + p.at), view.x_of(shown.position_secs + p.at + p.len));
        if x1 < clip_x.min || x0 > clip_x.max {
            continue;
        }
        let pass = if shown.looped.is_some() { shown.pass_region(&p) } else { shown.clone() };
        let rect = Rect::from_x_y_ranges(Rangef::new(x0, x1), body.y_range()).intersect(body);
        let ink = if p.index > 0 { theme::with_alpha(ink, 150) } else { ink };
        if pass.is_comped() {
            takes::draw_comp(painter, rect, row, view, &pass, waveforms, color, ink);
        } else if let Some(wf) = waveforms.get(&pass.file) {
            draw_waveform(painter, rect, row, view, &pass, wf, ink);
        }
    }
}

/// Loop on regions that don't loop yet (up to the next region), or off when they all do.
pub(crate) fn toggle(app: &mut DawApp, project: &Project, regions: &[RegionRef]) {
    let looped = |r: &RegionRef| region_view(project, *r).is_some_and(|v| v.looped().is_some());
    let off: Vec<RegionRef> = regions.iter().copied().filter(|r| !looped(r) && region_view(project, *r).is_some()).collect();
    match off.is_empty() {
        true if !regions.is_empty() => app.act(Action::LoopRegions { regions: regions.to_vec(), to: LoopTo::Off }),
        true => {}
        false => app.act(Action::LoopRegions { regions: off, to: LoopTo::Fill }),
    }
}

/// The region context menu's loop items.
pub(super) fn menu_items(app: &mut DawApp, ui: &mut egui::Ui, group: &[RegionRef], project: &Project) {
    let looped: Vec<RegionRef> = group.iter().copied().filter(|r| region_view(project, *r).is_some_and(|v| v.looped().is_some())).collect();
    let all = !group.is_empty() && looped.len() == group.len();
    let b = egui::Button::selectable(all, "Loop").shortcut_text("L");
    if ui.add(b).on_hover_text("Repeat the region up to the next one (drag its upper-right corner to set where loops end)").clicked() {
        toggle(app, project, group);
        ui.close();
    }
    if ui.add_enabled(!looped.is_empty(), egui::Button::new("Convert Loops to Regions")).clicked() {
        app.act(Action::ConvertLoops(looped));
        ui.close();
    }
}

impl DawApp {
    /// L: toggle looping on the selected regions.
    pub(crate) fn loop_shortcut(&mut self, ui: &egui::Ui, snap: &Snapshot) {
        if ui.input(|i| i.key_pressed(egui::Key::L) && !i.modifiers.any()) && !self.selection.is_empty() {
            let selection = self.selection.clone();
            toggle(self, &snap.project, &selection);
        }
    }

    /// The selected region's loop in the inspector: on/off, and how long and how often.
    pub(crate) fn loop_inspector(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        let [r] = self.selection[..] else { return };
        let Some(view) = region_view(&snap.project, r) else { return };
        if r.kind == RegionKind::Midi {
            ui.add_space(8.0);
            widgets::section(ui, "Region");
            ui.label(RichText::new(view.name).color(theme::TEXT_DIM));
        }
        ui.horizontal(|ui| {
            let mut on = view.looped().is_some();
            let check = ui.checkbox(&mut on, "Loop").on_hover_text("Repeat the region up to the next one (L); drag its upper-right corner to set where the loops end");
            widgets::label(&check, egui::Role::CheckBox, "Loop");
            if check.changed() {
                self.act(Action::LoopRegions { regions: vec![r], to: if on { LoopTo::Fill } else { LoopTo::Off } });
            }
            if let Some(l) = view.looped() {
                // A MIDI region's loop is in beats (it follows the tempo).
                let (length, unit) = match view.content {
                    Content::Midi(c) => (c.length_beats, "beats"),
                    Content::Audio(_) => (view.length, "s"),
                };
                let passes = l.passes(length).len();
                ui.label(RichText::new(format!("{:.2} {unit} × {passes}", l.len)).size(11.0).color(theme::TEXT_FAINT)).on_hover_text("Loop length and how many times it plays");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;
    use opentake_engine::Engine;

    use super::*;
    use crate::server::SharedEngine;

    /// A 1 s region ("riff", 1.0 – 2.0 s) on track "Guitar", and a second one at `other`.
    fn harness(other: Option<f64>, wgpu: bool) -> (Harness<'static, DawApp>, SharedEngine, PathBuf) {
        let dir = std::env::temp_dir().join(format!("opentake-gui-loop-{}-{:?}", std::process::id(), std::thread::current().id()));
        std::fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("riff.wav");
        let spec = hound::WavSpec { channels: 1, sample_rate: 48_000, bits_per_sample: 32, sample_format: hound::SampleFormat::Float };
        let mut w = hound::WavWriter::create(&wav, spec).unwrap();
        for i in 0..96_000 {
            let t = i as f32 / 48_000.0;
            w.write_sample((t * 220.0 * std::f32::consts::TAU).sin() * (-6.0 * (t % 0.25)).exp() * 0.8).unwrap();
        }
        w.finalize().unwrap();
        let engine: SharedEngine = Arc::new(Mutex::new(Engine::new()));
        {
            let mut e = engine.lock().unwrap();
            e.add_track("Guitar").unwrap();
            e.add_clip(0, &wav, 1.0).unwrap();
            e.trim_clip(0, 0, None, Some(2.0)).unwrap();
            if let Some(at) = other {
                e.add_clip(0, &wav, at).unwrap();
            }
        }
        let app_engine = engine.clone();
        let builder = Harness::builder().with_size([1100.0, 520.0]).with_step_dt(1.0 / 60.0);
        let builder = if wgpu { builder.with_pixels_per_point(2.0).wgpu() } else { builder };
        let mut harness = builder.build_eframe(move |cc| {
            theme::apply(&cc.egui_ctx);
            DawApp::new(app_engine, None)
        });
        harness.state_mut().snap_mode = SnapMode::Off;
        harness.state_mut().view.px_per_sec = 100.0;
        harness.step();
        harness.step();
        (harness, engine, dir)
    }

    fn drag(harness: &mut Harness<'static, DawApp>, from: Pos2, to: Pos2) {
        harness.hover_at(from);
        harness.step();
        harness.drag_at(from);
        harness.step();
        for i in 1..=4 {
            harness.hover_at(from + (to - from) * (i as f32 / 4.0));
            harness.step();
        }
        harness.drop_at(to);
        harness.step();
        harness.step();
    }

    fn looped(engine: &SharedEngine) -> Vec<(f64, f64, Option<f64>)> {
        let e = engine.lock().unwrap();
        let r = |x: f64| (x * 100.0).round() / 100.0;
        e.project().tracks[0].clips.iter().map(|c| (r(c.position_secs), r(c.length_secs), c.looped.map(|l| l.len))).collect()
    }

    #[test]
    fn dragging_the_loop_handle_adds_repeats() {
        let (mut harness, engine, dir) = harness(None, false);
        let rect = harness.get_by_label("Region riff").rect();
        // The upper-right corner (the name bar's end); below it the edge still trims.
        let corner = pos2(rect.right() - 2.0, rect.top() + 6.0);
        drag(&mut harness, corner, corner + vec2(300.0, 0.0));
        assert_eq!(looped(&engine), [(1.0, 4.0, Some(1.0))], "three repeats after the content");
        assert_eq!(engine.lock().unwrap().project().tracks[0].clips[0].loop_passes().len(), 4);
        assert_eq!(engine.lock().unwrap().history_info().undo.as_deref(), Some("Loop Region"));
        // Dragging it back stops at the first pass.
        let rect = harness.get_by_label("Region riff").rect();
        let corner = pos2(rect.right() - 2.0, rect.top() + 6.0);
        drag(&mut harness, corner, corner - vec2(1000.0, 0.0));
        assert_eq!(looped(&engine), [(1.0, 1.0, Some(1.0))]);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn loop_from_the_context_menu_fills_up_to_the_next_region() {
        let (mut harness, engine, dir) = harness(Some(4.0), false);
        let at = harness.query_all_by_label("Region riff").next().unwrap().rect().center();
        harness.hover_at(at);
        harness.step();
        for pressed in [true, false] {
            harness.event(egui::Event::PointerButton { pos: at, button: egui::PointerButton::Secondary, pressed, modifiers: egui::Modifiers::NONE });
        }
        harness.step();
        harness.step();
        harness.get_by_label("Loop L").click();
        harness.step();
        harness.step();
        assert_eq!(looped(&engine)[0], (1.0, 3.0, Some(1.0)), "loops up to the region at 4 s");
        assert_eq!(harness.state().selection, vec![RegionRef::audio(0, 0)]);
        // The inspector shows it; L turns it off again.
        assert!(harness.query_by_label("Loop").is_some());
        harness.key_press(egui::Key::L);
        harness.step();
        harness.step();
        assert_eq!(looped(&engine)[0], (1.0, 1.0, None));
        std::fs::remove_dir_all(dir).ok();
    }

    /// Renders looped audio and MIDI regions to a PNG for visual review:
    /// `OPENTAKE_SCREENSHOT=/tmp/loops.png cargo test -p opentake render_loops -- --ignored`
    #[test]
    #[ignore = "renders with wgpu; run manually for visual checks"]
    fn render_loops() {
        let out = std::env::var("OPENTAKE_SCREENSHOT").unwrap_or_else(|_| "opentake-loops.png".into());
        let (mut harness, engine, dir) = harness(None, true);
        {
            let mut e = engine.lock().unwrap();
            e.set_region_loop(&[RegionRef::audio(0, 0)], LoopTo::Until(4.5)).unwrap();
            let t = e.add_instrument_track("Keys", "synth").unwrap();
            let c = e.add_midi_clip(t, 1.0, 1.0, None).unwrap();
            let note = |s: f64, p: u8| opentake_engine::project::Note { start_beats: 2.0 * s, length_beats: 0.4, pitch: p, velocity: 100, tab: None };
            e.set_midi_notes(t, c, vec![note(0.0, 60), note(0.25, 64), note(0.5, 67), note(0.75, 72)]).unwrap();
            e.set_region_loop(&[RegionRef::midi(t, c)], LoopTo::Passes(4)).unwrap();
        }
        harness.state_mut().selection = vec![RegionRef::audio(0, 0)];
        harness.state_mut().view.px_per_sec = 110.0;
        for _ in 0..3 {
            harness.step();
        }
        let rect = harness.get_by_label("Region riff").rect();
        harness.hover_at(pos2(rect.right() - 2.0, rect.top() + 6.0));
        for _ in 0..3 {
            harness.step();
        }
        harness.render().expect("render").save(&out).expect("save png");
        std::fs::remove_dir_all(dir).ok();
    }
}
