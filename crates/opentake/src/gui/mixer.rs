//! Channel strips (used by the inspector and the full mixer) and the mixer panel.

use opentake_engine::SlotRef;
use opentake_engine::{SendUpdate, TrackUpdate};
use opentake_engine::project::automation::{gain_to_pos, pos_to_gain};
use opentake_engine::project::{AutomationTarget, Track};
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
    /// Height reserved for the Sends list.
    pub sends_height: f32,
}

/// Full mixer: one strip per track, Stereo Out pinned on the right.
pub fn mixer_panel(app: &mut DawApp, ui: &mut egui::Ui, snap: &Snapshot) {
    const STRIP: f32 = 104.0;
    let layout = StripLayout { width: STRIP, fx_height: 72.0, sends_height: SENDS_H };
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
                // Empty strip space selects the track on click and moves it on drag. Registered
                // before the strip's widgets so those keep their own clicks.
                let mut strips = Vec::with_capacity(snap.project.tracks.len());
                let mut dragged = None;
                for (t, track) in snap.project.tracks.iter().enumerate() {
                    let area = Rect::from_min_size(ui.cursor().min, vec2(layout.width + 10.0, ui.available_height()));
                    let bg = ui.interact(area, ui.make_persistent_id(("strip", t)), Sense::click_and_drag());
                    widgets::label(&bg, egui::Role::Button, &format!("Strip {}", track.name));
                    strip_frame(ui, app.selected_track == Some(t), layout.width, |ui| {
                        track_strip(app, ui, t, track, snap, layout);
                    });
                    if bg.clicked() || bg.drag_started() {
                        app.selected_track = Some(t);
                    }
                    if bg.drag_started() {
                        app.track_drag = Some(t);
                    }
                    if app.track_drag == Some(t) && (bg.dragged() || bg.drag_stopped()) {
                        dragged = Some((t, bg.drag_stopped()));
                    }
                    strips.push(area);
                }
                if let Some((from, dropped)) = dragged {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
                    let x = ui.ctx().pointer_latest_pos().map_or(strips[from].center().x, |p| p.x);
                    let to = strips.iter().position(|r| x < r.right()).unwrap_or(strips.len() - 1);
                    if dropped {
                        app.track_drag = None;
                        if to != from {
                            app.act(Action::MoveTrack { from, to });
                        }
                    } else if to != from {
                        let x = if to > from { strips[to].right() + 3.0 } else { strips[to].left() - 3.0 };
                        ui.painter().vline(x, strips[to].y_range(), Stroke::new(2.0, theme::ACCENT));
                    }
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

    if track.bus {
        widgets::section(ui, "Input");
        let plural = |n: usize, what: &str| format!("{n} {what}{}", if n == 1 { "" } else { "s" });
        let text = match (snap.project.sources_of(t).len(), snap.project.send_sources_of(t).len()) {
            (0, 0) => "No inputs".to_string(),
            (n, 0) => plural(n, "track"),
            (0, s) => plural(s, "send"),
            (n, s) => format!("{n} trk · {}", plural(s, "send")),
        };
        ui.add_sized([w, 20.0], egui::Label::new(RichText::new(text).color(theme::TEXT_DIM)))
            .on_hover_text("Tracks routed to this bus (set their Output) and sends to it");
    } else if let Some(inst) = &track.instrument {
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
    sends_section(app, ui, t, track, snap, layout);
    widgets::section(ui, "Output");
    output_button(app, ui, t, snap, w);

    // Automated controls follow their lanes (see the track headers).
    let playhead = snap.status.position_secs;
    let mut pan = track.automated(AutomationTarget::Pan, playhead).map_or(track.pan, |v| v * 2.0 - 1.0);
    ui.horizontal(|ui| {
        ui.add_space((w - 34.0) / 2.0);
        let r = widgets::knob(ui, &mut pan, -1.0, 1.0, 0.0, true, 34.0, "Pan");
        if r.changed() {
            app.control(&track.automation, Some(t), AutomationTarget::Pan, (pan + 1.0) / 2.0, Action::UpdateTrack(t, TrackUpdate { pan: Some(pan), ..Default::default() }));
        }
        if r.drag_stopped() {
            app.act(Action::ReleaseControl { owner: Some(t), target: AutomationTarget::Pan });
        }
    });
    let db = snap.status.track_peak_db.get(t).copied().unwrap_or(-120.0);
    let mut gain = track.automated(AutomationTarget::Volume, playhead).map_or(track.volume, pos_to_gain);
    let (changed, released) = fader_section(ui, &mut gain, [db, db], "Volume", w);
    if changed {
        app.control(&track.automation, Some(t), AutomationTarget::Volume, gain_to_pos(gain), Action::UpdateTrack(t, TrackUpdate { volume: Some(gain), ..Default::default() }));
    }
    if released {
        app.act(Action::ReleaseControl { owner: Some(t), target: AutomationTarget::Volume });
    }
    let mute = track.automated(AutomationTarget::Mute, playhead).map_or(track.mute, |v| v >= 0.5);

    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 3.0;
        let records = !track.is_instrument() && !track.bus;
        let n = if records { 4.0 } else { 2.0 };
        let size = vec2((w - 3.0 * (n - 1.0)) / n, 20.0);
        if widgets::chip(ui, mute, "M", theme::MUTE, "Mute", size).clicked() {
            app.control(&track.automation, Some(t), AutomationTarget::Mute, if mute { 0.0 } else { 1.0 }, Action::UpdateTrack(t, TrackUpdate { mute: Some(!mute), ..Default::default() }));
        }
        if widgets::chip(ui, track.solo, "S", theme::SOLO, "Solo", size).clicked() {
            app.act(Action::UpdateTrack(t, TrackUpdate { solo: Some(!track.solo), ..Default::default() }));
        }
        if records {
            if widgets::chip(ui, track.armed, "R", theme::RECORD, "Record Enable", size).clicked() {
                app.act(Action::UpdateTrack(t, TrackUpdate { armed: Some(!track.armed), ..Default::default() }));
            }
            if widgets::chip(ui, track.monitor_input, "I", theme::INPUT, "Input Monitoring", size).clicked() {
                app.act(Action::UpdateTrack(t, TrackUpdate { monitor_input: Some(!track.monitor_input), ..Default::default() }));
            }
        }
    });
    if name_plate(ui, &track.name, theme::color_of(track, t), w).clicked() {
        app.selected_track = Some(t);
    }
}

