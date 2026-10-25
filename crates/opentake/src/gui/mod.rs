//! Desktop GUI (egui). Shares the engine with the MCP server, so changes made by an
//! agent show up live and vice versa.
//!
//! Each frame: lock the engine briefly to take a [`Snapshot`], draw from the snapshot
//! while collecting [`Action`]s, then lock again to apply them.

mod menu;
mod mixer;
mod piano_roll;
mod tab_view;
mod theme;
mod timeline;
mod widgets;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use opentake_engine::project::{Note, Project};
use opentake_engine::{AudioConfig, DeviceList, Engine, EngineStatus, HistoryInfo, ParamView, RegionData, SlotRef, TrackUpdate, Waveform};
use opentake_vst3::ScannedPlugin;
use eframe::egui;
use egui::{Align, Align2, Color32, FontId, Layout, Rect, RichText, Sense, Stroke, StrokeKind, UiBuilder, pos2, vec2};

use crate::server::SharedEngine;
use menu::{MenuCommand, MenuState};
use timeline::{RegionKind, RegionRef};
use widgets::Icon;

/// The app icon for the window and dock/taskbar.
fn app_icon() -> egui::IconData {
    eframe::icon_data::from_png_bytes(include_bytes!("../../../../assets/logo/opentake-256.png")).unwrap_or_default()
}

pub fn run(engine: SharedEngine, mcp_url: Option<String>, settings_path: Option<PathBuf>) -> anyhow::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("OpenTake")
            .with_inner_size([1360.0, 860.0])
            .with_min_inner_size([1000.0, 600.0])
            .with_drag_and_drop(true)
            .with_icon(std::sync::Arc::new(app_icon())),
        ..Default::default()
    };
    eframe::run_native(
        "OpenTake",
        options,
        Box::new(move |cc| {
            theme::apply(&cc.egui_ctx);
            let has_tracks = !engine.lock().map(|e| e.project().tracks.is_empty()).unwrap_or(true);
            let mut app = DawApp::new(engine, mcp_url);
            app.recent = settings_path.as_deref().map(crate::settings::Settings::load).map(|s| s.existing_recent()).unwrap_or_default();
            app.settings_path = settings_path;
            if has_tracks {
                app.selected_track = Some(0);
            }
            #[cfg(target_os = "macos")]
            match menu::NativeMenu::install() {
                Ok(m) => app.native_menu = Some(m),
                Err(e) => tracing::warn!("native menu unavailable, using in-window menu: {e}"),
            }
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| anyhow::anyhow!("GUI failed: {e}"))
}

/// Everything the UI needs for one frame, copied out of the engine.
pub(crate) struct Snapshot {
    pub project: Project,
    pub project_path: Option<PathBuf>,
    pub status: EngineStatus,
    pub waveforms: HashMap<PathBuf, Arc<Waveform>>,
    pub scanned: Vec<ScannedPlugin>,
    pub scan_errors: Vec<String>,
    pub plugin_paths: Vec<PathBuf>,
    /// Instruments for new instrument tracks: (display name, id for the engine).
    pub instruments: Vec<(String, String)>,
    pub history: HistoryInfo,
    /// Parameters of plugins whose windows are open, keyed by (track, slot).
    pub params: HashMap<(usize, SlotRef), Vec<ParamView>>,
}

/// Copied regions (⌘C): each with its track offset from the top-most copied track, and
/// positions relative to the earliest one, so a paste keeps the layout.
#[derive(Clone, Debug)]
pub(crate) struct Clipboard {
    pub items: Vec<(usize, RegionData)>,
}

/// Editors in the bottom panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BottomTab {
    Mixer,
    PianoRoll,
    Tab,
}

pub(crate) enum Action {
    Play,
    Stop,
    Record,
    Seek(f64),
    SetTempo(f64),
    SetMasterVolume(f32),
    SetMetronome { enabled: Option<bool>, volume: Option<f32> },
    StartAudio(AudioConfig),
    StopAudio,
    RefreshDevices,
    NewProject,
    OpenProject(PathBuf),
    SaveProject(Option<PathBuf>),
    Undo,
    Redo,
    AddTrack,
    AddInstrumentTrack(String),
    RemoveTrack(usize),
    UpdateTrack(usize, TrackUpdate),
    AddClip { track: Option<usize>, file: PathBuf, position: f64 },
    RemoveRegions(Vec<RegionRef>),
    /// Move (or with `copy`, duplicate) regions by a time and track offset.
    MoveRegions { regions: Vec<RegionRef>, delta_secs: f64, delta_track: isize, copy: bool },
    SplitRegions { regions: Vec<RegionRef>, at: f64 },
    TrimRegion { region: RegionRef, start: Option<f64>, end: Option<f64> },
    /// Paste the clipboard with its top-most track at `track`, starting at `position`.
    Paste { track: usize, position: f64 },
    SetLoop { enabled: Option<bool>, start: Option<f64>, end: Option<f64> },
    ImportGuitarPro { path: PathBuf, options: opentake_engine::GuitarProImport },
    AddMidiRegion { track: usize, position: f64, length: f64 },
    SetNotes { track: usize, clip: usize, notes: Vec<Note> },
    LiveNote { track: usize, pitch: u8, velocity: u8 },
    ScanPlugins,
    AddPluginPath(PathBuf),
    RemovePluginPath(PathBuf),
    RestoreDefaultPluginPaths,
    InsertPlugin(usize, String),
    SetInstrument { track: usize, query: Option<String> },
    RemovePlugin(usize, SlotRef),
    SetBypass(usize, SlotRef, bool),
    SetParam(usize, SlotRef, u32, f64),
}

pub(crate) struct DawApp {
    engine: SharedEngine,
    mcp_url: Option<String>,
    pub selected_track: Option<usize>,
    /// Selected regions (click, Shift-click, or drag a rectangle on empty lane space).
    pub selection: Vec<RegionRef>,
    pub view: timeline::View,
    pub snap_mode: timeline::SnapMode,
    /// Mode restored when snapping is toggled back on.
    last_snap_mode: timeline::SnapMode,
    pub tool: timeline::Tool,
    /// Region copied with ⌘C / Copy.
    pub clipboard: Option<Clipboard>,
    show_inspector: bool,
    pub bottom: Option<BottomTab>,
    /// What the tablature view shows (notation, tab or both).
    pub tab_mode: tab_view::TabMode,
    pub piano: piano_roll::PianoRoll,
    pub open_plugins: Vec<(usize, SlotRef)>,
    devices: Option<DeviceList>,
    audio_cfg: AudioConfig,
    show_audio_settings: bool,
    show_mcp_info: bool,
    pub show_plugin_manager: bool,
    show_instrument_picker: bool,
    /// Open "Import Guitar Pro" dialog.
    pub gp_import: Option<GpImportDialog>,
    /// Where settings are saved; `None` disables persistence (tests).
    settings_path: Option<PathBuf>,
    /// Recently opened/saved projects, newest first.
    pub recent: Vec<PathBuf>,
    /// Project path seen last frame (to notice opens/saves from anywhere, incl. agents).
    seen_project_path: Option<PathBuf>,
    #[cfg(target_os = "macos")]
    native_menu: Option<menu::NativeMenu>,
    name_buf: (Option<usize>, String),
    message: Option<(String, bool, Instant)>,
    actions: Vec<Action>,
}

/// State of the "Import Guitar Pro" dialog.
pub(crate) struct GpImportDialog {
    pub path: PathBuf,
    pub summary: opentake_engine::GuitarProSummary,
    pub selected: Vec<bool>,
    /// Instrument ids (as in `Snapshot::instruments`).
    pub instrument: String,
    pub drum_instrument: String,
    pub set_tempo: bool,
}

/// "VST3: Name (Vendor)" -> "Name"
pub(crate) fn short_plugin_name(full: &str) -> &str {
    let rest = full.split_once(": ").map_or(full, |(_, r)| r);
    rest.rsplit_once(" (").map_or(rest, |(n, _)| n)
}

impl DawApp {
    fn new(engine: SharedEngine, mcp_url: Option<String>) -> Self {
        Self {
            engine,
            mcp_url,
            selected_track: None,
            selection: Vec::new(),
            view: timeline::View::default(),
            snap_mode: timeline::SnapMode::Smart,
            last_snap_mode: timeline::SnapMode::Smart,
            tool: timeline::Tool::Pointer,
            clipboard: None,
            show_inspector: true,
            bottom: None,
            tab_mode: tab_view::TabMode::default(),
            piano: piano_roll::PianoRoll::default(),
            open_plugins: Vec::new(),
            devices: None,
            audio_cfg: AudioConfig::default(),
            show_audio_settings: false,
            show_mcp_info: false,
            show_plugin_manager: false,
            show_instrument_picker: false,
            gp_import: None,
            settings_path: None,
            recent: Vec::new(),
            seen_project_path: None,
            #[cfg(target_os = "macos")]
            native_menu: None,
            name_buf: (None, String::new()),
            message: None,
            actions: Vec::new(),
        }
    }

    pub(crate) fn act(&mut self, a: Action) {
        self.actions.push(a);
    }

    fn info(&mut self, msg: impl Into<String>) {
        self.message = Some((msg.into(), false, Instant::now()));
    }

