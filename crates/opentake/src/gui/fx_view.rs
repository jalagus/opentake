//! Editors for the built-in compressor, multiband compressor, limiter, delay and reverb: knobs
//! for every parameter, plus a transfer curve and gain-reduction meter (compressor), the bands
//! with draggable crossovers (multiband), meters (limiter), tempo-synced note values (delay)
//! and the decay tail (reverb). The dynamics show the track keying them; plug-in windows
//! choose it in their header ([`side_chain_menu`]).

use std::sync::Arc;

use eframe::egui;
use egui::{Align2, Color32, FontId, Pos2, Rect, RichText, Sense, Shape, Stroke, pos2, vec2};
use opentake_engine::delay;
use opentake_engine::multiband::{self, MultibandMeter};
use opentake_engine::reverb;
use opentake_engine::{chorus, deesser, gate, phaser};
use opentake_engine::dynamics::{self, CompSettings, GainMeter, comp, limiter};
use opentake_engine::project::Project;
use opentake_engine::{ParamView, SlotRef};

use super::{Action, DawApp, theme, widgets};

/// Which built-in effect a slot path is, if it has an editor here.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Compressor,
    Multiband,
    Limiter,
    Delay,
    Reverb,
    Gate,
    DeEsser,
    Chorus,
    Phaser,
}

impl Kind {
    pub fn of(path: &str) -> Option<Self> {
        match path {
            comp::PATH => Some(Kind::Compressor),
            multiband::PATH => Some(Kind::Multiband),
            limiter::PATH => Some(Kind::Limiter),
            delay::PATH => Some(Kind::Delay),
            reverb::PATH => Some(Kind::Reverb),
            gate::PATH => Some(Kind::Gate),
            deesser::PATH => Some(Kind::DeEsser),
            chorus::PATH => Some(Kind::Chorus),
            phaser::PATH => Some(Kind::Phaser),
            _ => None,
        }
    }

    pub fn window_size(self) -> [f32; 2] {
        match self {
            Kind::Compressor => [600.0, 430.0],
            Kind::Multiband => [760.0, 470.0],
            Kind::Limiter => [420.0, 400.0],
            Kind::Delay => [560.0, 250.0],
            Kind::Reverb => [770.0, 360.0],
            Kind::Gate => [560.0, 500.0],
            Kind::DeEsser => [560.0, 420.0],
            Kind::Chorus => [620.0, 330.0],
            Kind::Phaser => [620.0, 330.0],
        }
    }
}

/// What an editor shows besides its parameters.
pub struct View<'a> {
    pub meter: Option<&'a Arc<GainMeter>>,
    pub bands: Option<&'a Arc<MultibandMeter>>,
    /// Tempo at the playhead (delay note values).
    pub bpm: f64,
    /// Name of the track keying the slot.
    pub key: Option<&'a str>,
}

pub fn show(app: &mut DawApp, ui: &mut egui::Ui, t: usize, s: SlotRef, kind: Kind, params: &[ParamView], view: View) {
    let View { meter, bands, bpm, key } = view;
    if let Some(name) = key.filter(|_| !matches!(kind, Kind::Delay | Kind::Reverb)) {
        key_badge(ui, name);
    }
    match kind {
        Kind::Compressor => compressor(app, ui, t, s, params, meter),
        Kind::Multiband => multiband_view(app, ui, t, s, params, bands),
        Kind::Limiter => limiter_view(app, ui, t, s, params, meter),
        Kind::Delay => delay_view(app, ui, t, s, params, bpm),
        Kind::Reverb => reverb_view(app, ui, t, s, params, meter),
        Kind::Gate => gate_view(app, ui, t, s, params, meter),
        Kind::DeEsser => deesser_view(app, ui, t, s, params, meter),
        Kind::Chorus => chorus_view(app, ui, t, s, params),
        Kind::Phaser => phaser_view(app, ui, t, s, params),
    }
}

/// "KEY ‹track›": the detector listens to that track, not the slot's own input.
fn key_badge(ui: &mut egui::Ui, name: &str) {
    ui.horizontal(|ui| {
        let text = RichText::new(format!("KEY  {name}")).size(10.5).strong().color(theme::BG_DEEP);
        let r = ui.add(egui::Button::new(text).fill(theme::TEMPO).corner_radius(4.0).sense(Sense::hover()));
        widgets::label(&r, egui::Role::Label, &format!("Key: {name}"));
        r.on_hover_text(format!("Side chain: the detector listens to {name}"));
    });
    ui.add_space(4.0);
}

