//! File → Export Audio…: the full mix and/or stems, WAV or FLAC, rendered offline on a
//! background thread while the app keeps running.

use std::path::PathBuf;
use std::sync::mpsc;

use eframe::egui;
use egui::{RichText, vec2};
use opentake_engine::{AudioFormat, ExportOptions, ExportReport, SAMPLE_RATES};

use super::{DawApp, Snapshot, theme};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Range {
    Song,
    Cycle,
}

/// The Export Audio dialog's choices.
pub struct ExportDialog {
    pub mix: bool,
    pub stems: bool,
    pub format: AudioFormat,
    pub bit_depth: u16,
    pub sample_rate: u32,
    pub range: Range,
    pub tail_secs: f64,
}

impl Default for ExportDialog {
    fn default() -> Self {
        Self { mix: true, stems: false, format: AudioFormat::Wav, bit_depth: 24, sample_rate: 48_000, range: Range::Song, tail_secs: 1.0 }
    }
}

/// A running export (result arrives on the channel).
pub type ExportTask = mpsc::Receiver<Result<ExportReport, String>>;

fn segmented<T: PartialEq + Copy>(ui: &mut egui::Ui, current: &mut T, options: &[(T, &str)], enabled: impl Fn(T) -> bool) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        for &(value, label) in options {
            let b = egui::Button::new(label).selected(*current == value).min_size(vec2(56.0, 22.0));
            if ui.add_enabled(enabled(value), b).clicked() {
                *current = value;
            }
        }
    });
}