    fn error(&mut self, msg: impl Into<String>) {
        self.message = Some((msg.into(), true, Instant::now()));
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Engine> {
        // A panic while holding the lock shouldn't take the GUI down with it.
        self.engine.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Show a MIDI region in the piano roll.
    pub(crate) fn open_piano_roll(&mut self, region: RegionRef) {
        if region.kind == RegionKind::Midi {
            self.piano.open(region.track, region.index);
            self.selection = vec![region];
            self.selected_track = Some(region.track);
            self.bottom = Some(BottomTab::PianoRoll);
        }
    }

    fn toggle_bottom(&mut self, tab: BottomTab) {
        self.bottom = if self.bottom == Some(tab) { None } else { Some(tab) };
        // Opening the piano roll edits the selected MIDI region, if any.
        if self.bottom == Some(BottomTab::PianoRoll) {
            if let Some(r) = self.selection.iter().rev().copied().find(|r| r.kind == RegionKind::Midi) {
                self.piano.open(r.track, r.index);
            }
        }
    }

    fn snapshot(&mut self) -> Snapshot {
        let open = self.open_plugins.clone();
        let mut e = self.lock();
        let project = e.project().clone();
        let mut waveforms = HashMap::new();
        for clip in project.tracks.iter().flat_map(|t| &t.clips) {
            if !waveforms.contains_key(&clip.file) {
                if let Ok(w) = e.waveform(&clip.file) {
                    waveforms.insert(clip.file.clone(), w);
                }
            }
        }
        let params = open.into_iter().filter_map(|(t, s)| Some(((t, s), e.slot_params(t, s).ok()?))).collect();
        Snapshot {
            project_path: e.project_path().map(|p| p.to_path_buf()),
            status: e.status(),
            scanned: e.scanned_plugins().to_vec(),
            scan_errors: e.scan_errors().to_vec(),
            plugin_paths: e.plugin_search_paths().to_vec(),
            instruments: e.available_instruments(),
            history: e.history_info(),
            waveforms,
            params,
            project,
        }
    }

    fn apply_actions(&mut self) {
        if self.actions.is_empty() {
            return;
        }
        let actions = std::mem::take(&mut self.actions);
        let engine = self.engine.clone();
        let mut e = engine.lock().unwrap_or_else(|p| p.into_inner());
        for action in actions {
            if let Err(err) = self.apply(&mut e, action) {
                self.error(format!("{err:#}"));
            }
        }
    }

    fn apply(&mut self, e: &mut Engine, action: Action) -> anyhow::Result<()> {
        match action {
            Action::Play => e.play()?,
            Action::Stop => {
                let clips = e.stop()?;
                if !clips.is_empty() {
                    self.info(format!("Recorded {} region(s)", clips.len()));
                }
            }
            Action::Record => {
                let files = e.record_audio()?;
                self.info(format!("Recording {} track(s)", files.len()));
            }
            Action::Seek(t) => e.seek(t)?,
            Action::SetTempo(bpm) => e.set_tempo(bpm, None)?,
            Action::SetMasterVolume(g) => e.set_master_volume(g)?,
            Action::SetMetronome { enabled, volume } => e.set_metronome(enabled, volume)?,
            Action::StartAudio(cfg) => {
                let s = e.start_audio(&cfg)?;
                self.info(format!(
                    "Audio running at {} Hz — out: {}, in: {}",
                    s.sample_rate,
                    s.output_device.unwrap_or_default(),
                    s.input_device.unwrap_or_else(|| "none".into())
                ));
            }
            Action::StopAudio => e.stop_audio()?,
            Action::RefreshDevices => self.devices = Some(e.list_devices()?),
            Action::NewProject => {
                e.new_project("Untitled")?;
                self.reset_selection(Some(0));
            }
            Action::OpenProject(path) => {
                let warnings = e.load_project(&path)?;
                // Never leave the user with an empty arrangement.
                if e.project().tracks.is_empty() {
                    e.add_track("Audio 1")?;
                }
                self.reset_selection(Some(0));
                if warnings.is_empty() {
                    self.info(format!("Opened {}", path.display()));
                } else {
                    self.error(warnings.join("\n"));
                }
            }
            Action::SaveProject(path) => {
                let saved = e.save_project(path.as_deref())?;
                let p = e.project();
                let rpp = saved.extension().is_some_and(|x| x.eq_ignore_ascii_case("rpp"));
                let has_params = p.tracks.iter().flat_map(|t| t.instrument.iter().chain(&t.plugins)).any(|s| !s.params.is_empty());
                let has_midi = p.tracks.iter().any(|t| !t.midi_clips.is_empty());
                if rpp && (has_params || has_midi) {
                    self.error(format!(
                        "Saved {} — note: .rpp files don't store {}; save as .json to keep everything",
                        saved.display(),
                        if has_midi { "MIDI regions or plug-in parameter values" } else { "plug-in parameter values" }
                    ));
                } else {
                    self.info(format!("Saved {}", saved.display()));
                }
            }
            Action::Undo => match e.undo()? {
                Some(label) => {
                    self.validate_selection(e);
                    self.info(format!("Undo {label}"));
                }
                None => self.info("Nothing to undo"),
            },
            Action::Redo => match e.redo()? {
                Some(label) => {
                    self.validate_selection(e);
                    self.info(format!("Redo {label}"));
                }
                None => self.info("Nothing to redo"),
            },
            Action::AddTrack => {
                let n = e.project().tracks.len() + 1;
                self.selected_track = Some(e.add_track(&format!("Audio {n}"))?);
            }
            Action::AddInstrumentTrack(instrument) => {
                let n = e.project().tracks.len() + 1;
                let t = e.add_instrument_track(&format!("Inst {n}"), &instrument)?;
                self.selected_track = Some(t);
            }
            Action::RemoveTrack(t) => {
                e.remove_track(t)?;
                self.reset_selection(None);
            }
            Action::UpdateTrack(t, u) => {
                e.update_track(t, u)?;
            }
            Action::AddClip { track, file, position } => {
                let track = match track {
                    Some(t) => t,
                    None => {
                        let name = file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                        e.add_track(&name)?
                    }
                };
                e.add_clip(track, &file, position)?;
                self.selected_track = Some(track);
            }
            Action::RemoveRegions(regions) => {
                e.remove_regions(&regions)?;
                self.selection.clear();
            }
            Action::MoveRegions { regions, delta_secs, delta_track, copy } => {
                let moved = e.move_regions(&regions, delta_secs, delta_track, copy)?;
                // Keep the piano roll on its region if that region moved.
                if !copy {
                    if let Some(i) = regions.iter().position(|r| r.kind == RegionKind::Midi && self.piano.region == Some((r.track, r.index))) {
                        self.piano.open(moved[i].track, moved[i].index);
                    }
                }
                self.selection = moved;
            }
            Action::SplitRegions { regions, at } => {
                e.split_regions(&regions, at)?;
                // Select the right-hand parts, like Logic. Splitting in index order shifts
                // later regions on the same track by the number of splits before them.
                let mut sorted = regions.clone();
                sorted.sort();
                self.selection = sorted
                    .iter()
                    .enumerate()
                    .map(|(k, r)| {
                        let before = sorted[..k].iter().filter(|o| o.track == r.track && o.kind == r.kind).count();
                        RegionRef { index: r.index + before + 1, ..*r }
                    })
                    .filter(|r| timeline::region_view(e.project(), *r).is_some())
                    .collect();
            }
            Action::TrimRegion { region: r, start, end } => match r.kind {
                RegionKind::Audio => drop(e.trim_clip(r.track, r.index, start, end)?),
                RegionKind::Midi => {
                    if let Some(end) = end {
                        let pos = e.project().tracks[r.track].midi_clips[r.index].position_secs;
                        let u = opentake_engine::MidiClipUpdate { length_secs: Some(end - pos), ..Default::default() };
                        e.update_midi_clip(r.track, r.index, u)?;
                    }
                }
            },
            Action::Paste { track, position } => {
                if let Some(clip) = self.clipboard.clone() {
                    let last = e.project().tracks.len().saturating_sub(1);
                    let items = clip
                        .items
                        .into_iter()
                        .map(|(dt, data)| {
                            let target = (track + dt).min(last);
                            let placed = match data {
                                RegionData::Audio(c) => RegionData::Audio(opentake_engine::project::Clip { position_secs: position + c.position_secs, ..c }),
                                RegionData::Midi(c) => RegionData::Midi(opentake_engine::project::MidiClip { position_secs: position + c.position_secs, ..c }),
                            };
                            (target, placed)
                        })
                        .collect();
                    self.selection = e.insert_regions(items)?;
                    self.selected_track = Some(track);
                }
            }
            Action::SetLoop { enabled, start, end } => e.set_loop(enabled, start, end)?,
            Action::ImportGuitarPro { path, options } => {
                let created = e.import_guitar_pro(&path, &options)?;
                self.selection.clear();
                // Show the tab right away, starting with the first imported track that has one.
                let tabbed: Vec<usize> = tab_view::tab_tracks(e.project()).into_iter().filter(|t| created.contains(t)).collect();
                self.selected_track = tabbed.first().or(created.first()).copied();
                if !tabbed.is_empty() {
                    self.bottom = Some(BottomTab::Tab);
                }
                self.info(format!("Imported {} track(s) from {}", created.len(), path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()));
            }
            Action::AddMidiRegion { track, position, length } => {
                let i = e.add_midi_clip(track, position, length, None)?;
                self.open_piano_roll(RegionRef::midi(track, i));
            }
            Action::SetNotes { track, clip, notes } => e.set_midi_notes(track, clip, notes)?,
            Action::LiveNote { track, pitch, velocity } => e.live_note(track, pitch, velocity)?,
            Action::ScanPlugins => {
                let (plugins, errors) = e.scan_plugins();
                if errors.is_empty() {
                    self.info(format!("Found {} plugin(s)", plugins.len()));
                } else {
                    self.error(format!("Found {} plugin(s); errors:\n{}", plugins.len(), errors.join("\n")));
                }
            }
            Action::AddPluginPath(p) => {
                e.add_plugin_search_path(p);
                self.after_plugin_paths_changed(e, false);
            }
            Action::RemovePluginPath(p) => {
                e.remove_plugin_search_path(&p);
                self.after_plugin_paths_changed(e, false);
            }
            Action::RestoreDefaultPluginPaths => {
                e.set_plugin_search_paths(opentake_vst3::scan::default_search_paths());
                self.after_plugin_paths_changed(e, true);
            }
            Action::InsertPlugin(t, query) => {
                let slot = e.insert_plugin(t, &query, None)?;
                self.open_plugins.push((t, SlotRef::Effect(slot)));
            }
            Action::SetInstrument { track, query } => {
                e.set_instrument(track, query.as_deref())?;
                self.open_plugins.retain(|&(t, s)| !(t == track && s == SlotRef::Instrument));
            }
            Action::RemovePlugin(t, which) => {
                match which {
                    SlotRef::Instrument => drop(e.set_instrument(t, None)?),
                    SlotRef::Effect(s) => drop(e.remove_plugin(t, s)?),
                }
                // Slot indices shift; simplest is to close this track's windows.
                self.open_plugins.retain(|&(pt, _)| pt != t);
            }
            Action::SetBypass(t, s, b) => e.set_slot_bypass(t, s, b)?,
            Action::SetParam(t, s, id, v) => {
                e.set_slot_param(t, s, id, v)?;
            }
        }
        Ok(())
    }

    fn reset_selection(&mut self, track: Option<usize>) {
        self.selected_track = track;
        self.selection.clear();
        self.open_plugins.clear();
        self.piano.region = None;
    }

    /// After undo/redo, drop selections that no longer point at anything.
    fn validate_selection(&mut self, e: &Engine) {
        let p = e.project();
        if self.selected_track.is_some_and(|t| t >= p.tracks.len()) {
            self.selected_track = None;
        }
        self.selection.retain(|r| timeline::region_view(p, *r).is_some());
        self.open_plugins.retain(|&(t, s)| {
            p.tracks.get(t).is_some_and(|tr| match s {
                SlotRef::Instrument => tr.instrument.is_some(),
                SlotRef::Effect(i) => i < tr.plugins.len(),
            })
        });
    }

    /// Note a newly opened/saved project at the top of the recent list (and persist it).
    fn track_recent(&mut self, snap: &Snapshot) {
        if snap.project_path == self.seen_project_path {
            return;
        }
        self.seen_project_path = snap.project_path.clone();
        if let Some(path) = &snap.project_path {
            let mut list = crate::settings::Settings { recent_projects: std::mem::take(&mut self.recent), ..Default::default() };
            list.add_recent(path);
            self.recent = list.recent_projects;
            self.save_recent();
        }
    }

    fn save_recent(&mut self) {
        if let Some(settings_path) = &self.settings_path {
            let mut settings = crate::settings::Settings::load(settings_path);
            settings.recent_projects = self.recent.clone();
            if let Err(e) = settings.save(settings_path) {
                self.error(format!("Could not save settings: {e:#}"));
            }
        }
    }

    /// Menu labels for the recent projects: "song.json — Music/Songs".
    fn recent_labels(&self) -> Vec<String> {
        self.recent
            .iter()
            .map(|p| {
                let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let parent: Vec<String> = p.parent().map(|d| d.iter().rev().take(2).map(|c| c.to_string_lossy().into_owned()).collect()).unwrap_or_default();
                let folder = parent.into_iter().rev().collect::<Vec<_>>().join("/");
                if folder.is_empty() { name } else { format!("{name} — {folder}") }
            })
            .collect()
    }

    /// Persist the scan folders and rescan. `defaults` stores "use platform defaults".
    fn after_plugin_paths_changed(&mut self, e: &mut Engine, defaults: bool) {
        if let Some(path) = &self.settings_path {
            let mut settings = crate::settings::Settings::load(path);
            settings.plugin_paths = (!defaults).then(|| e.plugin_search_paths().to_vec());
            if let Err(err) = settings.save(path) {
                self.error(format!("Could not save settings: {err:#}"));
                return;
            }
        }
        let (plugins, errors) = e.scan_plugins();
        let msg = format!("Found {} plug-in(s) in {} folder(s)", plugins.len(), e.plugin_search_paths().len());
        if errors.is_empty() { self.info(msg) } else { self.error(format!("{msg}; {} failed to load", errors.len())) }
    }

    fn handle_shortcuts(&mut self, ui: &egui::Ui, snap: &Snapshot) {
        if ui.egui_wants_keyboard_input() {
            return;
        }
        let plain = |i: &egui::InputState, k: egui::Key| i.key_pressed(k) && !i.modifiers.any();
        let (space, rec, home, inspector, metronome, cycle, mixer, piano, tab, delete, escape, zoom_fit) = ui.input(|i| {
            (
                i.key_pressed(egui::Key::Space),
                plain(i, egui::Key::R),
                i.key_pressed(egui::Key::Home),
                plain(i, egui::Key::I),
                plain(i, egui::Key::K),
                plain(i, egui::Key::C),
                plain(i, egui::Key::X),
                plain(i, egui::Key::P),
                plain(i, egui::Key::T),
                (i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace)).then_some(i.modifiers.command),
                i.key_pressed(egui::Key::Escape),
                plain(i, egui::Key::Z),
            )
        });
        if zoom_fit {
            self.zoom_to_fit(snap);
        }
        match delete {
            // ⌘⌫ deletes the selected track (as in Logic).
            Some(true) => {
                if let Some(t) = self.selected_track {
                    self.act(Action::RemoveTrack(t));
                }
            }
            Some(false) => self.delete_selection(snap),
            None => {}
        }
        if escape {
            self.tool = timeline::Tool::Pointer;
            self.selection.clear();
            self.piano.selected.clear();
        }
        if space {
            self.act(if snap.status.playing { Action::Stop } else { Action::Play });
        }
        if rec {
            self.act(if snap.status.recording { Action::Stop } else { Action::Record });
        }
        if home {
            self.act(Action::Seek(0.0));
        }
        if inspector {
            self.show_inspector = !self.show_inspector;
        }
        if metronome {
            self.act(Action::SetMetronome { enabled: Some(!snap.project.metronome_enabled), volume: None });
        }
        if cycle {
            self.toggle_cycle(snap);
        }
        if mixer {
            self.toggle_bottom(BottomTab::Mixer);
        }
        if piano {
            self.toggle_bottom(BottomTab::PianoRoll);
        }
        if tab {
            self.toggle_bottom(BottomTab::Tab);
        }
    }

    /// Delete: selected notes in the open piano roll take priority over the selected region.
    fn delete_selection(&mut self, snap: &Snapshot) {
        if self.bottom == Some(BottomTab::PianoRoll) && !self.piano.selected.is_empty() {
            if let Some((t, c)) = self.piano.region {
                if let Some(clip) = snap.project.tracks.get(t).and_then(|tr| tr.midi_clips.get(c)) {
                    let notes = clip.notes.iter().filter(|n| !self.piano.selected.contains(n)).copied().collect();
                    self.piano.selected.clear();
                    self.act(Action::SetNotes { track: t, clip: c, notes });
                    return;
                }
            }
        }
        if !self.selection.is_empty() {
            self.act(Action::RemoveRegions(self.selection.clone()));
        } else if let Some(t) = self.selected_track.filter(|_| self.bottom != Some(BottomTab::PianoRoll)) {
            // Nothing else selected: ⌫ deletes the selected track (not while editing notes).
            self.act(Action::RemoveTrack(t));
        }
    }

    /// Zoom the shared timeline to the selected regions, or to the whole song.
    pub(crate) fn zoom_to_fit(&mut self, snap: &Snapshot) {
        let views: Vec<_> = if self.selection.is_empty() {
            snap.project.tracks.iter().enumerate().flat_map(|(i, t)| timeline::regions_of(t, i)).collect()
        } else {
            self.selection.iter().filter_map(|&r| timeline::region_view(&snap.project, r)).collect()
        };
        let from = views.iter().map(|v| v.position).fold(f64::INFINITY, f64::min);
        let to = views.iter().map(|v| v.position + v.length).fold(0.0, f64::max);
        if from.is_finite() {
            self.view.zoom_to_fit(from, to);
        } else {
            self.view.zoom_to_fit(0.0, 8.0 * timeline::bar_secs(&snap.project));
        }
    }

    /// Read a Guitar Pro file and show the import dialog.
    pub(crate) fn open_guitar_pro_dialog(&mut self, path: PathBuf) {
        match opentake_engine::inspect_guitar_pro(&path) {
            Ok(summary) => {
                let selected = summary.tracks.iter().map(|t| t.notes > 0).collect();
                self.gp_import = Some(GpImportDialog {
                    path,
                    summary,
                    selected,
                    instrument: "synth".into(),
                    drum_instrument: "drums".into(),
                    set_tempo: true,
                });
            }
            Err(e) => self.error(format!("{e:#}")),
        }
    }

    fn guitar_pro_window(&mut self, ctx: &egui::Context, snap: &Snapshot) {
        let Some(mut dialog) = self.gp_import.take() else { return };
        let mut open = true;
        let mut import = false;
        let mut cancel = false;
        egui::Window::new("Import Guitar Pro")
            .pivot(Align2::CENTER_CENTER)
            .default_pos(ctx.content_rect().center())
            .open(&mut open)
            .resizable(false)
            .collapsible(false)
            .show(ctx, |ui| {
                ui.set_min_width(420.0);
                let s = &dialog.summary;
                let title = if s.title.is_empty() { dialog.path.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default() } else { s.title.clone() };
                ui.label(RichText::new(title).heading());
                let mut info = format!("{:.0} BPM · {}/{} · {}", s.tempo_bpm, s.time_signature.0, s.time_signature.1, format_time(s.duration_secs));
                if !s.artist.is_empty() {
                    info = format!("{} · {info}", s.artist);
                }
                ui.label(RichText::new(info).color(theme::TEXT_DIM));
                ui.add_space(8.0);
                widgets::section(ui, "Tracks");
                egui::ScrollArea::vertical().max_height(240.0).show(ui, |ui| {
                    egui::Grid::new("gp_tracks").num_columns(3).striped(true).spacing([14.0, 6.0]).show(ui, |ui| {
                        for (t, on) in s.tracks.iter().zip(dialog.selected.iter_mut()) {
                            ui.checkbox(on, &t.name);
                            ui.label(RichText::new(&t.kind).color(theme::TEXT_DIM));
                            ui.label(RichText::new(format!("{} notes", t.notes)).color(theme::TEXT_FAINT));
                            ui.end_row();
                        }
                    });
                });
                ui.add_space(8.0);
                let has_drums = s.tracks.iter().zip(&dialog.selected).any(|(t, on)| *on && t.kind == "drums");
                egui::Grid::new("gp_options").num_columns(2).spacing([14.0, 8.0]).show(ui, |ui| {
                    ui.label(RichText::new("Instrument").color(theme::TEXT_DIM));
                    instrument_combo(ui, "gp_inst", &mut dialog.instrument, &snap.instruments);
                    ui.end_row();
                    if has_drums {
                        ui.label(RichText::new("Drum instrument").color(theme::TEXT_DIM));
                        instrument_combo(ui, "gp_drums", &mut dialog.drum_instrument, &snap.instruments);
                        ui.end_row();
                    }
                });
                ui.checkbox(&mut dialog.set_tempo, "Use the song's tempo and time signature");
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    let any = dialog.selected.iter().any(|&on| on);
                    let button = egui::Button::new(RichText::new("Import").color(Color32::WHITE)).fill(theme::ACCENT);
                    if ui.add_enabled(any, button).clicked() {
                        import = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
        if import {
            let tracks = dialog.selected.iter().enumerate().filter(|(_, on)| **on).map(|(i, _)| i).collect();
            let options = opentake_engine::GuitarProImport {
                tracks: Some(tracks),
                instrument: dialog.instrument.clone(),
                drum_instrument: Some(dialog.drum_instrument.clone()),
                set_tempo: dialog.set_tempo,
                position_secs: 0.0,
            };
            self.act(Action::ImportGuitarPro { path: dialog.path.clone(), options });
        } else if open && !cancel {
            self.gp_import = Some(dialog);
        }
    }

    fn handle_dropped_files(&mut self, ui: &egui::Ui, snap: &Snapshot) {
        let (files, pos) = ui.input(|i| (i.raw.dropped_files.clone(), i.pointer.hover_pos()));
        let mut position = snap.status.position_secs;
        let mut track = None;
        if let Some(p) = pos {
            track = self.view.track_at(p.y);
            if let Some(t) = self.view.time_at(p.x) {
                position = t;
            }
        }
        for f in files.iter().map(|f| f.path().to_path_buf()) {
            if opentake_project::guitarpro::is_guitar_pro_path(&f) {
                self.open_guitar_pro_dialog(f);
            } else {
                self.act(Action::AddClip { track, file: f, position });
            }
        }
    }

    fn save_as(&mut self, snap: &Snapshot) {
        let name = format!("{}.json", snap.project.name);
        if let Some(p) = rfd::FileDialog::new()
            .add_filter("OpenTake project", &["json"])
            .add_filter("Reaper project", &["rpp"])
            .set_file_name(name)
            .save_file()
        {
            self.act(Action::SaveProject(Some(p)));
        }
    }

    // ------------------------------------------------------------------ control bar

    fn control_bar(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        let full = ui.max_rect();
        ui.scope_builder(UiBuilder::new().max_rect(full).layout(Layout::left_to_right(Align::Center)), |ui| {
            self.left_cluster(ui);
        });
        // The small master meter only fits next to the centred transport on wide windows.
        let show_meter = full.width() >= 1500.0;
        ui.scope_builder(UiBuilder::new().max_rect(full).layout(Layout::right_to_left(Align::Center)), |ui| {
            self.right_cluster(ui, snap, show_meter);
        });
        let center = Rect::from_center_size(full.center(), vec2(8.0 * TRANSPORT_SEG_W + 20.0 + LCD_W, full.height()));
        ui.scope_builder(UiBuilder::new().max_rect(center).layout(Layout::left_to_right(Align::Center)), |ui| {
            self.transport(ui, snap);
        });
    }

    fn left_cluster(&mut self, ui: &mut egui::Ui) {
        let r = widgets::icon_button(ui, Icon::Inspector, self.show_inspector, theme::ACCENT, "Inspector (I)", vec2(30.0, 26.0));
        if r.clicked() {
            self.show_inspector = !self.show_inspector;
        }
    }

    fn has_native_menu(&self) -> bool {
        #[cfg(target_os = "macos")]
        return self.native_menu.is_some();
        #[cfg(not(target_os = "macos"))]
        return false;
    }

    fn menu_state(&self, snap: &Snapshot) -> MenuState {
        MenuState {
            has_selected_track: self.selected_track.is_some(),
            has_selected_region: !self.selection.is_empty(),
            cycle_on: snap.project.loop_enabled,
            has_clipboard: self.clipboard.is_some(),
            inspector_visible: self.show_inspector,
            mixer_visible: self.bottom == Some(BottomTab::Mixer),
            tab_visible: self.bottom == Some(BottomTab::Tab),
            piano_roll_visible: self.bottom == Some(BottomTab::PianoRoll),
            snap_on: self.snap_mode != timeline::SnapMode::Off,
            metronome_on: snap.project.metronome_enabled,
            audio_running: snap.status.audio_running,
            playing: snap.status.playing,
            undo: snap.history.undo.clone(),
            redo: snap.history.redo.clone(),
            recent: self.recent_labels(),
        }
    }

    /// Commands chosen from the native menu since last frame (none without one).
    fn native_menu_commands(&mut self, snap: &Snapshot) -> Vec<MenuCommand> {
        #[cfg(target_os = "macos")]
        {
            let state = self.menu_state(snap);
            if let Some(m) = self.native_menu.as_mut() {
                m.update(state);
                return m.poll();
            }
        }
        let _ = snap;
        Vec::new()
    }

    /// ⌘C / ⌘V for regions. Not native menu accelerators, so text fields keep their own
    /// copy/paste; we only react when no text field has keyboard focus. egui reports these
    /// as Copy/Paste events (or as plain key presses when the clipboard holds no text).
    fn clipboard_shortcuts(&mut self, ui: &egui::Ui) -> Vec<MenuCommand> {
        if ui.egui_wants_keyboard_input() {
            return Vec::new();
        }
        ui.input(|i| {
            let key = |k| i.events.iter().any(|e| matches!(e, egui::Event::Key { key, pressed: true, modifiers, .. } if *key == k && modifiers.command));
            let copy = key(egui::Key::C) || i.events.iter().any(|e| matches!(e, egui::Event::Copy));
            let paste = key(egui::Key::V) || i.events.iter().any(|e| matches!(e, egui::Event::Paste(_)));
            [(copy, MenuCommand::CopyRegion), (paste, MenuCommand::PasteRegion)].into_iter().filter(|(on, _)| *on).map(|(_, c)| c).collect()
        })
    }

    /// Keyboard equivalents for the in-window menu (native menus handle their own).
    fn menu_shortcuts(&mut self, ui: &egui::Ui) -> Vec<MenuCommand> {
        use egui::{Key, KeyboardShortcut, Modifiers};
        let typing = ui.egui_wants_keyboard_input();
        let table = [
            (Modifiers::COMMAND, Key::N, MenuCommand::NewProject),
            (Modifiers::COMMAND, Key::O, MenuCommand::Open),
            (Modifiers::COMMAND | Modifiers::SHIFT, Key::S, MenuCommand::SaveAs),
            (Modifiers::COMMAND, Key::S, MenuCommand::Save),
            (Modifiers::COMMAND | Modifiers::SHIFT, Key::I, MenuCommand::ImportAudio),
            (Modifiers::COMMAND | Modifiers::ALT, Key::N, MenuCommand::NewTrack),
            (Modifiers::COMMAND, Key::Comma, MenuCommand::AudioSettings),
            (Modifiers::COMMAND, Key::T, MenuCommand::SplitAtPlayhead),
            (Modifiers::COMMAND, Key::D, MenuCommand::DuplicateRegion),
            (Modifiers::COMMAND, Key::G, MenuCommand::ToggleSnap),
            (Modifiers::COMMAND, Key::ArrowRight, MenuCommand::ZoomIn),
            (Modifiers::COMMAND, Key::ArrowLeft, MenuCommand::ZoomOut),
            (Modifiers::COMMAND | Modifiers::SHIFT, Key::Z, MenuCommand::Redo),
            (Modifiers::COMMAND, Key::Z, MenuCommand::Undo),
        ];
        ui.input_mut(|i| {
            table
                .iter()
                // Text fields keep ⌘Z for their own undo.
                .filter(|(_, k, _)| !(typing && *k == Key::Z))
                .filter(|(m, k, _)| i.consume_shortcut(&KeyboardShortcut::new(*m, *k)))
                .map(|(_, _, c)| *c)
                .collect()
        })
    }

    /// Copy the selected regions to the clipboard (layout preserved).
    pub(crate) fn copy_selection(&mut self, project: &Project) {
        let views: Vec<_> = self.selection.iter().filter_map(|&r| timeline::region_view(project, r)).collect();
        let Some(top) = views.iter().map(|v| v.r.track).min() else { return };
        let start = views.iter().map(|v| v.position).fold(f64::INFINITY, f64::min);
        let items = views
            .iter()
            .map(|v| {
                let data = match v.to_data() {
                    RegionData::Audio(c) => RegionData::Audio(opentake_engine::project::Clip { position_secs: c.position_secs - start, ..c }),
                    RegionData::Midi(c) => RegionData::Midi(opentake_engine::project::MidiClip { position_secs: c.position_secs - start, ..c }),
                };
                (v.r.track - top, data)
            })
            .collect();
        self.clipboard = Some(Clipboard { items });
    }

    /// Duplicate the selection right after its end (⌘D).
    pub(crate) fn duplicate_selection(&mut self, project: &Project) {
        let views: Vec<_> = self.selection.iter().filter_map(|&r| timeline::region_view(project, r)).collect();
        if views.is_empty() {
            return;
        }
        let start = views.iter().map(|v| v.position).fold(f64::INFINITY, f64::min);
        let end = views.iter().map(|v| v.end()).fold(0.0, f64::max);
        let regions = views.iter().map(|v| v.r).collect();
        self.act(Action::MoveRegions { regions, delta_secs: end - start, delta_track: 0, copy: true });
    }

    /// Cycle on/off; turning it on without a range creates a 4-bar range at the playhead.
    fn toggle_cycle(&mut self, snap: &Snapshot) {
        let p = &snap.project;
        if !p.loop_enabled && p.loop_end_secs <= p.loop_start_secs {
            let bar = timeline::bar_secs(p);
            let start = (snap.status.position_secs / bar).floor() * bar;
            self.act(Action::SetLoop { enabled: Some(true), start: Some(start), end: Some(start + 4.0 * bar) });
        } else {
            self.act(Action::SetLoop { enabled: Some(!p.loop_enabled), start: None, end: None });
        }
    }

    fn run_command(&mut self, cmd: MenuCommand, ui: &egui::Ui, snap: &Snapshot) {
        match cmd {
            MenuCommand::NewProject => self.act(Action::NewProject),
            MenuCommand::Open => {
                if let Some(p) = rfd::FileDialog::new().add_filter("Project", &["json", "rpp"]).pick_file() {
                    self.act(Action::OpenProject(p));
                }
            }
            MenuCommand::RecentMenu => {}
            MenuCommand::OpenRecent(i) => match self.recent.get(i as usize).cloned() {
                Some(path) if path.exists() => self.act(Action::OpenProject(path)),
                Some(path) => {
                    self.error(format!("{} no longer exists", path.display()));
                    self.recent.retain(|p| *p != path);
                    self.save_recent();
                }
                None => {}
            },
            MenuCommand::ClearRecent => {
                self.recent.clear();
                self.save_recent();
            }
            MenuCommand::Save => match &snap.project_path {
                Some(_) => self.act(Action::SaveProject(None)),
                None => self.save_as(snap),
            },
            MenuCommand::SaveAs => self.save_as(snap),
            MenuCommand::ImportAudio => {
                if let Some(files) = rfd::FileDialog::new().add_filter("WAV audio", &["wav"]).pick_files() {
                    for file in files {
                        self.act(Action::AddClip { track: self.selected_track, file, position: snap.status.position_secs });
                    }
                }
            }
            MenuCommand::ImportGuitarPro => {
                if let Some(path) = rfd::FileDialog::new().add_filter("Guitar Pro 7/8", &["gp", "gpif"]).pick_file() {
                    self.open_guitar_pro_dialog(path);
                }
            }
            MenuCommand::Undo | MenuCommand::Redo => {
                if ui.egui_wants_keyboard_input() {
                    // A text field has focus: let it undo its own typing instead.
                    let shift = cmd == MenuCommand::Redo;
                    let modifiers = egui::Modifiers { shift, ..egui::Modifiers::COMMAND };
                    ui.ctx().input_mut(|i| {
                        i.events.push(egui::Event::Key { key: egui::Key::Z, physical_key: None, pressed: true, repeat: false, modifiers })
                    });
                } else {
                    self.act(if cmd == MenuCommand::Undo { Action::Undo } else { Action::Redo });
                }
            }
            MenuCommand::SplitAtPlayhead => self.split_at_playhead(snap),
            MenuCommand::CopyRegion => self.copy_selection(&snap.project),
            MenuCommand::PasteRegion => {
                if self.clipboard.is_some() {
                    let track = self.selected_track.or(self.selection.first().map(|r| r.track)).unwrap_or(0);
                    if track < snap.project.tracks.len() {
                        self.act(Action::Paste { track, position: snap.status.position_secs });
                    }
                }
            }
            MenuCommand::DuplicateRegion => self.duplicate_selection(&snap.project),
            MenuCommand::ToggleCycle => self.toggle_cycle(snap),
            MenuCommand::DeleteRegion => self.delete_selection(snap),
            MenuCommand::ToggleSnap => {
                if self.snap_mode == timeline::SnapMode::Off {
                    self.snap_mode = self.last_snap_mode;
                } else {
                    self.last_snap_mode = self.snap_mode;
                    self.snap_mode = timeline::SnapMode::Off;
                }
            }
            MenuCommand::NewTrack => self.act(Action::AddTrack),
            MenuCommand::NewInstrumentTrack => self.show_instrument_picker = true,
            MenuCommand::DeleteTrack => {
                if let Some(t) = self.selected_track {
                    self.act(Action::RemoveTrack(t));
                }
            }
            MenuCommand::PlayStop => self.act(if snap.status.playing { Action::Stop } else { Action::Play }),
            MenuCommand::Record => self.act(if snap.status.recording { Action::Stop } else { Action::Record }),
            MenuCommand::GoToStart => self.act(Action::Seek(0.0)),
            MenuCommand::ToggleMetronome => {
                self.act(Action::SetMetronome { enabled: Some(!snap.project.metronome_enabled), volume: None })
            }
            MenuCommand::ToggleInspector => self.show_inspector = !self.show_inspector,
            MenuCommand::ToggleMixer => self.toggle_bottom(BottomTab::Mixer),
            MenuCommand::TogglePianoRoll => self.toggle_bottom(BottomTab::PianoRoll),
            MenuCommand::ToggleTab => self.toggle_bottom(BottomTab::Tab),
            MenuCommand::ZoomIn => self.view.zoom_by(timeline::ZOOM_STEP, snap.status.position_secs),
            MenuCommand::ZoomOut => self.view.zoom_by(1.0 / timeline::ZOOM_STEP, snap.status.position_secs),
            MenuCommand::ZoomToFit => self.zoom_to_fit(snap),
            MenuCommand::AudioSettings => {
                self.show_audio_settings = true;
                self.act(Action::RefreshDevices);
            }
            MenuCommand::ToggleAudioEngine => {
                self.act(if snap.status.audio_running { Action::StopAudio } else { Action::StartAudio(self.audio_cfg.clone()) });
            }
            MenuCommand::RescanPlugins => self.act(Action::ScanPlugins),
            MenuCommand::PluginManager => self.show_plugin_manager = true,
            MenuCommand::McpInfo => self.show_mcp_info = true,
        }
    }

    /// Split the selected regions at the playhead (Logic's ⌘T). Without a selection,
    /// split every region under the playhead on the selected track, or on all tracks.
    fn split_at_playhead(&mut self, snap: &Snapshot) {
        let at = snap.status.position_secs;
        let inside = |v: &timeline::RegionView| at > v.position + 0.01 && at < v.end() - 0.01;
        let candidates: Vec<RegionRef> = if self.selection.is_empty() {
            snap.project
                .tracks
                .iter()
                .enumerate()
                .filter(|(t, _)| self.selected_track.is_none_or(|sel| sel == *t))
                .flat_map(|(t, tr)| timeline::regions_of(tr, t).map(|v| v.r).collect::<Vec<_>>())
                .collect()
        } else {
            self.selection.clone()
        };
        let regions: Vec<RegionRef> =
            candidates.into_iter().filter(|&r| timeline::region_view(&snap.project, r).is_some_and(|v| inside(&v))).collect();
        if regions.is_empty() {
            self.error("No region under the playhead to split");
            return;
        }
        self.act(Action::SplitRegions { regions, at });
    }

    fn instrument_picker_window(&mut self, ctx: &egui::Context, snap: &Snapshot) {
        let mut open = self.show_instrument_picker;
        egui::Window::new("New Instrument Track")
            .pivot(Align2::CENTER_CENTER)
            .default_pos(ctx.content_rect().center())
            .open(&mut open)
            .resizable(false)
            .collapsible(false)
            .show(ctx, |ui| {
                ui.set_min_width(320.0);
                ui.label(RichText::new("Choose an instrument:").color(theme::TEXT_DIM));
                ui.add_space(4.0);
                for (name, id) in &snap.instruments {
                    if ui.add_sized([ui.available_width(), 26.0], egui::Button::new(name.as_str())).clicked() {
                        self.act(Action::AddInstrumentTrack(id.clone()));
                        self.show_instrument_picker = false;
                    }
                }
                if snap.instruments.len() == 1 {
                    ui.add_space(4.0);
                    ui.label(RichText::new("No VST3 instruments found — add folders in the Plug-in Manager.").size(10.5).color(theme::TEXT_FAINT));
                }
            });
        self.show_instrument_picker &= open;
    }

    fn plugin_manager_window(&mut self, ctx: &egui::Context, snap: &Snapshot) {
        let mut open = self.show_plugin_manager;
        egui::Window::new("Plug-in Manager")
            .pivot(Align2::CENTER_CENTER)
            .default_pos(ctx.content_rect().center())
            .open(&mut open)
            .collapsible(false)
            .default_size([620.0, 480.0])
            .show(ctx, |ui| {
                let defaults = opentake_vst3::scan::default_search_paths();
                widgets::section(ui, "Scan Locations");
                ui.label(RichText::new("VST3 plug-ins are searched recursively in these folders.").color(theme::TEXT_DIM));
                ui.add_space(4.0);
                egui::Frame::new().fill(theme::BG_DEEP).corner_radius(6).inner_margin(8).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    if snap.plugin_paths.is_empty() {
                        ui.label(RichText::new("No folders — add one below.").color(theme::TEXT_FAINT));
                    }
                    for path in &snap.plugin_paths {
                        ui.horizontal(|ui| {
                            let exists = path.exists();
                            let text = RichText::new(path.display().to_string()).monospace();
                            ui.label(if exists { text } else { text.color(theme::TEXT_FAINT) });
                            if defaults.contains(path) {
                                ui.label(RichText::new("default").size(10.0).color(theme::TEXT_FAINT));
                            }
                            if !exists {
                                ui.label(RichText::new("not found").size(10.0).color(theme::INPUT));
                            }
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if widgets::icon_plain(ui, Icon::Close, theme::TEXT_DIM, &format!("Remove {}", path.display()), 18.0).clicked() {
                                    self.act(Action::RemovePluginPath(path.clone()));
                                }
                            });
                        });
                    }
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.add(egui::Button::new(RichText::new("Add Folder…").color(Color32::WHITE)).fill(theme::ACCENT)).clicked() {
                        if let Some(dir) = rfd::FileDialog::new().set_title("Add VST3 Folder").pick_folder() {
                            self.act(Action::AddPluginPath(dir));
                        }
                    }
                    if ui.button("Restore Defaults").clicked() {
                        self.act(Action::RestoreDefaultPluginPaths);
                    }
                    if ui.button("Rescan").clicked() {
                        self.act(Action::ScanPlugins);
                    }
                });

                ui.add_space(12.0);
                widgets::section(ui, &format!("Plug-ins ({})", snap.scanned.len()));
                egui::ScrollArea::vertical().auto_shrink([false, false]).max_height(220.0).show(ui, |ui| {
                    egui::Grid::new("plugin_list").num_columns(3).striped(true).spacing([16.0, 6.0]).show(ui, |ui| {
                        for p in &snap.scanned {
                            ui.label(RichText::new(&p.class.name).strong()).on_hover_text(p.bundle.display().to_string());
                            ui.label(RichText::new(&p.class.vendor).color(theme::TEXT_DIM));
                            ui.label(RichText::new(p.class.sub_categories.replace('|', " · ")).color(theme::TEXT_FAINT));
                            ui.end_row();
                        }
                    });
                    if snap.scanned.is_empty() {
                        ui.label(RichText::new("No plug-ins found in these folders.").color(theme::TEXT_FAINT));
                    }
                    for err in &snap.scan_errors {
                        ui.label(RichText::new(format!("⚠ {err}")).color(Color32::from_rgb(240, 120, 110)));
                    }
                });
            });
        self.show_plugin_manager = open;
    }