/// The side-chain menu in a plug-in window's header (as in Logic): None, or any track or bus
/// the slot may be keyed by (not one mixed after it), and a PRE toggle for tapping the key
/// before the source's fader and mute.
pub fn side_chain_menu(app: &mut DawApp, ui: &mut egui::Ui, project: &Project, t: usize, s: SlotRef) {
    let (owner, k) = match s {
        SlotRef::Effect(k) => (Some(t), k),
        SlotRef::Master(k) => (None, k),
        SlotRef::Instrument => return,
    };
    let slots = owner.map_or(&project.master_plugins, |i| &project.tracks[i].plugins);
    let current = project.key_source(owner, k);
    let pre = slots.get(k).and_then(|slot| slot.side_chain).is_some_and(|c| c.pre_fader);
    // Right to left: the PRE toggle, the menu, its caption.
    if current.is_some() && widgets::chip(ui, pre, "PRE", theme::TEMPO, "Key before the source's fader and mute", vec2(34.0, 20.0)).clicked() {
        app.act(Action::SetSideChain(t, s, current, !pre));
    }
    let selected = current.map_or("None", |i| project.tracks[i].name.as_str());
    let picker = egui::ComboBox::from_id_salt(("side_chain", t, s)).selected_text(RichText::new(selected).size(11.0)).width(120.0).truncate().show_ui(ui, |ui| {
        let none = ui.selectable_label(current.is_none(), "None");
        widgets::label(&none, egui::Role::Button, "No Side Chain");
        if none.clicked() && current.is_some() {
            app.act(Action::SetSideChain(t, s, None, false));
        }
        for i in (0..project.tracks.len()).filter(|&i| project.can_key(owner, i)) {
            let name = &project.tracks[i].name;
            let item = ui.selectable_label(current == Some(i), name);
            widgets::label(&item, egui::Role::Button, &format!("Key from {name}"));
            if item.clicked() && current != Some(i) {
                app.act(Action::SetSideChain(t, s, Some(i), pre));
            }
        }
    });
    widgets::label(&picker.response, egui::Role::ComboBox, "Side Chain");
    picker.response.on_hover_text("Side chain: the track whose signal drives the detector");
    ui.label(RichText::new("Side Chain:").size(10.5).color(theme::TEXT_FAINT));
}

fn value(params: &[ParamView], id: u32) -> f64 {
    params.iter().find(|p| p.id == id).map_or(0.0, |p| p.value)
}

/// A labelled knob for one parameter (value shown under it).
fn knob(app: &mut DawApp, ui: &mut egui::Ui, t: usize, s: SlotRef, p: &ParamView, bipolar: bool) {
    knob_labeled(app, ui, t, s, p, &p.title, bipolar);
}

