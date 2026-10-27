//! Desktop GUI (egui). Shares the engine with the MCP server, so changes made by an
//! agent show up live and vice versa.
//!
//! Each frame: lock the engine briefly to take a [`Snapshot`], draw from the snapshot
//! while collecting [`Action`]s, then lock again to apply them.

mod menu;
mod mixer;
mod eq_view;
mod export_view;
mod flex_view;
mod midi_import_view;
mod fx_view;
mod piano_roll;
mod tab_view;
mod theme;
mod timeline;
mod widgets;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use opentake_engine::project::{AutomationLane, AutomationMode, AutomationTarget, Curve, Note, Project};
use opentake_engine::midi_input::MidiSelection;
use opentake_engine::{AudioConfig, AutomationParam, DeviceList, Engine, EngineStatus, HistoryInfo, LaneOwner, MidiInputs, ParamView, PluginEditor, RegionData, SendUpdate, SlotRef, TrackUpdate, Waveform};
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
            crate::open_files::set_context(&cc.egui_ctx);
            crate::open_files::install();
            let has_tracks = !engine.lock().map(|e| e.project().tracks.is_empty()).unwrap_or(true);
            let mut app = DawApp::new(engine, mcp_url);
            let settings = settings_path.as_deref().map(crate::settings::Settings::load).unwrap_or_default();
            app.recent = settings.existing_recent();
            app.tab_scale = settings.tab_scale.unwrap_or(1.0).clamp(0.75, 2.5);
            app.tab_paper = settings.tab_paper.unwrap_or(false);
            app.midi_saved = settings.midi_selection();
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
    /// Installed Audio Units (macOS).
    pub audio_units: Vec<opentake_engine::AudioUnitInfo>,
    pub scan_errors: Vec<String>,
    pub plugin_paths: Vec<PathBuf>,
    /// Instruments for new instrument tracks: (display name, id for the engine).
    pub instruments: Vec<(String, String)>,
    pub history: HistoryInfo,
    /// An imported song's own tempo differs from the project's tempo map.
    pub song_tempo_differs: bool,
    /// Parameters of plugins whose windows are open, keyed by (track, slot).
    pub params: HashMap<(usize, SlotRef), Vec<ParamView>>,
    /// Analyzer taps of built-in EQs whose windows are open.
    pub taps: HashMap<(usize, SlotRef), Arc<opentake_engine::eq::Tap>>,
    /// Gain-reduction meters of built-in compressors/limiters whose windows are open.
    pub meters: HashMap<(usize, SlotRef), Arc<opentake_engine::dynamics::GainMeter>>,
    /// Band meters of built-in multiband compressors whose windows are open.
    pub multiband: HashMap<(usize, SlotRef), Arc<opentake_engine::multiband::MultibandMeter>>,
    /// Open plug-in windows whose plug-in takes a side chain.
    pub side_chain_inputs: std::collections::HashSet<(usize, SlotRef)>,
    /// Open plug-in windows whose plug-in has an editor of its own.
    pub native_editors: std::collections::HashSet<(usize, SlotRef)>,
    /// What can be automated, per track index, for tracks showing their automation.
    pub auto_params: HashMap<usize, Vec<AutomationParam>>,
    /// The chosen MIDI inputs, and while recording the notes played so far (timeline seconds).
    pub midi_selection: MidiSelection,
    pub recording_notes: Vec<Note>,
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

/// Track index used with `SlotRef::Master` (the master bus isn't a track).
pub(crate) const MASTER_UI: usize = usize::MAX;

pub(crate) enum Action {
    Play,
    Stop,
    Record,
    Seek(f64),
    SetTempo(f64),
    SetTimeSignature((u32, u32)),
    SetTempoChanges { changes: Vec<opentake_engine::project::TempoChange>, merge: Option<String> },
    SetCountIn { bars: Option<u32>, on_play: Option<bool> },
    SetAutoMonitor(bool),
    /// Make the project's tempo map follow an imported song's tempo.
    AdoptSongTempo,
    InsertMasterPlugin(String),
    /// Several parameters as one undo step: (track, slot, values, label).
    SetParams(usize, SlotRef, Vec<(u32, f64)>, String),
    SetMasterVolume(f32),
    SetMetronome { enabled: Option<bool>, volume: Option<f32> },
    StartAudio(AudioConfig),
    StopAudio,
    /// Manual recording latency correction (ms) for the current device setup.
    SetRecordOffset(f64),
    /// Loopback-measure the recording latency on an input channel.
    MeasureLatency(usize),
    /// Rescan audio devices and MIDI inputs.
    RefreshDevices,
    SetMidiInputs(MidiSelection),
    NewProject,
    OpenProject(PathBuf),
    SaveProject(Option<PathBuf>),
    Undo,
    Redo,
    AddTrack,
    AddInstrumentTrack(String),
    /// New bus, with these tracks routed to it.
    AddBus(Vec<usize>),
    /// Route a track to a bus (by index) or the master (`None`).
    SetOutput(usize, Option<usize>),
    /// Send from a track to a bus (track indices); change or remove a send.
    AddSend(usize, usize),
    UpdateSend(usize, usize, SendUpdate),
    RemoveSend(usize, usize),
    /// A new bus fed by a new send (or, with `send`, an existing one moved there); `reverb`
    /// names it "Reverb" and puts the built-in reverb on it.
    SendToNewBus { track: usize, send: Option<usize>, reverb: bool },
    RemoveTrack(usize),
    UpdateTrack(usize, TrackUpdate),
    AddClip { track: Option<usize>, file: PathBuf, position: f64 },
    RemoveRegions(Vec<RegionRef>),
    /// Move (or with `copy`, duplicate) regions by a time and track offset.
    MoveRegions { regions: Vec<RegionRef>, delta_secs: f64, delta_track: isize, copy: bool },
    SplitRegions { regions: Vec<RegionRef>, at: f64 },
    /// Play another take of a take folder, or delete one.
    SelectTake { track: usize, clip: usize, take: usize },
    DeleteTake { track: usize, clip: usize, take: usize },
    /// A take folder's new comp (a swipe on a take lane; `merge` makes the swipe one step).
    SetComp { track: usize, clip: usize, comp: Vec<opentake_engine::project::CompSegment>, merge: String },
    ClearComp { track: usize, clip: usize },
    FlattenComp { track: usize, clip: usize },
    /// Open or close a take folder's take lanes.
    ShowTakes { track: usize, clip: usize, show: bool },
    TrimRegion { region: RegionRef, start: Option<f64>, end: Option<f64> },
    /// Loop regions (or stop looping them), and turn looped regions' repeats into regions.
    LoopRegions { regions: Vec<RegionRef>, to: opentake_engine::LoopTo },
    ConvertLoops(Vec<RegionRef>),
    /// Pitch-shift / time-stretch an audio region.
    PitchStretch { track: usize, clip: usize, ps: opentake_engine::PitchStretch },
    MoveTrack { from: usize, to: usize },
    /// Region gain in dB, or normalize the region's peak to 0 dBFS.
    ClipGain { track: usize, clip: usize, gain_db: f32 },
    /// Region fades (a handle drag or the inspector; drags merge into one undo step).
    ClipFades { track: usize, clip: usize, update: opentake_engine::FadeUpdate },
    Crossfade { track: usize, a: usize, b: usize, secs: f64 },
    /// Merge (join) audio regions of a track into one new region.
    MergeRegions { track: usize, clips: Vec<usize> },
    NormalizeClip { track: usize, clip: usize },
    /// Paste the clipboard with its top-most track at `track`, starting at `position`.
    Paste { track: usize, position: f64 },
    SetLoop { enabled: Option<bool>, start: Option<f64>, end: Option<f64> },
    /// Markers, by id: a new one at a time ("Marker N" without a name), an edit (`merge`
    /// makes a drag one step), deleting one, and cycling its section.
    AddMarker { time: f64, name: Option<String> },
    UpdateMarker { id: u32, update: opentake_engine::MarkerUpdate, merge: Option<String> },
    RemoveMarker(u32),
    CycleMarker(u32),
    ImportGuitarPro { path: PathBuf, options: opentake_engine::GuitarProImport },
    ImportMidiFile { path: PathBuf, options: opentake_engine::MidiFileImport },
    ExportMidiFile(PathBuf),
    AddMidiRegion { track: usize, position: f64, length: f64 },
    SetNotes { track: usize, clip: usize, notes: Vec<Note> },
    /// Quantize a region's notes (these indices, or all).
    Quantize { track: usize, clip: usize, notes: Option<Vec<usize>>, q: opentake_engine::project::Quantize },
    /// Quantize every note of the MIDI regions among these.
    QuantizeRegions { regions: Vec<RegionRef>, q: opentake_engine::project::Quantize },
    SetRecordQuantize(Option<opentake_engine::project::Quantize>),
    /// Replace one controller lane of a MIDI region.
    SetControllers { track: usize, clip: usize, kind: opentake_engine::project::Controller, events: Vec<opentake_engine::project::ControllerEvent> },
    LiveNote { track: usize, pitch: u8, velocity: u8 },
    ScanPlugins,
    AddPluginPath(PathBuf),
    RemovePluginPath(PathBuf),
    RestoreDefaultPluginPaths,
    InsertPlugin(usize, String),
    SetInstrument { track: usize, query: Option<String> },
    RemovePlugin(usize, SlotRef),
    SetBypass(usize, SlotRef, bool),
    /// Key a slot from a track (index; `None` = no side chain), tapped pre-fader or not.
    SetSideChain(usize, SlotRef, Option<usize>, bool),
    SetParam(usize, SlotRef, u32, f64),
    /// Open a plug-in's own editor window (OpenTake's window for built-ins, and when the
    /// plug-in has none).
    OpenEditor(usize, SlotRef),
    /// Show or hide automation under a track (`None` = every track).
    ShowAutomation { track: Option<usize>, show: bool },
    /// Show or hide the master bus's automation lanes.
    ShowMasterAutomation(bool),
    /// New lane for `target` (`None` = the next thing without one).
    AddLane { owner: LaneOwner, target: Option<AutomationTarget> },
    RemoveLane { owner: LaneOwner, lane: usize },
    SetLaneTarget { owner: LaneOwner, lane: usize, target: AutomationTarget },
    SetLaneMode { owner: LaneOwner, lane: usize, mode: AutomationMode },
    SetLaneShown { owner: LaneOwner, lane: usize, shown: bool },
    ClearLane { owner: LaneOwner, lane: usize },
    AddPoint { owner: LaneOwner, lane: usize, time: f64, value: f32 },
    /// Drag a point; `merge` makes the drag one undo step.
    MovePoint { owner: LaneOwner, lane: usize, index: usize, time: f64, value: f32, merge: String },
    DeletePoints { owner: LaneOwner, lane: usize, indices: Vec<usize> },
    SetPointCurve { owner: LaneOwner, lane: usize, index: usize, curve: Curve },
    /// A control with an automation lane moved (normalized value): the lane decides
    /// (see `Engine::control_automation`), else `plain` applies.
    AutomatedControl { owner: LaneOwner, target: AutomationTarget, value: f32, plain: Box<Action> },
    /// An automated control was let go (ends a Touch write).
    ReleaseControl { owner: LaneOwner, target: AutomationTarget },
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
    /// Tablature view size (1.0 = normal) and paper (white) mode; saved in the settings.
    pub tab_scale: f32,
    pub tab_paper: bool,
    pub piano: piano_roll::PianoRoll,
    pub open_plugins: Vec<(usize, SlotRef)>,
    /// Plug-ins' own editor windows (native, outside egui).
    editors: Vec<NativeEditor>,
    /// Track being dragged to a new place (lane headers or mixer strips).
    pub track_drag: Option<usize>,
    devices: Option<DeviceList>,
    /// MIDI inputs as last scanned (or why they couldn't be), the choice last saved in the
    /// settings, and the note count last seen with when it changed (the activity light).
    pub midi_ports: Option<Result<MidiInputs, String>>,
    midi_saved: MidiSelection,
    midi_seen: (u64, Option<Instant>),
    audio_cfg: AudioConfig,
    show_audio_settings: bool,
    /// Input channel for the loopback latency measurement.
    latency_channel: usize,
    show_mcp_info: bool,
    pub show_plugin_manager: bool,
    show_instrument_picker: bool,
    /// Open "Import Guitar Pro" dialog.
    pub gp_import: Option<GpImportDialog>,
    /// Open "Import MIDI File" dialog.
    pub midi_import: Option<midi_import_view::MidiImportDialog>,
    /// Open "Export Audio" dialog, and the export running in the background.
    pub export_dialog: Option<export_view::ExportDialog>,
    export_task: Option<export_view::ExportTask>,
    /// A region pitch/stretch render running in the background.
    flex_task: Option<flex_view::FlexTask>,
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
    /// Automatable parameters per track id: (slot ids they were listed for, when, list).
    auto_params: HashMap<u64, (Vec<u32>, Instant, Vec<AutomationParam>)>,
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

/// A track's (or with `MASTER_UI`, the master's) plug-in slot.
fn slot_at(project: &Project, t: usize, s: SlotRef) -> Option<&opentake_engine::project::PluginSlot> {
    match s {
        SlotRef::Master(i) => project.master_plugins.get(i),
        SlotRef::Instrument => project.tracks.get(t).and_then(|tr| tr.instrument.as_ref()),
        SlotRef::Effect(i) => project.tracks.get(t).and_then(|tr| tr.plugins.get(i)),
    }
}

/// "Track — Plug-in", the title of a plug-in's windows.
fn plugin_title(project: &Project, t: usize, s: SlotRef) -> Option<String> {
    let slot = slot_at(project, t, s)?;
    let owner = if matches!(s, SlotRef::Master(_)) { "Stereo Out" } else { project.tracks.get(t)?.name.as_str() };
    Some(format!("{owner} — {}", short_plugin_name(&slot.name)))
}

/// An open plug-in editor window, by the instance it edits (slots move; instances don't).
struct NativeEditor {
    instance: u64,
    editor: PluginEditor,
    title: String,
    /// OpenTake's window for the slot was open: it comes back when this one is closed.
    restore: bool,
}

impl DawApp {
    fn new(engine: SharedEngine, mcp_url: Option<String>) -> Self {
        engine.lock().unwrap_or_else(|p| p.into_inner()).set_editor_host(true);
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
            tab_scale: 1.0,
            tab_paper: false,
            piano: piano_roll::PianoRoll::default(),
            open_plugins: Vec::new(),
            editors: Vec::new(),
            track_drag: None,
            devices: None,
            midi_ports: None,
            midi_saved: MidiSelection::All,
            midi_seen: (0, None),
            audio_cfg: AudioConfig::default(),
            show_audio_settings: false,
            latency_channel: 0,
            show_mcp_info: false,
            show_plugin_manager: false,
            show_instrument_picker: false,
            gp_import: None,
            midi_import: None,
            export_dialog: None,
            export_task: None,
            flex_task: None,
            settings_path: None,
            recent: Vec::new(),
            seen_project_path: None,
            #[cfg(target_os = "macos")]
            native_menu: None,
            name_buf: (None, String::new()),
            message: None,
            actions: Vec::new(),
            auto_params: HashMap::new(),
        }
    }

    pub(crate) fn act(&mut self, a: Action) {
        self.actions.push(a);
    }