    fn mcp_info_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_mcp_info;
        egui::Window::new("Connect an Agent").pivot(Align2::CENTER_CENTER).default_pos(ctx.content_rect().center()).open(&mut open).resizable(false).collapsible(false).show(ctx, |ui| {
            ui.set_max_width(460.0);
            let Some(url) = self.mcp_url.clone() else {
                ui.label("The MCP server is off (started with --no-mcp).");
                return;
            };
            ui.label("Agents can control this session over MCP. Changes they make appear here live.");
            ui.add_space(8.0);
            let command = format!("claude mcp add --transport http opentake {url}");
            for (title, value) in [("Server URL", url.as_str()), ("Claude Code", command.as_str())] {
                widgets::section(ui, title);
                ui.horizontal(|ui| {
                    let mut text = value.to_string();
                    ui.add(egui::TextEdit::singleline(&mut text).font(egui::TextStyle::Monospace).desired_width(370.0));
                    if ui.button("Copy").clicked() {
                        ui.ctx().copy_text(value.to_string());
                        self.info("Copied to clipboard");
                    }
                });
                ui.add_space(6.0);
            }
        });
        self.show_mcp_info = open;
    }

    fn transport(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        use widgets::TransportButton as B;
        let s = &snap.status;
        let plain = |icon, a11y| B { icon, active: false, color: theme::TEXT, idle_color: theme::TEXT, a11y };
        let buttons = [
            plain(Icon::ToStart, "Go to Beginning (Home)"),
            plain(Icon::Rewind, "Rewind One Bar"),
            plain(Icon::Forward, "Forward One Bar"),
            plain(Icon::Stop, "Stop (Space)"),
            B { icon: Icon::Play, active: s.playing && !s.recording, color: theme::PLAY, idle_color: theme::TEXT, a11y: "Play (Space)" },
            B { icon: Icon::Record, active: s.recording, color: theme::RECORD, idle_color: theme::shade(theme::RECORD, 0.85), a11y: "Record (R)" },
            B { icon: Icon::Cycle, active: snap.project.loop_enabled, color: theme::SOLO, idle_color: theme::TEXT, a11y: "Cycle (C)" },
        ];
        let r = widgets::transport(ui, "transport", &buttons, vec2(TRANSPORT_SEG_W, 30.0));
        let bar = timeline::bar_secs(&snap.project);
        let pos = s.position_secs;
        if r[0].clicked() {
            self.act(Action::Seek(0.0));
        }
        if r[1].clicked() {
            // Back to the previous bar line (or one more bar if already on one).
            let prev = ((pos - 1e-6) / bar).floor() * bar;
            self.act(Action::Seek(prev.max(0.0)));
        }
        if r[2].clicked() {
            self.act(Action::Seek(((pos + 1e-6) / bar).floor() * bar + bar));
        }
        if r[3].clicked() {
            self.act(Action::Stop);
        }
        if r[4].clicked() {
            self.act(Action::Play);
        }
        if r[5].clicked() {
            self.act(if s.recording { Action::Stop } else { Action::Record });
        }
        if r[6].clicked() {
            self.toggle_cycle(snap);
        }
        ui.add_space(10.0);
        self.lcd(ui, snap);
        ui.add_space(10.0);
        self.metronome_button(ui, snap);
    }

    fn metronome_button(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        let on = snap.project.metronome_enabled;
        let button = widgets::TransportButton {
            icon: Icon::Metronome,
            active: on,
            color: theme::ACCENT,
            idle_color: theme::TEXT,
            a11y: "Metronome (K) — right-click for volume",
        };
        let resp = widgets::transport(ui, "metronome", &[button], vec2(TRANSPORT_SEG_W, 30.0)).remove(0);
        if resp.clicked() {
            self.act(Action::SetMetronome { enabled: Some(!on), volume: None });
        }
        egui::Popup::context_menu(&resp).show(|ui| {
            ui.set_min_width(200.0);
            widgets::section(ui, "Metronome");
            let mut enabled = on;
            if ui.checkbox(&mut enabled, "Click while playing").changed() {
                self.act(Action::SetMetronome { enabled: Some(enabled), volume: None });
            }
            let mut db = widgets::gain_to_db(snap.project.metronome_volume).max(-40.0);
            let slider = egui::Slider::new(&mut db, -40.0..=6.0).suffix(" dB").fixed_decimals(1).text("Volume");
            if ui.add(slider).changed() {
                self.act(Action::SetMetronome { enabled: None, volume: Some(widgets::db_to_gain(db)) });
            }
        });
    }

    fn lcd(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        let (rect, _) = ui.allocate_exact_size(vec2(LCD_W, 40.0), Sense::hover());
        let p = ui.painter();
        p.rect_filled(rect, 7.0, theme::LCD_BG);
        p.rect_stroke(rect, 7.0, Stroke::new(1.0, Color32::from_gray(10)), StrokeKind::Inside);

        let s = &snap.status;
        let label_y = rect.bottom() - 9.0;
        let value_y = rect.top() + 15.0;
        let small = FontId::proportional(8.5);
        let mut x = rect.left() + 14.0;

        // State LED; pulses on each beat while the transport runs (brighter on the downbeat).
        let base = if s.recording {
            theme::RECORD
        } else if s.playing {
            theme::PLAY
        } else {
            Color32::from_gray(60)
        };
        let beat = 60.0 / snap.project.tempo_bpm * 4.0 / snap.project.time_signature.1.max(1) as f64;
        let beats = s.position_secs / beat;
        let on_beat = s.playing && beats.fract() < 0.15;
        let downbeat = (beats.floor() as u64) % snap.project.time_signature.0.max(1) as u64 == 0;
        let led = match (on_beat, downbeat) {
            (true, true) => Color32::WHITE,
            (true, false) => theme::shade(base, 1.35),
            _ if s.playing => theme::shade(base, 0.55),
            _ => base,
        };
        if on_beat {
            p.circle_filled(pos2(x, rect.center().y), 6.0, theme::with_alpha(led, 50));
        }
        p.circle_filled(pos2(x, rect.center().y), 3.5, led);
        x += 14.0;

        // Position: bar . beat . division
        let (bar, beat, div) = bar_beat(s.position_secs, &snap.project);
        let pos_rect = Rect::from_min_size(pos2(x, rect.top()), vec2(118.0, rect.height()));
        // One column per field, so each number sits centred over its label.
        let columns = [(bar.to_string(), "BAR", 0.42), (beat.to_string(), "BEAT", 0.29), (div.to_string(), "DIV", 0.29)];
        let mut cx = pos_rect.left();
        for (value, label, share) in columns {
            let w = pos_rect.width() * share;
            let center = cx + w / 2.0;
            p.text(pos2(center, value_y), Align2::CENTER_CENTER, value, FontId::monospace(19.0), theme::LCD_TEXT);
            p.text(pos2(center, label_y), Align2::CENTER_CENTER, label, small.clone(), theme::LCD_DIM);
            cx += w;
        }
        x = pos_rect.right();
        let sep = |p: &egui::Painter, x: f32| p.vline(x, (rect.top() + 7.0)..=(rect.bottom() - 7.0), Stroke::new(1.0, Color32::from_gray(40)));
        sep(p, x);

        // Clock time.
        let time_rect = Rect::from_min_size(pos2(x, rect.top()), vec2(104.0, rect.height()));
        p.text(pos2(time_rect.center().x, value_y), Align2::CENTER_CENTER, format_time(s.position_secs), FontId::monospace(14.0), theme::LCD_TEXT);
        p.text(pos2(time_rect.center().x, label_y), Align2::CENTER_CENTER, "TIME", small.clone(), theme::LCD_DIM);
        x = time_rect.right();
        sep(p, x);

        // Tempo (drag to change, Shift for fine).
        let tempo_rect = Rect::from_min_size(pos2(x, rect.top()), vec2(70.0, rect.height()));
        let resp = ui.interact(tempo_rect, ui.make_persistent_id("lcd_tempo"), Sense::drag());
        let bpm = snap.project.tempo_bpm;
        if resp.dragged() {
            let fine = ui.input(|i| i.modifiers.shift);
            let new = (bpm - resp.drag_delta().y as f64 * if fine { 0.05 } else { 0.5 }).clamp(20.0, 999.0);
            if (new - bpm).abs() > f64::EPSILON {
                self.act(Action::SetTempo((new * 10.0).round() / 10.0));
            }
        }
        // While an imported song with a tempo map plays, show the tempo it's playing at;
        // hovering/dragging shows (and edits) the project tempo again.
        let song = snap.project.song_tempo_at(s.position_secs);
        let hint = match song {
            Some(t) => format!("Song tempo {t:.1} at the playhead (from the imported score).\nProject tempo {bpm:.1} — drag to change it (Shift = fine)"),
            None => "Drag to change tempo (Shift = fine)".to_string(),
        };
        let resp = resp.on_hover_text(hint);
        let editing = resp.hovered() || resp.dragged();
        let (value, label, color) = match song {
            Some(t) if !editing => (t, "SONG TEMPO", theme::TEMPO),
            _ => (bpm, "TEMPO", if editing { Color32::WHITE } else { theme::LCD_TEXT }),
        };
        let p = ui.painter();
        p.text(pos2(tempo_rect.center().x, value_y), Align2::CENTER_CENTER, format!("{value:.1}"), FontId::monospace(16.0), color);
        p.text(pos2(tempo_rect.center().x, label_y), Align2::CENTER_CENTER, label, small.clone(), if song.is_some() && !editing { theme::with_alpha(theme::TEMPO, 170) } else { theme::LCD_DIM });
        x = tempo_rect.right();
        sep(p, x);

        let (n, d) = snap.project.time_signature;
        let sig_rect = Rect::from_min_max(pos2(x, rect.top()), rect.max);
        p.text(pos2(sig_rect.center().x, value_y), Align2::CENTER_CENTER, format!("{n}/{d}"), FontId::monospace(16.0), theme::LCD_TEXT);
        p.text(pos2(sig_rect.center().x, label_y), Align2::CENTER_CENTER, "SIGNATURE", small, theme::LCD_DIM);
    }

    fn right_cluster(&mut self, ui: &mut egui::Ui, snap: &Snapshot, show_meter: bool) {
        let s = &snap.status;
        if widgets::icon_button(ui, Icon::Gear, false, theme::TEXT, "Audio Settings", vec2(30.0, 26.0)).clicked() {
            self.show_audio_settings = true;
            self.act(Action::RefreshDevices);
        }
        match &self.mcp_url {
            Some(url) => {
                pill(ui, theme::ACCENT, "MCP", &format!("Agents can connect at {url}\n\nclaude mcp add --transport http opentake {url}"));
            }
            None => {
                pill(ui, theme::TEXT_FAINT, "MCP off", "Started with --no-mcp");
            }
        }
        if s.audio_running {
            let khz = s.sample_rate as f32 / 1000.0;
            let tip = format!(
                "Output: {}\nInput: {}",
                s.output_device.clone().unwrap_or_default(),
                s.input_device.clone().map(|i| format!("{i} ({} ch)", s.input_channels)).unwrap_or_else(|| "none".into())
            );
            if pill(ui, theme::PLAY, &format!("{khz} kHz"), &tip).clicked() {
                self.show_audio_settings = true;
                self.act(Action::RefreshDevices);
            }
        } else if pill(ui, theme::INPUT, "Audio off — click to start", "Start the audio engine").clicked() {
            self.act(Action::StartAudio(self.audio_cfg.clone()));
        }
        if show_meter {
            ui.add_space(6.0);
            widgets::stereo_hmeter(ui, vec2(84.0, 10.0), s.master_peak_db).on_hover_text("Stereo Out level");
        }
    }

    // ------------------------------------------------------------------ inspector

    fn inspector(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        let selected = self.selected_track.filter(|&t| t < snap.project.tracks.len());
        ui.add_space(4.0);
        match selected {
            Some(t) => {
                let track = &snap.project.tracks[t];
                if self.name_buf.0 != Some(t) {
                    self.name_buf = (Some(t), track.name.clone());
                }
                widgets::section(ui, "Track");
                let r = ui.add(egui::TextEdit::singleline(&mut self.name_buf.1).desired_width(f32::INFINITY));
                if r.lost_focus() && self.name_buf.1 != track.name {
                    let update = TrackUpdate { name: Some(self.name_buf.1.clone()), ..Default::default() };
                    self.act(Action::UpdateTrack(t, update));
                }
            }
            None => {
                widgets::section(ui, "Track");
                ui.label(RichText::new("No track selected").color(theme::TEXT_FAINT));
            }
        }
        ui.add_space(6.0);
        ui.separator();

        let strip_w = (ui.available_width() - 8.0) / 2.0;
        let layout = mixer::StripLayout { width: strip_w, fx_height: FX_AREA_H };
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.allocate_ui_with_layout(vec2(strip_w, ui.available_height()), Layout::top_down(Align::Center), |ui| match selected {
                Some(t) => mixer::track_strip(self, ui, t, &snap.project.tracks[t], snap, layout),
                None => {
                    ui.add_space(60.0);
                    ui.label(RichText::new("Select a track").color(theme::TEXT_FAINT));
                }
            });
            ui.allocate_ui_with_layout(vec2(strip_w, ui.available_height()), Layout::top_down(Align::Center), |ui| {
                mixer::master_strip(self, ui, snap, layout);
            });
        });
    }

    fn plugin_windows(&mut self, ui: &egui::Ui, snap: &Snapshot) {
        let tracks = &snap.project.tracks;
        let slot_of = |t: usize, s: SlotRef| {
            tracks.get(t).and_then(|tr| match s {
                SlotRef::Instrument => tr.instrument.as_ref(),
                SlotRef::Effect(i) => tr.plugins.get(i),
            })
        };
        self.open_plugins.retain(|&(t, s)| slot_of(t, s).is_some());
        for (n, (t, s)) in self.open_plugins.clone().into_iter().enumerate() {
            let Some(slot) = slot_of(t, s) else { continue };
            let name = short_plugin_name(&slot.name).to_string();
            let mut open = true;
            egui::Window::new(format!("{} — {}", tracks[t].name, name))
                .id(ui.make_persistent_id(("plugin_window", t, s)))
                .open(&mut open)
                .default_size([400.0, 360.0])
                .default_pos(pos2(640.0 + 28.0 * n as f32, 260.0))
                .show(ui.ctx(), |ui| {
                    ui.horizontal(|ui| {
                        if widgets::chip(ui, !slot.bypass, "ON", theme::ACCENT, "Plug-in enabled", vec2(34.0, 20.0)).clicked() {
                            self.act(Action::SetBypass(t, s, !slot.bypass));
                        }
                        ui.label(RichText::new(&name).strong());
                        if s == SlotRef::Instrument {
                            ui.label(RichText::new("Instrument").size(10.5).color(theme::TEXT_FAINT));
                        }
                    });
                    ui.add_space(6.0);
                    let params = snap.params.get(&(t, s)).map(Vec::as_slice).unwrap_or_default();
                    if params.is_empty() {
                        ui.label(RichText::new("This plug-in exposes no parameters.").color(theme::TEXT_FAINT));
                        return;
                    }
                    egui::ScrollArea::vertical().auto_shrink([false, true]).show(ui, |ui| {
                        egui::Grid::new(("params", t, s)).num_columns(3).spacing([12.0, 8.0]).show(ui, |ui| {
                            for p in params {
                                ui.label(RichText::new(&p.title).color(theme::TEXT_DIM));
                                let mut v = p.value;
                                let mut slider = egui::Slider::new(&mut v, 0.0..=1.0).show_value(false);
                                if p.step_count > 0 {
                                    slider = slider.step_by(1.0 / p.step_count as f64);
                                }
                                if ui.add_enabled(!p.read_only, slider).changed() {
                                    self.act(Action::SetParam(t, s, p.id, v));
                                }
                                let display = p.display.clone().unwrap_or_else(|| format!("{:.3}", p.value));
                                ui.label(RichText::new(format!("{display} {}", p.units)).monospace());
                                ui.end_row();
                            }
                        });
                    });
                });
            if !open {
                self.open_plugins.retain(|&x| x != (t, s));
            }
        }
    }

    fn bottom_panel(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        let Some(tab) = self.bottom else { return };
        let frame = egui::Frame::new().fill(theme::BG_PANEL).inner_margin(egui::Margin::symmetric(8, 6));
        egui::Panel::bottom("editors").frame(frame).resizable(true).default_size(320.0).min_size(180.0).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                for (t, label, key) in [(BottomTab::Mixer, "Mixer", "X"), (BottomTab::PianoRoll, "Piano Roll", "P"), (BottomTab::Tab, "Tablature", "T")] {
                    let on = tab == t;
                    let text = RichText::new(label).size(11.5).color(if on { Color32::WHITE } else { theme::TEXT_DIM });
                    let b = egui::Button::new(text).fill(if on { theme::BG_CONTROL_HOVER } else { Color32::TRANSPARENT }).min_size(vec2(84.0, 22.0));
                    if ui.add(b).on_hover_text(format!("{label} ({key})")).clicked() {
                        self.bottom = Some(t);
                        if t == BottomTab::PianoRoll {
                            self.toggle_bottom(BottomTab::PianoRoll);
                            self.bottom = Some(BottomTab::PianoRoll);
                        }
                    }
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if widgets::icon_plain(ui, Icon::Close, theme::TEXT_DIM, "Close editor", 20.0).clicked() {
                        self.bottom = None;
                    }
                });
            });
            ui.add_space(4.0);
            match tab {
                BottomTab::Mixer => mixer::mixer_panel(self, ui, snap),
                BottomTab::PianoRoll => piano_roll::show(self, ui, snap),
                BottomTab::Tab => tab_view::show(self, ui, snap),
            }
        });
    }

    fn audio_settings_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_audio_settings;
        egui::Window::new("Audio Settings").pivot(Align2::CENTER_CENTER).default_pos(ctx.content_rect().center()).open(&mut open).resizable(false).collapsible(false).show(ctx, |ui| {
            let Some(devices) = self.devices.clone() else {
                ui.label("Loading devices…");
                return;
            };
            egui::Grid::new("audio_grid").num_columns(2).spacing([14.0, 10.0]).show(ui, |ui| {
                ui.label(RichText::new("Driver").color(theme::TEXT_DIM));
                ui.label(&devices.host);
                ui.end_row();

                ui.label(RichText::new("Output Device").color(theme::TEXT_DIM));
                device_combo(ui, "out_dev", &mut self.audio_cfg.output_device, &devices.outputs);
                ui.end_row();

                ui.label(RichText::new("Input Device").color(theme::TEXT_DIM));
                ui.horizontal(|ui| {
                    ui.add_enabled_ui(!self.audio_cfg.disable_input, |ui| {
                        device_combo(ui, "in_dev", &mut self.audio_cfg.input_device, &devices.inputs);
                    });
                    let mut enabled = !self.audio_cfg.disable_input;
                    ui.checkbox(&mut enabled, "Enabled");
                    self.audio_cfg.disable_input = !enabled;
                });
                ui.end_row();

                ui.label(RichText::new("Sample Rate").color(theme::TEXT_DIM));
                option_combo(ui, "sr", &mut self.audio_cfg.sample_rate, &[44_100, 48_000, 88_200, 96_000], |v| format!("{v} Hz"));
                ui.end_row();

                ui.label(RichText::new("I/O Buffer Size").color(theme::TEXT_DIM));
                option_combo(ui, "bs", &mut self.audio_cfg.buffer_size, &[32, 64, 128, 256, 512, 1024, 2048], |v| {
                    format!("{v} samples")
                });
                ui.end_row();
            });
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.add(egui::Button::new(RichText::new("Apply").color(Color32::WHITE)).fill(theme::ACCENT)).clicked() {
                    self.act(Action::StartAudio(self.audio_cfg.clone()));
                }
                if ui.button("Refresh Devices").clicked() {
                    self.act(Action::RefreshDevices);
                }
            });
        });
        self.show_audio_settings = open;
    }

    fn status_bar(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        ui.horizontal_centered(|ui| {
            let title = match &snap.project_path {
                Some(p) => p.display().to_string(),
                None => format!("{} — not saved", snap.project.name),
            };
            ui.label(RichText::new(title).size(10.5).color(theme::TEXT_FAINT));
            if let Some((msg, is_err, at)) = &self.message {
                if at.elapsed() < Duration::from_secs(if *is_err { 12 } else { 5 }) {
                    let color = if *is_err { Color32::from_rgb(240, 120, 110) } else { theme::TEXT_DIM };
                    ui.label(RichText::new("·").color(theme::TEXT_FAINT));
                    ui.label(RichText::new(msg.lines().next().unwrap_or_default()).size(10.5).color(color)).on_hover_text(msg);
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(RichText::new("Space  play / stop    R  record    Home  start    I  inspector").size(10.0).color(theme::TEXT_FAINT));
            });
        });
    }
}

