//! File → Import MIDI File…: choose which parts of a Standard MIDI File become instrument
//! tracks, with which instruments, and whether to adopt its tempo map.

use std::path::PathBuf;

use eframe::egui;
use egui::{Align2, Color32, RichText};
use opentake_engine::{MidiFileImport, MidiFileSummary};

use super::{Action, DawApp, Snapshot, format_time, instrument_combo, theme, widgets};

/// State of the "Import MIDI File" dialog.
pub(crate) struct MidiImportDialog {
    pub path: PathBuf,
    pub summary: MidiFileSummary,
    pub selected: Vec<bool>,
    /// Instrument ids (as in `Snapshot::instruments`).
    pub instrument: String,
    pub drum_instrument: String,
    pub set_tempo: bool,
}

impl DawApp {
    /// Read a MIDI file and show the import dialog.
    pub(crate) fn open_midi_file_dialog(&mut self, path: PathBuf) {
        match opentake_engine::inspect_midi_file(&path) {
            Ok(summary) if summary.tracks.is_empty() => self.error(format!("{} has no notes", path.display())),
            Ok(summary) => {
                let selected = vec![true; summary.tracks.len()];
                self.midi_import = Some(MidiImportDialog { path, summary, selected, instrument: "synth".into(), drum_instrument: "drums".into(), set_tempo: true });
            }
            Err(e) => self.error(format!("{e:#}")),
        }
    }

    pub(super) fn midi_import_window(&mut self, ctx: &egui::Context, snap: &Snapshot) {
        let Some(mut dialog) = self.midi_import.take() else { return };
        let mut open = true;
        let mut import = false;
        let mut cancel = false;
        egui::Window::new("Import MIDI File")
            .pivot(Align2::CENTER_CENTER)
            .default_pos(ctx.content_rect().center())
            .open(&mut open)
            .resizable(false)
            .collapsible(false)
            .show(ctx, |ui| {
                ui.set_min_width(420.0);
                let s = &dialog.summary;
                let title = if s.name.is_empty() { dialog.path.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default() } else { s.name.clone() };
                ui.label(RichText::new(title).heading());
                let mut info = format!("{:.0} BPM · {}/{} · {}", s.tempo_bpm, s.time_signature.0, s.time_signature.1, format_time(s.duration_secs));
                if s.tempo_changes > 0 {
                    info += &format!(" · {} tempo/meter changes", s.tempo_changes);
                }
                ui.label(RichText::new(info).color(theme::TEXT_DIM));
                ui.add_space(8.0);
                widgets::section(ui, "Tracks");
                egui::ScrollArea::vertical().max_height(240.0).show(ui, |ui| {
                    egui::Grid::new("midi_tracks").num_columns(3).striped(true).spacing([14.0, 6.0]).show(ui, |ui| {
                        for (t, on) in s.tracks.iter().zip(dialog.selected.iter_mut()) {
                            ui.checkbox(on, &t.name);
                            let what = t.program.as_deref().unwrap_or("Drums");
                            ui.label(RichText::new(format!("Ch {} · {what}", t.channel)).color(theme::TEXT_DIM));
                            ui.label(RichText::new(format!("{} notes", t.notes)).color(theme::TEXT_FAINT));
                            ui.end_row();
                        }
                    });
                });
                ui.add_space(8.0);
                let has = |drums: bool| s.tracks.iter().zip(&dialog.selected).any(|(t, on)| *on && (t.kind == "drums") == drums);
                let (pitched, drums) = (has(false), has(true));
                egui::Grid::new("midi_options").num_columns(2).spacing([14.0, 8.0]).show(ui, |ui| {
                    if pitched {
                        ui.label(RichText::new("Instrument").color(theme::TEXT_DIM));
                        instrument_combo(ui, "midi_inst", &mut dialog.instrument, &snap.instruments);
                        ui.end_row();
                    }
                    if drums {
                        ui.label(RichText::new("Drum instrument").color(theme::TEXT_DIM));
                        instrument_combo(ui, "midi_drums", &mut dialog.drum_instrument, &snap.instruments);
                        ui.end_row();
                    }
                });
                ui.checkbox(&mut dialog.set_tempo, "Use the file's tempo map (tempo and time-signature changes)");
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    let button = egui::Button::new(RichText::new("Import").color(Color32::WHITE)).fill(theme::ACCENT);
                    if ui.add_enabled(pitched || drums, button).clicked() {
                        import = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
        if import {
            let tracks = dialog.selected.iter().enumerate().filter(|(_, on)| **on).map(|(i, _)| i).collect();
            let options = MidiFileImport {
                tracks: Some(tracks),
                instrument: dialog.instrument.clone(),
                drum_instrument: Some(dialog.drum_instrument.clone()),
                set_tempo: dialog.set_tempo,
                position_secs: 0.0,
            };
            self.act(Action::ImportMidiFile { path: dialog.path.clone(), options });
        } else if open && !cancel {
            self.midi_import = Some(dialog);
        }
    }

    /// File → Export MIDI File…
    pub(super) fn export_midi_file(&mut self, snap: &Snapshot) {
        let name = format!("{}.mid", snap.project.name);
        if let Some(path) = rfd::FileDialog::new().add_filter("MIDI file", &["mid"]).set_file_name(name).save_file() {
            self.act(Action::ExportMidiFile(path));
        }
    }
}