/// Rows of the Sends list (two show without scrolling).
pub const SENDS_H: f32 = 43.0;

/// Logic's sends under the inserts: one row per send (bus, level knob, pre/post, mute) and
/// an empty slot that adds one.
fn sends_section(app: &mut DawApp, ui: &mut egui::Ui, t: usize, track: &Track, snap: &Snapshot, layout: StripLayout) {
    widgets::section(ui, "Sends");
    let w = layout.width;
    let (rect, _) = ui.allocate_exact_size(vec2(w, layout.sends_height), Sense::hover());
    ui.scope_builder(UiBuilder::new().max_rect(rect).layout(Layout::top_down(Align::Min)), |ui| {
        ui.spacing_mut().item_spacing.y = 3.0;
        egui::ScrollArea::vertical().id_salt(("sends", t)).auto_shrink([false, false]).show(ui, |ui| {
            for k in 0..track.sends.len() {
                send_row(app, ui, t, k, track, snap, w);
            }
            add_send_button(app, ui, t, snap, w);
        });
    });
}

fn send_row(app: &mut DawApp, ui: &mut egui::Ui, t: usize, k: usize, track: &Track, snap: &Snapshot, w: f32) {
    let project = &snap.project;
    let send = &track.sends[k];
    let dest = project.send_dest(t, k).map_or("—", |b| project.tracks[b].name.as_str());
    let (rect, _) = ui.allocate_exact_size(vec2(w - 2.0, 20.0), Sense::hover());
    ui.painter().rect_filled(rect, 4.0, theme::BG_CONTROL);
    ui.scope_builder(UiBuilder::new().max_rect(rect.shrink2(vec2(2.0, 1.0))).layout(Layout::left_to_right(Align::Center)), |ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        let n = k + 1;
        // Destination: pick another bus, a new one, or no send.
        let color = if send.mute { theme::TEXT_FAINT } else { theme::BUS };
        let name_w = (ui.available_width() - 18.0 - 26.0 - 16.0 - 6.0).max(16.0);
        let label = egui::Label::new(RichText::new(dest).size(10.5).color(color)).truncate().sense(Sense::click());
        let resp = ui.add_sized([name_w, 18.0], label);
        widgets::label(&resp, egui::Role::Button, &format!("Send {n}: {dest}"));
        let resp = resp.on_hover_text("Send destination (click to change)");
        egui::Popup::menu(&resp).show(|ui| {
            ui.set_min_width(160.0);
            for b in (0..project.tracks.len()).filter(|&b| b == project.send_dest(t, k).unwrap_or(usize::MAX) || project.can_send(t, b)) {
                if ui.add(egui::Button::new(&project.tracks[b].name).selected(Some(b) == project.send_dest(t, k))).clicked() {
                    app.act(Action::UpdateSend(t, k, SendUpdate { bus: Some(b), ..Default::default() }));
                    ui.close();
                }
            }
            ui.separator();
            if ui.button("New Bus").clicked() {
                app.act(Action::SendToNewBus { track: t, send: Some(k), reverb: false });
                ui.close();
            }
            if ui.button("No Send").clicked() {
                app.act(Action::RemoveSend(t, k));
                ui.close();
            }
        });
        // Level on the fader's scale; automated sends follow their lane.
        let target = AutomationTarget::Send { send: send.id };
        let level = track.automated(target, snap.status.position_secs).map_or(send.level, pos_to_gain);
        let mut pos = gain_to_pos(level);
        let r = widgets::knob(ui, &mut pos, 0.0, 1.0, gain_to_pos(1.0), false, 18.0, &format!("Send {n} Level"));
        let r = r.on_hover_text(format!("Send level {} dB (double-click: 0 dB)", widgets::format_db(pos_to_gain(pos))));
        if r.changed() {
            app.control(&track.automation, Some(t), target, pos, Action::UpdateSend(t, k, SendUpdate { level: Some(pos_to_gain(pos)), ..Default::default() }));
        }
        if r.drag_stopped() {
            app.act(Action::ReleaseControl { owner: Some(t), target });
        }
        let tip = if send.pre_fader { "Pre-fader (click for post-fader)" } else { "Post-fader (click for pre-fader)" };
        if widgets::chip(ui, send.pre_fader, "Pre", theme::ACCENT, &format!("Send {n} Pre-Fader"), vec2(26.0, 16.0)).on_hover_text(tip).clicked() {
            app.act(Action::UpdateSend(t, k, SendUpdate { pre_fader: Some(!send.pre_fader), ..Default::default() }));
        }
        if widgets::chip(ui, send.mute, "M", theme::MUTE, &format!("Send {n} Mute"), vec2(16.0, 16.0)).clicked() {
            app.act(Action::UpdateSend(t, k, SendUpdate { mute: Some(!send.mute), ..Default::default() }));
        }
    });
}