fn knob_labeled(app: &mut DawApp, ui: &mut egui::Ui, t: usize, s: SlotRef, p: &ParamView, label: &str, bipolar: bool) {
    ui.allocate_ui_with_layout(vec2(66.0, 86.0), egui::Layout::top_down(egui::Align::Center), |ui| {
        ui.label(RichText::new(label.to_uppercase()).size(9.0).color(theme::TEXT_FAINT).strong());
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

/// Scrolling history of gain reduction (orange, hanging from the top) over the input level
/// (grey, from the bottom), so the editor shows what the dynamics did over the last seconds.
fn history(ui: &mut egui::Ui, id: egui::Id, gr: f32, input_db: f32) {
    const RATE: f64 = 30.0;
    const LEN: usize = 120; // 4 s
    let now = ui.input(|i| i.time);
    let (mut last, mut points): (f64, std::collections::VecDeque<(f32, f32)>) = ui.data(|d| d.get_temp(id)).unwrap_or((now, Default::default()));
    let steps = (((now - last) * RATE) as usize).min(LEN);
    for _ in 0..steps {
        points.push_back((gr.max(0.0), input_db));
        if points.len() > LEN {
            points.pop_front();
        }
    }
    if steps > 0 {
        last += steps as f64 / RATE;
    }
    ui.data_mut(|d| d.insert_temp(id, (last, points.clone())));

    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 84.0), Sense::hover());
    widgets::label(&resp, egui::Role::Image, "Gain Reduction History");
    let p = ui.painter().with_clip_rect(rect);
    p.rect_filled(rect, 6.0, theme::BG_DEEP);
    // Gain reduction scale: 0 dB at the top, 12, 24 or 48 dB at the bottom (whichever fits).
    let peak = points.iter().map(|p| p.0).fold(0.0f32, f32::max);
    let range = [12.0f32, 24.0, 48.0].into_iter().find(|&r| peak <= r).unwrap_or(48.0);
    for db in [3.0, 6.0, 12.0, 24.0].into_iter().filter(|&d| d < range) {
        let y = rect.top() + db / range * rect.height();
        p.hline(rect.x_range(), y, Stroke::new(1.0, theme::HAIRLINE));
        p.text(pos2(rect.right() - 4.0, y), Align2::RIGHT_BOTTOM, format!("-{db:.0}"), FontId::proportional(9.0), theme::TEXT_FAINT);
    }
    let dx = rect.width() / (LEN - 1) as f32;
    let x0 = rect.right() - (points.len().max(1) - 1) as f32 * dx;
    for (i, &(g, inp)) in points.iter().enumerate() {
        let x = x0 + i as f32 * dx;
        let level = ((inp + 60.0) / 60.0).clamp(0.0, 1.0) * rect.height();
        if level > 0.5 {
            p.line_segment([pos2(x, rect.bottom()), pos2(x, rect.bottom() - level)], Stroke::new(dx + 0.5, Color32::from_white_alpha(28)));
        }
        let h = (g / range).clamp(0.0, 1.0) * rect.height();
        if h > 0.5 {
            p.line_segment([pos2(x, rect.top()), pos2(x, rect.top() + h)], Stroke::new(dx + 0.5, theme::with_alpha(theme::TEMPO, 190)));
        }
    }
    p.text(rect.left_top() + vec2(6.0, 4.0), Align2::LEFT_TOP, "GAIN REDUCTION", FontId::proportional(9.0), theme::TEXT_FAINT);
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
    let raw = meter.map_or((0.0, -120.0, -120.0), |m| m.take());
    let levels = smoothed(ui, ui.make_persistent_id(("comp_meter", t, s)), raw);
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
            knob_row(app, ui, t, s, params, &[comp::THRESHOLD, comp::RATIO, comp::KNEE, comp::KEY_LOW_CUT]);
            ui.add_space(6.0);
            knob_row(app, ui, t, s, params, &[comp::ATTACK, comp::RELEASE, comp::MAKEUP, comp::MIX]);
        });
    });
    ui.add_space(8.0);
    history(ui, ui.make_persistent_id(("comp_history", t, s)), raw.0, raw.1);
}

