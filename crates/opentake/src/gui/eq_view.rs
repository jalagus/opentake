//! Editor for the built-in EQ: a frequency-response graph with a draggable handle per
//! band (frequency left/right, gain up/down, scroll for Q, double-click to switch a band
//! on/off), and knobs underneath. The curve is computed by the same code the audio
//! thread runs.

use eframe::egui;
use egui::{Align2, Color32, FontId, Pos2, Rect, RichText, Sense, Shape, Stroke, pos2, vec2};
use std::sync::Arc;

use opentake_engine::eq::{self, BANDS, Field, Settings, Tap};
use opentake_engine::{ParamView, SlotRef};

use super::{Action, DawApp, theme, widgets};

const DB_RANGE: f64 = 24.0;
const BAND_COLORS: [Color32; 6] = [
    Color32::from_rgb(255, 110, 90),
    Color32::from_rgb(255, 176, 64),
    Color32::from_rgb(230, 220, 80),
    Color32::from_rgb(90, 210, 130),
    Color32::from_rgb(80, 170, 255),
    Color32::from_rgb(190, 120, 255),
];

fn x_of(rect: Rect, freq: f64) -> f32 {
    rect.left() + eq::norm_freq(freq) as f32 * rect.width()
}

fn freq_at(rect: Rect, x: f32) -> f64 {
    eq::freq_of(((x - rect.left()) / rect.width()) as f64)
}

fn y_of(rect: Rect, db: f64) -> f32 {
    rect.center().y - (db / DB_RANGE) as f32 * rect.height() / 2.0
}

fn db_at(rect: Rect, y: f32) -> f64 {
    ((rect.center().y - y) / (rect.height() / 2.0)) as f64 * DB_RANGE
}

/// Where a band's handle sits: its frequency, at its gain (cut bands sit on the 0 dB line).
fn handle_pos(rect: Rect, settings: &Settings, band: usize) -> Pos2 {
    let b = &settings.bands[band];
    pos2(x_of(rect, b.freq), y_of(rect, if b.kind.has_gain() { b.gain_db } else { 0.0 }))
}