impl DawApp {
    /// The dialog (if open) and the running export's progress/result.
    pub(super) fn export_window(&mut self, ctx: &egui::Context, snap: &Snapshot) {
        self.poll_export();
        let Some(mut d) = self.export_dialog.take() else { return };
        let mut open = true;
        let mut start = false;
        let has_cycle = snap.project.loop_end_secs > snap.project.loop_start_secs;
        egui::Window::new("Export Audio").open(&mut open).collapsible(false).resizable(false).default_width(380.0).show(ctx, |ui| {
            egui::Grid::new("export_grid").num_columns(2).spacing([14.0, 10.0]).show(ui, |ui| {
                ui.label(RichText::new("Export").color(theme::TEXT_DIM));
                ui.horizontal(|ui| {
                    ui.checkbox(&mut d.mix, "Full mix");
                    ui.checkbox(&mut d.stems, "Stems (one file per track)");
                });
                ui.end_row();
                ui.label(RichText::new("Format").color(theme::TEXT_DIM));
                segmented(ui, &mut d.format, &[(AudioFormat::Wav, "WAV"), (AudioFormat::Flac, "FLAC")], |_| true);
                ui.end_row();
                if d.format == AudioFormat::Flac && d.bit_depth == 32 {
                    d.bit_depth = 24;
                }
                ui.label(RichText::new("Bit depth").color(theme::TEXT_DIM));
                let wav = d.format == AudioFormat::Wav;
                segmented(ui, &mut d.bit_depth, &[(16, "16-bit"), (24, "24-bit"), (32, "32 float")], |b| wav || b != 32);
                ui.end_row();
                ui.label(RichText::new("Sample rate").color(theme::TEXT_DIM));
                egui::ComboBox::from_id_salt("export_rate").selected_text(format!("{:.1} kHz", d.sample_rate as f64 / 1000.0)).show_ui(ui, |ui| {
                    for rate in SAMPLE_RATES {
                        ui.selectable_value(&mut d.sample_rate, rate, format!("{:.1} kHz", rate as f64 / 1000.0));
                    }
                });
                ui.end_row();
                ui.label(RichText::new("Range").color(theme::TEXT_DIM));
                if !has_cycle && d.range == Range::Cycle {
                    d.range = Range::Song;
                }
                segmented(ui, &mut d.range, &[(Range::Song, "Whole song"), (Range::Cycle, "Cycle")], |r| r == Range::Song || has_cycle);
                ui.end_row();
                ui.label(RichText::new("Tail").color(theme::TEXT_DIM));
                ui.add(egui::DragValue::new(&mut d.tail_secs).range(0.0..=10.0).speed(0.1).fixed_decimals(1).suffix(" s"))
                    .on_hover_text("Extra time after the end, so delay repeats aren't cut off");
                ui.end_row();
            });
            if d.bit_depth == 16 {
                ui.label(RichText::new("16-bit files get triangular dither.").size(10.5).color(theme::TEXT_FAINT));
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let busy = self.export_task.is_some();
                let label = if busy { "Exporting…" } else { "Export…" };
                if ui.add_enabled(!busy && (d.mix || d.stems), egui::Button::new(label)).clicked() {
                    start = true;
                }
            });
        });
        if start {
            if let Some(options) = self.choose_destination(&d, snap) {
                self.start_export(options);
            }
        }
        if open {
            self.export_dialog = Some(d);
        }
    }

    /// Ask where to write; stems go in a folder next to the mix (or a chosen folder).
    fn choose_destination(&mut self, d: &ExportDialog, snap: &Snapshot) -> Option<ExportOptions> {
        let ext = d.format.extension();
        let base = snap.project.name.clone();
        let start_dir = snap.project_path.as_ref().and_then(|p| p.parent().map(PathBuf::from));
        let mut dialog = rfd::FileDialog::new();
        if let Some(dir) = &start_dir {
            dialog = dialog.set_directory(dir);
        }
        let (mix_path, stems_dir) = if d.mix {
            let path = dialog.add_filter(if ext == "wav" { "WAV audio" } else { "FLAC audio" }, &[ext]).set_file_name(format!("{base}.{ext}")).save_file()?;
            let stem_name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or(base);
            let stems = d.stems.then(|| path.with_file_name(format!("{stem_name} - Stems")));
            (Some(path), stems)
        } else {
            (None, Some(dialog.pick_folder()?))
        };
        let range = match d.range {
            Range::Song => None,
            Range::Cycle => Some((snap.project.loop_start_secs, snap.project.loop_end_secs)),
        };
        Some(ExportOptions { format: d.format, bit_depth: d.bit_depth, sample_rate: d.sample_rate, mix_path, stems_dir, range, tail_secs: d.tail_secs })
    }

    /// Snapshot under the engine lock, then render and write on a background thread.
    pub(super) fn start_export(&mut self, options: ExportOptions) {
        let job = match self.engine.lock().map_err(|_| anyhow::anyhow!("engine lock poisoned")).and_then(|mut e| e.export_job(&options)) {
            Ok(job) => job,
            Err(e) => return self.error(format!("Export failed: {e:#}")),
        };
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(job.export(&options).map_err(|e| format!("{e:#}")));
        });
        self.export_task = Some(rx);
        self.info("Exporting audio…");
    }

    fn poll_export(&mut self) {
        let Some(rx) = &self.export_task else { return };
        match rx.try_recv() {
            Ok(Ok(report)) => {
                self.export_task = None;
                let clipped: usize = report.files.iter().map(|f| f.clipped_samples).sum();
                let first = report.files.first().map(|f| f.path.display().to_string()).unwrap_or_default();
                let mut msg = match report.files.len() {
                    1 => format!("Exported {first}"),
                    n => format!("Exported {n} files ({first} …)"),
                };
                if clipped > 0 {
                    msg.push_str(&format!(" — {clipped} samples clipped; lower the master or add a limiter"));
                }
                if !report.warnings.is_empty() {
                    msg.push_str(&format!(" — {}", report.warnings.join("; ")));
                }
                if clipped > 0 || !report.warnings.is_empty() { self.error(msg) } else { self.info(msg) }
            }
            Ok(Err(e)) => {
                self.export_task = None;
                self.error(format!("Export failed: {e}"));
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                self.export_task = None;
                self.error("Export failed");
            }
        }
    }
}