fn multiband_view(app: &mut DawApp, ui: &mut egui::Ui, t: usize, s: SlotRef, params: &[ParamView], bands: Option<&Arc<MultibandMeter>>) {
    use multiband::{BANDS, BAND_NAMES, HIGH_CROSSOVER, LOW_CROSSOVER, band_param};
    let values: Vec<(u32, f64)> = params.iter().map(|p| (p.id, p.value)).collect();
    let settings = multiband::Settings::from_params(&values);
    let none = (0.0, -120.0, -120.0);
    let levels: [(f32, f32, f32); BANDS] =
        std::array::from_fn(|b| smoothed(ui, ui.make_persistent_id(("mb_meter", t, s, b)), bands.map_or(none, |m| m.bands[b].take())));
    let total = smoothed(ui, ui.make_persistent_id(("mb_total", t, s)), bands.map_or(none, |m| m.total.take()));
    let color = |b: usize| [theme::TEMPO, theme::ACCENT, Color32::from_rgb(170, 120, 255)][b];

    // The spectrum split into bands: gain reduction hangs from the top of each, and the
    // crossovers can be dragged.
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 120.0), Sense::hover());
    let p = ui.painter().with_clip_rect(rect);
    p.rect_filled(rect, 6.0, theme::BG_DEEP);
    let x_of = |hz: f64| rect.left() + ((hz / 20.0).ln() / 1000f64.ln()) as f32 * rect.width();
    let hz_of = |x: f32| 20.0 * 1000f64.powf(((x - rect.left()) / rect.width()).clamp(0.0, 1.0) as f64);
    for hz in [50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0, 5000.0, 10_000.0] {
        p.vline(x_of(hz), rect.y_range(), Stroke::new(1.0, theme::HAIRLINE));
        let label = if hz >= 1000.0 { format!("{}k", hz / 1000.0) } else { format!("{hz}") };
        p.text(pos2(x_of(hz) + 3.0, rect.bottom() - 3.0), Align2::LEFT_BOTTOM, label, FontId::proportional(9.0), theme::TEXT_FAINT);
    }
    let edges = [20.0, settings.low_hz, settings.high_hz, 20_000.0];
    let any_solo = settings.bands.iter().any(|b| b.solo);
    for b in 0..BANDS {
        let band = Rect::from_x_y_ranges(x_of(edges[b])..=x_of(edges[b + 1]), rect.y_range());
        let live = !(any_solo && !settings.bands[b].solo);
        p.rect_filled(band.shrink2(vec2(1.0, 0.0)), 0.0, theme::with_alpha(color(b), if live { 34 } else { 10 }));
        let gr = levels[b].0;
        let h = (gr / 24.0).clamp(0.0, 1.0) * rect.height();
        p.rect_filled(Rect::from_min_size(band.min + vec2(1.0, 0.0), vec2(band.width() - 2.0, h)), 0.0, theme::with_alpha(color(b), 150));
        let text = if settings.bands[b].bypass { format!("{} · bypassed", BAND_NAMES[b]) } else { format!("{} -{:.1} dB", BAND_NAMES[b], gr.max(0.0)) };
        p.text(pos2(band.center().x, rect.top() + 14.0), Align2::CENTER_CENTER, text, FontId::proportional(10.5), Color32::WHITE);
    }
    for (id, hz) in [(LOW_CROSSOVER, settings.low_hz), (HIGH_CROSSOVER, settings.high_hz)] {
        let x = x_of(hz);
        let handle = Rect::from_center_size(pos2(x, rect.center().y), vec2(10.0, rect.height()));
        let r = ui.interact(handle, ui.make_persistent_id(("mb_x", t, s, id)), Sense::drag());
        widgets::label(&r, egui::Role::Slider, if id == LOW_CROSSOVER { "Low Crossover" } else { "High Crossover" });
        let hot = r.hovered() || r.dragged();
        if hot {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
        }
        p.vline(x, rect.y_range(), Stroke::new(if hot { 2.0 } else { 1.5 }, Color32::WHITE));
        p.circle_filled(pos2(x, rect.center().y), 4.0, Color32::WHITE);
        p.text(pos2(x + 6.0, rect.center().y + 6.0), Align2::LEFT_TOP, multiband::display(id, multiband::norm_crossover(id, hz)), FontId::proportional(10.0), Color32::WHITE);
        if r.dragged() {
            if let Some(pos) = r.interact_pointer_pos() {
                app.act(Action::SetParam(t, s, id, multiband::norm_crossover(id, hz_of(pos.x))));
            }
        }
    }

    ui.add_space(8.0);
    ui.horizontal_top(|ui| {
        for b in 0..BANDS {
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(BAND_NAMES[b].to_uppercase()).strong().color(color(b)));
                    let band = settings.bands[b];
                    let (bypass, solo) = (band_param(b, multiband::BYPASS), band_param(b, multiband::SOLO));
                    if widgets::chip(ui, band.bypass, "BYP", theme::TEXT_DIM, &format!("Bypass {}", BAND_NAMES[b]), vec2(34.0, 18.0)).clicked() {
                        app.act(Action::SetParam(t, s, bypass, if band.bypass { 0.0 } else { 1.0 }));
                    }
                    if widgets::chip(ui, band.solo, "S", theme::SOLO, &format!("Solo {}", BAND_NAMES[b]), vec2(22.0, 18.0)).clicked() {
                        app.act(Action::SetParam(t, s, solo, if band.solo { 0.0 } else { 1.0 }));
                    }
                });
                for row in [[multiband::THRESHOLD, multiband::RATIO, multiband::GAIN].as_slice(), &[multiband::ATTACK, multiband::RELEASE]] {
                    ui.horizontal_top(|ui| {
                        ui.spacing_mut().item_spacing.x = 0.0;
                        for &offset in row {
                            if let Some(p) = params.iter().find(|p| p.id == band_param(b, offset)) {
                                let label = p.title.split_once(' ').map_or(p.title.as_str(), |(_, rest)| rest);
                                knob_labeled(app, ui, t, s, p, label, offset == multiband::GAIN);
                            }
                        }
                    });
                }
            });
            ui.add_space(6.0);
        }
        ui.vertical(|ui| {
            if let Some(p) = params.iter().find(|p| p.id == multiband::OUTPUT) {
                knob(app, ui, t, s, p, true);
            }
            ui.horizontal_top(|ui| {
                bar(ui, "IN", total.1, false);
                bar(ui, "OUT", total.2, false);
            });
        });
    });
}

