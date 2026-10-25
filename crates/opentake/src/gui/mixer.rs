//! Channel strips (used by the inspector and the full mixer) and the mixer panel.

use opentake_engine::SlotRef;
use opentake_engine::TrackUpdate;
use opentake_engine::project::Track;
use eframe::egui;
use egui::{Align, Align2, Color32, FontId, Layout, Rect, RichText, Sense, Stroke, StrokeKind, UiBuilder, vec2};

use super::widgets::{self, Icon};
use super::{Action, DawApp, MASTER_UI, Snapshot, short_plugin_name, theme};

/// Strip geometry.
#[derive(Clone, Copy)]
pub struct StripLayout {
    pub width: f32,
    /// Height reserved for the Audio FX slot list.
    pub fx_height: f32,
}

/// Full mixer: one strip per track, Stereo Out pinned on the right.
pub fn mixer_panel(app: &mut DawApp, ui: &mut egui::Ui, snap: &Snapshot) {
    const STRIP: f32 = 104.0;
    let layout = StripLayout { width: STRIP, fx_height: 72.0 };
    let full = ui.available_rect_before_wrap();
    let master_w = STRIP + 20.0;
    let tracks_rect = Rect::from_min_max(full.min, egui::pos2(full.right() - master_w, full.bottom()));
    let master_rect = Rect::from_min_max(egui::pos2(full.right() - master_w, full.top()), full.max);

    ui.scope_builder(UiBuilder::new().max_rect(tracks_rect), |ui| {
        egui::ScrollArea::horizontal().id_salt("mixer_strips").auto_shrink([false, false]).show(ui, |ui| {
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                if snap.project.tracks.is_empty() {
                    ui.label(RichText::new("No tracks").color(theme::TEXT_FAINT));
                }
                for (t, track) in snap.project.tracks.iter().enumerate() {
                    strip_frame(ui, app.selected_track == Some(t), layout.width, |ui| {
                        track_strip(app, ui, t, track, snap, layout);
                    });
                }
            });
        });
    });
    ui.painter().vline(master_rect.left() + 4.0, master_rect.y_range(), Stroke::new(1.0, theme::BORDER));
    ui.scope_builder(UiBuilder::new().max_rect(master_rect.shrink2(vec2(12.0, 0.0)).translate(vec2(6.0, 0.0))), |ui| {
        strip_frame(ui, false, layout.width, |ui| master_strip(app, ui, snap, layout));
    });
    ui.allocate_rect(full, Sense::hover());
}

/// Rounded background behind a strip (lighter when its track is selected).
fn strip_frame(ui: &mut egui::Ui, selected: bool, width: f32, add: impl FnOnce(&mut egui::Ui)) {
    let fill = if selected { theme::BG_HEADER_SELECTED } else { theme::BG_HEADER };
    egui::Frame::new().fill(fill).corner_radius(6).inner_margin(egui::Margin::symmetric(5, 6)).show(ui, |ui| {
        ui.set_width(width);
        ui.set_min_height(ui.available_height());
        ui.with_layout(Layout::top_down(Align::Center), add);
    });
}

pub fn track_strip(app: &mut DawApp, ui: &mut egui::Ui, t: usize, track: &Track, snap: &Snapshot, layout: StripLayout) {
    ui.spacing_mut().item_spacing.y = 6.0;
    let w = layout.width;

    if let Some(inst) = &track.instrument {
        widgets::section(ui, "Instrument");
        slot_pill(app, ui, t, SlotRef::Instrument, &inst.name, inst.bypass, w, &snap.instruments);
    } else {
        widgets::section(ui, "Input");
        let max = snap.status.input_channels.max(1) as u16;
        let mut ch = track.input_channel + 1;
        let hi = max.max(ch);
        let r = ui.add_sized([w, 20.0], egui::DragValue::new(&mut ch).range(1..=hi).prefix("Input "));
        if r.changed() {
            app.act(Action::UpdateTrack(t, TrackUpdate { input_channel: Some(ch - 1), ..Default::default() }));
        }
    }

    widgets::section(ui, "Audio FX");
    let (fx_rect, _) = ui.allocate_exact_size(vec2(w, layout.fx_height), Sense::hover());
    ui.scope_builder(UiBuilder::new().max_rect(fx_rect).layout(Layout::top_down(Align::Min)), |ui| {
        ui.spacing_mut().item_spacing.y = 3.0;
        egui::ScrollArea::vertical().id_salt(("fx", t)).auto_shrink([false, false]).show(ui, |ui| {
            for (s, slot) in track.plugins.iter().enumerate() {
                slot_pill(app, ui, t, SlotRef::Effect(s), &slot.name, slot.bypass, w, &[]);
            }
            add_fx_button(app, ui, t, w, snap);
        });
    });

    let mut pan = track.pan;
    ui.horizontal(|ui| {
        ui.add_space((w - 34.0) / 2.0);
        if widgets::knob(ui, &mut pan, -1.0, 1.0, 0.0, true, 34.0, "Pan").changed() {
            app.act(Action::UpdateTrack(t, TrackUpdate { pan: Some(pan), ..Default::default() }));
        }
    });
    let db = snap.status.track_peak_db.get(t).copied().unwrap_or(-120.0);
    let mut gain = track.volume;
    if fader_section(ui, &mut gain, [db, db], "Volume", w) {
        app.act(Action::UpdateTrack(t, TrackUpdate { volume: Some(gain), ..Default::default() }));
    }

    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 3.0;
        let n = if track.is_instrument() { 2.0 } else { 4.0 };
        let size = vec2((w - 3.0 * (n - 1.0)) / n, 20.0);
        if widgets::chip(ui, track.mute, "M", theme::MUTE, "Mute", size).clicked() {
            app.act(Action::UpdateTrack(t, TrackUpdate { mute: Some(!track.mute), ..Default::default() }));
        }
        if widgets::chip(ui, track.solo, "S", theme::SOLO, "Solo", size).clicked() {
            app.act(Action::UpdateTrack(t, TrackUpdate { solo: Some(!track.solo), ..Default::default() }));
        }
        if !track.is_instrument() {
            if widgets::chip(ui, track.armed, "R", theme::RECORD, "Record Enable", size).clicked() {
                app.act(Action::UpdateTrack(t, TrackUpdate { armed: Some(!track.armed), ..Default::default() }));
            }
            if widgets::chip(ui, track.monitor_input, "I", theme::INPUT, "Input Monitoring", size).clicked() {
                app.act(Action::UpdateTrack(t, TrackUpdate { monitor_input: Some(!track.monitor_input), ..Default::default() }));
            }
        }
    });
    if name_plate(ui, &track.name, theme::track_color(t), w).clicked() {
        app.selected_track = Some(t);
    }
}

