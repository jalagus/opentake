//! Editors for the built-in compressor, limiter and delay: knobs for every parameter, plus
//! a transfer curve and gain-reduction meter (compressor), meters (limiter) and tempo-synced
//! note values (delay).

use std::sync::Arc;

use eframe::egui;
use egui::{Align2, Color32, FontId, Rect, RichText, Sense, Shape, Stroke, pos2, vec2};
use opentake_engine::delay;
use opentake_engine::dynamics::{self, CompSettings, GainMeter, comp, limiter};
use opentake_engine::{ParamView, SlotRef};

use super::{Action, DawApp, theme, widgets};

/// Which built-in effect a slot path is, if it has an editor here.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Compressor,
    Limiter,
    Delay,
}

impl Kind {
    pub fn of(path: &str) -> Option<Self> {
        match path {
            comp::PATH => Some(Kind::Compressor),
            limiter::PATH => Some(Kind::Limiter),
            delay::PATH => Some(Kind::Delay),
            _ => None,
        }
    }

    pub fn window_size(self) -> [f32; 2] {
        match self {
            Kind::Compressor => [600.0, 330.0],
            Kind::Limiter => [420.0, 300.0],
            Kind::Delay => [560.0, 250.0],
        }
    }
}

pub fn show(app: &mut DawApp, ui: &mut egui::Ui, t: usize, s: SlotRef, kind: Kind, params: &[ParamView], meter: Option<&Arc<GainMeter>>, bpm: f64) {
    match kind {
        Kind::Compressor => compressor(app, ui, t, s, params, meter),
        Kind::Limiter => limiter_view(app, ui, t, s, params, meter),
        Kind::Delay => delay_view(app, ui, t, s, params, bpm),
    }
}

fn value(params: &[ParamView], id: u32) -> f64 {
    params.iter().find(|p| p.id == id).map_or(0.0, |p| p.value)
}

/// A labelled knob for one parameter (value shown under it).
fn knob(app: &mut DawApp, ui: &mut egui::Ui, t: usize, s: SlotRef, p: &ParamView, bipolar: bool) {
    ui.allocate_ui_with_layout(vec2(66.0, 86.0), egui::Layout::top_down(egui::Align::Center), |ui| {
        ui.label(RichText::new(p.title.to_uppercase()).size(9.0).color(theme::TEXT_FAINT).strong());
        let mut v = p.value as f32;
        let default = v;
        if widgets::knob(ui, &mut v, 0.0, 1.0, default, bipolar, 34.0, &p.title).changed() {
            app.act(Action::SetParam(t, s, p.id, v as f64));
        }
        let display = p.display.clone().unwrap_or_default();
        ui.label(RichText::new(format!("{display} {}", p.units)).size(10.5).color(theme::TEXT_DIM));
    });
}

fn knob_row(app: &mut DawApp, ui: &mut egui::Ui, t: usize, s: SlotRef, params: &[ParamView], ids: &[u32]) {
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        for id in ids {
            if let Some(p) = params.iter().find(|p| p.id == *id) {
                knob(app, ui, t, s, p, false);
            }
        }
    });
}

/// Peak-hold meter values that fall back smoothly between frames (dB), keyed per window.
fn smoothed(ui: &egui::Ui, id: egui::Id, now: (f32, f32, f32)) -> (f32, f32, f32) {
    let dt = ui.input(|i| i.stable_dt).min(0.1);
    let prev: (f32, f32, f32) = ui.data(|d| d.get_temp(id)).unwrap_or((0.0, -120.0, -120.0));
    let fall = 24.0 * dt;
    let next = (now.0.max(prev.0 - fall).max(0.0), now.1.max(prev.1 - fall * 2.0), now.2.max(prev.2 - fall * 2.0));
    ui.data_mut(|d| d.insert_temp(id, next));
    ui.ctx().request_repaint_after(std::time::Duration::from_millis(33));
    next
}