fn limiter_view(app: &mut DawApp, ui: &mut egui::Ui, t: usize, s: SlotRef, params: &[ParamView], meter: Option<&Arc<GainMeter>>) {
    let raw = meter.map_or((0.0, -120.0, -120.0), |m| m.take());
    let levels = smoothed(ui, ui.make_persistent_id(("limiter_meter", t, s)), raw);
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
    ui.add_space(8.0);
    history(ui, ui.make_persistent_id(("limiter_history", t, s)), raw.0, raw.1);
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

fn gate_view(app: &mut DawApp, ui: &mut egui::Ui, t: usize, s: SlotRef, params: &[ParamView], meter: Option<&Arc<GainMeter>>) {
    let raw = meter.map_or((0.0, -120.0, -120.0), |m| m.take());
    let levels = smoothed(ui, ui.make_persistent_id(("gate_meter", t, s)), raw);
    ui.horizontal_top(|ui| {
        bar(ui, "IN", levels.1, false);
        bar(ui, "ATT", levels.0, true);
        bar(ui, "OUT", levels.2, false);
        ui.add_space(12.0);
        ui.vertical(|ui| {
            knob_row(app, ui, t, s, params, &[gate::THRESHOLD, gate::HYSTERESIS, gate::RANGE]);
            knob_row(app, ui, t, s, params, &[gate::ATTACK, gate::HOLD, gate::RELEASE]);
            knob_row(app, ui, t, s, params, &[gate::KEY_LOW_CUT, gate::KEY_HIGH_CUT]);
        });
    });
    ui.add_space(8.0);
    history(ui, ui.make_persistent_id(("gate_history", t, s)), raw.0, raw.1);
}

fn deesser_view(app: &mut DawApp, ui: &mut egui::Ui, t: usize, s: SlotRef, params: &[ParamView], meter: Option<&Arc<GainMeter>>) {
    let raw = meter.map_or((0.0, -120.0, -120.0), |m| m.take());
    let levels = smoothed(ui, ui.make_persistent_id(("deesser_meter", t, s)), raw);
    let (wide, listen) = (value(params, deesser::MODE) >= 0.5, value(params, deesser::LISTEN) >= 0.5);
    ui.horizontal(|ui| {
        for (label, on) in [("SPLIT", !wide), ("WIDEBAND", wide)] {
            let tip = if on { "Reducing this way" } else { "Reduce this way" };
            if widgets::chip(ui, on, label, theme::ACCENT, tip, vec2(if label == "WIDEBAND" { 74.0 } else { 48.0 }, 20.0)).clicked() && !on {
                app.act(Action::SetParam(t, s, deesser::MODE, if wide { 0.0 } else { 1.0 }));
            }
        }
        ui.add_space(12.0);
        if widgets::chip(ui, listen, "LISTEN", theme::TEMPO, "Hear only the band being detected", vec2(56.0, 20.0)).clicked() {
            app.act(Action::SetParam(t, s, deesser::LISTEN, if listen { 0.0 } else { 1.0 }));
        }
    });
    ui.add_space(8.0);
    ui.horizontal_top(|ui| {
        bar(ui, "IN", levels.1, false);
        bar(ui, "GR", levels.0, true);
        bar(ui, "OUT", levels.2, false);
        ui.add_space(12.0);
        ui.vertical(|ui| {
            knob_row(app, ui, t, s, params, &[deesser::FREQUENCY, deesser::THRESHOLD]);
            knob_row(app, ui, t, s, params, &[deesser::RANGE, deesser::RELEASE]);
        });
    });
    ui.add_space(8.0);
    history(ui, ui.make_persistent_id(("deesser_history", t, s)), raw.0, raw.1);
}

fn chorus_view(app: &mut DawApp, ui: &mut egui::Ui, t: usize, s: SlotRef, params: &[ParamView]) {
    let values: Vec<(u32, f64)> = params.iter().map(|p| (p.id, p.value)).collect();
    let set = chorus::Settings::from_params(&values);
    // Each voice's delay over two LFO cycles.
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 110.0), Sense::hover());
    widgets::label(&resp, egui::Role::Image, "Chorus Voices");
    let p = ui.painter().with_clip_rect(rect);
    p.rect_filled(rect, 6.0, theme::BG_DEEP);
    // The scale fits the sweep, so its movement shows at any setting.
    let max_ms = ((set.delay_ms + set.depth_ms) * 1.25).max(5.0);
    let step = [1.0, 2.0, 5.0, 10.0, 20.0].into_iter().find(|s| max_ms / s <= 5.0).unwrap_or(20.0);
    let y_of = |ms: f64| rect.bottom() - 8.0 - (ms / max_ms) as f32 * (rect.height() - 16.0);
    for ms in (1..).map(|k| k as f64 * step).take_while(|&ms| ms < max_ms) {
        p.hline(rect.x_range(), y_of(ms), Stroke::new(1.0, theme::HAIRLINE));
        p.text(pos2(rect.right() - 4.0, y_of(ms)), Align2::RIGHT_BOTTOM, format!("{ms:.0} ms"), FontId::proportional(9.0), theme::TEXT_FAINT);
    }
    let colors = [theme::ACCENT, theme::TEMPO, Color32::from_rgb(170, 120, 255), theme::PLAY];
    for v in 0..set.voices {
        let pts: Vec<_> = (0..=200)
            .map(|i| {
                let f = i as f64 / 200.0;
                pos2(rect.left() + f as f32 * rect.width(), y_of(set.voice_delay(v, f * 4.0 * std::f64::consts::PI)))
            })
            .collect();
        p.add(Shape::line(pts, Stroke::new(1.6, colors[v % colors.len()])));
    }
    p.text(rect.left_top() + vec2(6.0, 4.0), Align2::LEFT_TOP, format!("{} voice{} · {:.2} Hz", set.voices, if set.voices == 1 { "" } else { "s" }, set.rate_hz), FontId::proportional(10.0), theme::TEXT_DIM);
    ui.add_space(8.0);
    knob_row(app, ui, t, s, params, &[chorus::RATE, chorus::DEPTH, chorus::DELAY, chorus::VOICES]);
    knob_row(app, ui, t, s, params, &[chorus::WIDTH, chorus::FEEDBACK, chorus::TONE, chorus::MIX]);
}