pub fn master_strip(app: &mut DawApp, ui: &mut egui::Ui, snap: &Snapshot, layout: StripLayout) {
    ui.spacing_mut().item_spacing.y = 6.0;
    let w = layout.width;
    widgets::section(ui, "Output");
    ui.add_sized([w, 20.0], egui::Label::new(RichText::new("Stereo Out").color(theme::TEXT_DIM)));
    widgets::section(ui, "Audio FX");
    let (fx_rect, _) = ui.allocate_exact_size(vec2(w, layout.fx_height), Sense::hover());
    ui.scope_builder(UiBuilder::new().max_rect(fx_rect).layout(Layout::top_down(Align::Min)), |ui| {
        ui.spacing_mut().item_spacing.y = 3.0;
        egui::ScrollArea::vertical().id_salt("fx_master").auto_shrink([false, false]).show(ui, |ui| {
            for (s, slot) in snap.project.master_plugins.iter().enumerate() {
                slot_pill(app, ui, MASTER_UI, SlotRef::Master(s), &slot.name, slot.bypass, w, &[]);
            }
            add_fx_button(app, ui, MASTER_UI, w, snap);
        });
    });
    ui.allocate_exact_size(vec2(w, 34.0), Sense::hover());
    let mut gain = snap.project.master_volume;
    if fader_section(ui, &mut gain, snap.status.master_peak_db, "Master Volume", w) {
        app.act(Action::SetMasterVolume(gain));
    }
    ui.allocate_exact_size(vec2(w, 20.0), Sense::hover());
    name_plate(ui, "Stereo Out", Color32::from_gray(120), w);
}

/// dB readouts, fader and meter. Returns true when the gain changed.
fn fader_section(ui: &mut egui::Ui, gain: &mut f32, peak_db: [f32; 2], a11y: &str, w: f32) -> bool {
    ui.horizontal(|ui| {
        let peak = peak_db[0].max(peak_db[1]);
        let readout = |ui: &mut egui::Ui, text: String, color: Color32| {
            let (r, _) = ui.allocate_exact_size(vec2((w - 6.0) / 2.0, 18.0), Sense::hover());
            ui.painter().rect_filled(r, 3.0, theme::LCD_BG);
            ui.painter().text(r.center(), Align2::CENTER_CENTER, text, FontId::monospace(10.5), color);
        };
        readout(ui, widgets::format_db(*gain), theme::LCD_TEXT);
        let peak_text = if peak <= widgets::MIN_DB { "-∞".to_string() } else { format!("{peak:.1}") };
        readout(ui, peak_text, if peak > -1.0 { theme::METER_HIGH } else { theme::LCD_DIM });
    });
    let fader_h = (ui.available_height() - 58.0).max(90.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        ui.add_space((w - 60.0 - 12.0 - 4.0) / 2.0);
        let changed = widgets::vfader(ui, gain, vec2(60.0, fader_h), a11y).changed();
        widgets::vmeter(ui, vec2(12.0, fader_h), peak_db);
        changed
    })
    .inner
}