const LCD_W: f32 = 388.0;
const TRANSPORT_SEG_W: f32 = 40.0;
const FX_AREA_H: f32 = 124.0;

impl eframe::App for DawApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let snap = self.snapshot();
        if self.selected_track.is_some_and(|t| t >= snap.project.tracks.len()) {
            self.selected_track = None;
        }
        self.selection.retain(|r| timeline::region_view(&snap.project, *r).is_some());
        self.track_recent(&snap);
        self.handle_shortcuts(ui, &snap);
        self.handle_dropped_files(ui, &snap);

        let mut commands = self.native_menu_commands(&snap);
        commands.extend(self.clipboard_shortcuts(ui));
        if !self.has_native_menu() {
            commands.extend(self.menu_shortcuts(ui));
            let state = self.menu_state(&snap);
            let frame = egui::Frame::new().fill(theme::BG_PANEL).inner_margin(egui::Margin::symmetric(8, 2));
            egui::Panel::top("menu_bar").frame(frame).show(ui, |ui| {
                commands.extend(menu::egui_menu_bar(ui, &state));
            });
        }
        for cmd in commands {
            self.run_command(cmd, ui, &snap);
        }

        let bar_frame = egui::Frame::new().fill(theme::BG_HEADER).inner_margin(egui::Margin::symmetric(10, 8));
        egui::Panel::top("control_bar").frame(bar_frame).exact_size(58.0).show(ui, |ui| {
            self.control_bar(ui, &snap);
        });

        let status_frame = egui::Frame::new().fill(theme::BG_DEEP).inner_margin(egui::Margin::symmetric(10, 3));
        egui::Panel::bottom("status_bar").frame(status_frame).exact_size(24.0).show(ui, |ui| {
            self.status_bar(ui, &snap);
        });

        // Like Logic: the inspector runs full height; editors sit below the tracks only.
        if self.show_inspector {
            let frame = egui::Frame::new().fill(theme::BG_PANEL).inner_margin(egui::Margin::same(10));
            egui::Panel::left("inspector").frame(frame).exact_size(260.0).show(ui, |ui| {
                self.inspector(ui, &snap);
            });
        }
        self.bottom_panel(ui, &snap);

        egui::CentralPanel::default().frame(egui::Frame::new().fill(theme::BG_DEEP)).show(ui, |ui| {
            timeline::show(self, ui, &snap);
        });

        self.plugin_windows(ui, &snap);
        self.audio_settings_window(ui.ctx());
        self.mcp_info_window(ui.ctx());
        self.plugin_manager_window(ui.ctx(), &snap);
        self.instrument_picker_window(ui.ctx(), &snap);
        self.guitar_pro_window(ui.ctx(), &snap);
        self.apply_actions();
        // Keep meters, playhead and agent-made changes live.
        ui.request_repaint_after(Duration::from_millis(33));
    }
}