fn phaser_view(app: &mut DawApp, ui: &mut egui::Ui, t: usize, s: SlotRef, params: &[ParamView]) {
    let values: Vec<(u32, f64)> = params.iter().map(|p| (p.id, p.value)).collect();
    let set = phaser::Settings::from_params(&values);
    let sr = 48_000.0;
    // The response at both ends of the sweep (thin) and at its centre (bold), with the
    // swept range shaded.
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 110.0), Sense::hover());
    widgets::label(&resp, egui::Role::Image, "Phaser Response");
    let p = ui.painter().with_clip_rect(rect);
    p.rect_filled(rect, 6.0, theme::BG_DEEP);
    let x_of = |hz: f64| rect.left() + ((hz / 20.0).ln() / 1000f64.ln()) as f32 * rect.width();
    let hz_of = |x: f32| 20.0 * 1000f64.powf(((x - rect.left()) / rect.width()) as f64);
    let y_of = |db: f64| rect.top() + 8.0 + ((6.0 - db.clamp(-36.0, 6.0)) / 42.0) as f32 * (rect.height() - 16.0);
    let (lo, hi) = (set.corner(-1.0, sr), set.corner(1.0, sr));
    p.rect_filled(Rect::from_x_y_ranges(x_of(lo)..=x_of(hi), rect.y_range()), 0.0, theme::with_alpha(theme::ACCENT, 22));
    for hz in [100.0, 1000.0, 10_000.0] {
        p.vline(x_of(hz), rect.y_range(), Stroke::new(1.0, theme::HAIRLINE));
        p.text(pos2(x_of(hz) + 3.0, rect.bottom() - 3.0), Align2::LEFT_BOTTOM, if hz >= 1000.0 { format!("{}k", hz / 1000.0) } else { format!("{hz}") }, FontId::proportional(9.0), theme::TEXT_FAINT);
    }
    p.hline(rect.x_range(), y_of(0.0), Stroke::new(1.0, theme::HAIRLINE));
    let curve = |corner: f64| -> Vec<Pos2> { (0..=rect.width() as usize).map(|i| {
        let x = rect.left() + i as f32;
        pos2(x, y_of(set.response_db(hz_of(x), corner, sr)))
    }).collect() };
    for corner in [lo, hi] {
        p.add(Shape::line(curve(corner), Stroke::new(1.0, theme::with_alpha(theme::ACCENT, 110))));
    }
    p.add(Shape::line(curve(set.corner(0.0, sr)), Stroke::new(2.0, theme::ACCENT)));
    p.text(rect.left_top() + vec2(6.0, 4.0), Align2::LEFT_TOP, format!("{} stages · {:.2} Hz", set.stages, set.rate_hz), FontId::proportional(10.0), theme::TEXT_DIM);
    ui.add_space(8.0);
    knob_row(app, ui, t, s, params, &[phaser::RATE, phaser::DEPTH, phaser::CENTER, phaser::FEEDBACK]);
    knob_row(app, ui, t, s, params, &[phaser::STAGES, phaser::SPREAD, phaser::MIX]);
}

