//! Region pitch shift and time stretch: the inspector controls, and applying an edit with
//! the render on a background thread when the region is long.

use std::ops::RangeInclusive;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use eframe::egui;
use egui::RichText;
use opentake_engine::project::FadeCurve;
use opentake_engine::{CLIP_GAIN_DB, Engine, FadeUpdate, FlexRender, MAX_SEMITONES, PitchStretch, STRETCH_RANGE};

use super::{Action, DawApp, Snapshot, theme, widgets};
use super::timeline::RegionKind;

/// Regions with less audio than this render right away; longer ones in the background.
const INLINE_SECS: f64 = 20.0;

/// Inspector row: label, hint, value, range, drag speed, suffix, decimals.
type Row<'a> = (&'a str, &'a str, f64, RangeInclusive<f64>, f64, &'a str, usize);

/// A pitch/stretch render running in the background, and the edit to apply when it's done.
pub(crate) struct FlexTask {
    rx: mpsc::Receiver<FlexRender>,
    track: usize,
    clip: usize,
    file: PathBuf,
    ps: PitchStretch,
}

impl DawApp {
    pub(super) fn apply_pitch_stretch(&mut self, e: &mut Engine, track: usize, clip: usize, ps: PitchStretch) -> anyhow::Result<()> {
        match e.pitch_stretch_job(track, clip, ps)? {
            Some(job) if job.source_secs() > INLINE_SECS => {
                let (tx, rx) = mpsc::channel();
                std::thread::spawn(move || drop(tx.send(job.render())));
                let file = e.project().tracks[track].clips[clip].file.clone();
                self.flex_task = Some(FlexTask { rx, track, clip, file, ps });
                self.info("Rendering pitch & stretch…");
            }
            Some(job) => {
                e.add_flex_render(job.render());
                e.set_clip_pitch_stretch(track, clip, ps)?;
            }
            None => drop(e.set_clip_pitch_stretch(track, clip, ps)?),
        }
        Ok(())
    }

    /// Apply a finished background render (if the region is still there).
    pub(super) fn poll_flex(&mut self, ctx: &egui::Context) {
        let Some(task) = &self.flex_task else { return };
        let render = match task.rx.try_recv() {
            Ok(r) => r,
            Err(mpsc::TryRecvError::Empty) => return ctx.request_repaint_after(Duration::from_millis(50)),
            Err(mpsc::TryRecvError::Disconnected) => {
                self.flex_task = None;
                return self.error("Pitch & stretch render failed");
            }
        };
        let FlexTask { track, clip, file, ps, .. } = self.flex_task.take().expect("checked above");
        let result = {
            let mut e = self.lock();
            e.add_flex_render(render);
            let same = e.project().tracks.get(track).and_then(|t| t.clips.get(clip)).is_some_and(|c| c.file == file);
            if same { e.set_clip_pitch_stretch(track, clip, ps).map(|_| ()) } else { Ok(()) }
        };
        match result {
            Ok(()) => self.info("Pitch & stretch applied"),
            Err(e) => self.error(format!("{e:#}")),
        }
    }