pub fn show(app: &mut DawApp, ui: &mut egui::Ui, t: usize, s: SlotRef, params: &[ParamView], sample_rate: f64, tap: Option<&Arc<Tap>>) {
    let values: Vec<(u32, f64)> = params.iter().map(|p| (p.id, p.value)).collect();
    let value = |id: u32| values.iter().find(|v| v.0 == id).map_or(0.0, |v| v.1);
    let settings = Settings::from_params(&values);
    let sr = if sample_rate > 0.0 { sample_rate } else { 48_000.0 };

    // ---- Graph
    let width = ui.available_width().clamp(360.0, 900.0);
    let (rect, resp) = ui.allocate_exact_size(vec2(width, 230.0), Sense::click_and_drag());
    widgets::label(&resp, egui::Role::Group, "EQ Graph");
    let p = ui.painter().with_clip_rect(rect);
    p.rect_filled(rect, 6.0, theme::BG_DEEP);
    for f in [50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0, 5000.0, 10000.0] {
        let x = x_of(rect, f);
        p.vline(x, rect.y_range(), Stroke::new(1.0, theme::HAIRLINE));
        let label = if f >= 1000.0 { format!("{}k", f / 1000.0) } else { format!("{f}") };
        p.text(pos2(x + 3.0, rect.bottom() - 3.0), Align2::LEFT_BOTTOM, label, FontId::proportional(9.5), theme::TEXT_FAINT);
    }
    for db in [-18.0, -12.0, -6.0, 0.0, 6.0, 12.0, 18.0] {
        let y = y_of(rect, db);
        let color = if db == 0.0 { theme::TEXT_FAINT } else { theme::HAIRLINE };
        p.hline(rect.x_range(), y, Stroke::new(1.0, color));
        p.text(pos2(rect.left() + 4.0, y - 1.0), Align2::LEFT_BOTTOM, format!("{db:+}"), FontId::proportional(9.0), theme::TEXT_FAINT);
    }

    // Analyzer: what goes in (grey) and what comes out (teal), behind the curve.
    if let Some(tap) = tap {
        draw_analyzer(ui, &p, rect, tap, ui.make_persistent_id(("eq_spectrum", t, s)));
    }

    // Total response, filled toward 0 dB.
    let n = (rect.width() as usize).clamp(64, 600);
    let points: Vec<Pos2> = (0..=n)
        .map(|i| {
            let x = rect.left() + rect.width() * i as f32 / n as f32;
            let db = settings.response_db(freq_at(rect, x), sr).clamp(-DB_RANGE * 1.2, DB_RANGE * 1.2);
            pos2(x, y_of(rect, db))
        })
        .collect();
    let zero = y_of(rect, 0.0);
    for w in points.windows(2) {
        let quad = vec![w[0], w[1], pos2(w[1].x, zero), pos2(w[0].x, zero)];
        p.add(Shape::convex_polygon(quad, theme::with_alpha(theme::ACCENT, 34), Stroke::NONE));
    }
    p.add(Shape::line(points, Stroke::new(2.0, theme::ACCENT)));

    // ---- Handles and interaction
    let drag_id = ui.make_persistent_id(("eq_drag", t, s));
    let mut dragging: Option<usize> = ui.data(|d| d.get_temp(drag_id));
    let pointer = resp.hover_pos().or(resp.interact_pointer_pos());
    let nearest = |pos: Pos2| -> Option<usize> {
        (0..BANDS.len())
            .map(|b| (b, handle_pos(rect, &settings, b).distance(pos)))
            .filter(|&(_, d)| d <= 12.0)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(b, _)| b)
    };
    let hovered = dragging.or_else(|| pointer.and_then(nearest));
    for (b, &(name, kind, ..)) in BANDS.iter().enumerate() {
        let pos = handle_pos(rect, &settings, b);
        let color = BAND_COLORS[b];
        let on = settings.bands[b].on;
        if Some(b) == hovered {
            p.circle_filled(pos, 11.0, theme::with_alpha(color, 50));
            let q = format!(" · Q {:.2}", settings.bands[b].q);
            let gain = if kind.has_gain() { format!(" · {:+.1} dB", settings.bands[b].gain_db) } else { String::new() };
            let text = format!("{name}: {}{gain}{q}", eq::display(eq::param_id(b, Field::Freq), value(eq::param_id(b, Field::Freq))));
            let anchor = if pos.x > rect.center().x { Align2::RIGHT_BOTTOM } else { Align2::LEFT_BOTTOM };
            p.text(pos + vec2(if pos.x > rect.center().x { -12.0 } else { 12.0 }, -10.0), anchor, text, FontId::proportional(10.5), theme::TEXT);
        }
        if on {
            p.circle_filled(pos, 6.0, color);
        } else {
            p.circle_stroke(pos, 5.5, Stroke::new(1.5, theme::with_alpha(color, 140)));
        }
        p.text(pos, Align2::CENTER_CENTER, (b + 1).to_string(), FontId::proportional(8.5), if on { Color32::from_rgb(20, 20, 24) } else { color });
    }
    let resp = resp.on_hover_text("Drag a band's point to set frequency and gain · scroll on it for Q (tightness) · double-click to switch it on/off");

    if resp.drag_started() {
        dragging = ui.input(|i| i.pointer.press_origin()).and_then(nearest);
    }
    if let (Some(b), true, Some(pos)) = (dragging, resp.dragged(), resp.interact_pointer_pos()) {
        let kind = BANDS[b].1;
        let mut changes = vec![(eq::param_id(b, Field::Freq), eq::norm_freq(freq_at(rect, pos.x.clamp(rect.left(), rect.right()))))];
        if kind.has_gain() {
            changes.push((eq::param_id(b, Field::Gain), eq::norm_gain(db_at(rect, pos.y).clamp(-DB_RANGE, DB_RANGE))));
        }
        if !settings.bands[b].on {
            changes.push((eq::param_id(b, Field::On), 1.0));
        }
        app.act(Action::SetParams(t, s, changes, format!("Adjust {}", BANDS[b].0)));
    }
    if resp.drag_stopped() {
        dragging = None;
    }
    ui.data_mut(|d| match dragging {
        Some(b) => {
            d.insert_temp(drag_id, b);
        }
        None => d.remove::<usize>(drag_id),
    });
    if resp.double_clicked() {
        if let Some(b) = pointer.and_then(nearest) {
            let on = eq::param_id(b, Field::On);
            app.act(Action::SetParam(t, s, on, if settings.bands[b].on { 0.0 } else { 1.0 }));
        }
    }
    // Scroll over a band's point changes its Q (bell width, shelf knee, cut resonance).
    if let Some(b) = hovered {
        let scroll = ui.input(|i| i.smooth_scroll_delta.y);
        if resp.hovered() && scroll != 0.0 {
            let q = (settings.bands[b].q * (scroll as f64 * 0.01).exp()).clamp(0.1, 10.0);
            app.act(Action::SetParams(t, s, vec![(eq::param_id(b, Field::Q), eq::norm_q(q))], format!("Adjust {} Q", BANDS[b].0)));
        }
    }

    // ---- Knobs per band
    ui.add_space(8.0);
    ui.horizontal_top(|ui| {
        // Six band columns plus the output column, with the spacing between them, fit the
        // window exactly (anything wider would make the window grow every frame).
        let spacing = ui.spacing().item_spacing.x;
        let col_w = ((ui.available_width() - 70.0 - spacing * BANDS.len() as f32) / BANDS.len() as f32).max(64.0);
        for (b, &(name, kind, ..)) in BANDS.iter().enumerate() {
            ui.allocate_ui_with_layout(vec2(col_w, 150.0), egui::Layout::top_down(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.y = 3.0;
                let on_id = eq::param_id(b, Field::On);
                if widgets::chip(ui, settings.bands[b].on, name, BAND_COLORS[b], &format!("{name} on/off"), vec2(col_w - 6.0, 20.0)).clicked() {
                    app.act(Action::SetParam(t, s, on_id, if settings.bands[b].on { 0.0 } else { 1.0 }));
                }
                let mut fields = vec![(Field::Freq, "Freq")];
                if kind.has_gain() {
                    fields.push((Field::Gain, "Gain"));
                }
                fields.push((Field::Q, "Q"));
                for (field, label) in fields {
                    let id = eq::param_id(b, field);
                    knob(app, ui, t, s, id, value(id), &format!("{name} {label}"), label, field == Field::Gain);
                }
                if kind.has_slope() {
                    // Cut slope: 12 / 24 / 48 dB per octave.
                    let id = eq::param_id(b, Field::Slope);
                    let current = eq::sections_of(value(id));
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 2.0;
                        for (n, v) in [(1usize, 0.0), (2, 0.5), (4, 1.0)] {
                            let label = RichText::new(format!("{}", 12 * n)).size(9.5);
                            if ui.add(egui::Button::new(label).selected(current == n).min_size(vec2(22.0, 16.0))).on_hover_text(format!("{} dB/oct", 12 * n)).clicked() {
                                app.act(Action::SetParam(t, s, id, v));
                            }
                        }
                    });
                }
            });
        }
        ui.allocate_ui_with_layout(vec2(70.0, 150.0), egui::Layout::top_down(egui::Align::Center), |ui| {
            ui.label(RichText::new("OUTPUT").size(9.5).color(theme::TEXT_FAINT).strong());
            knob(app, ui, t, s, eq::OUTPUT, value(eq::OUTPUT), "EQ Output", "Gain", true);
        });
    });
}