fn reverb_view(app: &mut DawApp, ui: &mut egui::Ui, t: usize, s: SlotRef, params: &[ParamView], meter: Option<&Arc<GainMeter>>) {
    let values: Vec<(u32, f64)> = params.iter().map(|p| (p.id, p.value)).collect();
    let settings = reverb::Settings::from_params(&values);
    let levels = smoothed(ui, ui.make_persistent_id(("reverb_meter", t, s)), meter.map_or((0.0, -120.0, -120.0), |m| m.take()));
    ui.horizontal_top(|ui| {
        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width() - 84.0, 190.0), Sense::hover());
        widgets::label(&resp, egui::Role::Image, "Reverb Decay");
        decay_graph(ui, rect, &settings);
        ui.add_space(8.0);
        bar(ui, "IN", levels.1, false);
        bar(ui, "OUT", levels.2, false);
    });
    ui.add_space(8.0);
    ui.horizontal_top(|ui| {
        let groups: [(&str, &[u32]); 4] = [
            ("Time", &[reverb::PRE_DELAY, reverb::SIZE, reverb::DECAY]),
            ("Tone", &[reverb::DAMPING, reverb::LOW_CUT]),
            ("Character", &[reverb::DIFFUSION, reverb::MODULATION, reverb::WIDTH]),
            ("Output", &[reverb::MIX, reverb::LEVEL]),
        ];
        for (i, (name, ids)) in groups.into_iter().enumerate() {
            if i > 0 {
                ui.separator();
            }
            ui.vertical(|ui| {
                widgets::section(ui, name);
                knob_row(app, ui, t, s, params, ids);
            });
        }
    });
}