/// Vertical meter bar. `reduction` bars grow downward from the top in orange.
fn bar(ui: &mut egui::Ui, label: &str, db: f32, reduction: bool) {
    ui.allocate_ui_with_layout(vec2(34.0, 190.0), egui::Layout::top_down(egui::Align::Center), |ui| {
        ui.label(RichText::new(label).size(9.0).color(theme::TEXT_FAINT).strong());
        let (rect, _) = ui.allocate_exact_size(vec2(12.0, 150.0), Sense::hover());
        let p = ui.painter();
        p.rect_filled(rect, 2.0, theme::BG_DEEP);
        if reduction {
            let h = (db / 24.0).clamp(0.0, 1.0) * rect.height();
            p.rect_filled(Rect::from_min_size(rect.min, vec2(rect.width(), h)), 2.0, theme::TEMPO);
            ui.label(RichText::new(format!("-{:.1}", db.max(0.0))).size(9.5).monospace().color(theme::TEXT_DIM));
        } else {
            let frac = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
            let top = rect.bottom() - frac * rect.height();
            let color = if db > -1.0 { theme::METER_HIGH } else { theme::PLAY };
            p.rect_filled(Rect::from_min_max(pos2(rect.left(), top), rect.max), 2.0, color);
            let text = if db <= -119.0 { "-∞".to_string() } else { format!("{db:.1}") };
            ui.label(RichText::new(text).size(9.5).monospace().color(theme::TEXT_DIM));
        }
    });
}

fn compressor(app: &mut DawApp, ui: &mut egui::Ui, t: usize, s: SlotRef, params: &[ParamView], meter: Option<&Arc<GainMeter>>) {
    let values: Vec<(u32, f64)> = params.iter().map(|p| (p.id, p.value)).collect();
    let settings = CompSettings::from_params(&values);
    let levels = smoothed(ui, ui.make_persistent_id(("comp_meter", t, s)), meter.map_or((0.0, -120.0, -120.0), |m| m.take()));
    ui.horizontal_top(|ui| {
        // Transfer curve: input level (x) to output level (y), -60..0 dB.
        let (rect, _) = ui.allocate_exact_size(vec2(170.0, 170.0), Sense::hover());
        widgets::label(&ui.interact(rect, ui.make_persistent_id(("comp_curve", t, s)), Sense::hover()), egui::Role::Group, "Compressor Curve");
        let p = ui.painter().with_clip_rect(rect);
        p.rect_filled(rect, 6.0, theme::BG_DEEP);
        let to_x = |db: f64| rect.left() + ((db + 60.0) / 60.0) as f32 * rect.width();
        let to_y = |db: f64| rect.bottom() - ((db + 60.0) / 60.0) as f32 * rect.height();
        for db in [-48.0, -36.0, -24.0, -12.0] {
            p.vline(to_x(db), rect.y_range(), Stroke::new(1.0, theme::HAIRLINE));
            p.hline(rect.x_range(), to_y(db), Stroke::new(1.0, theme::HAIRLINE));
        }
        p.line_segment([pos2(rect.left(), rect.bottom()), pos2(rect.right(), rect.top())], Stroke::new(1.0, theme::TEXT_FAINT));
        let points: Vec<_> = (0..=120).map(|i| {
            let x = -60.0 + i as f64 * 0.5;
            pos2(to_x(x), to_y(settings.curve(x) + settings.makeup_db))
        }).collect();
        p.add(Shape::line(points, Stroke::new(2.0, theme::ACCENT)));
        let th = settings.threshold_db;
        p.vline(to_x(th), rect.y_range(), Stroke::new(1.0, theme::with_alpha(theme::TEMPO, 120)));
        // Where the signal is now.
        if levels.1 > -60.0 {
            let x = levels.1 as f64;
            p.circle_filled(pos2(to_x(x), to_y(settings.curve(x) + settings.makeup_db)), 4.0, Color32::WHITE);
        }
        p.text(rect.left_top() + vec2(6.0, 4.0), Align2::LEFT_TOP, format!("{:.1}:1 at {:.0} dB", settings.ratio, th), FontId::proportional(10.0), theme::TEXT_DIM);

        ui.add_space(8.0);
        bar(ui, "IN", levels.1, false);
        bar(ui, "GR", levels.0, true);
        bar(ui, "OUT", levels.2, false);
        ui.add_space(8.0);
        ui.vertical(|ui| {
            knob_row(app, ui, t, s, params, &[comp::THRESHOLD, comp::RATIO, comp::KNEE]);
            ui.add_space(6.0);
            knob_row(app, ui, t, s, params, &[comp::ATTACK, comp::RELEASE, comp::MAKEUP, comp::MIX]);
        });
    });
}