/// An insert or instrument slot: power (bypass), name (opens the plug-in window), remove.
#[allow(clippy::too_many_arguments)]
fn slot_pill(app: &mut DawApp, ui: &mut egui::Ui, t: usize, which: SlotRef, name: &str, bypass: bool, w: f32, instruments: &[(String, String)]) {
    let instruments = instruments.to_vec();
    let (rect, _) = ui.allocate_exact_size(vec2(w - 2.0, 22.0), Sense::hover());
    let open = app.open_plugins.contains(&(t, which));
    let fill = match (bypass, which) {
        (true, _) => theme::shade(theme::BG_CONTROL, 0.8),
        (false, SlotRef::Instrument) => Color32::from_rgb(84, 70, 104),
        (false, SlotRef::Effect(_) | SlotRef::Master(_)) => Color32::from_rgb(58, 74, 98),
    };
    ui.painter().rect_filled(rect, 4.0, fill);
    if open {
        ui.painter().rect_stroke(rect, 4.0, Stroke::new(1.0, theme::ACCENT), StrokeKind::Inside);
    }
    ui.scope_builder(UiBuilder::new().max_rect(rect.shrink2(vec2(2.0, 1.0))).layout(Layout::left_to_right(Align::Center)), |ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        let power = if bypass { theme::TEXT_FAINT } else { theme::ACCENT };
        if widgets::icon_plain(ui, Icon::Power, power, if bypass { "Enable plug-in" } else { "Bypass plug-in" }, 18.0).clicked() {
            app.act(Action::SetBypass(t, which, !bypass));
        }
        let label_w = ui.available_width() - 20.0;
        let color = if bypass { theme::TEXT_FAINT } else { theme::TEXT };
        let label = egui::Label::new(RichText::new(short_plugin_name(name)).size(11.0).color(color)).truncate().sense(Sense::click());
        let r = ui.add_sized([label_w, 18.0], label);
        let r = if which == SlotRef::Instrument { r.on_hover_text("Open instrument (right-click to change)") } else { r.on_hover_text("Open plug-in window") };
        if r.clicked() {
            if open {
                app.open_plugins.retain(|&x| x != (t, which));
            } else {
                app.open_plugins.push((t, which));
            }
        }
        if which == SlotRef::Instrument {
            r.context_menu(|ui| {
                widgets::section(ui, "Change Instrument");
                for (name, id) in &instruments {
                    if ui.button(name).clicked() {
                        app.act(Action::SetInstrument { track: t, query: Some(id.clone()) });
                        ui.close();
                    }
                }
            });
        }
        let remove_tip = if which == SlotRef::Instrument { "Remove instrument" } else { "Remove plug-in" };
        if widgets::icon_plain(ui, Icon::Close, theme::TEXT_DIM, remove_tip, 18.0).clicked() {
            app.act(Action::RemovePlugin(t, which));
        }
    });
}

fn add_fx_button(app: &mut DawApp, ui: &mut egui::Ui, t: usize, w: f32, snap: &Snapshot) {
    let (rect, resp) = ui.allocate_exact_size(vec2(w - 2.0, 22.0), Sense::click());
    let stroke = Stroke::new(1.0, if resp.hovered() { theme::TEXT_DIM } else { theme::HAIRLINE });
    ui.painter().rect_stroke(rect, 4.0, stroke, StrokeKind::Inside);
    widgets::draw_icon(ui.painter(), Icon::Plus, rect.center(), 4.0, theme::TEXT_DIM);
    let resp = resp.on_hover_text(if t == MASTER_UI { "Add audio effect to the master bus" } else { "Add audio effect" });
    let insert = |app: &mut DawApp, query: String| {
        app.act(if t == MASTER_UI { Action::InsertMasterPlugin(query) } else { Action::InsertPlugin(t, query) });
    };
    egui::Popup::menu(&resp).show(|ui| {
        ui.set_min_width(220.0);
        widgets::section(ui, "Built-in");
        if ui.button(opentake_engine::eq::NAME).clicked() {
            insert(app, opentake_engine::eq::PATH.into());
            ui.close();
        }
        ui.separator();
        widgets::section(ui, "VST3");
        let effects: Vec<_> = snap.scanned.iter().filter(|p| !p.class.is_instrument()).collect();
        if effects.is_empty() {
            ui.label(RichText::new("No effect plug-ins found").color(theme::TEXT_FAINT));
        }
        for p in effects {
            if ui.button(format!("{}  ·  {}", p.class.name, p.class.vendor)).clicked() {
                insert(app, p.bundle.to_string_lossy().into_owned());
                ui.close();
            }
        }
        ui.separator();
        if ui.button("Plug-in Manager…").clicked() {
            app.show_plugin_manager = true;
            ui.close();
        }
    });
}

/// Coloured name plate at the bottom of a channel strip.
fn name_plate(ui: &mut egui::Ui, name: &str, color: Color32, w: f32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 22.0), Sense::click());
    ui.painter().rect_filled(rect, 4.0, theme::shade(color, 0.55));
    ui.painter().rect_filled(Rect::from_min_size(rect.min, vec2(rect.width(), 3.0)), 1.0, color);
    let text = ui.painter().layout(name.to_string(), FontId::proportional(11.0), theme::TEXT, w - 8.0);
    ui.painter().galley(rect.center() - text.size() / 2.0 + vec2(0.0, 1.0), text, theme::TEXT);
    resp
}