#[allow(clippy::too_many_arguments)]
fn knob(app: &mut DawApp, ui: &mut egui::Ui, t: usize, s: SlotRef, id: u32, value: f64, a11y: &str, label: &str, bipolar: bool) {
    let mut v = value as f32;
    let default = eq::params().into_iter().find(|p| p.0 == id).map_or(0.5, |p| p.3) as f32;
    let r = widgets::knob(ui, &mut v, 0.0, 1.0, default, bipolar, 26.0, a11y);
    if r.changed() {
        app.act(Action::SetParam(t, s, id, v as f64));
    }
    ui.label(RichText::new(format!("{label} {}", eq::display(id, v as f64))).size(9.5).color(theme::TEXT_DIM));
}

// ---------------------------------------------------------------- analyzer

const FFT_SIZE: usize = 4096;
const FLOOR_DB: f32 = -90.0;
const TOP_DB: f32 = -6.0;

/// In-place radix-2 FFT of (re, im); `re.len()` must be a power of two.
fn fft(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let angle = -std::f32::consts::TAU / len as f32;
        let (wr, wi) = (angle.cos(), angle.sin());
        for start in (0..n).step_by(len) {
            let (mut cr, mut ci) = (1.0f32, 0.0f32);
            for k in 0..len / 2 {
                let (a, b) = (start + k, start + k + len / 2);
                let (xr, xi) = (re[b] * cr - im[b] * ci, re[b] * ci + im[b] * cr);
                re[b] = re[a] - xr;
                im[b] = im[a] - xi;
                re[a] += xr;
                im[a] += xi;
                let next = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = next;
            }
        }
        len <<= 1;
    }
}