/// Rounded status pill with a coloured dot.
fn pill(ui: &mut egui::Ui, color: Color32, text: &str, tooltip: &str) -> egui::Response {
    let galley = ui.painter().layout_no_wrap(text.to_string(), FontId::proportional(11.0), theme::TEXT);
    let size = vec2(galley.size().x + 26.0, 22.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let p = ui.painter();
    p.rect_filled(rect, 11.0, if resp.hovered() { theme::BG_CONTROL_HOVER } else { theme::BG_CONTROL });
    p.circle_filled(pos2(rect.left() + 11.0, rect.center().y), 3.5, color);
    p.galley(pos2(rect.left() + 19.0, rect.center().y - galley.size().y / 2.0), galley, theme::TEXT);
    resp.on_hover_text(tooltip)
}

fn instrument_combo(ui: &mut egui::Ui, id: &str, value: &mut String, instruments: &[(String, String)]) {
    let current = instruments.iter().find(|(_, i)| i == value).map_or(value.clone(), |(n, _)| n.clone());
    egui::ComboBox::from_id_salt(id).selected_text(current).width(240.0).show_ui(ui, |ui| {
        for (name, inst) in instruments {
            ui.selectable_value(value, inst.clone(), name);
        }
    });
}

fn device_combo(ui: &mut egui::Ui, id: &str, value: &mut Option<String>, devices: &[opentake_engine::DeviceInfo]) {
    let current = value
        .as_ref()
        .and_then(|v| devices.iter().find(|d| &d.id == v).map(|d| d.name.clone()))
        .unwrap_or_else(|| "System Default".into());
    egui::ComboBox::from_id_salt(id).selected_text(current).width(260.0).show_ui(ui, |ui| {
        ui.selectable_value(value, None, "System Default");
        for d in devices {
            ui.selectable_value(value, Some(d.id.clone()), format!("{} ({} ch)", d.name, d.max_channels));
        }
    });
}

fn option_combo(ui: &mut egui::Ui, id: &str, value: &mut Option<u32>, choices: &[u32], fmt: impl Fn(u32) -> String) {
    let current = value.map_or("Device Default".to_string(), &fmt);
    egui::ComboBox::from_id_salt(id).selected_text(current).show_ui(ui, |ui| {
        ui.selectable_value(value, None, "Device Default");
        for &c in choices {
            ui.selectable_value(value, Some(c), fmt(c));
        }
    });
}

pub(crate) fn format_time(secs: f64) -> String {
    let ms = (secs.max(0.0) * 1000.0).round() as u64;
    format!("{:02}:{:02}.{:03}", ms / 60_000, (ms / 1000) % 60, ms % 1000)
}

/// 1-based bar, beat and sixteenth division at `secs`.
pub(crate) fn bar_beat(secs: f64, project: &Project) -> (u64, u64, u64) {
    let beats = secs.max(0.0) * project.tempo_bpm / 60.0;
    let whole = beats as u64;
    let per_bar = project.time_signature.0.max(1) as u64;
    let div = ((beats - whole as f64) * 4.0) as u64 + 1;
    (whole / per_bar + 1, whole % per_bar + 1, div)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;

    use super::*;

    fn harness_with_track() -> (Harness<'static, DawApp>, SharedEngine) {
        let engine: SharedEngine = Arc::new(Mutex::new(Engine::new()));
        engine.lock().unwrap().add_track("Guitar").unwrap();
        let app_engine = engine.clone();
        // Real frame timing (kittest defaults to 0.25 s per step, too slow for double-clicks).
        let mut harness = Harness::builder().with_size([1360.0, 840.0]).with_step_dt(1.0 / 60.0).build_eframe(move |cc| {
            theme::apply(&cc.egui_ctx);
            DawApp::new(app_engine, None)
        });
        harness.step();
        (harness, engine)
    }

    /// Accessible label of an in-window menu item (text plus shortcut).
    fn menu_label(cmd: MenuCommand) -> String {
        match cmd.shortcut_text() {
            Some(s) => format!("{} {s}", cmd.label()),
            None => cmd.label().to_string(),
        }
    }

    fn click(harness: &mut Harness<'static, DawApp>, label: &str) {
        harness.get_by_label(label).click();
        harness.step();
        harness.step();
    }

    #[test]
    fn track_header_toggles_are_clickable() {
        let (mut harness, engine) = harness_with_track();
        for label in ["Mute", "Solo", "Record Enable", "Input Monitoring"] {
            click(&mut harness, label);
        }
        let e = engine.lock().unwrap();
        let t = &e.project().tracks[0];
        assert!(t.mute, "mute button did not toggle");
        assert!(t.solo, "solo button did not toggle");
        assert!(t.armed, "record-enable button did not toggle");
        assert!(t.monitor_input, "input-monitoring button did not toggle");
    }

    #[test]
    fn clicking_track_name_selects_track_and_shows_strip() {
        let (mut harness, engine) = harness_with_track();
        assert_eq!(harness.state().selected_track, None);
        click(&mut harness, "Guitar");
        assert_eq!(harness.state().selected_track, Some(0));
        // The channel strip now has its own Mute button; it must work too.
        let mutes = harness.query_all_by_label("Mute").count();
        assert_eq!(mutes, 2, "header + channel strip");
        if let Some(strip_mute) = harness.query_all_by_label("Mute").last() {
            strip_mute.click();
        }
        harness.step();
        harness.step();
        assert!(engine.lock().unwrap().project().tracks[0].mute);
    }

    /// One track ("Guitar") with a 4 s region at 1.0 s; snapping off for exact pixel math.
    fn harness_with_region() -> (Harness<'static, DawApp>, SharedEngine, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("opentake-gui-{}-{:?}", std::process::id(), std::thread::current().id()));
        std::fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("riff.wav");
        write_tone(&wav, 220.0, 4.0, 2.0);
        let (mut harness, engine) = harness_with_track();
        engine.lock().unwrap().add_clip(0, &wav, 1.0).unwrap();
        harness.state_mut().snap_mode = timeline::SnapMode::Off;
        harness.step();
        harness.step();
        (harness, engine, dir)
    }

    fn clips(engine: &SharedEngine) -> Vec<(f64, f64, f64)> {
        let e = engine.lock().unwrap();
        e.project().tracks[0].clips.iter().map(|c| (c.position_secs, c.length_secs, c.source_offset_secs)).collect()
    }

    fn drag(harness: &mut Harness<'static, DawApp>, from: egui::Pos2, to: egui::Pos2) {
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

    #[test]
    fn scissors_tool_splits_region() {
        let (mut harness, engine, dir) = harness_with_region();
        click(&mut harness, "Scissors Tool — click a region to split it");
        click(&mut harness, "Region riff");
        let c = clips(&engine);
        assert_eq!(c.len(), 2, "region should be split in two: {c:?}");
        assert!((c[0].0 + c[0].1 - c[1].0).abs() < 1e-9, "parts must be contiguous");
        assert!((c[1].2 - c[0].1).abs() < 1e-9, "right part's source offset = left length");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn split_at_playhead_from_edit_menu() {
        let (mut harness, engine, dir) = harness_with_region();
        engine.lock().unwrap().seek(2.0).unwrap();
        harness.step();
        click(&mut harness, "Edit");
        click(&mut harness, &menu_label(MenuCommand::SplitAtPlayhead));
        let c = clips(&engine);
        assert_eq!(c.len(), 2);
        assert!((c[0].1 - 1.0).abs() < 1e-6 && (c[1].0 - 2.0).abs() < 1e-6, "{c:?}");
        // The right part is now selected; Delete removes it.
        harness.key_press(egui::Key::Delete);
        harness.step();
        harness.step();
        assert_eq!(clips(&engine).len(), 1);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn dragging_region_edge_trims_and_body_moves() {
        let (mut harness, engine, dir) = harness_with_region();
        let px = harness.state().view.px_per_sec;
        let rect = harness.get_by_label("Region riff").rect();

        // Trim the end 2 seconds to the left.
        let edge = egui::pos2(rect.right() - 2.0, rect.center().y);
        drag(&mut harness, edge, edge - egui::vec2(2.0 * px, 0.0));
        let (pos, len, off) = clips(&engine)[0];
        assert!((pos - 1.0).abs() < 1e-6 && off == 0.0, "trim end must not move the start");
        assert!((len - 2.0).abs() < 0.05, "length should be ~2 s after trim, got {len}");

        // Trim the start 0.5 s to the right: audio stays put, offset grows.
        let rect = harness.get_by_label("Region riff").rect();
        let edge = egui::pos2(rect.left() + 2.0, rect.center().y);
        drag(&mut harness, edge, edge + egui::vec2(0.5 * px, 0.0));
        let (pos, len, off) = clips(&engine)[0];
        assert!((pos - 1.5).abs() < 0.05 && (off - 0.5).abs() < 0.05 && (len - 1.5).abs() < 0.05, "{pos} {len} {off}");

        // Drag the body 1 s to the right.
        let rect = harness.get_by_label("Region riff").rect();
        drag(&mut harness, rect.center(), rect.center() + egui::vec2(px, 0.0));
        let (pos2, len2, off2) = clips(&engine)[0];
        assert!((pos2 - (pos + 1.0)).abs() < 0.05 && len2 == len && off2 == off, "move must keep length/offset");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn cmd_drag_copies_region_and_keeps_original() {
        let (mut harness, engine, dir) = harness_with_region();
        let px = harness.state().view.px_per_sec;
        let rect = harness.get_by_label("Region riff").rect();
        harness.event(egui::Event::ModifiersChanged(egui::Modifiers::COMMAND));
        drag(&mut harness, rect.center(), rect.center() + egui::vec2(5.0 * px, 0.0));
        harness.event(egui::Event::ModifiersChanged(egui::Modifiers::NONE));
        harness.step();
        let c = clips(&engine);
        assert_eq!(c.len(), 2, "⌘-drag should leave the original and add a copy: {c:?}");
        assert!((c[0].0 - 1.0).abs() < 1e-6, "original stays at 1.0");
        assert!((c[1].0 - 6.0).abs() < 0.05, "copy lands ~5 s later, got {}", c[1].0);
        assert_eq!(harness.state().selection, vec![RegionRef::audio(0, 1)], "copy is selected");

        // Plain drag still moves.
        let rect = harness.query_all_by_label("Region riff").next().unwrap().rect();
        drag(&mut harness, rect.center(), rect.center() + egui::vec2(px, 0.0));
        assert_eq!(clips(&engine).len(), 2);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn copy_paste_and_duplicate_regions() {
        let (mut harness, engine, dir) = harness_with_region();
        click(&mut harness, "Region riff"); // select
        harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::C);
        harness.step();
        assert!(harness.state().clipboard.is_some(), "⌘C copies the selected region");

        engine.lock().unwrap().seek(8.0).unwrap();
        harness.step();
        harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::V);
        harness.step();
        harness.step();
        let c = clips(&engine);
        assert_eq!(c.len(), 2);
        assert!((c[1].0 - 8.0).abs() < 1e-6, "pasted at the playhead");

        // Duplicate places the copy right after the selected (pasted) region.
        click(&mut harness, "Edit");
        click(&mut harness, &menu_label(MenuCommand::DuplicateRegion));
        let c = clips(&engine);
        assert_eq!(c.len(), 3);
        assert!((c[2].0 - (c[1].0 + c[1].1)).abs() < 1e-6, "duplicate follows the original");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn plugin_manager_edits_and_persists_scan_folders() {
        let (mut harness, engine) = harness_with_track();
        let dir = std::env::temp_dir().join(format!("opentake-pm-{}", std::process::id()));
        let settings = dir.join("settings.json");
        harness.state_mut().settings_path = Some(settings.clone());
        engine.lock().unwrap().set_plugin_search_paths(vec![dir.join("a"), dir.join("b")]);

        click(&mut harness, "Plug-ins");
        click(&mut harness, &menu_label(MenuCommand::PluginManager));
        click(&mut harness, &format!("Remove {}", dir.join("a").display()));
        assert_eq!(engine.lock().unwrap().plugin_search_paths(), &[dir.join("b")]);
        let saved = crate::settings::Settings::load(&settings);
        assert_eq!(saved.plugin_paths, Some(vec![dir.join("b")]), "change is persisted");

        click(&mut harness, "Restore Defaults");
        assert_eq!(engine.lock().unwrap().plugin_search_paths(), opentake_vst3::scan::default_search_paths().as_slice());
        assert_eq!(crate::settings::Settings::load(&settings).plugin_paths, None, "defaults stored as None");
        std::fs::remove_dir_all(dir).ok();
    }

    fn double_click_at(harness: &mut Harness<'static, DawApp>, pos: egui::Pos2) {
        harness.hover_at(pos);
        // Let earlier clicks age out, or egui counts this as a triple-click.
        for _ in 0..30 {
            harness.step();
        }
        for _ in 0..2 {
            for pressed in [true, false] {
                harness.event(egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE });
            }
            harness.step();
        }
        harness.step();
        harness.step();
    }

    #[test]
    fn undo_and_redo_with_keyboard_and_menu() {
        let (mut harness, engine) = harness_with_track();
        click(&mut harness, "Track");
        click(&mut harness, &menu_label(MenuCommand::NewTrack));
        assert_eq!(engine.lock().unwrap().project().tracks.len(), 2);

        harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Z);
        harness.step();
        harness.step();
        assert_eq!(engine.lock().unwrap().project().tracks.len(), 1, "⌘Z undoes the new track");

        // The Edit menu names the step that redo would apply.
        click(&mut harness, "Edit");
        let redo = format!("Redo New Track {}", MenuCommand::Redo.shortcut_text().unwrap());
        click(&mut harness, &redo);
        assert_eq!(engine.lock().unwrap().project().tracks.len(), 2);
    }

    #[test]
    fn mixer_opens_with_x_and_its_strips_work() {
        let (mut harness, engine) = harness_with_track();
        engine.lock().unwrap().add_track("Bass").unwrap();
        harness.step();
        harness.key_press(egui::Key::X);
        harness.step();
        harness.step();
        assert_eq!(harness.state().bottom, Some(BottomTab::Mixer));
        // Two tracks: header + mixer strip each have a Mute button.
        let mutes: Vec<_> = harness.query_all_by_label("Mute").map(|n| n.rect()).collect();
        assert_eq!(mutes.len(), 4);
        // The lowest Mute buttons on screen belong to the mixer strips; click the second strip's.
        let mut strip_mutes: Vec<_> = mutes.iter().filter(|r| r.top() > 400.0).collect();
        strip_mutes.sort_by(|a, b| a.left().total_cmp(&b.left()));
        let target = strip_mutes[1].center();
        harness.hover_at(target);
        harness.step();
        for pressed in [true, false] {
            harness.event(egui::Event::PointerButton { pos: target, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE });
        }
        harness.step();
        harness.step();
        let e = engine.lock().unwrap();
        assert!(!e.project().tracks[0].mute && e.project().tracks[1].mute, "only the Bass strip is muted");
    }

    #[test]
    fn instrument_track_midi_region_and_piano_roll_editing() {
        let (mut harness, engine) = harness_with_track();
        click(&mut harness, "New Track");
        click(&mut harness, opentake_engine::synth::NAME);
        assert!(engine.lock().unwrap().project().tracks[1].is_instrument());

        // Double-click the instrument lane: a one-bar MIDI region, opened in the piano roll.
        let lane = harness.get_by_label("Lane Inst 2").rect();
        double_click_at(&mut harness, lane.left_center() + egui::vec2(40.0, 0.0));
        assert_eq!(engine.lock().unwrap().project().tracks[1].midi_clips.len(), 1);
        assert_eq!(harness.state().bottom, Some(BottomTab::PianoRoll));

        // Double-click in the grid adds a note (snap off so it starts exactly at the pointer).
        harness.state_mut().snap_mode = timeline::SnapMode::Off;
        let grid = harness.get_by_label("Piano Roll Grid").rect();
        let spot = grid.left_center() + egui::vec2(60.0, 0.0);
        double_click_at(&mut harness, spot);
        let notes = engine.lock().unwrap().project().tracks[1].midi_clips[0].notes.clone();
        assert_eq!(notes.len(), 1, "double-click adds a note");

        // Drag it up by three rows (pitch +3).
        let key_h = 14.0;
        drag(&mut harness, spot + egui::vec2(2.0, 0.0), spot + egui::vec2(2.0, -3.0 * key_h));
        let moved = engine.lock().unwrap().project().tracks[1].midi_clips[0].notes.clone();
        assert_eq!(moved.len(), 1);
        assert_eq!(moved[0].pitch, notes[0].pitch + 3, "dragging moves the note in pitch");

        // ⌫ deletes the selected note (not the region); ⌘Z brings it back.
        harness.key_press(egui::Key::Backspace);
        harness.step();
        harness.step();
        let e = engine.lock().unwrap();
        assert_eq!(e.project().tracks[1].midi_clips.len(), 1, "region stays");
        assert!(e.project().tracks[1].midi_clips[0].notes.is_empty(), "note deleted");
        drop(e);
        harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Z);
        harness.step();
        harness.step();
        assert_eq!(engine.lock().unwrap().project().tracks[1].midi_clips[0].notes.len(), 1);
    }

    fn press_drag(harness: &mut Harness<'static, DawApp>, from: egui::Pos2, to: egui::Pos2, modifiers: egui::Modifiers) {
        harness.event(egui::Event::ModifiersChanged(modifiers));
        drag(harness, from, to);
        harness.event(egui::Event::ModifiersChanged(egui::Modifiers::NONE));
        harness.step();
    }

    #[test]
    fn marquee_selects_across_tracks_then_group_move_copy_delete() {
        let (mut harness, engine, dir) = harness_with_region(); // "riff" on Guitar at 1.0 s
        {
            let mut e = engine.lock().unwrap();
            e.add_track("Bass").unwrap();
            let wav = dir.join("riff.wav");
            e.add_clip(1, &wav, 1.0).unwrap();
        }
        harness.step();
        let px = harness.state().view.px_per_sec;
        let lane_a = harness.get_by_label("Lane Guitar").rect();
        let lane_b = harness.get_by_label("Lane Bass").rect();

        // Rectangle from empty space before the regions (t≈0.2 s) across both lanes.
        let from = egui::pos2(lane_a.left() + 0.2 * px, lane_a.top() + 6.0);
        let to = egui::pos2(lane_a.left() + 2.0 * px, lane_b.center().y);
        press_drag(&mut harness, from, to, egui::Modifiers::NONE);
        let mut sel = harness.state().selection.clone();
        sel.sort();
        assert_eq!(sel, vec![RegionRef::audio(0, 0), RegionRef::audio(1, 0)], "marquee selects both regions");

        // Dragging one selected region moves the whole group.
        let region = harness.query_all_by_label("Region riff").map(|n| n.rect()).min_by(|a, b| a.top().total_cmp(&b.top())).unwrap();
        press_drag(&mut harness, region.center(), region.center() + egui::vec2(px, 0.0), egui::Modifiers::NONE);
        let positions = |e: &SharedEngine| {
            let e = e.lock().unwrap();
            e.project().tracks.iter().flat_map(|t| t.clips.iter().map(|c| c.position_secs)).collect::<Vec<_>>()
        };
        let p = positions(&engine);
        assert!(p.iter().all(|&x| (x - 2.0).abs() < 0.05), "both moved by 1 s: {p:?}");
        assert_eq!(engine.lock().unwrap().history_info().undo.as_deref(), Some("Move Regions"));

        // ⌘-drag copies the group; ⌫ then deletes the (selected) copies.
        let region = harness.query_all_by_label("Region riff").map(|n| n.rect()).min_by(|a, b| a.top().total_cmp(&b.top())).unwrap();
        press_drag(&mut harness, region.center(), region.center() + egui::vec2(5.0 * px, 0.0), egui::Modifiers::COMMAND);
        assert_eq!(positions(&engine).len(), 4, "⌘-drag copies both regions");
        assert_eq!(harness.state().selection.len(), 2, "the copies are selected");
        harness.key_press(egui::Key::Backspace);
        harness.step();
        harness.step();
        assert_eq!(positions(&engine).len(), 2);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn cycle_range_from_ruler_and_transport_toggle() {
        let (mut harness, engine) = harness_with_track();
        harness.state_mut().snap_mode = timeline::SnapMode::Off;
        let px = harness.state().view.px_per_sec;
        let area = harness.get_by_label("Cycle Area").rect();
        let y = area.center().y;
        press_drag(&mut harness, egui::pos2(area.left() + 2.0 * px, y), egui::pos2(area.left() + 6.0 * px, y), egui::Modifiers::NONE);
        {
            let e = engine.lock().unwrap();
            let p = e.project();
            assert!(p.loop_enabled, "drawing a range turns cycle on");
            assert!((p.loop_start_secs - 2.0).abs() < 0.05 && (p.loop_end_secs - 6.0).abs() < 0.05, "{} {}", p.loop_start_secs, p.loop_end_secs);
        }
        click(&mut harness, "Cycle (C)");
        assert!(!engine.lock().unwrap().project().loop_enabled, "transport button toggles cycle off");
        harness.key_press(egui::Key::C);
        harness.step();
        harness.step();
        assert!(engine.lock().unwrap().project().loop_enabled, "C toggles it back on");
    }

    #[test]
    fn piano_roll_marquee_selection_and_cmd_drag_copy() {
        let (mut harness, engine) = harness_with_track();
        let notes = vec![
            Note { start_secs: 0.0, length_secs: 0.25, pitch: 60, velocity: 100, tab: None },
            Note { start_secs: 0.5, length_secs: 0.25, pitch: 62, velocity: 100, tab: None },
            Note { start_secs: 1.5, length_secs: 0.25, pitch: 72, velocity: 100, tab: None },
        ];
        {
            let mut e = engine.lock().unwrap();
            let t = e.add_instrument_track("Keys", "synth").unwrap();
            let c = e.add_midi_clip(t, 0.0, 2.0, None).unwrap();
            e.set_midi_notes(t, c, notes.clone()).unwrap();
        }
        harness.state_mut().snap_mode = timeline::SnapMode::Off;
        // The piano roll shares the arrangement's zoom; zoom in so the notes are wide.
        harness.state_mut().view.px_per_sec = 400.0;
        harness.state_mut().open_piano_roll(RegionRef::midi(1, 0));
        harness.step();
        harness.step();

        // Rectangle around the first two notes only.
        let a = piano_roll::tests::note_center(harness.state(), &notes[0]);
        let b = piano_roll::tests::note_center(harness.state(), &notes[1]);
        let from = egui::pos2(a.x - 20.0, b.y - 12.0);
        let to = egui::pos2(b.x + 20.0, a.y + 12.0);
        press_drag(&mut harness, from, to, egui::Modifiers::NONE);
        let mut sel = harness.state().piano.selected.clone();
        sel.sort_by_key(|n| n.pitch);
        assert_eq!(sel, notes[..2].to_vec(), "marquee selects the two notes it covers");

        // ⌘-drag one of them: both are copied 0.5 s later, originals stay.
        let pps = harness.state().piano.geo.unwrap().3;
        press_drag(&mut harness, a, a + egui::vec2(0.5 * pps, 0.0), egui::Modifiers::COMMAND);
        let after = engine.lock().unwrap().project().tracks[1].midi_clips[0].notes.clone();
        assert_eq!(after.len(), 5, "two copies added: {after:?}");
        for n in &notes {
            assert!(after.contains(n), "original {n:?} kept");
        }
        assert!(after.iter().any(|n| n.pitch == 60 && (n.start_secs - 0.5).abs() < 0.02));
        assert!(after.iter().any(|n| n.pitch == 62 && (n.start_secs - 1.0).abs() < 0.02));
    }

    #[test]
    fn recent_projects_are_tracked_persisted_and_reopenable() {
        let (mut harness, engine) = harness_with_track();
        let dir = std::env::temp_dir().join(format!("opentake-recent-{}", std::process::id()));
        let settings = dir.join("settings.json");
        harness.state_mut().settings_path = Some(settings.clone());
        let (a, b) = (dir.join("a.json"), dir.join("b.json"));
        // Saves made outside the GUI (e.g. by an agent) are picked up too.
        engine.lock().unwrap().save_project(Some(&a)).unwrap();
        harness.step();
        engine.lock().unwrap().add_track("Second").unwrap();
        engine.lock().unwrap().save_project(Some(&b)).unwrap();
        harness.step();
        assert_eq!(harness.state().recent, vec![b.clone(), a.clone()]);
        assert_eq!(crate::settings::Settings::load(&settings).recent_projects, vec![b.clone(), a.clone()], "persisted");

        // File → Open Recent → a.json reopens it (one track) and moves it to the top.
        click(&mut harness, "File");
        click(&mut harness, "Open Recent");
        let label = harness.state().recent_labels()[1].clone();
        click(&mut harness, &label);
        assert_eq!(engine.lock().unwrap().project_path(), Some(a.as_path()));
        assert_eq!(engine.lock().unwrap().project().tracks.len(), 1);
        harness.step();
        assert_eq!(harness.state().recent[0], a);

        click(&mut harness, "File");
        click(&mut harness, "Open Recent");
        click(&mut harness, "Clear Menu");
        assert!(harness.state().recent.is_empty());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn guitar_pro_import_dialog_creates_instrument_tracks() {
        let (mut harness, engine) = harness_with_track();
        let fixture = std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../opentake-project/tests/fixtures/simple.gp"));
        harness.state_mut().open_guitar_pro_dialog(fixture);
        harness.step();
        harness.step();
        // Untick the drum track, then import.
        click(&mut harness, "Drums");
        click(&mut harness, "Import");
        let e = engine.lock().unwrap();
        let names: Vec<_> = e.project().tracks.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["Guitar", "Guitar"], "existing track + imported guitar only");
        let imported = &e.project().tracks[1];
        assert!(imported.is_instrument() && !imported.midi_clips[0].notes.is_empty());
        drop(e);
        assert!(harness.state().gp_import.is_none(), "dialog closes after import");
    }

    #[test]
    fn tablature_view_follows_the_timeline_and_click_seeks() {
        let (mut harness, engine) = harness_with_track();
        let fixture = std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../opentake-project/tests/fixtures/simple.gp"));
        harness.state_mut().open_guitar_pro_dialog(fixture);
        harness.step();
        harness.step();
        click(&mut harness, "Import");
        // The tab opens by itself after importing tabbed tracks, on the guitar.
        assert_eq!(harness.state().bottom, Some(BottomTab::Tab));
        assert_eq!(harness.state().selected_track, Some(1));
        let tab = harness.get_by_label("Tab staff").rect();

        // A click lands on the same time as the arrangement column at that x.
        harness.state_mut().snap_mode = timeline::SnapMode::Off;
        let x = harness.state().view.x(4.0);
        let at = egui::pos2(x, tab.center().y);
        harness.hover_at(at);
        harness.step();
        for pressed in [true, false] {
            harness.event(egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE });
        }
        harness.step();
        harness.step();
        let pos = engine.lock().unwrap().status().position_secs;
        assert!((pos - 4.0).abs() < 0.05, "seeked to {pos}");

        // T toggles it, as does the View menu.
        harness.key_press(egui::Key::T);
        harness.step();
        assert_eq!(harness.state().bottom, None);
        click(&mut harness, "View");
        click(&mut harness, &menu_label(MenuCommand::ToggleTab));
        assert_eq!(harness.state().bottom, Some(BottomTab::Tab));
    }

    #[test]
    fn snap_toggle_restores_previous_mode() {
        let (mut harness, _engine) = harness_with_track();
        harness.state_mut().snap_mode = timeline::SnapMode::Sixteenth;
        click(&mut harness, "Edit");
        click(&mut harness, &menu_label(MenuCommand::ToggleSnap));
        assert_eq!(harness.state().snap_mode, timeline::SnapMode::Off);
        click(&mut harness, "Edit");
        click(&mut harness, &menu_label(MenuCommand::ToggleSnap));
        assert_eq!(harness.state().snap_mode, timeline::SnapMode::Sixteenth);
    }

    #[test]
    fn metronome_button_and_menu_toggle_click() {
        let (mut harness, engine) = harness_with_track();
        click(&mut harness, "Metronome (K) — right-click for volume");
        assert!(engine.lock().unwrap().project().metronome_enabled);
        click(&mut harness, "Transport");
        click(&mut harness, &menu_label(MenuCommand::ToggleMetronome));
        assert!(!engine.lock().unwrap().project().metronome_enabled);
    }

    #[test]
    fn in_window_menu_runs_commands() {
        let (mut harness, engine) = harness_with_track();
        click(&mut harness, "Track");
        click(&mut harness, &menu_label(MenuCommand::NewTrack));
        assert_eq!(engine.lock().unwrap().project().tracks.len(), 2);
        // Delete is only enabled with a selection; the new track is selected.
        click(&mut harness, "Track");
        click(&mut harness, &menu_label(MenuCommand::DeleteTrack));
        assert_eq!(engine.lock().unwrap().project().tracks.len(), 1);
    }

    #[test]
    fn tracks_delete_with_header_button_and_keys() {
        let (mut harness, engine) = harness_with_track();
        for name in ["Bass", "Keys", "Vox"] {
            engine.lock().unwrap().add_track(name).unwrap();
        }
        let names = |e: &SharedEngine| e.lock().unwrap().project().tracks.iter().map(|t| t.name.clone()).collect::<Vec<_>>();
        harness.state_mut().selected_track = Some(1);
        harness.step();
        click(&mut harness, "Delete Track");
        assert_eq!(names(&engine), ["Guitar", "Keys", "Vox"]);

        harness.state_mut().selected_track = Some(1);
        harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Backspace);
        harness.step();
        assert_eq!(names(&engine), ["Guitar", "Vox"], "⌘⌫ deletes the selected track");

        // ⌫ with no regions selected deletes the selected track too.
        harness.state_mut().selected_track = Some(1);
        harness.state_mut().selection.clear();
        harness.key_press(egui::Key::Backspace);
        harness.step();
        assert_eq!(names(&engine), ["Guitar"]);
        harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Z);
        harness.step();
        assert_eq!(names(&engine), ["Guitar", "Vox"], "undo brings it back");
    }

    #[test]
    fn piano_roll_shares_the_arrangement_zoom() {
        let (mut harness, engine) = harness_with_track();
        let note = Note { start_secs: 0.5, length_secs: 0.5, pitch: 60, velocity: 100, tab: None };
        {
            let mut e = engine.lock().unwrap();
            let t = e.add_instrument_track("Keys", "synth").unwrap();
            let c = e.add_midi_clip(t, 2.0, 4.0, None).unwrap();
            e.set_midi_notes(t, c, vec![note]).unwrap();
        }
        harness.state_mut().open_piano_roll(RegionRef::midi(1, 0));
        harness.step();
        harness.step();
        let check = |h: &Harness<'static, DawApp>| {
            let x = piano_roll::tests::note_center(h.state(), &note).x;
            let want = h.state().view.x(2.0 + 0.5 + 0.25);
            assert!((x - want).abs() < 0.5, "note at {x}, arrangement at {want}");
        };
        check(&harness);
        let before = harness.state().view.px_per_sec;
        harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::ArrowRight);
        harness.step();
        harness.step();
        assert!((harness.state().view.px_per_sec / before - timeline::ZOOM_STEP).abs() < 1e-3, "⌘→ zooms in");
        check(&harness);
        harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::ArrowLeft);
        harness.step();
        assert!((harness.state().view.px_per_sec - before).abs() < 1e-3, "⌘← zooms back out");

        // Z fits the selection (here: the MIDI region, 2–6 s) into the lanes.
        harness.state_mut().selection = vec![RegionRef::midi(1, 0)];
        harness.key_press(egui::Key::Z);
        harness.step();
        harness.step();
        let lanes = harness.state().view.lanes();
        let (x0, x1) = (harness.state().view.x(2.0), harness.state().view.x(6.0));
        assert!(x0 >= lanes.min && x1 <= lanes.max && x1 - x0 > lanes.span() * 0.9, "{x0}..{x1} in {lanes:?}");
        check(&harness);
    }

    #[test]
    fn transport_buttons_drive_engine() {
        let (mut harness, _engine) = harness_with_track();
        click(&mut harness, "Go to Beginning (Home)");
        click(&mut harness, "Inspector (I)");
        assert!(!harness.state().show_inspector);
    }

    fn write_tone(path: &std::path::Path, freq: f32, secs: f32, decay: f32) {
        let spec = hound::WavSpec { channels: 1, sample_rate: 48_000, bits_per_sample: 32, sample_format: hound::SampleFormat::Float };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for i in 0..(48_000.0 * secs) as usize {
            let t = i as f32 / 48_000.0;
            let env = (-decay * (t % 0.5)).exp();
            w.write_sample((t * freq * std::f32::consts::TAU).sin() * env * 0.8).unwrap();
        }
        w.finalize().unwrap();
    }

    /// Renders a demo session to a PNG for visual review:
    /// `OPENTAKE_SCREENSHOT=/tmp/ui.png cargo test -p opentake render_screenshot -- --ignored`
    #[test]
    #[ignore = "renders with wgpu; run manually for visual checks"]
    fn render_screenshot() {
        let out = std::env::var("OPENTAKE_SCREENSHOT").unwrap_or_else(|_| "opentake-ui.png".into());
        let dir = std::env::temp_dir().join(format!("opentake-shot-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (drums, bass) = (dir.join("Drums.wav"), dir.join("Bass.wav"));
        write_tone(&drums, 70.0, 8.73, 14.0);
        write_tone(&bass, 55.0, 8.73, 3.0);

        let engine: SharedEngine = Arc::new(Mutex::new(Engine::new()));
        {
            let mut e = engine.lock().unwrap();
            e.set_tempo(110.0, None).unwrap();
            e.set_metronome(Some(true), None).unwrap();
            for (name, file) in [("Drums", &drums), ("Bass", &bass)] {
                let t = e.add_track(name).unwrap();
                e.add_clip(t, file, 0.0).unwrap();
            }
            e.add_clip(0, &drums, 8.73).unwrap();
            e.split_clip(1, 0, 3.27).unwrap();
            e.trim_clip(1, 1, Some(3.8), None).unwrap();
            e.update_track(1, TrackUpdate { volume: Some(0.6), pan: Some(-0.25), ..Default::default() }).unwrap();
            e.scan_plugins();
            let plugin = e.scanned_plugins().first().map(|p| p.bundle.to_string_lossy().into_owned());
            if let Some(p) = plugin {
                e.insert_plugin(1, &p, None).unwrap();
            }
            // Instrument track with a chord progression.
            let keys = e.add_instrument_track("Keys", "synth").unwrap();
            let beat = 60.0 / 110.0;
            let clip = e.add_midi_clip(keys, 0.0, 16.0 * beat, Some("Chords")).unwrap();
            let mut notes = Vec::new();
            for (bar, chord) in [[60u8, 64, 67], [57, 60, 64], [53, 57, 60], [55, 59, 62]].iter().enumerate() {
                for (k, &p) in chord.iter().enumerate() {
                    let start = bar as f64 * 4.0 * beat;
                    notes.push(Note { start_secs: start, length_secs: 3.5 * beat, pitch: p, velocity: 80 + 15 * k as u8, tab: None });
                }
                notes.push(Note { start_secs: (bar as f64 * 4.0 + 2.0) * beat, length_secs: beat, pitch: chord[2] + 5, velocity: 110, tab: None });
            }
            e.set_midi_notes(keys, clip, notes).unwrap();
            e.add_track("Audio 4").unwrap();
            e.set_loop(Some(true), Some(2.0 * 4.0 * beat), Some(3.0 * 4.0 * beat)).unwrap();
            e.seek(5.2).unwrap();
        }

        let app_engine = engine.clone();
        let mut harness = Harness::builder()
            .with_size([1360.0, 900.0])
            .with_pixels_per_point(2.0)
            .with_step_dt(1.0 / 60.0)
            .wgpu()
            .build_eframe(move |cc| {
                theme::apply(&cc.egui_ctx);
                DawApp::new(app_engine, Some("http://127.0.0.1:8808/mcp".into()))
            });
        harness.state_mut().selected_track = Some(1);
        harness.state_mut().selection = vec![RegionRef::audio(1, 1), RegionRef::audio(0, 0)];
        harness.state_mut().snap_mode = timeline::SnapMode::Sixteenth;
        harness.state_mut().view.px_per_sec = 80.0;
        harness.state_mut().bottom = Some(BottomTab::Mixer);
        for _ in 0..4 {
            harness.step();
        }
        harness.render().expect("render").save(&out).expect("save png");

        // Second shot: the piano roll on the chord region.
        harness.state_mut().open_piano_roll(RegionRef::midi(2, 0));
        harness.state_mut().selected_track = Some(2);
        for _ in 0..4 {
            harness.step();
        }
        let piano = out.replace(".png", "-piano.png");
        harness.render().expect("render").save(&piano).expect("save png");

        // Third shot: tablature from the Guitar Pro fixture (or OPENTAKE_SCREENSHOT_GP, seeking
        // to OPENTAKE_SCREENSHOT_AT seconds).
        let fixture = std::env::var("OPENTAKE_SCREENSHOT_GP")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../opentake-project/tests/fixtures/simple.gp")));
        let at: f64 = std::env::var("OPENTAKE_SCREENSHOT_AT").ok().and_then(|v| v.parse().ok()).unwrap_or(8.5);
        let opts = opentake_engine::GuitarProImport { set_tempo: false, ..Default::default() };
        let created = engine.lock().unwrap().import_guitar_pro(&fixture, &opts).unwrap();
        engine.lock().unwrap().seek(at).unwrap();
        harness.state_mut().selected_track = created.first().copied();
        harness.state_mut().bottom = Some(BottomTab::Tab);
        harness.state_mut().view.px_per_sec = 90.0;
        for _ in 0..4 {
            harness.step();
        }
        let tab = out.replace(".png", "-tab.png");
        harness.render().expect("render").save(&tab).expect("save png");
        std::fs::remove_dir_all(&dir).ok();
        eprintln!("wrote {out}, {piano} and {tab}");
    }
}