/// Empty send slot: send to a bus, a new bus, or a new bus with the built-in reverb.
fn add_send_button(app: &mut DawApp, ui: &mut egui::Ui, t: usize, snap: &Snapshot, w: f32) {
    let project = &snap.project;
    let (rect, resp) = ui.allocate_exact_size(vec2(w - 2.0, 20.0), Sense::click());
    let stroke = Stroke::new(1.0, if resp.hovered() { theme::TEXT_DIM } else { theme::HAIRLINE });
    ui.painter().rect_stroke(rect, 4.0, stroke, StrokeKind::Inside);
    widgets::draw_icon(ui.painter(), Icon::Plus, rect.center(), 4.0, theme::TEXT_DIM);
    widgets::label(&resp, egui::Role::Button, "Add Send");
    let resp = resp.on_hover_text("Send to a bus (e.g. a shared reverb)");
    egui::Popup::menu(&resp).show(|ui| {
        ui.set_min_width(180.0);
        for b in (0..project.tracks.len()).filter(|&b| project.can_send(t, b)) {
            if ui.button(&project.tracks[b].name).clicked() {
                app.act(Action::AddSend(t, b));
                ui.close();
            }
        }
        ui.separator();
        if ui.button("New Bus").clicked() {
            app.act(Action::SendToNewBus { track: t, send: None, reverb: false });
            ui.close();
        }
        if ui.button("New Reverb Send").clicked() {
            app.act(Action::SendToNewBus { track: t, send: None, reverb: true });
            ui.close();
        }
    });
}