    /// Transpose, fine tune, stretch and length of the selected audio region.
    pub(super) fn region_inspector(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        let [r] = self.selection[..] else { return };
        let Some(c) = (r.kind == RegionKind::Audio).then(|| snap.project.tracks.get(r.track).and_then(|t| t.clips.get(r.index))).flatten() else { return };
        ui.add_space(8.0);
        widgets::section(ui, "Region");
        ui.label(RichText::new(&c.name).color(theme::TEXT_DIM));
        let busy = self.flex_task.is_some();
        let (mut ps, mut gain, mut normalize, mut fades) = (None, None, false, None);
        let id = ui.make_persistent_id(("flex", r));
        egui::Grid::new(id).num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
            let semis = MAX_SEMITONES as f64;
            let (lo, hi) = STRETCH_RANGE;
            let rows: [Row; 4] = [
                ("Transpose", "Pitch shift in semitones", c.pitch_semitones as f64, -semis..=semis, 0.1, " st", 0),
                ("Fine Tune", "Pitch fine tuning in cents", c.pitch_cents as f64, -100.0..=100.0, 0.5, " ct", 0),
                ("Stretch", "Time stretch: length relative to the original (pitch is kept). ⌥-drag the region's end to stretch it there.", c.stretch * 100.0, lo * 100.0..=hi * 100.0, 0.5, " %", 1),
                ("Length", "Stretch the region to this length", c.length_secs, (c.length_secs * lo / c.stretch).max(0.01)..=c.length_secs * hi / c.stretch, 0.01, " s", 2),
            ];
            for (i, (label, hint, value, range, speed, suffix, decimals)) in rows.into_iter().enumerate() {
                ui.label(RichText::new(label).color(theme::TEXT_DIM)).on_hover_text(hint);
                ui.add_enabled_ui(!busy, |ui| {
                    if let Some(v) = deferred_drag(ui, id.with(i), value, range, speed, suffix, decimals, label) {
                        ps = Some(match i {
                            0 => PitchStretch { semitones: Some(v.round() as i32), ..Default::default() },
                            1 => PitchStretch { cents: Some(v.round() as i32), ..Default::default() },
                            2 => PitchStretch { stretch: Some(v / 100.0), ..Default::default() },
                            _ => PitchStretch { length_secs: Some(v), ..Default::default() },
                        });
                    }
                });
                ui.end_row();
            }
            // Region gain applies live (no rendering), so a plain drag value.
            ui.label(RichText::new("Gain").color(theme::TEXT_DIM)).on_hover_text("Region level before the track fader");
            ui.horizontal(|ui| {
                let mut db = 20.0 * c.gain.max(1e-6).log10();
                let (lo, hi) = CLIP_GAIN_DB;
                let drag = ui.add(egui::DragValue::new(&mut db).range(lo..=hi).speed(0.1).fixed_decimals(1).suffix(" dB"));
                widgets::label(&drag, egui::Role::SpinButton, "Region Gain");
                if drag.changed() {
                    gain = Some(db);
                }
                if ui.add_enabled(!busy, egui::Button::new("Normalize").small()).on_hover_text("Set the gain so the region's peak reaches 0 dBFS").clicked() {
                    normalize = true;
                }
            });
            ui.end_row();
            // Fades also apply live; the handles in the region's top corners set them too.
            for (i, (label, fade)) in [("Fade In", c.fade_in), ("Fade Out", c.fade_out)].into_iter().enumerate() {
                ui.label(RichText::new(label).color(theme::TEXT_DIM)).on_hover_text("Drag the handle in the region's top corner, or set the length and curve here");
                ui.horizontal(|ui| {
                    let mut ms = fade.secs * 1000.0;
                    let drag = ui.add(egui::DragValue::new(&mut ms).range(0.0..=c.length_secs * 1000.0).speed(1.0).fixed_decimals(0).suffix(" ms"));
                    widgets::label(&drag, egui::Role::SpinButton, label);
                    let mut curve = fade.curve;
                    egui::ComboBox::from_id_salt(id.with(("fade", i))).width(96.0).selected_text(curve.label()).show_ui(ui, |ui| {
                        for c in FadeCurve::ALL {
                            ui.selectable_value(&mut curve, c, c.label());
                        }
                    });
                    let secs = drag.changed().then_some(ms / 1000.0);
                    if secs.is_some() || curve != fade.curve {
                        let curve = (curve != fade.curve).then_some(curve);
                        fades = Some(if i == 0 {
                            FadeUpdate { fade_in_secs: secs, curve_in: curve, ..Default::default() }
                        } else {
                            FadeUpdate { fade_out_secs: secs, curve_out: curve, ..Default::default() }
                        });
                    }
                });
                ui.end_row();
            }
        });
        if let Some(gain_db) = gain {
            self.act(Action::ClipGain { track: r.track, clip: r.index, gain_db });
        }
        if normalize {
            self.act(Action::NormalizeClip { track: r.track, clip: r.index });
        }
        if let Some(update) = fades {
            self.act(Action::ClipFades { track: r.track, clip: r.index, update });
        }
        if c.is_flexed() && ui.add_enabled(!busy, egui::Button::new("Reset Pitch & Stretch").small()).clicked() {
            ps = Some(PitchStretch { semitones: Some(0), cents: Some(0), stretch: Some(1.0), length_secs: None });
        }
        if let Some(ps) = ps {
            self.act(Action::PitchStretch { track: r.track, clip: r.index, ps });
        }
    }
}

/// A drag value that edits a local copy and reports the new value once the drag (or typed
/// entry) ends: each change re-renders the region, so it shouldn't happen every frame.
#[allow(clippy::too_many_arguments)]
fn deferred_drag(ui: &mut egui::Ui, id: egui::Id, current: f64, range: RangeInclusive<f64>, speed: f64, suffix: &str, decimals: usize, a11y: &str) -> Option<f64> {
    let mut v = ui.data(|d| d.get_temp::<f64>(id)).unwrap_or(current);
    let r = ui.add(egui::DragValue::new(&mut v).range(range).speed(speed).suffix(suffix).fixed_decimals(decimals).update_while_editing(false));
    widgets::label(&r, egui::Role::SpinButton, a11y);
    if r.dragged() || r.is_pointer_button_down_on() {
        ui.data_mut(|d| d.insert_temp(id, v));
        return None;
    }
    ui.data_mut(|d| d.remove_temp::<f64>(id));
    ((v - current).abs() > 1e-9).then_some(v)
}