/// The reverb's level over time (dB): the dry hit, the pre-delay gap, a short build-up and
/// the RT60 slope, with the damped highs (8 kHz) fading out sooner on top.
fn decay_graph(ui: &egui::Ui, rect: Rect, s: &reverb::Settings) {
    const FLOOR: f64 = -66.0;
    const HIGH: Color32 = Color32::from_rgb(170, 120, 255);
    let p = ui.painter().with_clip_rect(rect);
    p.rect_filled(rect, 6.0, theme::BG_DEEP);
    let pre = s.pre_delay_ms * 0.001;
    let high = s.decay_at(8000.0, 48_000.0);
    // Round time spans, so the axis only jumps between a few scales as Decay moves.
    let span = [0.5, 1.0, 2.0, 3.0, 5.0, 8.0, 12.0, 16.0, 25.0].into_iter().find(|&d| d >= pre + s.decay_secs * 1.1).unwrap_or(25.0);
    let plot = Rect::from_min_max(pos2(rect.left() + 10.0, rect.top() + 24.0), pos2(rect.right() - 10.0, rect.bottom() - 16.0));
    let x_of = |secs: f64| plot.left() + (secs / span) as f32 * plot.width();
    let y_of = |db: f64| plot.top() + (db / FLOOR).clamp(0.0, 1.0) as f32 * plot.height();
    for db in [-12.0, -24.0, -36.0, -48.0, -60.0] {
        p.hline(rect.x_range(), y_of(db), Stroke::new(1.0, theme::HAIRLINE));
        p.text(pos2(rect.right() - 4.0, y_of(db) - 1.0), Align2::RIGHT_BOTTOM, format!("{db:.0}"), FontId::proportional(9.0), theme::TEXT_FAINT);
    }
    let step = [0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0].into_iter().find(|&d| span / d <= 8.0).unwrap_or(5.0);
    for k in 1..(span / step).round() as usize {
        let secs = k as f64 * step;
        p.vline(x_of(secs), plot.y_range(), Stroke::new(1.0, theme::GRID_SUB));
        let label = if secs < 1.0 { format!("{:.0} ms", secs * 1000.0) } else { format!("{secs} s") };
        p.text(pos2(x_of(secs), rect.bottom() - 3.0), Align2::CENTER_BOTTOM, label, FontId::proportional(9.0), theme::TEXT_FAINT);
    }
    // Levels relative to the input: the wet part follows Mix and Level.
    let wet_db = 20.0 * (s.mix * 10f64.powf(s.level_db / 20.0)).max(1e-6).log10();
    let dry_db = 20.0 * (1.0 - s.mix).max(1e-6).log10();
    // Size and diffusion stretch the build-up from the first reflections to the full tail.
    let build = 0.004 + 0.03 * s.size + 0.02 * s.diffusion;
    let envelope = |secs: f64, rt: f64, offset: f64| {
        let t = secs - pre;
        let onset = if t < build { -15.0 * (1.0 - t / build) } else { 0.0 };
        (t >= 0.0).then(|| wet_db + offset + onset - 60.0 * t / rt).filter(|db| *db > FLOOR)
    };
    let columns = (plot.width() / 2.0) as usize;
    for (rt, offset, color) in [(s.decay_secs, 0.0, theme::ACCENT), (high, -2.0, HIGH)] {
        let mut line = Vec::with_capacity(columns + 1);
        for c in 0..=columns {
            let secs = c as f64 / columns as f64 * span;
            let Some(db) = envelope(secs, rt, offset) else { continue };
            let top = pos2(x_of(secs), y_of(db));
            p.line_segment([pos2(top.x, plot.bottom()), top], Stroke::new(2.0, theme::with_alpha(color, 40)));
            line.push(top);
        }
        p.add(Shape::line(line, Stroke::new(1.5, color)));
    }
    if dry_db > FLOOR {
        let x = x_of(0.0) + 1.0;
        p.line_segment([pos2(x, plot.bottom()), pos2(x, y_of(dry_db))], Stroke::new(2.0, theme::TEXT));
    }
    if pre > 0.0005 {
        let x = x_of(pre);
        for k in 0..(plot.height() / 6.0) as usize {
            let y = plot.top() + k as f32 * 6.0;
            p.line_segment([pos2(x, y), pos2(x, y + 3.0)], Stroke::new(1.0, theme::TEXT_DIM));
        }
        p.text(pos2(x + 4.0, plot.bottom() - 2.0), Align2::LEFT_BOTTOM, format!("PRE {:.0} ms", s.pre_delay_ms), FontId::proportional(9.0), theme::TEXT_DIM);
    }
    // Where the tail has fallen 60 dB.
    let end = x_of(pre + s.decay_secs);
    if end < plot.right() {
        p.circle_filled(pos2(end, y_of(wet_db - 60.0)), 3.0, theme::ACCENT);
    }
    let fmt = |secs: f64| if secs < 10.0 { format!("{secs:.2} s") } else { format!("{secs:.1} s") };
    p.text(rect.left_top() + vec2(8.0, 5.0), Align2::LEFT_TOP, format!("RT60 {}", fmt(s.decay_secs)), FontId::proportional(11.0), theme::ACCENT);
    p.text(rect.left_top() + vec2(92.0, 6.0), Align2::LEFT_TOP, format!("8 kHz {}", fmt(high)), FontId::proportional(10.0), HIGH);
    let mix = format!("{:.0}% wet", s.mix * 100.0);
    p.text(rect.right_top() + vec2(-8.0, 6.0), Align2::RIGHT_TOP, mix, FontId::proportional(10.0), theme::TEXT_DIM);
}