/// Spectrum in dB per column across `rect` (log frequency), with a +3 dB/octave tilt around
/// 1 kHz so typical music reads roughly flat.
fn spectrum_columns(samples: &[f32], sr: f64, rect: Rect, columns: usize) -> Vec<f32> {
    let n = samples.len();
    let mut re: Vec<f32> = samples
        .iter()
        .enumerate()
        .map(|(i, &v)| v * (0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / n as f32).cos()))
        .collect();
    let mut im = vec![0.0; n];
    fft(&mut re, &mut im);
    let norm = 2.0 / (n as f32 * 0.5); // Hann window's coherent gain is 0.5
    let mag = |k: usize| ((re[k] * re[k] + im[k] * im[k]).sqrt() * norm).max(1e-9);
    let bin_hz = sr / n as f64;
    (0..columns)
        .map(|c| {
            let x0 = rect.left() + rect.width() * c as f32 / columns as f32;
            let x1 = rect.left() + rect.width() * (c + 1) as f32 / columns as f32;
            let (f0, f1) = (freq_at(rect, x0), freq_at(rect, x1));
            let (k0, k1) = ((f0 / bin_hz).floor() as usize, ((f1 / bin_hz).ceil() as usize).min(n / 2 - 1));
            let peak = if k1 > k0 + 1 {
                (k0..=k1).map(mag).fold(0.0, f32::max)
            } else {
                // Narrower than a bin (low frequencies): interpolate between neighbours.
                let pos = ((f0 + f1) / 2.0 / bin_hz) as f32;
                let (k, frac) = (pos.floor() as usize, pos.fract());
                mag(k.min(n / 2 - 1)) * (1.0 - frac) + mag((k + 1).min(n / 2 - 1)) * frac
            };
            let tilt = 3.0 * ((f0 * f1).sqrt() / 1000.0).log2() as f32;
            20.0 * peak.log10() + tilt
        })
        .collect()
}

fn draw_analyzer(ui: &egui::Ui, p: &egui::Painter, rect: Rect, tap: &Tap, id: egui::Id) {
    let (mut pre, mut post) = (Vec::new(), Vec::new());
    tap.read(FFT_SIZE, &mut pre, &mut post);
    let columns = ((rect.width() / 2.0) as usize).max(16);
    let sr = tap.sample_rate();
    let now = [spectrum_columns(&pre, sr, rect, columns), spectrum_columns(&post, sr, rect, columns)];
    // Peaks rise at once and fall back at 36 dB/s, like a meter.
    let dt = ui.input(|i| i.stable_dt).min(0.1);
    let mut held: [Vec<f32>; 2] = ui.data(|d| d.get_temp(id)).unwrap_or_else(|| [vec![FLOOR_DB; columns], vec![FLOOR_DB; columns]]);
    for (h, n) in held.iter_mut().zip(&now) {
        h.resize(columns, FLOOR_DB);
        for (h, &n) in h.iter_mut().zip(n) {
            *h = n.max(*h - 36.0 * dt).max(FLOOR_DB);
        }
    }
    let y = |db: f32| rect.bottom() - ((db - FLOOR_DB) / (TOP_DB - FLOOR_DB)).clamp(0.0, 1.0) * rect.height();
    let x = |c: usize| rect.left() + rect.width() * (c as f32 + 0.5) / columns as f32;
    for (side, fill, line) in [(0, Color32::from_white_alpha(18), Color32::from_white_alpha(60)), (1, Color32::from_rgba_unmultiplied(70, 200, 190, 40), Color32::from_rgb(70, 200, 190))] {
        let pts: Vec<Pos2> = held[side].iter().enumerate().map(|(c, &db)| pos2(x(c), y(db))).collect();
        for w in pts.windows(2) {
            p.add(Shape::convex_polygon(vec![w[0], w[1], pos2(w[1].x, rect.bottom()), pos2(w[0].x, rect.bottom())], fill, Stroke::NONE));
        }
        p.add(Shape::line(pts, Stroke::new(1.0, line)));
    }
    ui.data_mut(|d| d.insert_temp(id, held));
    ui.ctx().request_repaint_after(std::time::Duration::from_millis(33));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analyzer_finds_a_sine_at_its_frequency() {
        let sr = 48_000.0;
        let samples: Vec<f32> = (0..FFT_SIZE).map(|i| (std::f64::consts::TAU * 1000.0 * i as f64 / sr).sin() as f32 * 0.5).collect();
        let rect = Rect::from_min_size(pos2(0.0, 0.0), vec2(600.0, 200.0));
        let cols = spectrum_columns(&samples, sr, rect, 300);
        let peak = cols.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap().0;
        let peak_hz = freq_at(rect, rect.width() * (peak as f32 + 0.5) / 300.0);
        assert!((peak_hz / 1000.0 - 1.0).abs() < 0.05, "peak at {peak_hz} Hz");
        // A half-scale sine reads about -6 dBFS (no tilt at 1 kHz).
        assert!((cols[peak] + 6.0).abs() < 1.5, "{} dB", cols[peak]);
    }
}