    /// A control moved to `value` (normalized): through its automation lane if it has one
    /// that isn't off, else `plain`.
    pub(crate) fn control(&mut self, lanes: &[AutomationLane], owner: LaneOwner, target: AutomationTarget, value: f32, plain: Action) {
        if lanes.iter().any(|l| l.target == target && l.mode != AutomationMode::Off) {
            self.act(Action::AutomatedControl { owner, target, value, plain: Box::new(plain) });
        } else {
            self.act(plain);
        }
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

    /// Keep track-index state pointing at the same tracks after a track moved.
    fn track_moved(&mut self, from: usize, to: usize) {
        let map = |i: usize| match i {
            i if i == from => to,
            i if from < to && i > from && i <= to => i - 1,
            i if to < from && i >= to && i < from => i + 1,
            i => i,
        };
        self.selected_track = self.selected_track.map(map);
        for r in &mut self.selection {
            r.track = map(r.track);
        }
        for (t, _) in &mut self.open_plugins {
            if *t != MASTER_UI {
                *t = map(*t);
            }
        }
        if let Some((t, _)) = &mut self.piano.region {
            *t = map(*t);
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

    /// Apply edits made in plug-ins' own editors; close editors whose window was closed or
    /// whose plug-in left the project (removed, undone, another project loaded); keep the
    /// rest titled, sized and showing automation.
    fn poll_editors(&mut self) {
        let engine = self.engine.clone();
        let mut e = engine.lock().unwrap_or_else(|p| p.into_inner());
        if let Err(err) = e.apply_editor_edits() {
            tracing::warn!("plug-in editor: {err:#}");
        }
        // Editors asked for over MCP open here, on the main thread.
        for (t, s) in e.take_editor_requests() {
            let t = if matches!(s, SlotRef::Master(_)) { MASTER_UI } else { t };
            self.act(Action::OpenEditor(t, s));
        }
        if self.editors.is_empty() {
            return;
        }
        let (mut reopen, mut closed) = (Vec::new(), Vec::new());
        self.editors.retain_mut(|ed| {
            let Some((t, s)) = e.slot_of_instance(ed.instance) else { return false };
            let t = if matches!(s, SlotRef::Master(_)) { MASTER_UI } else { t };
            if !ed.editor.is_open() {
                if ed.restore {
                    reopen.push((t, s));
                }
                closed.push(ed.instance);
                return false;
            }
            let title = plugin_title(e.project(), t, s).unwrap_or_default();
            if title != ed.title {
                ed.editor.set_title(&title);
                ed.title = title;
            }
            ed.editor.idle();
            true
        });
        for w in reopen {
            if !self.open_plugins.contains(&w) {
                self.open_plugins.push(w);
            }
        }
        for instance in closed {
            if let Err(err) = e.editor_closed(instance) {
                tracing::warn!("plug-in editor: {err:#}");
            }
        }
        let open: Vec<u64> = self.editors.iter().map(|ed| ed.instance).collect();
        e.sync_editors(&open);
    }

    fn snapshot(&mut self) -> Snapshot {
        self.poll_editors();
        let open = self.open_plugins.clone();
        let mut cache = std::mem::take(&mut self.auto_params);
        let mut e = self.lock();
        // MIDI input plays the selected track when no instrument track is armed.
        if let Err(err) = e.select_track(self.selected_track) {
            tracing::warn!("MIDI input: {err:#}");
        }
        // Held and latched controls keep writing their lanes while playing.
        if let Err(err) = e.automation_tick() {
            tracing::warn!("automation: {err:#}");
        }
        let project = e.project().clone();
        // The parameter lists are re-read when the plug-ins change, and every few seconds.
        let mut auto_params = HashMap::new();
        let shown = project.tracks.iter().enumerate().filter(|(_, t)| t.show_automation).map(|(i, t)| (Some(i), t.id, t.slots().map(|s| s.id).collect::<Vec<u32>>()));
        let master = project.show_master_automation.then(|| (None, u64::MAX, project.master_plugins.iter().map(|s| s.id).collect()));
        for (owner, id, sig) in shown.chain(master) {
            if !cache.get(&id).is_some_and(|(s, at, _)| *s == sig && at.elapsed() < Duration::from_secs(3)) {
                if let Ok(list) = e.automation_params(owner) {
                    cache.insert(id, (sig, Instant::now(), list));
                }
            }
            if let Some((_, _, list)) = cache.get(&id) {
                auto_params.insert(owner.unwrap_or(MASTER_UI), list.clone());
            }
        }
        let mut waveforms = HashMap::new();
        // Every take of a folder too (take lanes and comps draw them).
        let files = project.tracks.iter().flat_map(|t| &t.clips).flat_map(|c| std::iter::once(&c.file).chain(c.takes.iter().map(|t| &t.file)));
        for file in files {
            if !waveforms.contains_key(file) {
                if let Ok(w) = e.waveform(file) {
                    waveforms.insert(file.clone(), w);
                }
            }
        }
        let taps = open.iter().filter_map(|&(t, s)| Some(((t, s), e.eq_tap(t, s)?))).collect();
        let meters = open.iter().filter_map(|&(t, s)| Some(((t, s), e.fx_meter(t, s)?))).collect();
        let multiband = open.iter().filter_map(|&(t, s)| Some(((t, s), e.multiband_meter(t, s)?))).collect();
        let side_chain_inputs = open.iter().copied().filter(|&(t, s)| e.supports_side_chain(t, s)).collect();
        let native_editors = open.iter().copied().filter(|&(t, s)| e.has_editor(t, s)).collect();
        let params = open.into_iter().filter_map(|(t, s)| Some(((t, s), e.slot_params(t, s).ok()?))).collect();
        let status = e.status();
        let snap = Snapshot {
            project_path: e.project_path().map(|p| p.to_path_buf()),
            recording_notes: if status.recording { e.recording_notes() } else { Vec::new() },
            midi_selection: e.midi_selection().clone(),
            status,
            scanned: e.scanned_plugins().to_vec(),
            audio_units: e.audio_units().to_vec(),
            scan_errors: e.scan_errors().to_vec(),
            plugin_paths: e.plugin_search_paths().to_vec(),
            instruments: e.available_instruments(),
            history: e.history_info(),
            song_tempo_differs: e.song_tempo_differs(),
            waveforms,
            params,
            taps,
            meters,
            multiband,
            side_chain_inputs,
            native_editors,
            auto_params,
            project,
        };
        drop(e);
        self.auto_params = cache;
        snap
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
                let recorded = e.stop()?;
                if recorded.count() > 0 {
                    self.info(format!("Recorded {} region(s)", recorded.count()));
                }
            }
            Action::Record => {
                let r = e.start_recording()?;
                self.info(format!("Recording {} track(s)", r.files.len() + r.midi_tracks.len()));
            }
            Action::Seek(t) => e.seek(t)?,
            Action::SetTempo(bpm) => e.set_tempo(bpm, None)?,
            Action::SetTimeSignature(sig) => e.set_tempo(e.project().tempo_bpm, Some(sig))?,
            Action::SetTempoChanges { changes, merge } => e.set_tempo_changes(changes, merge)?,
            Action::AdoptSongTempo => {
                e.adopt_song_tempo()?;
            }
            Action::SetCountIn { bars, on_play } => e.set_count_in(bars, on_play)?,
            Action::SetAutoMonitor(on) => e.set_auto_input_monitoring(on)?,
            Action::InsertMasterPlugin(query) => {
                let slot = e.insert_master_plugin(&query, None)?;
                self.open_plugins.push((MASTER_UI, SlotRef::Master(slot)));
            }
            Action::SetParams(t, s, values, label) => {
                // Automated parameters go through their lanes, the rest as one step.
                let owner = if matches!(s, SlotRef::Master(_)) { None } else { Some(t) };
                let mut plain = Vec::new();
                for (id, v) in values {
                    let target = e.param_target(owner, s, id)?;
                    if !e.control_automation(owner, target, v as f32)? {
                        plain.push((id, v));
                    }
                }
                if !plain.is_empty() {
                    e.set_slot_params(t, s, &plain, &label)?;
                }
            }
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
            Action::SetRecordOffset(ms) => e.set_record_offset(ms)?,
            Action::MeasureLatency(channel) => {
                let r = e.measure_latency(channel)?;
                self.info(format!(
                    "Round trip {:.1} ms (device reports {:.1} ms) — recordings now move {:+.1} ms more",
                    r.round_trip_ms, r.reported_ms, r.record_offset_ms
                ));
            }
            Action::RefreshDevices => {
                self.midi_ports = Some(e.list_midi_inputs().map_err(|err| format!("{err:#}")));
                self.devices = Some(e.list_devices()?);
            }
            Action::SetMidiInputs(selection) => {
                let inputs = e.set_midi_inputs(selection)?;
                if !inputs.errors.is_empty() {
                    self.error(format!("Couldn't open MIDI input {}", inputs.errors.join("; ")));
                }
                self.midi_ports = Some(Ok(inputs));
            }
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
            Action::AddBus(tracks) => {
                let name = format!("Bus {}", e.project().tracks.iter().filter(|t| t.bus).count() + 1);
                self.selected_track = Some(if tracks.is_empty() { e.add_bus(&name)? } else { e.create_bus_from(&tracks, &name)? });
            }
            Action::SetOutput(t, to) => e.set_track_output(t, to)?,
            Action::AddSend(t, bus) => {
                e.add_send(t, bus, 1.0, false)?;
            }
            Action::UpdateSend(t, k, u) => {
                e.update_send(t, k, u)?;
            }
            Action::RemoveSend(t, k) => {
                e.remove_send(t, k)?;
            }
            Action::SendToNewBus { track, send, reverb } => {
                let name = if reverb { "Reverb".to_string() } else { format!("Bus {}", e.project().tracks.iter().filter(|t| t.bus).count() + 1) };
                match send {
                    Some(k) => e.move_send_to_new_bus(track, k, &name)?,
                    None => e.send_to_new_bus(track, &name, reverb.then_some("reverb"), 1.0, false)?.0,
                };
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
            Action::SelectTake { track, clip, take } => {
                e.select_take(track, clip, take)?;
            }
            Action::DeleteTake { track, clip, take } => {
                e.remove_take(track, clip, take)?;
            }
            Action::SetComp { track, clip, comp, merge } => drop(e.set_comp(track, clip, comp, Some(merge))?),
            Action::ClearComp { track, clip } => drop(e.clear_comp(track, clip)?),
            Action::FlattenComp { track, clip } => drop(e.flatten_comp(track, clip)?),
            Action::ShowTakes { track, clip, show } => e.set_takes_shown(track, clip, show)?,
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
                        // To the beat the end lands on.
                        let pos = e.project().tracks[r.track].midi_clips[r.index].position_beats;
                        let u = opentake_engine::MidiClipUpdate { length_beats: Some(e.beats_at(end) - pos), ..Default::default() };
                        e.update_midi_clip(r.track, r.index, u)?;
                    }
                }
            },
            Action::LoopRegions { regions, to } => e.set_region_loop(&regions, to)?,
            Action::ConvertLoops(regions) => {
                // The passes are selected (other indices on their tracks shifted).
                self.selection = e.convert_loops(&regions)?;
            }
            Action::PitchStretch { track, clip, ps } => self.apply_pitch_stretch(e, track, clip, ps)?,
            Action::MoveTrack { from, to } => {
                e.move_track(from, to)?;
                self.track_moved(from, to);
                self.view.auto_sel = None;
            }
            Action::ClipGain { track, clip, gain_db } => {
                e.set_clip_gain(track, clip, gain_db)?;
            }
            Action::ClipFades { track, clip, update } => drop(e.set_clip_fades(track, clip, update)?),
            Action::Crossfade { track, a, b, secs } => drop(e.crossfade_clips(track, a, b, Some(secs))?),
            Action::MergeRegions { track, clips } => {
                let (index, _) = e.merge_clips(track, &clips)?;
                self.selection = vec![RegionRef::audio(track, index)];
            }
            Action::NormalizeClip { track, clip } => {
                let c = e.normalize_clip(track, clip, 0.0)?;
                self.info(format!("Normalized: region gain {:+.1} dB", 20.0 * c.gain.log10()));
            }
            Action::Paste { track, position } => {
                if let Some(clip) = self.clipboard.clone() {
                    let last = e.project().tracks.len().saturating_sub(1);
                    let at_beats = e.beats_at(position);
                    let items = clip
                        .items
                        .into_iter()
                        .map(|(dt, data)| {
                            let target = (track + dt).min(last);
                            let placed = match data {
                                RegionData::Audio(c) => RegionData::Audio(opentake_engine::project::Clip { position_secs: position + c.position_secs, ..c }),
                                RegionData::Midi(c) => RegionData::Midi(opentake_engine::project::MidiClip { position_beats: at_beats + c.position_beats, ..c }),
                            };
                            (target, placed)
                        })
                        .collect();
                    self.selection = e.insert_regions(items)?;
                    self.selected_track = Some(track);
                }
            }
            Action::SetLoop { enabled, start, end } => e.set_loop(enabled, start, end)?,
            Action::AddMarker { time, name } => self.view.marker_sel = Some(e.add_marker(time, name, None)?.id),
            Action::UpdateMarker { id, update, merge } => {
                e.update_marker(id, update, merge)?;
            }
            Action::RemoveMarker(id) => {
                e.remove_marker(id)?;
            }
            Action::CycleMarker(id) => {
                e.cycle_marker(id)?;
            }
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
            Action::ImportMidiFile { path, options } => {
                let created = e.import_midi_file(&path, &options)?;
                self.selection.clear();
                self.selected_track = created.first().copied();
                self.info(format!("Imported {} track(s) from {}", created.len(), path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()));
            }
            Action::ExportMidiFile(path) => {
                let n = e.export_midi_file(&path)?;
                self.info(format!("Exported {n} track(s) to {}", path.display()));
            }
            Action::AddMidiRegion { track, position, length } => {
                let i = e.add_midi_clip(track, position, length, None)?;
                self.open_piano_roll(RegionRef::midi(track, i));
            }
            Action::SetNotes { track, clip, notes } => e.set_midi_notes(track, clip, notes)?,
            Action::Quantize { track, clip, notes, q } => {
                let n = e.quantize_midi(track, clip, &q, notes.as_deref())?;
                self.info(format!("Quantized {n} note(s) to {}", q.grid.name()));
            }
            Action::QuantizeRegions { regions, q } => {
                let n = e.quantize_regions(&regions, &q)?;
                self.info(format!("Quantized {n} note(s) to {}", q.grid.name()));
            }
            Action::SetRecordQuantize(q) => e.set_record_quantize(q),
            Action::SetControllers { track, clip, kind, events } => drop(e.set_midi_controllers(track, clip, kind, events, true)?),
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
                    SlotRef::Master(s) => drop(e.remove_master_plugin(s)?),
                }
                // Slot indices shift; simplest is to close this track's (or the master's) windows.
                let master = matches!(which, SlotRef::Master(_));
                self.open_plugins.retain(|&(pt, s)| if master { !matches!(s, SlotRef::Master(_)) } else { pt != t || matches!(s, SlotRef::Master(_)) });
            }
            Action::SetBypass(t, s, b) => e.set_slot_bypass(t, s, b)?,
            Action::SetSideChain(t, s, source, pre) => e.set_side_chain(t, s, source, pre)?,
            Action::SetParam(t, s, id, v) => {
                let owner = if matches!(s, SlotRef::Master(_)) { None } else { Some(t) };
                let target = e.param_target(owner, s, id)?;
                if !e.control_automation(owner, target, v as f32)? {
                    e.set_slot_param(t, s, id, v)?;
                }
            }
            Action::OpenEditor(t, s) => {
                let generic = self.open_plugins.contains(&(t, s));
                if !e.has_editor(t, s) {
                    if !generic {
                        self.open_plugins.push((t, s));
                    }
                    return Ok(());
                }
                let instance = slot_at(e.project(), t, s).map(|slot| slot.instance);
                if let Some(ed) = self.editors.iter().find(|ed| Some(ed.instance) == instance) {
                    ed.editor.bring_to_front();
                    return Ok(());
                }
                let title = plugin_title(e.project(), t, s).unwrap_or_default();
                match e.open_editor(t, s, &title) {
                    Ok((instance, editor)) => {
                        // The plug-in's editor replaces the generic one (back when it closes).
                        self.open_plugins.retain(|&x| x != (t, s));
                        self.editors.push(NativeEditor { instance, editor, title, restore: generic });
                    }
                    Err(err) => {
                        if !generic {
                            self.open_plugins.push((t, s));
                        }
                        return Err(err.context("showing its generic controls"));
                    }
                }
            }
            Action::ShowAutomation { track, show } => e.set_automation_shown(track, show)?,
            Action::ShowMasterAutomation(show) => e.set_master_automation_shown(show)?,
            Action::AddLane { owner, target } => {
                let target = match target {
                    Some(t) => t,
                    None => e.next_lane_target(owner)?,
                };
                e.add_lane(owner, target)?;
            }
            Action::RemoveLane { owner, lane } => {
                e.remove_lane(owner, lane)?;
                self.view.auto_sel = None;
            }
            Action::SetLaneTarget { owner, lane, target } => e.set_lane_target(owner, lane, target)?,
            Action::SetLaneMode { owner, lane, mode } => e.set_lane_mode(owner, lane, mode)?,
            Action::SetLaneShown { owner, lane, shown } => e.set_lane_shown(owner, lane, shown)?,
            Action::ClearLane { owner, lane } => {
                e.clear_automation(owner, lane, None, None)?;
                self.view.auto_sel = None;
            }
            Action::AddPoint { owner, lane, time, value } => {
                let point = e.insert_point(owner, lane, opentake_engine::project::AutomationPoint::new(time, value))?;
                self.view.auto_sel = Some(timeline::AutoSel { owner, lane, point });
            }
            Action::MovePoint { owner, lane, index, time, value, merge } => e.move_point(owner, lane, index, time, value, Some(merge))?,
            Action::DeletePoints { owner, lane, indices } => {
                e.delete_points(owner, lane, &indices)?;
                self.view.auto_sel = None;
            }
            Action::SetPointCurve { owner, lane, index, curve } => e.set_point_curve(owner, lane, index, curve)?,
            Action::AutomatedControl { owner, target, value, plain } => {
                if !e.control_automation(owner, target, value)? {
                    self.apply(e, *plain)?;
                }
            }
            Action::ReleaseControl { owner, target } => e.release_automation(owner, target)?,
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
        let lanes = |owner: Option<usize>| match owner {
            Some(t) => p.tracks.get(t).map(|t| &t.automation),
            None => Some(&p.master_automation),
        };
        if self.view.auto_sel.is_some_and(|s| lanes(s.owner).and_then(|l| l.get(s.lane)).is_none_or(|l| s.point >= l.points.len())) {
            self.view.auto_sel = None;
        }
        if self.selected_track.is_some_and(|t| t >= p.tracks.len()) {
            self.selected_track = None;
        }
        self.selection.retain(|r| timeline::region_view(p, *r).is_some());
        self.open_plugins.retain(|&(t, s)| match s {
            SlotRef::Master(i) => i < p.master_plugins.len(),
            SlotRef::Instrument => p.tracks.get(t).is_some_and(|tr| tr.instrument.is_some()),
            SlotRef::Effect(i) => p.tracks.get(t).is_some_and(|tr| i < tr.plugins.len()),
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

    /// Remember the MIDI inputs when they change (here or by an agent).
    fn save_midi_inputs(&mut self, snap: &Snapshot) {
        if snap.midi_selection == self.midi_saved {
            return;
        }
        self.midi_saved = snap.midi_selection.clone();
        if let Some(path) = &self.settings_path {
            let mut settings = crate::settings::Settings::load(path);
            settings.set_midi_selection(&snap.midi_selection);
            if let Err(e) = settings.save(path) {
                self.error(format!("Could not save settings: {e:#}"));
            }
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
        let (space, rec, home, inspector, metronome, cycle, mixer, piano, tab, delete, escape, zoom_fit, automation) = ui.input(|i| {
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
                plain(i, egui::Key::A),
            )
        });
        if automation {
            self.toggle_automation(snap);
        }
        // Markers: M creates one at the playhead, ⌥← / ⌥→ jump between them.
        let marker_keys = ui.input(|i| {
            let alt = |k| i.key_pressed(k) && i.modifiers.alt && !i.modifiers.command && !i.modifiers.shift;
            [(plain(i, egui::Key::M), MenuCommand::CreateMarker), (alt(egui::Key::ArrowLeft), MenuCommand::PreviousMarker), (alt(egui::Key::ArrowRight), MenuCommand::NextMarker)]
        });
        for (_, cmd) in marker_keys.into_iter().filter(|k| k.0) {
            self.run_marker_command(cmd, snap);
        }
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
            self.view.marker_sel = None;
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
        self.loop_shortcut(ui, snap);
        self.quantize_shortcut(ui, snap);
    }

    /// Show automation under every track, or hide it if any track shows it (Logic's "A").
    fn toggle_automation(&mut self, snap: &Snapshot) {
        let shown = snap.project.tracks.iter().any(|t| t.show_automation);
        self.act(Action::ShowAutomation { track: None, show: !shown });
    }

    /// Delete: a selected automation point, then a selected controller event or notes in the
    /// open piano roll, take priority over the selected region.
    fn delete_selection(&mut self, snap: &Snapshot) {
        if let Some(id) = self.view.marker_sel.take().filter(|&id| snap.project.marker_index(id).is_some()) {
            self.act(Action::RemoveMarker(id));
            return;
        }
        if let Some(s) = self.view.auto_sel.take() {
            self.act(Action::DeletePoints { owner: s.owner, lane: s.lane, indices: vec![s.point] });
            return;
        }
        if self.bottom == Some(BottomTab::PianoRoll)
            && let (Some(i), Some((t, c))) = (self.piano.ctrl_sel.take(), self.piano.region)
            && let Some(lane) = snap.project.tracks.get(t).and_then(|tr| tr.midi_clips.get(c)).and_then(|clip| clip.lane(self.piano.ctrl_kind))
            && i < lane.events.len()
        {
            let mut events = lane.events.clone();
            events.remove(i);
            self.act(Action::SetControllers { track: t, clip: c, kind: lane.kind, events });
            return;
        }
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
            let map = snap.project.tempo_map();
            snap.project.tracks.iter().enumerate().flat_map(|(i, t)| timeline::regions_of(t, i, &map)).collect()
        } else {
            self.selection.iter().filter_map(|&r| timeline::region_view(&snap.project, r)).collect()
        };
        let from = views.iter().map(|v| v.position).fold(f64::INFINITY, f64::min);
        let to = views.iter().map(|v| v.position + v.length).fold(0.0, f64::max);
        if from.is_finite() {
            self.view.zoom_to_fit(from, to);
        } else {
            self.view.zoom_to_fit(0.0, snap.project.tempo_map().secs_of(9, 1.0));
        }
    }

    /// Change the tablature view's size / paper mode and remember them.
    pub(crate) fn set_tab_view_prefs(&mut self, scale: f32, paper: bool) {
        self.tab_scale = scale.clamp(0.75, 2.5);
        self.tab_paper = paper;
        if let Some(path) = &self.settings_path {
            let mut settings = crate::settings::Settings::load(path);
            settings.tab_scale = Some(self.tab_scale);
            settings.tab_paper = Some(paper);
            if let Err(e) = settings.save(path) {
                self.error(format!("Could not save settings: {e:#}"));
            }
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
                ui.checkbox(&mut dialog.set_tempo, "Use the song's tempo map (tempo and time-signature changes)");
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
            if opentake_engine::is_project_path(&f) {
                self.act(Action::OpenProject(f));
            } else if opentake_project::guitarpro::is_guitar_pro_path(&f) {
                self.open_guitar_pro_dialog(f);
            } else if opentake_engine::is_midi_file_path(&f) {
                self.open_midi_file_dialog(f);
            } else {
                self.act(Action::AddClip { track, file: f, position });
            }
        }
    }

    /// Files the OS asked us to open (double-click, "Open with", command line).
    fn handle_open_requests(&mut self, snap: &Snapshot) {
        for f in crate::open_files::take() {
            if opentake_engine::is_project_path(&f) {
                self.act(Action::OpenProject(f));
            } else if opentake_project::guitarpro::is_guitar_pro_path(&f) {
                self.open_guitar_pro_dialog(f);
            } else if opentake_engine::is_midi_file_path(&f) {
                self.open_midi_file_dialog(f);
            } else {
                self.act(Action::AddClip { track: None, file: f, position: snap.status.position_secs });
            }
        }
    }

    fn save_as(&mut self, snap: &Snapshot) {
        let name = format!("{}.{}", snap.project.name, opentake_engine::PROJECT_EXTENSION);
        if let Some(p) = rfd::FileDialog::new()
            .add_filter("OpenTake project", &[opentake_engine::PROJECT_EXTENSION])
            .add_filter("Reaper project", &["rpp"])
            .add_filter("DAWproject (Bitwig, Studio One, Cubase)", &["dawproject"])
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

    /// Quick view toggles, like Logic's control bar: inspector, mixer, piano roll, tablature.
    fn left_cluster(&mut self, ui: &mut egui::Ui) {
        ui.spacing_mut().item_spacing.x = 4.0;
        let size = vec2(30.0, 26.0);
        if widgets::icon_button(ui, Icon::Inspector, self.show_inspector, theme::ACCENT, "Inspector (I)", size).clicked() {
            self.show_inspector = !self.show_inspector;
        }
        let views = [(Icon::Mixer, BottomTab::Mixer, "Mixer (X)"), (Icon::PianoRoll, BottomTab::PianoRoll, "Piano Roll (P)"), (Icon::Tablature, BottomTab::Tab, "Tablature (T)")];
        for (icon, tab, a11y) in views {
            if widgets::icon_button(ui, icon, self.bottom == Some(tab), theme::ACCENT, a11y, size).clicked() {
                self.toggle_bottom(tab);
            }
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
            can_merge: self.audio_selection().is_some(),
            can_crossfade: self.audio_selection().is_some_and(|(_, c)| c.len() == 2),
            cycle_on: snap.project.loop_enabled,
            has_clipboard: self.clipboard.is_some(),
            inspector_visible: self.show_inspector,
            mixer_visible: self.bottom == Some(BottomTab::Mixer),
            tab_visible: self.bottom == Some(BottomTab::Tab),
            piano_roll_visible: self.bottom == Some(BottomTab::PianoRoll),
            automation_visible: snap.project.tracks.iter().any(|t| t.show_automation),
            master_automation_visible: snap.project.show_master_automation,
            snap_on: self.snap_mode != timeline::SnapMode::Off,
            metronome_on: snap.project.metronome_enabled,
            count_in_on: snap.project.count_in_bars > 0,
            count_in_on_play: snap.project.count_in_on_play,
            auto_monitor: snap.project.auto_input_monitoring,
            audio_running: snap.status.audio_running,
            playing: snap.status.playing,
            undo: snap.history.undo.clone(),
            redo: snap.history.redo.clone(),
            recent: self.recent_labels(),
            has_markers: !snap.project.markers.is_empty(),
            marker_to_delete: self.marker_to_delete(snap).is_some(),
        }
    }

    /// The marker Delete Marker removes: the selected one, else one at the playhead.
    fn marker_to_delete(&self, snap: &Snapshot) -> Option<u32> {
        let p = &snap.project;
        let at_playhead = || p.markers.iter().find(|m| (m.time_secs - snap.status.position_secs).abs() < 0.01).map(|m| m.id);
        self.view.marker_sel.filter(|&id| p.marker_index(id).is_some()).or_else(at_playhead)
    }

    /// Marker commands (the menus, the keys and the marker track's "+").
    pub(crate) fn run_marker_command(&mut self, cmd: MenuCommand, snap: &Snapshot) {
        let p = &snap.project;
        let pos = snap.status.position_secs;
        match cmd {
            MenuCommand::CreateMarker => {
                // At the playhead; with snapping on, at the nearest bar (as Logic rounds it).
                let map = p.tempo_map();
                let bar = map.bar_at(pos);
                let next = map.secs_of(bar.index + 2, 1.0);
                let t = match self.snap_mode {
                    timeline::SnapMode::Off => pos,
                    _ if pos - bar.secs <= next - pos => bar.secs,
                    _ => next,
                };
                match p.markers.iter().find(|m| (m.time_secs - t).abs() < 1e-3) {
                    Some(m) => self.view.marker_sel = Some(m.id),
                    None => self.act(Action::AddMarker { time: t, name: None }),
                }
            }
            MenuCommand::DeleteMarker => {
                if let Some(id) = self.marker_to_delete(snap) {
                    self.view.marker_sel = None;
                    self.act(Action::RemoveMarker(id));
                }
            }
            MenuCommand::PreviousMarker | MenuCommand::NextMarker => {
                let m = if cmd == MenuCommand::NextMarker { p.next_marker(pos) } else { p.previous_marker(pos) };
                if let Some(m) = m {
                    self.view.marker_sel = Some(m.id);
                    self.act(Action::Seek(m.time_secs));
                }
            }
            _ => {}
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
            (Modifiers::COMMAND | Modifiers::SHIFT, Key::E, MenuCommand::ExportAudio),
            (Modifiers::COMMAND | Modifiers::ALT, Key::N, MenuCommand::NewTrack),
            (Modifiers::COMMAND, Key::Comma, MenuCommand::AudioSettings),
            (Modifiers::COMMAND, Key::T, MenuCommand::SplitAtPlayhead),
            (Modifiers::COMMAND, Key::D, MenuCommand::DuplicateRegion),
            (Modifiers::COMMAND, Key::J, MenuCommand::MergeRegions),
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
        // Offsets from the first region: seconds, or quarter notes for MIDI (pasted on the beat).
        let start_beats = project.tempo_map().quarters_at(start);
        let items = views
            .iter()
            .map(|v| {
                let data = match v.to_data() {
                    RegionData::Audio(c) => RegionData::Audio(opentake_engine::project::Clip { position_secs: c.position_secs - start, ..c }),
                    RegionData::Midi(c) => RegionData::Midi(opentake_engine::project::MidiClip { position_beats: c.position_beats - start_beats, ..c }),
                };
                (v.r.track - top, data)
            })
            .collect();
        self.clipboard = Some(Clipboard { items });
    }

    /// The selected regions as (track, audio clip indices) when they're two or more audio
    /// regions on one track (what merge and crossfade work on).
    pub(crate) fn audio_selection(&self) -> Option<(usize, Vec<usize>)> {
        let track = self.selection.first()?.track;
        let ok = self.selection.len() >= 2 && self.selection.iter().all(|r| r.track == track && r.kind == RegionKind::Audio);
        ok.then(|| (track, self.selection.iter().map(|r| r.index).collect()))
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
            let map = p.tempo_map();
            let bar = map.bar_at(snap.status.position_secs);
            let end = map.secs_of(bar.index + 5, 1.0);
            self.act(Action::SetLoop { enabled: Some(true), start: Some(bar.secs), end: Some(end) });
        } else {
            self.act(Action::SetLoop { enabled: Some(!p.loop_enabled), start: None, end: None });
        }
    }

    fn run_command(&mut self, cmd: MenuCommand, ui: &egui::Ui, snap: &Snapshot) {
        match cmd {
            MenuCommand::NewProject => self.act(Action::NewProject),
            MenuCommand::Open => {
                let mut exts = vec!["otp", "rpp", "json"];
                exts.extend(opentake_project::import::EXTENSIONS);
                if let Some(p) = rfd::FileDialog::new().add_filter("Project", &exts).pick_file() {
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
            MenuCommand::ExportAudio => {
                if self.export_dialog.is_none() {
                    let rate = snap.status.sample_rate;
                    self.export_dialog = Some(export_view::ExportDialog {
                        sample_rate: if opentake_engine::SAMPLE_RATES.contains(&rate) { rate } else { 48_000 },
                        ..Default::default()
                    });
                }
            }
            MenuCommand::ImportGuitarPro => {
                if let Some(path) = rfd::FileDialog::new().add_filter("Guitar Pro 7/8", &["gp", "gpif"]).pick_file() {
                    self.open_guitar_pro_dialog(path);
                }
            }
            MenuCommand::ImportMidi => {
                if let Some(path) = rfd::FileDialog::new().add_filter("MIDI file", &["mid", "midi"]).pick_file() {
                    self.open_midi_file_dialog(path);
                }
            }
            MenuCommand::ExportMidi => self.export_midi_file(snap),
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
            MenuCommand::MergeRegions => {
                if let Some((track, clips)) = self.audio_selection() {
                    self.act(Action::MergeRegions { track, clips });
                }
            }
            MenuCommand::CrossfadeRegions => {
                if let Some((track, [a, b])) = self.audio_selection().and_then(|(t, c)| Some((t, <[usize; 2]>::try_from(c).ok()?))) {
                    self.act(Action::Crossfade { track, a, b, secs: opentake_engine::CROSSFADE_SECS });
                }
            }
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
            MenuCommand::NewBus => self.act(Action::AddBus(Vec::new())),
            MenuCommand::NewBusFromSelection => {
                let mut tracks: Vec<usize> = self.selected_track.into_iter().chain(self.selection.iter().map(|r| r.track)).collect();
                tracks.sort_unstable();
                tracks.dedup();
                self.act(Action::AddBus(tracks));
            }
            MenuCommand::DeleteTrack => {
                if let Some(t) = self.selected_track {
                    self.act(Action::RemoveTrack(t));
                }
            }
            MenuCommand::PlayStop => self.act(if snap.status.playing { Action::Stop } else { Action::Play }),
            MenuCommand::Record => self.act(if snap.status.recording { Action::Stop } else { Action::Record }),
            MenuCommand::GoToStart => self.act(Action::Seek(0.0)),
            MenuCommand::CreateMarker | MenuCommand::DeleteMarker | MenuCommand::PreviousMarker | MenuCommand::NextMarker => self.run_marker_command(cmd, snap),
            MenuCommand::ToggleMetronome => {
                self.act(Action::SetMetronome { enabled: Some(!snap.project.metronome_enabled), volume: None })
            }
            MenuCommand::ToggleCountIn => {
                let bars = if snap.project.count_in_bars > 0 { 0 } else { 1 };
                self.act(Action::SetCountIn { bars: Some(bars), on_play: None })
            }
            MenuCommand::ToggleCountInOnPlay => {
                // Turning it on also enables a 1-bar count-in if none is set.
                let on = !snap.project.count_in_on_play;
                let bars = (on && snap.project.count_in_bars == 0).then_some(1);
                self.act(Action::SetCountIn { bars, on_play: Some(on) })
            }
            MenuCommand::ToggleAutoMonitor => self.act(Action::SetAutoMonitor(!snap.project.auto_input_monitoring)),
            MenuCommand::ToggleInspector => self.show_inspector = !self.show_inspector,
            MenuCommand::ToggleMixer => self.toggle_bottom(BottomTab::Mixer),
            MenuCommand::ToggleAutomation => self.toggle_automation(snap),
            MenuCommand::ToggleMasterAutomation => self.act(Action::ShowMasterAutomation(!snap.project.show_master_automation)),
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
                .flat_map(|(t, tr)| timeline::regions_of(tr, t, &snap.project.tempo_map()).map(|v| v.r).collect::<Vec<_>>())
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
                let note = if cfg!(target_os = "macos") { " Audio Units are found through macOS." } else { "" };
                ui.label(RichText::new(format!("VST3 plug-ins are searched recursively in these folders.{note}")).color(theme::TEXT_DIM));
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
                widgets::section(ui, &format!("Plug-ins ({})", snap.scanned.len() + snap.audio_units.len()));
                egui::ScrollArea::vertical().auto_shrink([false, false]).max_height(220.0).show(ui, |ui| {
                    egui::Grid::new("plugin_list").num_columns(3).striped(true).spacing([16.0, 6.0]).show(ui, |ui| {
                        for p in &snap.scanned {
                            ui.label(RichText::new(&p.class.name).strong()).on_hover_text(p.bundle.display().to_string());
                            ui.label(RichText::new(&p.class.vendor).color(theme::TEXT_DIM));
                            ui.label(RichText::new(p.class.sub_categories.replace('|', " · ")).color(theme::TEXT_FAINT));
                            ui.end_row();
                        }
                        for a in &snap.audio_units {
                            ui.label(RichText::new(&a.name).strong()).on_hover_text(&a.id);
                            ui.label(RichText::new(&a.manufacturer).color(theme::TEXT_DIM));
                            ui.label(RichText::new(if a.instrument { "Audio Unit · Instrument" } else { "Audio Unit · Effect" }).color(theme::TEXT_FAINT));
                            ui.end_row();
                        }
                    });
                    if snap.scanned.is_empty() && snap.audio_units.is_empty() {
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
            B { icon: Icon::Play, active: (s.playing || s.counting_in) && !s.recording, color: theme::PLAY, idle_color: theme::TEXT, a11y: "Play (Space)" },
            B { icon: Icon::Record, active: s.recording, color: theme::RECORD, idle_color: theme::shade(theme::RECORD, 0.85), a11y: "Record (R)" },
            B { icon: Icon::Cycle, active: snap.project.loop_enabled, color: theme::SOLO, idle_color: theme::TEXT, a11y: "Cycle (C)" },
        ];
        let r = widgets::transport(ui, "transport", &buttons, vec2(TRANSPORT_SEG_W, 30.0));
        let map = snap.project.tempo_map();
        let pos = s.position_secs;
        if r[0].clicked() {
            self.act(Action::Seek(0.0));
        }
        if r[1].clicked() {
            // Back to the previous bar line (or one more bar if already on one).
            self.act(Action::Seek(map.bar_at((pos - 1e-6).max(0.0)).secs.max(0.0)));
        }
        if r[2].clicked() {
            self.act(Action::Seek(map.secs_of(map.bar_at(pos + 1e-6).index + 2, 1.0)));
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
            a11y: "Metronome (K) — right-click for volume and count-in",
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
            ui.separator();
            widgets::section(ui, "Count-in");
            ui.horizontal(|ui| {
                for (bars, label) in [(0, "Off"), (1, "1 bar"), (2, "2 bars"), (4, "4 bars")] {
                    if ui.add(egui::Button::new(label).selected(snap.project.count_in_bars == bars)).clicked() {
                        self.act(Action::SetCountIn { bars: Some(bars), on_play: None });
                    }
                }
            });
            let mut on_play = snap.project.count_in_on_play;
            if ui.add_enabled(snap.project.count_in_bars > 0, egui::Checkbox::new(&mut on_play, "Count in when playing too")).changed() {
                self.act(Action::SetCountIn { bars: None, on_play: Some(on_play) });
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
        let base = if s.recording || s.counting_in {
            theme::RECORD
        } else if s.playing {
            theme::PLAY
        } else {
            Color32::from_gray(60)
        };
        let map = snap.project.tempo_map();
        let here = map.position(s.position_secs.max(0.0));
        let on_beat = s.playing && here.fraction < 0.15;
        let downbeat = here.beat == 1;
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
        let counting_in = s.counting_in;
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
        if counting_in {
            p.text(pos2(time_rect.center().x, label_y), Align2::CENTER_CENTER, "COUNT-IN", small.clone(), theme::RECORD);
        } else {
            p.text(pos2(time_rect.center().x, label_y), Align2::CENTER_CENTER, "TIME", small.clone(), theme::LCD_DIM);
        }
        x = time_rect.right();
        sep(p, x);

        // Tempo (drag to change, Shift for fine).
        let tempo_rect = Rect::from_min_size(pos2(x, rect.top()), vec2(70.0, rect.height()));
        let resp = ui.interact(tempo_rect, ui.make_persistent_id("lcd_tempo"), Sense::drag());
        // The tempo map's tempo at the playhead; dragging edits that section's tempo.
        let bpm = map.tempo_at(s.position_secs.max(0.0));
        let mut changes = snap.project.tempo_changes.clone();
        changes.sort_by(|a, b| a.at_secs.total_cmp(&b.at_secs));
        let section = changes.iter().rposition(|c| c.at_secs <= s.position_secs + 1e-9);
        if resp.dragged() {
            let fine = ui.input(|i| i.modifiers.shift);
            let new = ((bpm - resp.drag_delta().y as f64 * if fine { 0.05 } else { 0.5 }).clamp(20.0, 999.0) * 10.0).round() / 10.0;
            if (new - bpm).abs() > f64::EPSILON {
                match section {
                    None => self.act(Action::SetTempo(new)),
                    Some(i) => {
                        changes[i].bpm = new;
                        self.act(Action::SetTempoChanges { changes: changes.clone(), merge: Some(format!("tempo-bpm:{i}")) });
                    }
                }
            }
        }
        // An imported song that didn't adopt its tempo map: show the tempo it plays at;
        // hovering/dragging shows (and edits) the project tempo again.
        let song = snap.project.song_tempo_at(s.position_secs).filter(|t| (t - bpm).abs() > 0.05);
        let hint = match song {
            Some(t) => format!(
                "Song tempo {t:.1} at the playhead (from the imported score); the project, grid and metronome are at {bpm:.1}.\nClick \"Use song tempo\" in the Tempo row to make them follow the song. Drag here to change the project tempo (Shift = fine)."
            ),
            None if section.is_some() => "Tempo at the playhead (from the tempo track) — drag to change it (Shift = fine)".to_string(),
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

        let (n, d) = map.signature_at(s.position_secs.max(0.0));
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
        if self.midi_light(ui, snap).clicked() {
            self.show_audio_settings = true;
            self.act(Action::RefreshDevices);
        }
        if show_meter {
            ui.add_space(6.0);
            widgets::stereo_hmeter(ui, vec2(84.0, 10.0), s.master_peak_db).on_hover_text("Stereo Out level");
        }
    }

    /// MIDI activity: a dot that lights while notes come in (and keys are held).
    fn midi_light(&mut self, ui: &mut egui::Ui, snap: &Snapshot) -> egui::Response {
        let s = &snap.status;
        if s.midi_notes_received != self.midi_seen.0 {
            self.midi_seen = (s.midi_notes_received, Some(Instant::now()));
        }
        let lit = !s.midi_keys_down.is_empty() || self.midi_seen.1.is_some_and(|t| t.elapsed() < Duration::from_millis(180));
        let color = match (lit, s.midi_inputs.is_empty()) {
            (true, _) => theme::PLAY,
            (false, false) => theme::shade(theme::PLAY, 0.35),
            (false, true) => theme::TEXT_FAINT,
        };
        let names = |ids: &[usize]| ids.iter().filter_map(|&i| snap.project.tracks.get(i)).map(|t| t.name.as_str()).collect::<Vec<_>>().join(", ");
        let tip = match (s.midi_inputs.is_empty(), s.midi_input_tracks.is_empty()) {
            (true, _) if !s.audio_running => "MIDI inputs open when audio runs".to_string(),
            (true, _) => "No MIDI input connected".to_string(),
            (false, true) => format!("MIDI in: {}\nArm or select an instrument track to play it", s.midi_inputs.join(", ")),
            (false, false) => format!("MIDI in: {}\nPlaying: {}", s.midi_inputs.join(", "), names(&s.midi_input_tracks)),
        };
        let resp = pill(ui, color, "MIDI", &tip);
        widgets::label(&resp, egui::Role::Button, "MIDI activity");
        resp
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
        self.region_inspector(ui, snap);
        self.loop_inspector(ui, snap);
        self.quantize_inspector(ui, snap);
        ui.add_space(6.0);
        ui.separator();

        let strip_w = (ui.available_width() - 8.0) / 2.0;
        let layout = mixer::StripLayout { width: strip_w, fx_height: FX_AREA_H, sends_height: mixer::SENDS_H };
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
        let project = &snap.project;
        self.open_plugins.retain(|&(t, s)| slot_at(project, t, s).is_some());
        for (n, (t, s)) in self.open_plugins.clone().into_iter().enumerate() {
            let Some(slot) = slot_at(project, t, s) else { continue };
            let name = short_plugin_name(&slot.name).to_string();
            let title = plugin_title(project, t, s).unwrap_or_default();
            let builtin = slot.format == opentake_engine::project::PluginFormat::Builtin;
            let is_eq = builtin && slot.path == opentake_engine::eq::PATH;
            let fx = if builtin { fx_view::Kind::of(&slot.path) } else { None };
            let mut open = true;
            egui::Window::new(title)
                .id(ui.make_persistent_id(("plugin_window", t, s)))
                .open(&mut open)
                .default_size(if is_eq {
                    [660.0, 440.0]
                } else {
                    fx.map_or([400.0, 360.0], fx_view::Kind::window_size)
                })
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
                        let native = snap.native_editors.contains(&(t, s));
                        if native || snap.side_chain_inputs.contains(&(t, s)) {
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if native {
                                    let b = egui::Button::new(RichText::new("Open Editor").size(11.0)).min_size(vec2(0.0, 20.0));
                                    if ui.add(b).on_hover_text("Open the plug-in's own editor window").clicked() {
                                        self.act(Action::OpenEditor(t, s));
                                    }
                                }
                                if snap.side_chain_inputs.contains(&(t, s)) {
                                    fx_view::side_chain_menu(self, ui, project, t, s);
                                }
                            });
                        }
                    });
                    ui.add_space(6.0);
                    let params = snap.params.get(&(t, s)).map(Vec::as_slice).unwrap_or_default();
                    if is_eq && !params.is_empty() {
                        eq_view::show(self, ui, t, s, params, snap.status.sample_rate as f64, snap.taps.get(&(t, s)));
                        return;
                    }
                    if let Some(kind) = fx.filter(|_| !params.is_empty()) {
                        let bpm = snap.project.tempo_map().tempo_at(snap.status.position_secs.max(0.0));
                        let owner = if matches!(s, SlotRef::Master(_)) { None } else { Some(t) };
                        let key = match s {
                            SlotRef::Effect(k) | SlotRef::Master(k) => project.key_source(owner, k).map(|i| project.tracks[i].name.as_str()),
                            SlotRef::Instrument => None,
                        };
                        let view = fx_view::View { meter: snap.meters.get(&(t, s)), bands: snap.multiband.get(&(t, s)), bpm, key };
                        fx_view::show(self, ui, t, s, kind, params, view);
                        return;
                    }
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

    fn audio_settings_window(&mut self, ctx: &egui::Context, snap: &Snapshot) {
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

                let status = &snap.status;
                ui.label(RichText::new("Recording Latency").color(theme::TEXT_DIM));
                ui.horizontal(|ui| {
                    let measured = match status.io_latency_ms {
                        Some(ms) => format!("{ms:.1} ms measured"),
                        None if status.audio_running && status.input_channels > 0 => "not reported by the device".into(),
                        None => "measured while audio runs".into(),
                    };
                    ui.label(measured).on_hover_text(
                        "The interface's round trip, from the input to what you hear. Recordings are moved \
                         earlier by this much so they line up with what you played along to.",
                    );
                    ui.label(RichText::new("+").color(theme::TEXT_DIM));
                    let mut offset = status.record_offset_ms;
                    let drag = egui::DragValue::new(&mut offset).range(-500.0..=500.0).speed(0.1).fixed_decimals(1).suffix(" ms");
                    let resp = ui.add(drag).on_hover_text("Correction for this device setup if recordings still sit early (negative) or late (positive)");
                    widgets::label(&resp, egui::Role::SpinButton, "Recording offset");
                    if resp.changed() {
                        self.act(Action::SetRecordOffset(offset));
                    }
                });
                ui.end_row();

                ui.label(RichText::new("Calibrate").color(theme::TEXT_DIM));
                ui.horizontal(|ui| {
                    let can = status.audio_running && status.input_channels > 0 && !status.recording;
                    let channel = self.latency_channel.min(status.input_channels.max(1) - 1);
                    ui.add_enabled_ui(can, |ui| {
                        egui::ComboBox::from_id_salt("lat_ch").width(70.0).selected_text(format!("Input {}", channel + 1)).show_ui(ui, |ui| {
                            for ch in 0..status.input_channels {
                                ui.selectable_value(&mut self.latency_channel, ch, format!("Input {}", ch + 1));
                            }
                        });
                        if ui.button("Measure Latency").on_hover_text(
                            "Plays test chirps and listens for them on this input, which must hear the output: \
                             a cable from an output to the input, or speakers turned up near a microphone. \
                             Sets the correction for this device setup.",
                        ).clicked() {
                            self.act(Action::MeasureLatency(channel));
                        }
                    });
                });
                ui.end_row();
            });
            ui.add_space(12.0);
            self.midi_inputs_section(ui, snap);
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

    /// Which MIDI inputs to listen to: all (the default, also devices plugged in later) or
    /// the ones ticked. Changes apply right away.
    fn midi_inputs_section(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        widgets::section(ui, "MIDI Inputs");
        let all = snap.midi_selection == MidiSelection::All;
        let mut every = all;
        if ui.checkbox(&mut every, "All MIDI Inputs").on_hover_text("Listen to every MIDI input, including devices plugged in later").changed() {
            // Unticking "All" keeps the ports there are now, to untick one by one.
            let ports = self.midi_ports.as_ref().and_then(|r| r.as_ref().ok()).map(|m| m.ports.iter().map(|p| p.name.clone()).collect()).unwrap_or_default();
            self.act(Action::SetMidiInputs(if every { MidiSelection::All } else { MidiSelection::Ports(ports) }));
        }
        let ports = match &self.midi_ports {
            None => return drop(ui.label(RichText::new("Looking for MIDI inputs…").color(theme::TEXT_FAINT))),
            Some(Err(e)) => return drop(ui.label(RichText::new(format!("MIDI unavailable: {e}")).color(theme::TEXT_FAINT))),
            Some(Ok(m)) if m.ports.is_empty() => return drop(ui.label(RichText::new("No MIDI inputs found. Connect a keyboard, then Refresh Devices.").color(theme::TEXT_FAINT))),
            Some(Ok(m)) => m.ports.clone(),
        };
        let chosen = |name: &str| snap.midi_selection.includes(name);
        ui.indent("midi_ports", |ui| {
            for port in &ports {
                ui.horizontal(|ui| {
                    let mut on = chosen(&port.name);
                    let resp = ui.add_enabled(!all, egui::Checkbox::new(&mut on, &port.name));
                    if resp.changed() {
                        let names = ports.iter().filter(|p| if p.name == port.name { on } else { chosen(&p.name) }).map(|p| p.name.clone()).collect();
                        self.act(Action::SetMidiInputs(MidiSelection::Ports(names)));
                    }
                    let open = snap.status.midi_inputs.contains(&port.name);
                    let (color, state) = if open { (theme::PLAY, "connected") } else if chosen(&port.name) && snap.status.audio_running { (theme::INPUT, "not connected") } else { (theme::TEXT_FAINT, "off") };
                    let (dot, _) = ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
                    ui.painter().circle_filled(dot.center(), 3.0, color);
                    ui.label(RichText::new(state).size(10.0).color(theme::TEXT_FAINT));
                });
            }
        });
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
                if snap.status.latency_ms > 0.05 {
                    ui.label(RichText::new(format!("Plug-in delay {:.1} ms (compensated)", snap.status.latency_ms)).size(10.0).color(theme::TEXT_DIM))
                        .on_hover_text("Tracks are lined up automatically; live input monitoring is delayed by this much.");
                    ui.add_space(12.0);
                }
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
        self.save_midi_inputs(&snap);
        self.handle_shortcuts(ui, &snap);
        self.handle_dropped_files(ui, &snap);
        self.handle_open_requests(&snap);

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
        self.audio_settings_window(ui.ctx(), &snap);
        self.mcp_info_window(ui.ctx());
        self.plugin_manager_window(ui.ctx(), &snap);
        self.instrument_picker_window(ui.ctx(), &snap);
        self.guitar_pro_window(ui.ctx(), &snap);
        self.midi_import_window(ui.ctx(), &snap);
        self.export_window(ui.ctx(), &snap);
        if self.export_task.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }
        self.poll_flex(ui.ctx());
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
    let pos = project.tempo_map().position(secs.max(0.0));
    (pos.bar.max(1) as u64, pos.beat as u64, (pos.fraction * 4.0) as u64 + 1)
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
    fn option_drag_stretches_region_and_inspector_transposes() {
        let (mut harness, engine, dir) = harness_with_region(); // 1.0 .. 5.0
        let px = harness.state().view.px_per_sec;
        let rect = harness.get_by_label("Region riff").rect();
        let edge = egui::pos2(rect.right() - 2.0, rect.center().y);
        press_drag(&mut harness, edge, edge + egui::vec2(2.0 * px, 0.0), egui::Modifiers::ALT);
        let c = engine.lock().unwrap().project().tracks[0].clips[0].clone();
        assert!((c.length_secs - 6.0).abs() < 0.05 && (c.stretch - 1.5).abs() < 0.02, "⌥-drag stretches: {} {}", c.length_secs, c.stretch);
        assert_eq!((c.position_secs, c.source_offset_secs), (1.0, 0.0));

        // The selected region's inspector: type a transposition.
        assert_eq!(harness.state().selection, vec![RegionRef::audio(0, 0)]);
        harness.get_by_role_and_label(egui::accesskit::Role::SpinButton, "Transpose").click();
        harness.step();
        harness.get_by_role_and_label(egui::accesskit::Role::SpinButton, "Transpose").type_text("-7");
        harness.key_press(egui::Key::Enter);
        harness.step();
        harness.step();
        let c = engine.lock().unwrap().project().tracks[0].clips[0].clone();
        assert_eq!(c.pitch_semitones, -7, "inspector sets the pitch");
        assert!((c.stretch - 1.5).abs() < 0.02, "stretch is kept");
        click(&mut harness, "Reset Pitch & Stretch");
        let c = engine.lock().unwrap().project().tracks[0].clips[0].clone();
        assert!(!c.is_flexed() && (c.length_secs - 4.0).abs() < 1e-6, "reset restores the original length");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn fade_handle_drag_sets_fade_in_and_merge_joins_regions() {
        let (mut harness, engine, dir) = harness_with_region(); // 1.0 .. 5.0
        engine.lock().unwrap().save_project(Some(&dir.join("song.otp"))).unwrap();
        let px = harness.state().view.px_per_sec;
        let rect = harness.get_by_label("Region riff").rect();
        // The fade-in handle sits in the top-left corner, just below the name bar.
        let handle = egui::pos2(rect.left() + 1.0 + 3.5, rect.top() + 17.0 + 3.5);
        drag(&mut harness, handle, handle + egui::vec2(0.5 * px, 0.0));
        {
            let e = engine.lock().unwrap();
            let c = &e.project().tracks[0].clips[0];
            assert!((c.fade_in.secs - 0.5).abs() < 0.05, "fade-in {:?}", c.fade_in);
            assert!(c.position_secs == 1.0 && c.length_secs == 4.0 && c.fade_out.secs == 0.0, "the drag neither trimmed nor moved");
            assert_eq!(e.history_info().undo.as_deref(), Some("Region Fades"));
        }
        // The vertical edge below the handle still trims.
        let edge = egui::pos2(rect.left() + 2.0, rect.bottom() - 6.0);
        drag(&mut harness, edge, edge + egui::vec2(0.25 * px, 0.0));
        assert!((clips(&engine)[0].0 - 1.25).abs() < 0.05, "{:?}", clips(&engine));

        // Split, select both halves and join them again with ⌘J.
        engine.lock().unwrap().split_clip(0, 0, 3.0).unwrap();
        harness.step();
        let halves: Vec<egui::Rect> = harness.query_all_by_label("Region riff").map(|n| n.rect()).collect();
        assert_eq!(halves.len(), 2);
        click_at(&mut harness, halves[0].center());
        harness.event(egui::Event::ModifiersChanged(egui::Modifiers::SHIFT));
        click_at(&mut harness, halves[1].center());
        harness.event(egui::Event::ModifiersChanged(egui::Modifiers::NONE));
        assert_eq!(harness.state().selection.len(), 2);
        harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::J);
        harness.step();
        harness.step();
        {
            let e = engine.lock().unwrap();
            let c = &e.project().tracks[0].clips;
            assert_eq!(c.len(), 1, "merged into one region");
            assert!((c[0].position_secs - 1.25).abs() < 0.05 && (c[0].length_secs - 3.75).abs() < 0.05, "{:?}", c[0]);
            assert!(c[0].file.starts_with(dir.join("Audio")), "{}", c[0].file.display());
            assert_eq!(e.history_info().undo.as_deref(), Some("Merge Regions"));
        }
        assert_eq!(harness.state().selection, vec![RegionRef::audio(0, 0)]);

        // Undo brings the halves back; the Edit menu merges too.
        harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Z);
        harness.step();
        harness.step();
        assert_eq!(clips(&engine).len(), 2);
        harness.state_mut().selection = vec![RegionRef::audio(0, 0), RegionRef::audio(0, 1)];
        click(&mut harness, "Edit");
        click(&mut harness, &menu_label(MenuCommand::MergeRegions));
        assert_eq!(clips(&engine).len(), 1);
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

    fn click_at(harness: &mut Harness<'static, DawApp>, pos: egui::Pos2) {
        harness.hover_at(pos);
        for _ in 0..30 {
            harness.step(); // no double-click with an earlier click
        }
        for pressed in [true, false] {
            harness.event(egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE });
        }
        harness.step();
        harness.step();
    }

    #[test]
    fn automation_lane_points_are_added_dragged_and_deleted() {
        use opentake_engine::project::AutomationTarget;
        let (mut harness, engine) = harness_with_track();
        harness.state_mut().snap_mode = timeline::SnapMode::Off;
        click(&mut harness, "Show Automation");
        let lanes = |e: &SharedEngine| e.lock().unwrap().project().tracks[0].automation.clone();
        assert!(engine.lock().unwrap().project().tracks[0].show_automation);
        assert_eq!(lanes(&engine)[0].target, AutomationTarget::Volume, "showing a track's automation starts with a volume lane");
        harness.step();
        let lane = harness.get_by_label("Automation Guitar Volume").rect();
        // The lane sits right under the track's own row, and belongs to the track.
        let row = harness.get_by_label("Lane Guitar").rect();
        assert!((lane.top() - row.bottom()).abs() < 1.0 && (lane.left() - harness.state().view.lanes().min).abs() < 1.0, "{lane:?} under {row:?}");
        assert_eq!(harness.state().view.track_at(lane.center().y), Some(0));

        // Click at 2 s, a quarter down the lane: a point at ~0.75.
        let x = |h: &Harness<'static, DawApp>, t: f64| h.state().view.x(t);
        let plot = |v: f32| lane.bottom() - 6.0 - v * (lane.height() - 12.0);
        let at = egui::pos2(x(&harness, 2.0), plot(0.75));
        click_at(&mut harness, at);
        let p = lanes(&engine)[0].points.clone();
        assert_eq!(p.len(), 1);
        assert!((p[0].time_secs - 2.0).abs() < 0.03 && (p[0].value - 0.75).abs() < 0.03, "{p:?}");
        assert!(harness.state().view.auto_sel.is_some(), "the new point is selected");

        // Drag it later and down.
        let from = egui::pos2(x(&harness, p[0].time_secs), plot(p[0].value));
        drag(&mut harness, from, from + egui::vec2(48.0, 10.0));
        let q = lanes(&engine)[0].points[0];
        let px = harness.state().view.px_per_sec as f64;
        assert!((q.time_secs - (p[0].time_secs + 48.0 / px)).abs() < 0.03, "{q:?}");
        assert!(q.value < p[0].value - 0.1, "{q:?}");
        assert_eq!(engine.lock().unwrap().history_info().undo.as_deref(), Some("Move Automation Point"));

        // A second point, then ⌫ deletes the selected one and double-click the other.
        let at = egui::pos2(x(&harness, 4.0), plot(0.2));
        click_at(&mut harness, at);
        assert_eq!(lanes(&engine)[0].points.len(), 2);
        harness.key_press(egui::Key::Backspace);
        harness.step();
        harness.step();
        assert_eq!(lanes(&engine)[0].points.len(), 1, "⌫ deletes the selected point, not the track");
        assert_eq!(engine.lock().unwrap().project().tracks.len(), 1);
        let at = egui::pos2(x(&harness, q.time_secs), plot(q.value));
        double_click_at(&mut harness, at);
        assert!(lanes(&engine)[0].points.is_empty());

        // The header fader follows the lane; "+" adds another lane, "A" hides them all.
        engine.lock().unwrap().set_automation_points(Some(0), 0, vec![opentake_engine::project::AutomationPoint::new(0.0, 0.0)], true).unwrap();
        harness.step();
        harness.step();
        click(&mut harness, "Add Automation Lane");
        assert_eq!(lanes(&engine).iter().map(|l| l.target).collect::<Vec<_>>(), vec![AutomationTarget::Volume, AutomationTarget::Pan]);
        harness.key_press(egui::Key::A);
        harness.step();
        harness.step();
        assert!(!engine.lock().unwrap().project().tracks[0].show_automation);
        assert!(harness.query_by_label("Automation Guitar Volume").is_none());
    }

    #[test]
    fn master_automation_lanes_below_the_tracks() {
        let (mut harness, engine) = harness_with_track();
        // The master strip's A shows the master's lanes (starting with volume).
        harness.state_mut().bottom = Some(BottomTab::Mixer);
        harness.step();
        harness.step();
        harness.get_all_by_label("Show Master Automation").next().unwrap().click();
        harness.step();
        harness.step();
        let master = |e: &SharedEngine| e.lock().unwrap().project().master_automation.clone();
        assert!(engine.lock().unwrap().project().show_master_automation);
        assert_eq!(master(&engine).len(), 1);
        harness.state_mut().bottom = None;
        harness.step();
        harness.step();
        let lane = harness.get_by_label("Automation Stereo Out Volume").rect();
        assert!(lane.top() > harness.get_by_label("Lane Guitar").rect().bottom(), "below the tracks");
        assert_eq!(harness.state().view.track_at(lane.center().y), None, "not part of a track");

        let at = egui::pos2(harness.state().view.x(3.0), lane.center().y);
        click_at(&mut harness, at);
        let p = master(&engine)[0].points.clone();
        assert_eq!(p.len(), 1);
        assert!((p[0].time_secs - 3.0).abs() < 0.03, "{p:?}");
        assert_eq!(harness.state().view.auto_sel.map(|s| s.owner), Some(None), "a master point is selected");
        harness.key_press(egui::Key::Backspace);
        harness.step();
        harness.step();
        assert!(master(&engine)[0].points.is_empty(), "⌫ deletes it");

        click(&mut harness, "Hide Master Automation");
        assert!(harness.query_by_label("Automation Stereo Out Volume").is_none());
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
    fn audio_settings_show_recording_latency() {
        let engine: SharedEngine = Arc::new(Mutex::new(Engine::new()));
        let app_engine = engine.clone();
        let mut harness = Harness::builder().with_size([1000.0, 700.0]).build_eframe(move |cc| {
            theme::apply(&cc.egui_ctx);
            DawApp::new(app_engine, None)
        });
        let device = |name: &str| opentake_engine::DeviceInfo { id: name.into(), name: name.into(), is_default: true, default_channels: 2, max_channels: 2, default_sample_rate: 48_000 };
        harness.state_mut().devices = Some(DeviceList { host: "CoreAudio".into(), inputs: vec![device("Interface In")], outputs: vec![device("Interface Out")] });
        harness.state_mut().show_audio_settings = true;
        engine.lock().unwrap().set_record_offset(-1.5).unwrap();
        for _ in 0..4 {
            harness.step();
        }
        harness.get_by_label("measured while audio runs");
        harness.get_by_label("Measure Latency");
        assert_eq!(harness.get_by_label("Recording offset").value().as_deref(), Some("-1.5 ms"));
        harness.get_by_label("Recording offset").set_value_accesskit(4.0);
        harness.step();
        harness.step();
        assert_eq!(engine.lock().unwrap().record_offset_ms(), 4.0);
    }

    #[test]
    fn instrument_tracks_have_record_enable_and_take_midi_input() {
        let engine: SharedEngine = Arc::new(Mutex::new(Engine::new()));
        engine.lock().unwrap().add_instrument_track("Keys", "synth").unwrap();
        let app_engine = engine.clone();
        let mut harness = Harness::builder().with_size([1360.0, 840.0]).with_step_dt(1.0 / 60.0).build_eframe(move |cc| {
            theme::apply(&cc.egui_ctx);
            DawApp::new(app_engine, None)
        });
        harness.state_mut().bottom = Some(BottomTab::Mixer);
        harness.step();
        assert_eq!(harness.query_all_by_label("Input Monitoring").count(), 0, "MIDI plays through the instrument: no I");
        assert!(engine.lock().unwrap().midi_input_tracks().is_empty());
        // Selecting the track routes MIDI input to it; arming makes it record.
        click(&mut harness, "Keys");
        assert_eq!(engine.lock().unwrap().midi_input_tracks(), [0]);
        harness.query_all_by_label("Record Enable").next().expect("R on the instrument track").click();
        harness.step();
        harness.step();
        let mut e = engine.lock().unwrap();
        assert!(e.project().tracks[0].armed);
        e.select_track(None).unwrap();
        assert_eq!(e.midi_input_tracks(), [0], "armed tracks play the input without being selected");
    }

    #[test]
    fn audio_settings_choose_midi_inputs() {
        let engine: SharedEngine = Arc::new(Mutex::new(Engine::new()));
        let app_engine = engine.clone();
        let mut harness = Harness::builder().with_size([1000.0, 800.0]).build_eframe(move |cc| {
            theme::apply(&cc.egui_ctx);
            DawApp::new(app_engine, None)
        });
        let device = |name: &str| opentake_engine::DeviceInfo { id: name.into(), name: name.into(), is_default: true, default_channels: 2, max_channels: 2, default_sample_rate: 48_000 };
        let port = |name: &str| opentake_engine::midi_input::MidiPortInfo { name: name.into(), enabled: true, open: false };
        harness.state_mut().devices = Some(DeviceList { host: "CoreAudio".into(), inputs: vec![device("Interface In")], outputs: vec![device("Interface Out")] });
        harness.state_mut().midi_ports = Some(Ok(MidiInputs { selection: MidiSelection::All, ports: vec![port("Test Keys A"), port("Test Pads B")], errors: Vec::new() }));
        harness.state_mut().show_audio_settings = true;
        for _ in 0..4 {
            harness.step();
        }
        harness.get_by_label("MIDI activity");
        // Unticking "All" keeps the ports there are, then each can be unticked.
        click(&mut harness, "All MIDI Inputs");
        let selection = || engine.lock().unwrap().midi_selection().clone();
        assert_eq!(selection(), MidiSelection::Ports(vec!["Test Keys A".into(), "Test Pads B".into()]));
        click(&mut harness, "Test Keys A");
        assert_eq!(selection(), MidiSelection::Ports(vec!["Test Pads B".into()]));
        harness.step(); // the list re-laid out with the engine's ports
        click(&mut harness, "All MIDI Inputs");
        assert_eq!(selection(), MidiSelection::All);
    }

    #[test]
    fn region_gain_and_normalize_in_the_inspector() {
        let (mut harness, engine, dir) = harness_with_region();
        harness.state_mut().selection = vec![RegionRef::audio(0, 0)];
        harness.step();
        harness.step();
        harness.get_by_label("Region Gain").set_value_accesskit(-4.5);
        harness.step();
        harness.step();
        assert!((engine.lock().unwrap().project().tracks[0].clips[0].gain - 10f32.powf(-4.5 / 20.0)).abs() < 1e-4);
        click(&mut harness, "Normalize");
        let mut e = engine.lock().unwrap();
        let peak = e.clip_peak(0, 0).unwrap() * e.project().tracks[0].clips[0].gain;
        assert!((peak - 1.0).abs() < 1e-3, "{peak}");
        assert_eq!(e.history_info().undo.as_deref(), Some("Normalize Region"));
        drop(e);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn control_bar_buttons_toggle_the_views() {
        let (mut harness, _engine) = harness_with_track();
        click(&mut harness, "Mixer (X)");
        assert!(harness.state().bottom == Some(BottomTab::Mixer));
        click(&mut harness, "Tablature (T)");
        assert!(harness.state().bottom == Some(BottomTab::Tab));
        click(&mut harness, "Tablature (T)");
        assert!(harness.state().bottom.is_none());
        click(&mut harness, "Piano Roll (P)");
        assert!(harness.state().bottom == Some(BottomTab::PianoRoll));
    }

    #[test]
    fn tracks_reorder_by_dragging_headers_and_mixer_strips() {
        let (mut harness, engine) = harness_with_track();
        for name in ["Bass", "Keys"] {
            engine.lock().unwrap().add_track(name).unwrap();
        }
        harness.step();
        harness.step();
        let names = |e: &SharedEngine| e.lock().unwrap().project().tracks.iter().map(|t| t.name.clone()).collect::<Vec<_>>();
        // Lane view: drag Guitar's header (empty space right of its name) below Keys.
        let lane = |h: &Harness<'static, DawApp>, name: &str| h.get_by_label(&format!("Lane {name}")).rect();
        let header_x = lane(&harness, "Guitar").left() - 60.0;
        let (from, to) = (lane(&harness, "Guitar").center().y, lane(&harness, "Keys").center().y);
        drag(&mut harness, egui::pos2(header_x, from), egui::pos2(header_x, to));
        assert_eq!(names(&engine), ["Bass", "Keys", "Guitar"]);
        assert_eq!(harness.state().selected_track, Some(2), "the selection follows the track");

        // Mixer: clicking a strip selects its track; dragging moves it.
        harness.key_press(egui::Key::X);
        harness.step();
        harness.step();
        let strip = |h: &Harness<'static, DawApp>, name: &str| h.get_by_label(&format!("Strip {name}")).rect();
        let top = |r: egui::Rect| egui::pos2(r.center().x, r.top() + 4.0);
        let p = top(strip(&harness, "Keys"));
        harness.hover_at(p);
        harness.step();
        harness.drag_at(p);
        harness.step();
        harness.drop_at(p);
        harness.step();
        harness.step();
        assert_eq!(harness.state().selected_track, Some(1));
        let (a, b) = (top(strip(&harness, "Guitar")), top(strip(&harness, "Bass")));
        drag(&mut harness, a, b);
        assert_eq!(names(&engine), ["Guitar", "Bass", "Keys"]);
        engine.lock().unwrap().undo().unwrap();
        assert_eq!(names(&engine), ["Bass", "Keys", "Guitar"]);
    }

    #[test]
    fn multiband_editor_moves_crossovers_and_solos_bands() {
        let (mut harness, engine) = harness_with_track();
        engine.lock().unwrap().insert_master_plugin("multiband", None).unwrap();
        harness.state_mut().open_plugins.push((MASTER_UI, SlotRef::Master(0)));
        harness.step();
        harness.step();
        let value = |e: &SharedEngine, id: u32| e.lock().unwrap().slot_params(0, SlotRef::Master(0)).unwrap().iter().find(|p| p.id == id).unwrap().value;
        let before = value(&engine, opentake_engine::multiband::LOW_CROSSOVER);
        let handle = harness.get_by_label("Low Crossover").rect().center();
        drag(&mut harness, handle, handle + egui::vec2(60.0, 0.0));
        assert!(value(&engine, opentake_engine::multiband::LOW_CROSSOVER) > before + 0.05, "dragging right raises the crossover");
        click(&mut harness, "Solo Mid");
        assert_eq!(value(&engine, opentake_engine::multiband::band_param(1, opentake_engine::multiband::SOLO)), 1.0);
    }

    #[test]
    fn reverb_editor_shows_the_tail_and_turns_knobs() {
        let (mut harness, engine) = harness_with_track();
        let slot = engine.lock().unwrap().insert_plugin(0, "reverb", None).unwrap();
        harness.state_mut().open_plugins.push((0, SlotRef::Effect(slot)));
        harness.step();
        harness.step();
        harness.get_by_label("Reverb Decay");
        let value = |e: &SharedEngine, id: u32| e.lock().unwrap().slot_params(0, SlotRef::Effect(slot)).unwrap().iter().find(|p| p.id == id).unwrap().value;
        let before = value(&engine, opentake_engine::reverb::DECAY);
        let knob = harness.get_by_label("Decay").rect().center();
        drag(&mut harness, knob, knob - egui::vec2(0.0, 40.0));
        assert!(value(&engine, opentake_engine::reverb::DECAY) > before + 0.05, "dragging up lengthens the decay");
        for title in ["Pre-Delay", "Size", "Damping", "Low Cut", "Diffusion", "Modulation", "Width", "Mix", "Level"] {
            harness.get_by_label(title);
        }
    }

    #[test]
    fn double_clicking_a_mixer_slot_opens_its_editor() {
        let (mut harness, engine) = harness_with_track();
        engine.lock().unwrap().insert_plugin(0, "reverb", None).unwrap();
        harness.state_mut().selected_track = Some(0);
        harness.state_mut().bottom = Some(BottomTab::Mixer);
        harness.step();
        harness.step();
        let pill = harness.get_all_by_label(opentake_engine::reverb::NAME).next().unwrap().rect().center();
        double_click_at(&mut harness, pill);
        assert_eq!(harness.state().open_plugins, vec![(0, SlotRef::Effect(0))], "a built-in opens OpenTake's window");
        assert!(harness.query_by_label("Open Editor").is_none(), "built-ins have no other");
        // Asked for over MCP, it opens the same way.
        harness.state_mut().open_plugins.clear();
        engine.lock().unwrap().request_editor(0, SlotRef::Effect(0)).unwrap();
        for _ in 0..3 {
            harness.step();
        }
        assert_eq!(harness.state().open_plugins, vec![(0, SlotRef::Effect(0))]);

        // An audio unit with a view of its own: the header offers it. Tests don't run on the
        // main thread, so opening fails there and the generic controls stay.
        #[cfg(target_os = "macos")]
        {
            engine.lock().unwrap().insert_plugin(0, "au:aufx:dely:appl", None).unwrap();
            harness.state_mut().open_plugins = vec![(0, SlotRef::Effect(1))];
            harness.step();
            harness.step();
            click(&mut harness, "Open Editor");
            assert!(harness.state().editors.is_empty());
            assert_eq!(harness.state().open_plugins, vec![(0, SlotRef::Effect(1))]);
        }
    }

    #[test]
    fn plugin_header_picks_a_side_chain_source() {
        let (mut harness, engine) = harness_with_track();
        let (c, eq) = {
            let mut e = engine.lock().unwrap();
            e.add_track("Kick").unwrap();
            (e.insert_plugin(0, "comp", None).unwrap(), e.insert_plugin(0, "eq", None).unwrap())
        };
        harness.state_mut().bottom = Some(BottomTab::Mixer);
        harness.state_mut().open_plugins = vec![(0, SlotRef::Effect(c))];
        harness.step();
        harness.step();
        assert!(harness.query_by_label("Key from Guitar").is_none(), "never its own track");
        click(&mut harness, "Side Chain");
        click(&mut harness, "Key from Kick");
        let key = |e: &SharedEngine| e.lock().unwrap().side_chain(0, SlotRef::Effect(c));
        assert_eq!(key(&engine), Some((1, false)));
        harness.get_by_label("Key: Kick");
        harness.get_by_label("Side Chain Kick"); // the key on the mixer's slot
        harness.get_by_label("Key Low Cut");
        click(&mut harness, "Key before the source's fader and mute");
        assert_eq!(key(&engine), Some((1, true)));
        click(&mut harness, "Side Chain");
        click(&mut harness, "No Side Chain");
        assert_eq!(key(&engine), None);
        // An EQ takes no key: no menu in its header.
        harness.state_mut().open_plugins = vec![(0, SlotRef::Effect(eq))];
        harness.step();
        harness.step();
        assert!(harness.query_by_label("Side Chain").is_none());
    }

    #[test]
    fn deesser_editor_switches_mode_and_listen() {
        let (mut harness, engine) = harness_with_track();
        let d = engine.lock().unwrap().insert_plugin(0, "de-esser", None).unwrap();
        harness.state_mut().open_plugins = vec![(0, SlotRef::Effect(d))];
        harness.step();
        harness.step();
        let value = |e: &SharedEngine, id: u32| e.lock().unwrap().slot_params(0, SlotRef::Effect(d)).unwrap().iter().find(|p| p.id == id).unwrap().value;
        click(&mut harness, "Reduce this way"); // Wideband
        assert_eq!(value(&engine, opentake_engine::deesser::MODE), 1.0);
        click(&mut harness, "Hear only the band being detected");
        assert_eq!(value(&engine, opentake_engine::deesser::LISTEN), 1.0);
        harness.get_by_label("Frequency");
    }

    #[test]
    fn take_folder_badge_picks_the_take() {
        let (mut harness, engine, dir) = harness_with_region();
        take_folder(&engine);
        harness.state_mut().view.px_per_sec = 200.0;
        harness.step();
        harness.step();
        click(&mut harness, "Takes riff");
        click(&mut harness, "Take 1");
        let e = engine.lock().unwrap();
        let c = &e.project().tracks[0].clips[0];
        assert_eq!((c.active_take, c.source_offset_secs), (0, 0.0));
        assert_eq!(e.history_info().undo.as_deref(), Some("Select Take"));
        std::fs::remove_dir_all(dir).ok();
    }

    /// The demo region as a 1 s take folder at 1.0 s: three takes of one cycle recording in
    /// the same file, the last one playing.
    fn take_folder(engine: &SharedEngine) {
        let mut e = engine.lock().unwrap();
        let mut folder = e.remove_clip(0, 0).unwrap();
        let file = folder.file.clone();
        folder.length_secs = 1.0;
        folder.push_take("Take 2".into(), file.clone(), 1.0);
        folder.push_take("Take 3".into(), file, 2.0);
        e.insert_clip(0, folder).unwrap();
    }

    #[test]
    fn take_lanes_swipe_a_comp_and_pick_a_whole_take() {
        let (mut harness, engine, dir) = harness_with_region();
        take_folder(&engine);
        harness.state_mut().view.px_per_sec = 200.0;
        harness.step();
        harness.step();
        click(&mut harness, "Show Takes riff");
        let folder = |e: &SharedEngine| e.lock().unwrap().project().tracks[0].clips[0].clone();
        assert!(folder(&engine).show_takes);
        // Swipe across take 2's lane from 0.25 s to 0.75 s into the folder (snap is off).
        let lane = harness.get_by_label("Take Take 2 riff").rect();
        let x = |t: f64| lane.left() + (t * 200.0) as f32;
        drag(&mut harness, egui::pos2(x(1.25), lane.center().y), egui::pos2(x(1.75), lane.center().y));
        let c = folder(&engine);
        assert_eq!(c.comp.len(), 1, "{:?}", c.comp);
        assert!(c.comp[0].take == 1 && (c.comp[0].start_secs - 0.25).abs() < 1e-3 && (c.comp[0].end_secs - 0.75).abs() < 1e-3, "{:?}", c.comp);
        {
            // The swipe is one undo step.
            let mut e = engine.lock().unwrap();
            assert_eq!(e.history_info().undo.as_deref(), Some("Comp Take"));
            e.undo().unwrap();
            assert!(!e.project().tracks[0].clips[0].is_comped());
            e.redo().unwrap();
        }
        harness.step();
        assert!(harness.query_by_label("Take Lane riff Take 3").is_some());
        // Clicking take 1's header plays it all through.
        click(&mut harness, "Take Lane riff Take 1");
        let c = folder(&engine);
        assert_eq!((c.active_take, c.comp.len()), (0, 0));
        // Collapsing hides the lanes again.
        click(&mut harness, "Hide Takes riff");
        assert!(harness.query_by_label("Take Take 2 riff").is_none());
        std::fs::remove_dir_all(dir).ok();
    }

    /// Renders an open, comped take folder to a PNG for visual review:
    /// `OPENTAKE_SCREENSHOT=/tmp/takes.png cargo test -p opentake render_take_lanes -- --ignored`
    #[test]
    #[ignore = "renders with wgpu; run manually for visual checks"]
    fn render_take_lanes() {
        let out = std::env::var("OPENTAKE_SCREENSHOT").unwrap_or_else(|_| "opentake-takes.png".into());
        let dir = std::env::temp_dir().join(format!("opentake-takes-shot-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("Vocal.wav");
        write_tone(&wav, 330.0, 12.0, 2.5);
        let engine: SharedEngine = Arc::new(Mutex::new(Engine::new()));
        {
            let mut e = engine.lock().unwrap();
            e.add_track("Vocal").unwrap();
            e.add_clip(0, &wav, 0.0).unwrap();
            let mut folder = e.remove_clip(0, 0).unwrap();
            (folder.name, folder.position_secs, folder.length_secs, folder.show_takes) = ("Verse".into(), 1.0, 4.0, true);
            for take in 2..=4 {
                folder.push_take(format!("Take {take}"), wav.clone(), (take - 1) as f64 * 2.0);
            }
            e.insert_clip(0, folder).unwrap();
            for (from, to, take) in [(0.0, 1.1, 0), (1.1, 2.0, 1), (2.6, 3.2, 2)] {
                e.set_comp_segment(0, 0, from, to, take).unwrap();
            }
            e.add_track("Bass").unwrap();
            e.add_clip(1, &wav, 0.5).unwrap();
            e.seek(2.3).unwrap();
        }
        let app_engine = engine.clone();
        let mut harness = Harness::builder()
            .with_size([1100.0, 560.0])
            .with_pixels_per_point(2.0)
            .with_step_dt(1.0 / 60.0)
            .wgpu()
            .build_eframe(move |cc| {
                theme::apply(&cc.egui_ctx);
                DawApp::new(app_engine, None)
            });
        harness.state_mut().selection = vec![RegionRef::audio(0, 0)];
        harness.state_mut().view.px_per_sec = 140.0;
        for _ in 0..4 {
            harness.step();
        }
        harness.render().expect("render").save(&out).expect("save png");
        std::fs::remove_dir_all(dir).ok();
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
        // In beats: at 120 bpm 0, 0.5 and 1.5 s.
        let notes = vec![
            Note { start_beats: 0.0, length_beats: 0.5, pitch: 60, velocity: 100, tab: None },
            Note { start_beats: 1.0, length_beats: 0.5, pitch: 62, velocity: 100, tab: None },
            Note { start_beats: 3.0, length_beats: 0.5, pitch: 72, velocity: 100, tab: None },
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

        // ⌘-drag one of them: both are copied 0.5 s (a beat) later, originals stay.
        let beat = piano_roll::tests::x_of(harness.state(), 1.0) - piano_roll::tests::x_of(harness.state(), 0.0);
        press_drag(&mut harness, a, a + egui::vec2(beat, 0.0), egui::Modifiers::COMMAND);
        let after = engine.lock().unwrap().project().tracks[1].midi_clips[0].notes.clone();
        assert_eq!(after.len(), 5, "two copies added: {after:?}");
        for n in &notes {
            assert!(after.contains(n), "original {n:?} kept");
        }
        assert!(after.iter().any(|n| n.pitch == 60 && (n.start_beats - 1.0).abs() < 0.04));
        assert!(after.iter().any(|n| n.pitch == 62 && (n.start_beats - 2.0).abs() < 0.04));
    }

    #[test]
    fn piano_roll_q_quantizes_the_selection_or_everything_and_notes_follow_the_tempo() {
        use opentake_engine::project::QuantizeGrid;
        let (mut harness, engine) = harness_with_track();
        let note = |start: f64, pitch: u8| Note { start_beats: start, length_beats: 0.5, pitch, velocity: 100, tab: None };
        {
            let mut e = engine.lock().unwrap();
            let t = e.add_instrument_track("Keys", "synth").unwrap();
            let c = e.add_midi_clip_beats(t, 0.0, 8.0, None).unwrap();
            e.set_midi_notes(t, c, vec![note(0.1, 60), note(1.2, 64), note(2.9, 67)]).unwrap();
        }
        harness.state_mut().snap_mode = timeline::SnapMode::Off;
        harness.state_mut().view.px_per_sec = 300.0;
        harness.state_mut().piano.quantize.grid = Some(QuantizeGrid::Quarter);
        harness.state_mut().open_piano_roll(RegionRef::midi(1, 0));
        harness.step();
        harness.step();
        let starts = |e: &SharedEngine| e.lock().unwrap().project().tracks[1].midi_clips[0].notes.iter().map(|n| (n.start_beats * 1e6).round() / 1e6).collect::<Vec<_>>();

        // Click the middle note, Q: only it moves (to beat 1), and stays selected.
        let at = piano_roll::tests::note_center(harness.state(), &note(1.2, 64));
        harness.hover_at(at);
        harness.step();
        for pressed in [true, false] {
            harness.event(egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE });
            harness.step();
        }
        assert_eq!(harness.state().piano.selected, vec![note(1.2, 64)]);
        harness.key_press(egui::Key::Q);
        harness.step();
        harness.step();
        assert_eq!(starts(&engine), [0.1, 1.0, 2.9]);
        assert_eq!(harness.state().piano.selected, vec![note(1.0, 64)], "the selection follows the note");
        assert_eq!(engine.lock().unwrap().history_info().undo.as_deref(), Some("Quantize"));

        // Nothing selected: Q quantizes every note; the toolbar names the grid.
        harness.key_press(egui::Key::Escape);
        harness.step();
        harness.key_press(egui::Key::Q);
        harness.step();
        harness.step();
        assert_eq!(starts(&engine), [0.0, 1.0, 3.0]);
        assert!(harness.query_by_label("Quantize Grid").is_some());

        // Half the tempo: the note on beat 3 is drawn at 3 s (it was at 1.5 s), with the bars.
        let before = piano_roll::tests::x_of(harness.state(), 3.0);
        assert!((before - harness.state().view.x(1.5)).abs() < 0.5, "{before}");
        engine.lock().unwrap().set_tempo(60.0, None).unwrap();
        harness.step();
        harness.step();
        let after = piano_roll::tests::x_of(harness.state(), 3.0);
        assert!((after - harness.state().view.x(3.0)).abs() < 0.5, "{after} vs {}", harness.state().view.x(3.0));
        let center = piano_roll::tests::note_center(harness.state(), &note(3.0, 67)).x;
        assert!((center - harness.state().view.x(3.25)).abs() < 0.5, "half a beat long: 0.5 s");
    }

    /// A Keys track with a 2 s (4 beat) region of three notes, open in the piano roll at 400 px/s.
    fn harness_with_piano_roll() -> (Harness<'static, DawApp>, SharedEngine) {
        let (mut harness, engine) = harness_with_track();
        {
            let mut e = engine.lock().unwrap();
            let t = e.add_instrument_track("Keys", "synth").unwrap();
            let c = e.add_midi_clip(t, 0.0, 2.0, None).unwrap();
            // At 0, 0.5 and 1 s, 0.4 s long.
            let note = |start: f64, pitch: u8| Note { start_beats: 2.0 * start, length_beats: 0.8, pitch, velocity: 100, tab: None };
            e.set_midi_notes(t, c, vec![note(0.0, 60), note(0.5, 64), note(1.0, 67)]).unwrap();
        }
        harness.state_mut().snap_mode = timeline::SnapMode::Off;
        harness.state_mut().view.px_per_sec = 400.0;
        harness.state_mut().open_piano_roll(RegionRef::midi(1, 0));
        harness.step();
        harness.step();
        (harness, engine)
    }

    #[test]
    fn piano_roll_pencil_draws_a_pitch_bend_curve_as_one_step() {
        use opentake_engine::project::Controller;
        let (mut harness, engine) = harness_with_piano_roll();
        assert!(harness.state().piano.ctrl_rect.is_none(), "hidden until asked for");
        click(&mut harness, "MIDI Draw");
        assert_eq!(harness.state().piano.ctrl_kind, Controller::PitchBend, "the lane shows pitch bend first");
        let lane = harness.state().piano.ctrl_rect.expect("controller lane drawn");
        // Seconds into the region (two beats a second) on screen.
        let xs: Vec<f32> = [0.2, 0.8, 1.5].iter().map(|t| piano_roll::tests::x_of(harness.state(), 2.0 * t)).collect();
        let x = |t: f64| xs[[0.2, 0.8, 1.5].iter().position(|&s| s == t).unwrap()];
        // Pencil from the top (full bend up) at 0.2 s down to the bottom at 0.8 s.
        let (from, to) = (egui::pos2(x(0.2), lane.top() + 4.0), egui::pos2(x(0.8), lane.bottom() - 4.0));
        harness.hover_at(from);
        harness.step();
        harness.drag_at(from);
        harness.step();
        for i in 1..=24 {
            harness.hover_at(from + (to - from) * (i as f32 / 24.0));
            harness.step();
        }
        harness.drop_at(to);
        harness.step();
        harness.step();
        let events = {
            let e = engine.lock().unwrap();
            assert_eq!(e.history_info().undo.as_deref(), Some("Edit Pitch Bend"));
            e.project().tracks[1].midi_clips[0].lane(Controller::PitchBend).expect("a bend lane").events.clone()
        };
        assert!(events.len() >= 10, "a curve of events: {events:?}");
        assert!((events[0].time_beats - 0.4).abs() < 0.04 && events[0].value > 0.9, "{:?}", events[0]);
        assert!(events.last().unwrap().value < -0.9 && (events.last().unwrap().time_beats - 1.6).abs() < 0.04);
        assert!(events.windows(2).all(|w| w[1].time_beats > w[0].time_beats && w[1].value <= w[0].value), "falls left to right");

        // A click adds one event; right-click on it deletes it; each is one step.
        let click_at = egui::pos2(x(1.5), lane.center().y);
        let press = |h: &mut Harness<'static, DawApp>, button: egui::PointerButton| {
            for pressed in [true, false] {
                h.event(egui::Event::PointerButton { pos: click_at, button, pressed, modifiers: egui::Modifiers::NONE });
                h.step();
            }
            h.step();
        };
        harness.hover_at(click_at);
        harness.step();
        press(&mut harness, egui::PointerButton::Primary);
        let count = || engine.lock().unwrap().project().tracks[1].midi_clips[0].lane(Controller::PitchBend).unwrap().events.len();
        assert_eq!(count(), events.len() + 1);
        press(&mut harness, egui::PointerButton::Secondary);
        assert_eq!(count(), events.len());
        engine.lock().unwrap().undo().unwrap();
        engine.lock().unwrap().undo().unwrap();
        engine.lock().unwrap().undo().unwrap();
        assert!(engine.lock().unwrap().project().tracks[1].midi_clips[0].controllers.is_empty(), "three gestures, three steps");
    }

    /// Renders MIDI across a tempo change (a note on every beat, a looped region) with the
    /// piano roll's quantize menu open, to a PNG for visual review:
    /// `OPENTAKE_SCREENSHOT=/tmp/tempo.png cargo test -p opentake render_midi_following_the_tempo -- --ignored`
    #[test]
    #[ignore = "renders with wgpu; run manually for visual checks"]
    fn render_midi_following_the_tempo() {
        let out = std::env::var("OPENTAKE_SCREENSHOT").unwrap_or_else(|_| "opentake-tempo.png".into());
        let engine: SharedEngine = Arc::new(Mutex::new(Engine::new()));
        {
            let mut e = engine.lock().unwrap();
            // 120 bpm, slowing to 80 at bar 3 (4 s).
            e.set_tempo_changes(vec![opentake_engine::project::TempoChange { at_secs: 4.0, bpm: 80.0, time_signature: None }], None).unwrap();
            let t = e.add_instrument_track("Keys", "synth").unwrap();
            let c = e.add_midi_clip_beats(t, 0.0, 16.0, Some("Beats")).unwrap();
            let notes = (0..16).map(|b| Note { start_beats: b as f64 + if b % 2 == 1 { 0.12 } else { 0.0 }, length_beats: 0.5, pitch: 60 + (b % 4) as u8 * 4, velocity: 100, tab: None }).collect();
            e.set_midi_notes(t, c, notes).unwrap();
            let l = e.add_midi_clip_beats(t, 16.0, 4.0, Some("Loop")).unwrap();
            e.set_midi_notes(t, l, vec![Note { start_beats: 0.0, length_beats: 1.0, pitch: 67, velocity: 100, tab: None }, Note { start_beats: 2.0, length_beats: 1.0, pitch: 72, velocity: 100, tab: None }]).unwrap();
            e.set_region_loop(&[RegionRef::midi(t, l)], opentake_engine::LoopTo::Passes(2)).unwrap();
        }
        let app_engine = engine.clone();
        let mut harness = Harness::builder()
            .with_size([1200.0, 700.0])
            .with_pixels_per_point(2.0)
            .with_step_dt(1.0 / 60.0)
            .wgpu()
            .build_eframe(move |cc| {
                theme::apply(&cc.egui_ctx);
                DawApp::new(app_engine, None)
            });
        harness.state_mut().view.px_per_sec = 60.0;
        harness.state_mut().open_piano_roll(RegionRef::midi(0, 0));
        for _ in 0..4 {
            harness.step();
        }
        harness.get_by_label("Options").click();
        for _ in 0..4 {
            harness.step();
        }
        harness.render().expect("render").save(&out).expect("save png");
    }

    /// Renders the piano roll with a drawn pitch-bend curve to a PNG for visual review:
    /// `OPENTAKE_SCREENSHOT=/tmp/bend.png cargo test -p opentake render_controller_lane -- --ignored`
    #[test]
    #[ignore = "renders with wgpu; run manually for visual checks"]
    fn render_controller_lane() {
        use opentake_engine::project::{Controller, ControllerEvent};
        let out = std::env::var("OPENTAKE_SCREENSHOT").unwrap_or_else(|_| "opentake-bend.png".into());
        let engine: SharedEngine = Arc::new(Mutex::new(Engine::new()));
        {
            let mut e = engine.lock().unwrap();
            let t = e.add_instrument_track("Lead", "synth").unwrap();
            let c = e.add_midi_clip(t, 0.0, 4.0, None).unwrap();
            // Times in seconds, at 120 bpm: two beats each.
            let note = |start: f64, len: f64, pitch: u8| Note { start_beats: 2.0 * start, length_beats: 2.0 * len, pitch, velocity: 100, tab: None };
            e.set_midi_notes(t, c, vec![note(0.0, 0.9, 64), note(1.0, 1.4, 67), note(2.5, 1.2, 69)]).unwrap();
            let bend: Vec<ControllerEvent> = (0..=60).map(|i| {
                let t = 1.0 + i as f64 * 0.02;
                ControllerEvent { time_beats: 2.0 * t, value: (((t - 1.0) / 0.6).min(1.0) * 0.5) as f32 + if t > 1.6 { 0.08 * ((t - 1.6) * 35.0).sin() as f32 } else { 0.0 } }
            }).chain([ControllerEvent { time_beats: 4.8, value: 0.0 }]).collect();
            e.set_midi_controllers(t, c, Controller::PitchBend, bend, true).unwrap();
            e.set_midi_controllers(t, c, Controller::MOD_WHEEL, vec![ControllerEvent { time_beats: 5.0, value: 0.6 }], true).unwrap();
        }
        let app_engine = engine.clone();
        let mut harness = Harness::builder()
            .with_size([1100.0, 620.0])
            .with_pixels_per_point(2.0)
            .with_step_dt(1.0 / 60.0)
            .wgpu()
            .build_eframe(move |cc| {
                theme::apply(&cc.egui_ctx);
                DawApp::new(app_engine, None)
            });
        harness.state_mut().view.px_per_sec = 200.0;
        harness.state_mut().open_piano_roll(RegionRef::midi(0, 0));
        for _ in 0..4 {
            harness.step();
        }
        harness.render().expect("render").save(&out).expect("save png");
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
    fn midi_file_import_dialog_creates_instrument_tracks() {
        // A MIDI file with a keys part and a drum part, written by the engine's exporter.
        let path = std::env::temp_dir().join(format!("opentake-gui-midi-{}.mid", std::process::id()));
        let mut src = Engine::new();
        for (name, inst, pitch) in [("Keys", "synth", 60), ("Beat", "drums", 36)] {
            let t = src.add_instrument_track(name, inst).unwrap();
            let c = src.add_midi_clip(t, 0.0, 2.0, None).unwrap();
            src.set_midi_notes(t, c, vec![Note { start_beats: 0.0, length_beats: 1.0, pitch, velocity: 100, tab: None }]).unwrap();
        }
        src.export_midi_file(&path).unwrap();

        let (mut harness, engine) = harness_with_track();
        harness.state_mut().open_midi_file_dialog(path.clone());
        harness.step();
        harness.step();
        // Untick the drums, then import.
        click(&mut harness, "Beat");
        click(&mut harness, "Import");
        let e = engine.lock().unwrap();
        let names: Vec<_> = e.project().tracks.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["Guitar", "Keys"], "existing track + imported keys only");
        let imported = &e.project().tracks[1];
        assert!(imported.is_instrument() && imported.midi_clips[0].notes.len() == 1);
        drop(e);
        assert!(harness.state().midi_import.is_none(), "dialog closes after import");
        assert_eq!(harness.state().selected_track, Some(1));
        std::fs::remove_file(path).ok();
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
        click(&mut harness, "Metronome (K) — right-click for volume and count-in");
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
    fn buses_from_the_track_menu_and_the_output_selector() {
        let (mut harness, engine) = harness_with_track();
        let output = |e: &SharedEngine| e.lock().unwrap().project().output_of(0);
        click(&mut harness, "Guitar");
        click(&mut harness, "Track");
        click(&mut harness, &menu_label(MenuCommand::NewBusFromSelection));
        assert!(engine.lock().unwrap().project().tracks[1].bus);
        assert_eq!(output(&engine), Some(1));
        // The new bus is selected; neither its header nor its strip can record.
        assert_eq!(harness.state().selected_track, Some(1));
        assert_eq!(harness.query_all_by_label("Record Enable").count(), 1, "only the guitar's header");

        // The guitar's Output selector in the inspector: back to Stereo Out, then a new bus.
        harness.state_mut().selected_track = Some(0);
        harness.step();
        click(&mut harness, "Output: Bus 1");
        harness.get_by_role_and_label(egui::Role::Button, "Stereo Out").click();
        harness.step();
        harness.step();
        assert_eq!(output(&engine), None);
        click(&mut harness, "Output: Stereo Out");
        click(&mut harness, "New Bus");
        assert_eq!(output(&engine), Some(2));
        assert_eq!(harness.state().selected_track, Some(2));
        engine.lock().unwrap().undo().unwrap();
        assert_eq!(engine.lock().unwrap().project().tracks.len(), 2);
    }

    #[test]
    fn sends_in_the_channel_strip() {
        let (mut harness, engine) = harness_with_track();
        engine.lock().unwrap().add_bus("Verb").unwrap();
        let sends = |e: &SharedEngine| e.lock().unwrap().project().tracks[0].sends.clone();
        click(&mut harness, "Guitar");
        click(&mut harness, "Add Send");
        harness.get_by_role_and_label(egui::Role::Button, "Verb").click();
        harness.step();
        harness.step();
        assert_eq!(engine.lock().unwrap().project().send_dest(0, 0), Some(1));
        click(&mut harness, "Send 1 Pre-Fader");
        click(&mut harness, "Send 1 Mute");
        let s = &sends(&engine)[0];
        assert!(s.pre_fader && s.mute);

        // The knob: drag down to lower the level, double-click for 0 dB.
        let knob = harness.get_by_label("Send 1 Level").rect();
        drag(&mut harness, knob.center(), knob.center() + egui::vec2(0.0, 30.0));
        assert!(sends(&engine)[0].level < 0.5, "{}", sends(&engine)[0].level);
        assert_eq!(engine.lock().unwrap().history_info().undo_steps[..2], ["Send Level", "Send Mute"], "a drag is one step");
        double_click_at(&mut harness, knob.center());
        assert!((sends(&engine)[0].level - 1.0).abs() < 1e-4);

        // No Send removes it; New Reverb Send makes a "Reverb" bus fed by a new send.
        click(&mut harness, "Send 1: Verb");
        click(&mut harness, "No Send");
        assert!(sends(&engine).is_empty());
        click(&mut harness, "Add Send");
        click(&mut harness, "New Reverb Send");
        let p = engine.lock().unwrap().project().clone();
        assert_eq!((p.tracks[2].name.as_str(), p.send_dest(0, 0)), ("Reverb", Some(2)));
        // The bus's strip counts the send feeding it.
        harness.state_mut().selected_track = Some(2);
        harness.step();
        assert!(harness.query_by_label("1 send").is_some());
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
        // 0.5 s into the region, 0.5 s long, at 120 bpm.
        let note = Note { start_beats: 1.0, length_beats: 1.0, pitch: 60, velocity: 100, tab: None };
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
    fn marker_track_creates_moves_renames_and_navigates_markers() {
        let (mut harness, engine) = harness_with_track();
        harness.state_mut().snap_mode = timeline::SnapMode::Bar;
        engine.lock().unwrap().seek(4.3).unwrap();
        harness.step();
        // M: a marker at the playhead, rounded to the nearest bar (bar 3 = 4 s at 120 BPM).
        harness.key_press(egui::Key::M);
        harness.step();
        harness.step();
        let markers = |e: &SharedEngine| e.lock().unwrap().project().markers.iter().map(|m| (m.time_secs, m.name.clone())).collect::<Vec<_>>();
        assert_eq!(markers(&engine), vec![(4.0, "Marker 1".to_string())]);
        let row = harness.get_by_label("Marker Track").rect();

        // Drag its flag to bar 5 (8 s): snapped, one undo step.
        let from = egui::pos2(harness.state().view.x(4.0) + 10.0, row.center().y);
        let to = from + egui::vec2(harness.state().view.x(8.1) - harness.state().view.x(4.0), 0.0);
        press_drag(&mut harness, from, to, egui::Modifiers::NONE);
        assert_eq!(markers(&engine)[0].0, 8.0);
        assert_eq!(engine.lock().unwrap().history_info().undo_steps[..2], ["Move Marker", "Create Marker"]);

        // Double-click to rename in place: the name is selected, typing replaces it.
        let flag = egui::pos2(harness.state().view.x(8.0) + 10.0, row.center().y);
        double_click_at(&mut harness, flag);
        harness.event(egui::Event::Text("Verse".into()));
        harness.key_press(egui::Key::Enter);
        harness.step();
        harness.step();
        assert_eq!(markers(&engine)[0].1, "Verse");

        // Clicking a marker moves the playhead there; ⌥← / ⌥→ step between markers.
        engine.lock().unwrap().add_marker(2.0, Some("Intro".into()), None).unwrap();
        engine.lock().unwrap().seek(0.0).unwrap();
        click_at(&mut harness, flag);
        assert_eq!(engine.lock().unwrap().status().position_secs, 8.0);
        harness.key_press_modifiers(egui::Modifiers::ALT, egui::Key::ArrowLeft);
        harness.step();
        harness.step();
        assert_eq!(engine.lock().unwrap().status().position_secs, 2.0);
        harness.key_press_modifiers(egui::Modifiers::ALT, egui::Key::ArrowRight);
        harness.step();
        harness.step();
        assert_eq!(engine.lock().unwrap().status().position_secs, 8.0);

        // Right-click: cycle the marker's section (Intro runs to Verse).
        let intro = egui::pos2(harness.state().view.x(2.0) + 10.0, row.center().y);
        harness.hover_at(intro);
        harness.step();
        for pressed in [true, false] {
            harness.event(egui::Event::PointerButton { pos: intro, button: egui::PointerButton::Secondary, pressed, modifiers: egui::Modifiers::NONE });
        }
        harness.step();
        harness.step();
        click(&mut harness, "Set Cycle to Marker");
        {
            let e = engine.lock().unwrap();
            let p = e.project();
            assert!(p.loop_enabled && (p.loop_start_secs, p.loop_end_secs) == (2.0, 8.0));
        }

        // ⌫ deletes the selected marker (not the selected track).
        click_at(&mut harness, intro);
        harness.key_press(egui::Key::Backspace);
        harness.step();
        harness.step();
        assert_eq!(markers(&engine), vec![(8.0, "Verse".to_string())]);
        assert_eq!(engine.lock().unwrap().project().tracks.len(), 1);
        // Double-clicking empty marker track space adds one there.
        let empty = egui::pos2(harness.state().view.x(12.2), row.center().y);
        double_click_at(&mut harness, empty);
        assert_eq!(markers(&engine).last().cloned(), Some((12.0, "Marker 1".to_string())), "Marker 1 was renamed, so the name is free");
    }

    #[test]
    #[ignore = "renders with wgpu; run manually for visual checks"]
    fn render_marker_track() {
        let out = std::env::var("OPENTAKE_SCREENSHOT").unwrap_or_else(|_| "opentake-markers.png".into());
        let engine: SharedEngine = Arc::new(Mutex::new(Engine::new()));
        {
            let mut e = engine.lock().unwrap();
            e.add_track("Guitar").unwrap();
            e.add_track("Bass").unwrap();
            e.add_midi_clip(1, 0.0, 16.0, None).ok();
            for (t, name, color) in [(0.0, "Intro", None), (4.0, "Verse", Some([96, 190, 100])), (8.0, "Pre-Chorus", Some([86, 140, 230])), (8.5, "Hit", None), (12.0, "Chorus", Some([226, 84, 84]))] {
                e.add_marker(t, Some(name.into()), color).unwrap();
            }
            let pre = e.project().markers[2].id;
            e.cycle_marker(pre).unwrap();
            e.seek(9.0).unwrap();
        }
        let app_engine = engine.clone();
        let mut harness = Harness::builder()
            .with_size([1100.0, 420.0])
            .with_pixels_per_point(2.0)
            .with_step_dt(1.0 / 60.0)
            .wgpu()
            .build_eframe(move |cc| {
                theme::apply(&cc.egui_ctx);
                DawApp::new(app_engine, None)
            });
        harness.state_mut().view.px_per_sec = 50.0;
        harness.state_mut().view.marker_sel = engine.lock().unwrap().project().markers.get(1).map(|m| m.id);
        for _ in 0..4 {
            harness.step();
        }
        harness.render().expect("render").save(&out).expect("save png");
    }

    #[test]
    fn tempo_track_adds_and_edits_tempo_changes() {
        let (mut harness, engine) = harness_with_track();
        harness.state_mut().snap_mode = timeline::SnapMode::Bar;
        harness.step();
        let row = harness.get_by_label("Tempo Track").rect();
        // Double-click near bar 3 (4 s at 120 BPM): a change snapped to the bar line.
        let x = harness.state().view.x(4.1);
        double_click_at(&mut harness, egui::pos2(x, row.center().y));
        let changes = engine.lock().unwrap().project().tempo_changes.clone();
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!((changes[0].at_secs, changes[0].bpm), (4.0, 120.0));

        // Drag the section after it up 20 px: +10 BPM, as one undo step.
        let from = egui::pos2(harness.state().view.x(5.0), row.center().y);
        press_drag(&mut harness, from, from - egui::vec2(0.0, 20.0), egui::Modifiers::NONE);
        assert_eq!(engine.lock().unwrap().project().tempo_changes[0].bpm, 130.0);
        // Before the change, dragging edits the opening tempo.
        let from = egui::pos2(harness.state().view.x(1.0), row.center().y);
        press_drag(&mut harness, from, from + egui::vec2(0.0, 20.0), egui::Modifiers::NONE);
        assert_eq!(engine.lock().unwrap().project().tempo_bpm, 110.0);
        let map = engine.lock().unwrap().project().tempo_map();
        assert_eq!(map.tempo_at(6.0), 130.0);

        harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Z);
        harness.step();
        harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Z);
        harness.step();
        let e = engine.lock().unwrap();
        assert_eq!((e.project().tempo_bpm, e.project().tempo_changes[0].bpm), (120.0, 120.0), "each drag is one undo step");
    }

    #[test]
    fn count_in_settings_from_metronome_menu_and_transport_menu() {
        let (mut harness, engine) = harness_with_track();
        let button = harness.get_by_label("Metronome (K) — right-click for volume and count-in").rect().center();
        harness.hover_at(button);
        harness.step();
        for pressed in [true, false] {
            harness.event(egui::Event::PointerButton { pos: button, button: egui::PointerButton::Secondary, pressed, modifiers: egui::Modifiers::NONE });
        }
        harness.step();
        harness.step();
        click(&mut harness, "2 bars");
        assert_eq!(engine.lock().unwrap().project().count_in_bars, 2);
        click(&mut harness, "Transport");
        click(&mut harness, &menu_label(MenuCommand::ToggleCountInOnPlay));
        assert!(engine.lock().unwrap().project().count_in_on_play);
        click(&mut harness, "Transport");
        click(&mut harness, &menu_label(MenuCommand::ToggleCountIn));
        assert_eq!(engine.lock().unwrap().project().count_in_bars, 0);
    }

    #[test]
    fn master_eq_insert_and_drag_a_band_in_the_editor() {
        let (mut harness, engine) = harness_with_track();
        harness.key_press(egui::Key::X);
        harness.step();
        harness.step();
        // The inspector shows Stereo Out too; use the mixer's (lowest) add button.
        let add = harness.query_all_by_label("Add audio effect to the master bus").map(|n| n.rect()).max_by(|a, b| a.top().total_cmp(&b.top())).unwrap();
        harness.hover_at(add.center());
        harness.step();
        for pressed in [true, false] {
            harness.event(egui::Event::PointerButton { pos: add.center(), button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE });
        }
        harness.step();
        harness.step();
        click(&mut harness, opentake_engine::eq::NAME);
        assert_eq!(engine.lock().unwrap().project().master_plugins.len(), 1);
        assert!(harness.state().open_plugins.contains(&(MASTER_UI, SlotRef::Master(0))), "the EQ window opens");
        for _ in 0..3 {
            harness.step();
        }
        // Drag the Low Mid band (400 Hz, 0 dB) up.
        let graph = harness.get_by_label("EQ Graph").rect();
        let x = graph.left() + opentake_engine::eq::norm_freq(400.0) as f32 * graph.width();
        let from = egui::pos2(x, graph.center().y);
        press_drag(&mut harness, from, from - egui::vec2(0.0, 40.0), egui::Modifiers::NONE);
        let gain_id = opentake_engine::eq::param_id(2, opentake_engine::eq::Field::Gain);
        let gain = |e: &SharedEngine| {
            let e = e.lock().unwrap();
            let v = e.project().master_plugins[0].params.iter().find(|p| p.0 == gain_id).map(|p| p.1).unwrap_or(0.5);
            opentake_engine::eq::gain_of(v)
        };
        let boosted = gain(&engine);
        assert!(boosted > 5.0, "boosted {boosted} dB");
        assert_eq!(engine.lock().unwrap().history_info().undo.as_deref(), Some("Adjust Low Mid"));
        engine.lock().unwrap().undo().unwrap();
        assert!(gain(&engine).abs() < 1e-6, "the whole drag is one undo step");
    }

    #[test]
    fn compressor_limiter_and_delay_from_the_add_effect_menu() {
        let (mut harness, engine) = harness_with_track();
        harness.key_press(egui::Key::X);
        harness.step();
        harness.step();
        let add_fx = |harness: &mut Harness<'static, DawApp>| {
            // The mixer strip's add button (the inspector has one too): the lowest on screen.
            let r = harness.query_all_by_label("Add audio effect").map(|n| n.rect()).max_by(|a, b| a.top().total_cmp(&b.top())).unwrap();
            harness.hover_at(r.center());
            harness.step();
            for pressed in [true, false] {
                harness.event(egui::Event::PointerButton { pos: r.center(), button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE });
            }
            harness.step();
            harness.step();
        };
        for name in [opentake_engine::dynamics::comp::NAME, opentake_engine::dynamics::limiter::NAME, opentake_engine::delay::NAME] {
            add_fx(&mut harness);
            click(&mut harness, name);
        }
        let names: Vec<String> = engine.lock().unwrap().project().tracks[0].plugins.iter().map(|p| p.name.clone()).collect();
        assert_eq!(names, ["OpenTake Compressor", "OpenTake Limiter", "OpenTake Delay"]);
        for _ in 0..3 {
            harness.step();
        }
        // Each opens its editor.
        harness.get_by_label("Compressor Curve");
        harness.get_by_label("Ping-pong repeats");
        assert!(harness.query_all_by_label("Ceiling").next().is_some());
        // The limiter's look-ahead shows up as compensated latency (once audio runs it's in
        // the status bar); the engine reports it per slot through the processor.
        let p = engine.lock().unwrap().plugin_params(0, 1).unwrap();
        assert!(p.iter().any(|p| p.title == "Ceiling" && p.display.as_deref() == Some("-1.0")));
    }

    #[test]
    fn export_audio_dialog_and_background_export() {
        let (mut harness, engine) = harness_with_track();
        {
            let mut e = engine.lock().unwrap();
            let t = e.add_instrument_track("Keys", "synth").unwrap();
            let c = e.add_midi_clip(t, 0.0, 1.0, None).unwrap();
            e.set_midi_notes(t, c, vec![Note { start_beats: 0.0, length_beats: 1.6, pitch: 60, velocity: 100, tab: None }]).unwrap();
        }
        click(&mut harness, "File");
        click(&mut harness, &menu_label(MenuCommand::ExportAudio));
        assert!(harness.state().export_dialog.is_some());
        harness.get_by_label("Full mix");
        harness.get_by_label("FLAC");

        let dir = std::env::temp_dir().join(format!("opentake-gui-export-{}", std::process::id()));
        let options = opentake_engine::ExportOptions { mix_path: Some(dir.join("mix.wav")), stems_dir: Some(dir.join("stems")), tail_secs: 0.2, ..Default::default() };
        harness.state_mut().start_export(options);
        for _ in 0..600 {
            harness.step();
            if harness.state().export_task.is_none() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(harness.state().export_task.is_none(), "export finished");
        assert!(dir.join("mix.wav").exists());
        assert!(dir.join("stems").join("02 Keys.wav").exists(), "the empty audio track has no stem");
        assert!(!dir.join("stems").join("01 Guitar.wav").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tablature_size_and_paper_mode() {
        let (mut harness, engine) = harness_with_track();
        let fixture = std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../opentake-project/tests/fixtures/simple.gp"));
        engine.lock().unwrap().import_guitar_pro(&fixture, &opentake_engine::GuitarProImport::default()).unwrap();
        harness.state_mut().bottom = Some(BottomTab::Tab);
        harness.step();
        harness.step();
        click(&mut harness, "Paper");
        assert!(harness.state().tab_paper);
        click(&mut harness, "A+");
        click(&mut harness, "A+");
        assert_eq!(harness.state().tab_scale, 1.5);
        click(&mut harness, "A−");
        assert_eq!(harness.state().tab_scale, 1.25);
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
        let (drums, bass, vocal) = (dir.join("Drums.wav"), dir.join("Bass.wav"), dir.join("Vocal.wav"));
        write_tone(&drums, 70.0, 8.73, 14.0);
        write_tone(&bass, 55.0, 8.73, 3.0);
        write_tone(&vocal, 330.0, 8.73, 1.2);

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
            // Region fades, and drums overlapping into a crossfade.
            let fade = |secs, curve| opentake_engine::FadeUpdate { fade_out_secs: Some(secs), curve_out: Some(curve), ..Default::default() };
            e.set_clip_fades(1, 0, fade(0.8, opentake_engine::project::FadeCurve::SCurve)).unwrap();
            e.set_clip_fades(1, 1, opentake_engine::FadeUpdate { fade_in_secs: Some(0.5), ..Default::default() }).unwrap();
            e.move_clip(0, 1, 8.2, None).unwrap();
            e.update_track(1, TrackUpdate { volume: Some(0.6), pan: Some(-0.25), ..Default::default() }).unwrap();
            // A built-in effect, so the shot doesn't depend on the VST3s installed here.
            e.insert_plugin(1, "comp", None).unwrap();
            e.set_side_chain(1, SlotRef::Effect(0), Some(0), false).unwrap(); // the drums duck the bass
            // Instrument track with a chord progression.
            let keys = e.add_instrument_track("Keys", "synth").unwrap();
            let beat = 60.0 / 110.0;
            let clip = e.add_midi_clip(keys, 0.0, 16.0 * beat, Some("Chords")).unwrap();
            let mut notes = Vec::new();
            for (bar, chord) in [[60u8, 64, 67], [57, 60, 64], [53, 57, 60], [55, 59, 62]].iter().enumerate() {
                for (k, &p) in chord.iter().enumerate() {
                    notes.push(Note { start_beats: bar as f64 * 4.0, length_beats: 3.5, pitch: p, velocity: 80 + 15 * k as u8, tab: None });
                }
                notes.push(Note { start_beats: bar as f64 * 4.0 + 2.0, length_beats: 1.0, pitch: chord[2] + 5, velocity: 110, tab: None });
            }
            e.set_midi_notes(keys, clip, notes).unwrap();
            e.update_track(keys, TrackUpdate { armed: Some(true), ..Default::default() }).unwrap();
            // A vocal take folder from cycle recording over the loop, and a bus for bass and keys.
            let (start, bar) = (2.0 * 4.0 * beat, 4.0 * beat);
            let v = e.add_track("Vocal").unwrap();
            e.add_clip(v, &vocal, 0.0).unwrap();
            let mut folder = e.remove_clip(v, 0).unwrap();
            (folder.name, folder.position_secs, folder.length_secs) = ("Vocal".into(), start, bar);
            for take in 2..=3 {
                folder.push_take(format!("Take {take}"), vocal.clone(), (take - 1) as f64 * bar);
            }
            e.insert_clip(v, folder).unwrap();
            e.create_bus_from(&[1, 2], "Music Bus").unwrap();
            // A shared reverb fed by sends (the built-in reverb when there is one).
            let (verb, _) = e.send_to_new_bus(v, "Reverb", Some("reverb"), 0.4, false).unwrap();
            e.add_send(keys, verb, 0.25, false).unwrap();
            e.add_send(1, verb, 0.15, true).unwrap();
            // A volume ride on the bass, shown under its track.
            {
                use opentake_engine::project::automation::{AutomationPoint, AutomationTarget, Curve, gain_to_pos};
                let lane = e.add_lane(Some(1), AutomationTarget::Volume).unwrap();
                let point = |bars: f64, db: f32, curve| AutomationPoint { time_secs: bars * bar, value: gain_to_pos(10f32.powf(db / 20.0)), curve };
                let points = vec![point(0.0, 0.0, Curve::Linear), point(1.5, 0.0, Curve::Smooth), point(2.0, -9.0, Curve::Linear), point(2.6, -9.0, Curve::Smooth), point(3.4, 2.0, Curve::Linear)];
                e.set_automation_points(Some(1), lane, points, true).unwrap();
                e.set_automation_shown(Some(1), true).unwrap();
            }
            e.set_loop(Some(true), Some(start), Some(start + bar)).unwrap();
            // A tempo change into 3/4 at bar 5.
            let change = opentake_engine::project::TempoChange { at_secs: 16.0 * beat, bpm: 96.0, time_signature: Some((3, 4)) };
            e.set_tempo_changes(vec![change], None).unwrap();
            e.seek(5.2).unwrap();
        }

        let app_engine = engine.clone();
        let mut harness = Harness::builder()
            .with_size([1360.0, 1080.0])
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

        // Third: Audio Settings with MIDI inputs.
        let device = |name: &str| opentake_engine::DeviceInfo { id: name.into(), name: name.into(), is_default: true, default_channels: 2, max_channels: 8, default_sample_rate: 48_000 };
        let port = |name: &str, open| opentake_engine::midi_input::MidiPortInfo { name: name.into(), enabled: true, open };
        harness.state_mut().devices = Some(DeviceList { host: "CoreAudio".into(), inputs: vec![device("Interface In")], outputs: vec![device("Interface Out")] });
        harness.state_mut().midi_ports = Some(Ok(MidiInputs { selection: MidiSelection::All, ports: vec![port("Keystation 49", false), port("IAC Driver Bus 1", false)], errors: Vec::new() }));
        harness.state_mut().show_audio_settings = true;
        for _ in 0..4 {
            harness.step();
        }
        let settings = out.replace(".png", "-settings.png");
        harness.render().expect("render").save(&settings).expect("save png");


        // Third shot: tablature from the Guitar Pro fixture (or OPENTAKE_SCREENSHOT_GP, seeking
        // to OPENTAKE_SCREENSHOT_AT seconds).
        let fixture = std::env::var("OPENTAKE_SCREENSHOT_GP")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../opentake-project/tests/fixtures/simple.gp")));
        let at: f64 = std::env::var("OPENTAKE_SCREENSHOT_AT").ok().and_then(|v| v.parse().ok()).unwrap_or(8.5);
        let adopt = std::env::var("OPENTAKE_SCREENSHOT_ADOPT").is_ok();
        let opts = opentake_engine::GuitarProImport { set_tempo: adopt, ..Default::default() };
        let created = engine.lock().unwrap().import_guitar_pro(&fixture, &opts).unwrap();
        engine.lock().unwrap().seek(at).unwrap();
        let pick: usize = std::env::var("OPENTAKE_SCREENSHOT_TRACK").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
        harness.state_mut().selected_track = created.get(pick).or(created.first()).copied();
        harness.state_mut().bottom = Some(BottomTab::Tab);
        harness.state_mut().view.px_per_sec = 90.0;
        harness.state_mut().view.start = std::env::var("OPENTAKE_SCREENSHOT_VIEW_START").ok().and_then(|v| v.parse().ok()).unwrap_or(0.0);
        for _ in 0..4 {
            harness.step();
        }
        let tab = out.replace(".png", "-tab.png");
        harness.render().expect("render").save(&tab).expect("save png");

        // The same on paper, larger.
        harness.state_mut().tab_paper = true;
        harness.state_mut().tab_scale = 1.5;
        for _ in 0..4 {
            harness.step();
        }
        harness.render().expect("render").save(out.replace(".png", "-paper.png")).expect("save png");

        // Master EQ with a few bands set, its editor open over the mixer.
        {
            use opentake_engine::eq::{self, Field};
            let mut e = engine.lock().unwrap();
            e.insert_master_plugin("eq", None).unwrap();
            let m = SlotRef::Master(0);
            e.set_slot_param(0, m, eq::param_id(0, Field::On), 1.0).unwrap();
            e.set_slot_param(0, m, eq::param_id(1, Field::Gain), eq::norm_gain(3.0)).unwrap();
            e.set_slot_param(0, m, eq::param_id(2, Field::Gain), eq::norm_gain(-4.5)).unwrap();
            e.set_slot_param(0, m, eq::param_id(3, Field::Gain), eq::norm_gain(5.0)).unwrap();
            e.set_slot_param(0, m, eq::param_id(3, Field::Q), eq::norm_q(2.0)).unwrap();
            e.set_slot_param(0, m, eq::param_id(4, Field::Gain), eq::norm_gain(2.0)).unwrap();
        }
        // Run some noise through an EQ with the same settings into the slot's analyzer tap.
        {
            use opentake_engine::mixer::Processor;
            let e = engine.lock().unwrap();
            let tap = e.eq_tap(0, SlotRef::Master(0)).expect("EQ tap");
            let mut eq = opentake_engine::eq::Eq::with_tap(48_000.0, Some(tap));
            for &(id, v) in &e.project().master_plugins[0].params {
                eq.set_param(id, v);
            }
            let mut seed = 0x1234_5678u32;
            let mut brown = 0.0f32;
            for _ in 0..20 {
                let x: Vec<f32> = (0..480)
                    .map(|_| {
                        seed ^= seed << 13;
                        seed ^= seed >> 17;
                        seed ^= seed << 5;
                        let white = seed as f32 / u32::MAX as f32 * 2.0 - 1.0;
                        brown = (brown + white * 0.05).clamp(-1.0, 1.0) * 0.995;
                        brown * 0.5 + white * 0.05
                    })
                    .collect();
                let (mut l, mut r) = (vec![0.0; 480], vec![0.0; 480]);
                eq.process(&[&x, &x], &mut [&mut l, &mut r], 480, &Default::default(), &[]);
            }
        }
        harness.state_mut().bottom = Some(BottomTab::Mixer);
        harness.state_mut().open_plugins = vec![(MASTER_UI, SlotRef::Master(0))];
        let fx_shot = std::env::var("OPENTAKE_SCREENSHOT_FX").is_ok();
        for _ in 0..6 {
            harness.step();
        }
        harness.render().expect("render").save(out.replace(".png", "-eq.png")).expect("save png");

        // The multiband compressor working on a bass-heavy mix.
        {
            use opentake_engine::mixer::Processor;
            let slot = engine.lock().unwrap().insert_master_plugin("multiband", None).unwrap();
            let meter = engine.lock().unwrap().multiband_meter(0, SlotRef::Master(slot)).expect("meter");
            let mut mb = opentake_engine::multiband::Multiband::new(48_000.0, Some(meter));
            harness.state_mut().open_plugins = vec![(MASTER_UI, SlotRef::Master(slot))];
            let tau = std::f32::consts::TAU;
            for f in 0..8 {
                let x: Vec<f32> = (0..4800)
                    .map(|i| {
                        let t = (f * 4800 + i) as f32 / 48_000.0;
                        0.7 * (t * 70.0 * tau).sin() * (-(t % 0.5) * 6.0).exp() + 0.25 * (t * 900.0 * tau).sin() + 0.04 * (t * 7000.0 * tau).sin()
                    })
                    .collect();
                let (mut l, mut r) = (vec![0.0; 4800], vec![0.0; 4800]);
                mb.process(&[&x, &x], &mut [&mut l, &mut r], 4800, &Default::default(), &[]);
                harness.step();
            }
            harness.render().expect("render").save(out.replace(".png", "-multiband.png")).expect("save png");
        }
        if std::env::var("OPENTAKE_SCREENSHOT_EXPORT").is_ok() {
            harness.state_mut().open_plugins.clear();
            harness.state_mut().export_dialog = Some(export_view::ExportDialog { stems: true, ..Default::default() });
            for _ in 0..4 {
                harness.step();
            }
            harness.render().expect("render").save(out.replace(".png", "-export.png")).expect("save png");
            harness.state_mut().export_dialog = None;
        }
        if fx_shot {
            // Compressor, limiter, delay and reverb editors.
            {
                use opentake_engine::dynamics::{self, comp};
                let mut e = engine.lock().unwrap();
                let c = e.insert_plugin(1, "comp", None).unwrap();
                e.set_plugin_param(1, c, comp::THRESHOLD, dynamics::norm_threshold(-24.0)).unwrap();
                e.set_plugin_param(1, c, comp::MAKEUP, 4.0 / 24.0).unwrap();
                e.set_side_chain(1, SlotRef::Effect(c), Some(0), false).unwrap();
                let d = e.insert_plugin(1, "delay", None).unwrap();
                let r = e.insert_plugin(1, "reverb", None).unwrap();
                let l = e.insert_master_plugin("limiter", None).unwrap();
                drop(e);
                for (name, slot) in [("comp", (1, SlotRef::Effect(c))), ("delay", (1, SlotRef::Effect(d))), ("reverb", (1, SlotRef::Effect(r))), ("limiter", (MASTER_UI, SlotRef::Master(l)))] {
                    harness.state_mut().open_plugins = vec![slot];
                    for _ in 0..6 {
                        harness.step();
                    }
                    harness.render().expect("render").save(out.replace(".png", &format!("-{name}.png"))).expect("save png");
                }
            }
        }
        std::fs::remove_dir_all(&dir).ok();
        eprintln!("wrote {out}, {piano} and {tab}");
    }
}