fn limiter_view(app: &mut DawApp, ui: &mut egui::Ui, t: usize, s: SlotRef, params: &[ParamView], meter: Option<&Arc<GainMeter>>) {
    let levels = smoothed(ui, ui.make_persistent_id(("limiter_meter", t, s)), meter.map_or((0.0, -120.0, -120.0), |m| m.take()));
    ui.horizontal_top(|ui| {
        bar(ui, "IN", levels.1, false);
        bar(ui, "GR", levels.0, true);
        bar(ui, "OUT", levels.2, false);
        ui.add_space(12.0);
        ui.vertical(|ui| {
            knob_row(app, ui, t, s, params, &[limiter::GAIN, limiter::CEILING, limiter::RELEASE]);
            ui.add_space(10.0);
            let ceiling = dynamics::ceiling_of(value(params, limiter::CEILING));
            ui.label(RichText::new(format!("Peaks never exceed {ceiling:.1} dBFS.")).size(10.5).color(theme::TEXT_DIM));
            ui.label(
                RichText::new(format!("{:.1} ms look-ahead (compensated automatically).", limiter::LOOKAHEAD_SECS * 1000.0)).size(10.5).color(theme::TEXT_FAINT),
            );
        });
    });
}

fn delay_view(app: &mut DawApp, ui: &mut egui::Ui, t: usize, s: SlotRef, params: &[ParamView], bpm: f64) {
    let sync = value(params, delay::SYNC) >= 0.5;
    let ping = value(params, delay::PING_PONG) >= 0.5;
    ui.horizontal(|ui| {
        if widgets::chip(ui, sync, "SYNC", theme::ACCENT, "Sync to tempo", vec2(48.0, 20.0)).clicked() {
            app.act(Action::SetParam(t, s, delay::SYNC, if sync { 0.0 } else { 1.0 }));
        }
        if sync {
            let current = delay::note_of(value(params, delay::NOTE));
            egui::ComboBox::from_id_salt(("delay_note", t, s)).selected_text(delay::NOTES[current].0).width(70.0).show_ui(ui, |ui| {
                for (i, (label, _)) in delay::NOTES.iter().enumerate() {
                    if ui.selectable_label(i == current, *label).clicked() {
                        app.act(Action::SetParam(t, s, delay::NOTE, delay::norm_note(i)));
                    }
                }
            });
            let ms = delay::NOTES[current].1 * 60_000.0 / bpm.max(1.0);
            ui.label(RichText::new(format!("= {ms:.0} ms at {bpm:.0} BPM")).size(10.5).color(theme::TEXT_FAINT));
        }
        ui.add_space(12.0);
        if widgets::chip(ui, ping, "PING-PONG", theme::ACCENT, "Ping-pong repeats", vec2(80.0, 20.0)).clicked() {
            app.act(Action::SetParam(t, s, delay::PING_PONG, if ping { 0.0 } else { 1.0 }));
        }
    });
    ui.add_space(10.0);
    let mut ids = vec![delay::FEEDBACK, delay::MIX, delay::LOW_CUT, delay::HIGH_CUT];
    if !sync {
        ids.insert(0, delay::TIME);
    }
    knob_row(app, ui, t, s, params, &ids);
}