/// The track's output (Stereo Out or a bus); click to route it elsewhere or to a new bus.
fn output_button(app: &mut DawApp, ui: &mut egui::Ui, t: usize, snap: &Snapshot, w: f32) {
    let project = &snap.project;
    let current = project.output_of(t);
    let name = |o: Option<usize>| o.map_or("Stereo Out".to_string(), |b| project.tracks[b].name.clone());
    let text = RichText::new(name(current)).size(11.0).color(if current.is_some() { theme::BUS } else { theme::TEXT });
    let resp = ui.add_sized([w - 2.0, 22.0], egui::Button::new(text).truncate().fill(theme::BG_CONTROL).corner_radius(4));
    let label = format!("Output: {}", name(current));
    widgets::label(&resp, egui::Role::Button, &label);
    let resp = resp.on_hover_text("Where this track goes: the master (Stereo Out) or a bus");
    egui::Popup::menu(&resp).show(|ui| {
        ui.set_min_width(180.0);
        let targets = std::iter::once(None).chain((0..project.tracks.len()).filter(|&b| project.tracks[b].bus).map(Some));
        for to in targets.filter(|&to| project.can_route(t, to)) {
            if ui.add(egui::Button::new(name(to)).selected(to == current)).clicked() {
                app.act(Action::SetOutput(t, to));
                ui.close();
            }
        }
        ui.separator();
        if ui.button("New Bus").clicked() {
            app.act(Action::AddBus(vec![t]));
            ui.close();
        }
    });
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
    // Where track strips have their sends, so the faders line up.
    widgets::section(ui, "");
    ui.allocate_exact_size(vec2(w, layout.sends_height), Sense::hover());
    ui.allocate_exact_size(vec2(w, 34.0), Sense::hover());
    let lanes = &snap.project.master_automation;
    let mut gain = opentake_engine::project::automated(lanes, AutomationTarget::Volume, snap.status.position_secs).map_or(snap.project.master_volume, pos_to_gain);
    let (changed, released) = fader_section(ui, &mut gain, snap.status.master_peak_db, "Master Volume", w);
    if changed {
        app.control(lanes, None, AutomationTarget::Volume, gain_to_pos(gain), Action::SetMasterVolume(gain));
    }
    if released {
        app.act(Action::ReleaseControl { owner: None, target: AutomationTarget::Volume });
    }
    // In the row where track strips have M/S/R/I: show the master's automation lanes.
    ui.horizontal(|ui| {
        ui.add_space((w - 30.0) / 2.0);
        let shown = snap.project.show_master_automation;
        if widgets::chip(ui, shown, "A", theme::AUTOMATION, "Show Master Automation", vec2(30.0, 20.0)).clicked() {
            app.act(Action::ShowMasterAutomation(!shown));
        }
    });
    name_plate(ui, "Stereo Out", Color32::from_gray(120), w);
}

/// dB readouts, fader and meter. Returns whether the gain changed (or the fader is held) and
/// whether it was let go.
fn fader_section(ui: &mut egui::Ui, gain: &mut f32, peak_db: [f32; 2], a11y: &str, w: f32) -> (bool, bool) {
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
        let r = widgets::vfader(ui, gain, vec2(60.0, fader_h), a11y);
        widgets::vmeter(ui, vec2(12.0, fader_h), peak_db);
        (r.changed(), r.drag_stopped())
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
        let builtins = [
            (opentake_engine::eq::NAME, opentake_engine::eq::PATH),
            (opentake_engine::dynamics::comp::NAME, opentake_engine::dynamics::comp::PATH),
            (opentake_engine::multiband::NAME, opentake_engine::multiband::PATH),
            (opentake_engine::dynamics::limiter::NAME, opentake_engine::dynamics::limiter::PATH),
            (opentake_engine::delay::NAME, opentake_engine::delay::PATH),
            (opentake_engine::reverb::NAME, opentake_engine::reverb::PATH),
            (opentake_engine::gate::NAME, opentake_engine::gate::PATH),
            (opentake_engine::deesser::NAME, opentake_engine::deesser::PATH),
            (opentake_engine::chorus::NAME, opentake_engine::chorus::PATH),
            (opentake_engine::phaser::NAME, opentake_engine::phaser::PATH),
        ];
        for (name, path) in builtins {
            if ui.button(name).clicked() {
                insert(app, path.into());
                ui.close();
            }
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
        let units: Vec<_> = snap.audio_units.iter().filter(|a| !a.instrument).collect();
        if !units.is_empty() {
            ui.separator();
            widgets::section(ui, "Audio Units");
            egui::ScrollArea::vertical().id_salt("au_effects").max_height(280.0).show(ui, |ui| {
                for a in units {
                    if ui.button(format!("{}  ·  {}", a.name, a.manufacturer)).clicked() {
                        insert(app, a.id.clone());
                        ui.close();
                    }
                }
            });
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
