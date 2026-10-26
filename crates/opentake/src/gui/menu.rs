//! Application menus. On macOS these are real menus in the system menu bar (via `muda`);
//! elsewhere the same commands are shown in an in-window egui menu bar.

use eframe::egui;

/// Every menu action. Both the native and the in-window menus are built from this list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MenuCommand {
    NewProject,
    Open,
    /// Placeholder for the "Open Recent" submenu in the layout.
    RecentMenu,
    OpenRecent(u8),
    ClearRecent,
    Save,
    SaveAs,
    ImportAudio,
    ImportGuitarPro,
    ImportMidi,
    ExportAudio,
    ExportMidi,
    Undo,
    Redo,
    SplitAtPlayhead,
    DeleteRegion,
    CopyRegion,
    PasteRegion,
    DuplicateRegion,
    MergeRegions,
    CrossfadeRegions,
    ToggleSnap,
    NewTrack,
    NewInstrumentTrack,
    NewBus,
    NewBusFromSelection,
    DeleteTrack,
    PlayStop,
    Record,
    ToggleMetronome,
    ToggleCountIn,
    ToggleCountInOnPlay,
    ToggleAutoMonitor,
    ToggleCycle,
    GoToStart,
    CreateMarker,
    DeleteMarker,
    PreviousMarker,
    NextMarker,
    ToggleInspector,
    ToggleMixer,
    TogglePianoRoll,
    ToggleTab,
    ToggleAutomation,
    ToggleMasterAutomation,
    ZoomIn,
    ZoomOut,
    ZoomToFit,
    AudioSettings,
    ToggleAudioEngine,
    RescanPlugins,
    PluginManager,
    McpInfo,
}

// Ids and accelerators are only needed by the native (macOS) menu bar.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
impl MenuCommand {
    pub fn id(self) -> String {
        match self {
            Self::OpenRecent(i) => return format!("open_recent_{i}"),
            _ => {}
        }
        match self {
            Self::NewProject => "new_project",
            Self::Open => "open",
            Self::RecentMenu => "recent_menu",
            Self::OpenRecent(_) => unreachable!(),
            Self::ClearRecent => "clear_recent",
            Self::Save => "save",
            Self::SaveAs => "save_as",
            Self::ImportAudio => "import_audio",
            Self::ImportGuitarPro => "import_guitar_pro",
            Self::ImportMidi => "import_midi",
            Self::ExportMidi => "export_midi",
            Self::ExportAudio => "export_audio",
            Self::Undo => "undo",
            Self::Redo => "redo",
            Self::NewInstrumentTrack => "new_instrument_track",
            Self::NewBus => "new_bus",
            Self::NewBusFromSelection => "new_bus_from_selection",
            Self::ToggleMixer => "toggle_mixer",
            Self::TogglePianoRoll => "toggle_piano_roll",
            Self::ToggleTab => "toggle_tab",
            Self::ToggleAutomation => "toggle_automation",
            Self::ToggleMasterAutomation => "toggle_master_automation",
            Self::ZoomIn => "zoom_in",
            Self::ZoomOut => "zoom_out",
            Self::ZoomToFit => "zoom_to_fit",
            Self::SplitAtPlayhead => "split_at_playhead",
            Self::DeleteRegion => "delete_region",
            Self::CopyRegion => "copy_region",
            Self::PasteRegion => "paste_region",
            Self::DuplicateRegion => "duplicate_region",
            Self::MergeRegions => "merge_regions",
            Self::CrossfadeRegions => "crossfade_regions",
            Self::ToggleSnap => "toggle_snap",
            Self::NewTrack => "new_track",
            Self::DeleteTrack => "delete_track",
            Self::PlayStop => "play_stop",
            Self::Record => "record",
            Self::ToggleMetronome => "toggle_metronome",
            Self::ToggleCountIn => "toggle_count_in",
            Self::ToggleCountInOnPlay => "toggle_count_in_on_play",
            Self::ToggleAutoMonitor => "toggle_auto_input_monitoring",
            Self::ToggleCycle => "toggle_cycle",
            Self::GoToStart => "go_to_start",
            Self::CreateMarker => "create_marker",
            Self::DeleteMarker => "delete_marker",
            Self::PreviousMarker => "previous_marker",
            Self::NextMarker => "next_marker",
            Self::ToggleInspector => "toggle_inspector",
            Self::AudioSettings => "audio_settings",
            Self::ToggleAudioEngine => "toggle_audio_engine",
            Self::RescanPlugins => "rescan_plugins",
            Self::PluginManager => "plugin_manager",
            Self::McpInfo => "mcp_info",
        }
        .to_string()
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::NewProject => "New Project",
            Self::Open => "Open…",
            Self::RecentMenu => "Open Recent",
            Self::OpenRecent(_) => "Recent Project",
            Self::ClearRecent => "Clear Menu",
            Self::Save => "Save",
            Self::SaveAs => "Save As…",
            Self::ImportAudio => "Import Audio File…",
            Self::ImportGuitarPro => "Import Guitar Pro…",
            Self::ImportMidi => "Import MIDI File…",
            Self::ExportMidi => "Export MIDI File…",
            Self::ExportAudio => "Export Audio…",
            Self::Undo => "Undo",
            Self::Redo => "Redo",
            Self::NewInstrumentTrack => "New Instrument Track…",
            Self::NewBus => "New Bus",
            Self::NewBusFromSelection => "Create Bus from Selection",
            Self::ToggleMixer => "Mixer",
            Self::TogglePianoRoll => "Piano Roll",
            Self::ToggleTab => "Tablature",
            Self::ToggleAutomation => "Show Automation",
            Self::ToggleMasterAutomation => "Show Master Automation",
            Self::ZoomIn => "Zoom In",
            Self::ZoomOut => "Zoom Out",
            Self::ZoomToFit => "Zoom to Fit",
            Self::SplitAtPlayhead => "Split Regions at Playhead",
            Self::DeleteRegion => "Delete Region",
            Self::CopyRegion => "Copy Region",
            Self::PasteRegion => "Paste Region at Playhead",
            Self::DuplicateRegion => "Duplicate Region",
            Self::MergeRegions => "Merge Regions",
            Self::CrossfadeRegions => "Crossfade Regions",
            Self::ToggleSnap => "Snap to Grid",
            Self::NewTrack => "New Audio Track",
            Self::DeleteTrack => "Delete Track",
            Self::PlayStop => "Play / Stop",
            Self::Record => "Record",
            Self::ToggleMetronome => "Metronome",
            Self::ToggleCountIn => "Count-in When Recording",
            Self::ToggleCountInOnPlay => "Count-in When Playing",
            Self::ToggleAutoMonitor => "Auto Input Monitoring",
            Self::ToggleCycle => "Cycle",
            Self::GoToStart => "Go to Beginning",
            Self::CreateMarker => "Create Marker",
            Self::DeleteMarker => "Delete Marker",
            Self::PreviousMarker => "Go to Previous Marker",
            Self::NextMarker => "Go to Next Marker",
            Self::ToggleInspector => "Show Inspector",
            Self::AudioSettings => "Audio Settings…",
            Self::ToggleAudioEngine => "Stop Audio Engine",
            Self::RescanPlugins => "Rescan Plug-ins",
            Self::PluginManager => "Plug-in Manager…",
            Self::McpInfo => "Connect an Agent (MCP)…",
        }
    }

    /// Native accelerator (muda syntax). Keys without modifiers (Space, R, Home, I) are
    /// handled by the app itself so they don't steal keystrokes from text fields.
    pub fn accelerator(self) -> Option<&'static str> {
        match self {
            Self::NewProject => Some("CmdOrCtrl+N"),
            Self::Open => Some("CmdOrCtrl+O"),
            Self::Save => Some("CmdOrCtrl+S"),
            Self::SaveAs => Some("CmdOrCtrl+Shift+S"),
            Self::ImportAudio => Some("CmdOrCtrl+Shift+I"),
            Self::ExportAudio => Some("CmdOrCtrl+Shift+E"),
            // Text fields still get ⌘Z: the app forwards it to them when one has focus.
            Self::Undo => Some("CmdOrCtrl+Z"),
            Self::Redo => Some("CmdOrCtrl+Shift+Z"),
            Self::SplitAtPlayhead => Some("CmdOrCtrl+T"),
            Self::DuplicateRegion => Some("CmdOrCtrl+D"),
            Self::MergeRegions => Some("CmdOrCtrl+J"),
            Self::ToggleSnap => Some("CmdOrCtrl+G"),
            Self::ZoomIn => Some("CmdOrCtrl+ArrowRight"),
            Self::ZoomOut => Some("CmdOrCtrl+ArrowLeft"),
            Self::NewTrack => Some("CmdOrCtrl+Alt+N"),
            Self::AudioSettings => Some("CmdOrCtrl+Comma"),
            _ => None,
        }
    }

    /// Shortcut text for the in-window menu.
    pub fn shortcut_text(self) -> Option<&'static str> {
        let mac = cfg!(target_os = "macos");
        Some(match self {
            Self::NewProject => if mac { "⌘N" } else { "Ctrl+N" },
            Self::Open => if mac { "⌘O" } else { "Ctrl+O" },
            Self::Save => if mac { "⌘S" } else { "Ctrl+S" },
            Self::SaveAs => if mac { "⇧⌘S" } else { "Ctrl+Shift+S" },
            Self::ImportAudio => if mac { "⇧⌘I" } else { "Ctrl+Shift+I" },
            Self::ExportAudio => if mac { "⇧⌘E" } else { "Ctrl+Shift+E" },
            Self::Undo => if mac { "⌘Z" } else { "Ctrl+Z" },
            Self::Redo => if mac { "⇧⌘Z" } else { "Ctrl+Shift+Z" },
            Self::ToggleMixer => "X",
            Self::TogglePianoRoll => "P",
            Self::ToggleTab => "T",
            Self::ToggleAutomation => "A",
            Self::ZoomIn => if mac { "⌘→" } else { "Ctrl+→" },
            Self::ZoomOut => if mac { "⌘←" } else { "Ctrl+←" },
            Self::ZoomToFit => "Z",
            Self::DeleteTrack => if mac { "⌘⌫" } else { "Ctrl+Del" },
            Self::SplitAtPlayhead => if mac { "⌘T" } else { "Ctrl+T" },
            Self::ToggleSnap => if mac { "⌘G" } else { "Ctrl+G" },
            Self::DeleteRegion => if mac { "⌫" } else { "Del" },
            Self::CopyRegion => if mac { "⌘C" } else { "Ctrl+C" },
            Self::PasteRegion => if mac { "⌘V" } else { "Ctrl+V" },
            Self::DuplicateRegion => if mac { "⌘D" } else { "Ctrl+D" },
            Self::MergeRegions => if mac { "⌘J" } else { "Ctrl+J" },
            Self::NewTrack => if mac { "⌥⌘N" } else { "Ctrl+Alt+N" },
            Self::AudioSettings => if mac { "⌘," } else { "Ctrl+," },
            Self::PlayStop => "Space",
            Self::Record => "R",
            Self::ToggleMetronome => "K",
            Self::ToggleCycle => "C",
            Self::GoToStart => "Home",
            Self::CreateMarker => "M",
            Self::PreviousMarker => if mac { "⌥←" } else { "Alt+←" },
            Self::NextMarker => if mac { "⌥→" } else { "Alt+→" },
            Self::ToggleInspector => "I",
            _ => return None,
        })
    }

    fn from_id(id: &str) -> Option<Self> {
        if let Some(i) = id.strip_prefix("open_recent_").and_then(|n| n.parse().ok()) {
            return Some(Self::OpenRecent(i));
        }
        let extra = [Self::ClearRecent, Self::McpInfo];
        MENUS.iter().flat_map(|(_, items)| items.iter()).flatten().copied().chain(extra).find(|c| c.id() == id)
    }
}

/// Menu layout: (title, items); `None` is a separator.
pub const MENUS: &[(&str, &[Option<MenuCommand>])] = {
    use MenuCommand::*;
    &[
        ("File", &[Some(NewProject), Some(Open), Some(RecentMenu), None, Some(Save), Some(SaveAs), None, Some(ImportAudio), Some(ImportGuitarPro), Some(ImportMidi), None, Some(ExportAudio), Some(ExportMidi)]),
        (
            "Edit",
            &[
                Some(Undo),
                Some(Redo),
                None,
                Some(CopyRegion),
                Some(PasteRegion),
                Some(DuplicateRegion),
                None,
                Some(SplitAtPlayhead),
                Some(MergeRegions),
                Some(CrossfadeRegions),
                Some(DeleteRegion),
                None,
                Some(ToggleSnap),
            ],
        ),
        ("Track", &[Some(NewTrack), Some(NewInstrumentTrack), Some(NewBus), Some(NewBusFromSelection), None, Some(DeleteTrack)]),
        (
            "Transport",
            &[
                Some(PlayStop),
                Some(Record),
                Some(GoToStart),
                None,
                Some(ToggleCycle),
                Some(ToggleMetronome),
                Some(ToggleCountIn),
                Some(ToggleCountInOnPlay),
                Some(ToggleAutoMonitor),
                None,
                Some(CreateMarker),
                Some(DeleteMarker),
                Some(PreviousMarker),
                Some(NextMarker),
            ],
        ),
        ("View", &[Some(ToggleInspector), Some(ToggleMixer), Some(TogglePianoRoll), Some(ToggleTab), Some(ToggleAutomation), Some(ToggleMasterAutomation), None, Some(ZoomIn), Some(ZoomOut), Some(ZoomToFit)]),
        ("Audio", &[Some(AudioSettings), None, Some(ToggleAudioEngine)]),
        ("Plug-ins", &[Some(PluginManager), None, Some(RescanPlugins)]),
    ]
};

/// State the menus reflect (enabled items, toggles, dynamic labels).
#[derive(Clone, PartialEq)]
pub struct MenuState {
    pub has_selected_track: bool,
    pub has_selected_region: bool,
    /// The selection is two or more audio regions on one track (two for a crossfade).
    pub can_merge: bool,
    pub can_crossfade: bool,
    pub cycle_on: bool,
    pub has_clipboard: bool,
    pub inspector_visible: bool,
    pub mixer_visible: bool,
    pub tab_visible: bool,
    pub piano_roll_visible: bool,
    /// Some track shows its automation lanes.
    pub automation_visible: bool,
    pub master_automation_visible: bool,
    /// Labels of the steps undo/redo would apply.
    pub undo: Option<String>,
    pub redo: Option<String>,
    /// Recent projects as menu labels, newest first.
    pub recent: Vec<String>,
    pub snap_on: bool,
    pub metronome_on: bool,
    pub count_in_on: bool,
    pub count_in_on_play: bool,
    pub auto_monitor: bool,
    pub audio_running: bool,
    pub playing: bool,
    /// The project has markers, and one is selected or at the playhead (to delete).
    pub has_markers: bool,
    pub marker_to_delete: bool,
}

impl MenuState {
    fn enabled(&self, cmd: MenuCommand) -> bool {
        match cmd {
            MenuCommand::DeleteTrack => self.has_selected_track,
            MenuCommand::NewBusFromSelection => self.has_selected_track || self.has_selected_region,
            MenuCommand::DeleteRegion | MenuCommand::CopyRegion | MenuCommand::DuplicateRegion => self.has_selected_region,
            MenuCommand::PasteRegion => self.has_clipboard,
            MenuCommand::MergeRegions => self.can_merge,
            MenuCommand::CrossfadeRegions => self.can_crossfade,
            MenuCommand::Record => self.audio_running,
            MenuCommand::Undo => self.undo.is_some(),
            MenuCommand::Redo => self.redo.is_some(),
            MenuCommand::PreviousMarker | MenuCommand::NextMarker => self.has_markers,
            MenuCommand::DeleteMarker => self.marker_to_delete,
            _ => true,
        }
    }

    fn checked(&self, cmd: MenuCommand) -> Option<bool> {
        match cmd {
            MenuCommand::ToggleInspector => Some(self.inspector_visible),
            MenuCommand::ToggleSnap => Some(self.snap_on),
            MenuCommand::ToggleMetronome => Some(self.metronome_on),
            MenuCommand::ToggleCountIn => Some(self.count_in_on),
            MenuCommand::ToggleCountInOnPlay => Some(self.count_in_on_play),
            MenuCommand::ToggleAutoMonitor => Some(self.auto_monitor),
            MenuCommand::ToggleCycle => Some(self.cycle_on),
            MenuCommand::ToggleMixer => Some(self.mixer_visible),
            MenuCommand::TogglePianoRoll => Some(self.piano_roll_visible),
            MenuCommand::ToggleTab => Some(self.tab_visible),
            MenuCommand::ToggleAutomation => Some(self.automation_visible),
            MenuCommand::ToggleMasterAutomation => Some(self.master_automation_visible),
            _ => None,
        }
    }

    fn label(&self, cmd: MenuCommand) -> String {
        match cmd {
            MenuCommand::ToggleAudioEngine if !self.audio_running => "Start Audio Engine".into(),
            MenuCommand::PlayStop if self.playing => "Stop".into(),
            MenuCommand::PlayStop => "Play".into(),
            MenuCommand::Undo => self.undo.as_ref().map_or("Undo".into(), |l| format!("Undo {l}")),
            MenuCommand::Redo => self.redo.as_ref().map_or("Redo".into(), |l| format!("Redo {l}")),
            other => other.label().into(),
        }
    }
}

/// In-window menu bar (used where there is no native menu bar).
pub fn egui_menu_bar(ui: &mut egui::Ui, state: &MenuState) -> Option<MenuCommand> {
    let mut chosen = None;
    egui::MenuBar::new().ui(ui, |ui| {
        let titles = MENUS.iter().copied().chain(std::iter::once(("Help", &[Some(MenuCommand::McpInfo)][..])));
        for (title, items) in titles {
            ui.menu_button(title, |ui| {
                ui.set_min_width(220.0);
                for item in items {
                    let Some(cmd) = *item else {
                        ui.separator();
                        continue;
                    };
                    if cmd == MenuCommand::RecentMenu {
                        ui.menu_button(cmd.label(), |ui| {
                            ui.set_min_width(260.0);
                            if state.recent.is_empty() {
                                ui.add_enabled(false, egui::Button::new("No Recent Projects"));
                            }
                            for (i, name) in state.recent.iter().enumerate() {
                                if ui.button(name).clicked() {
                                    chosen = Some(MenuCommand::OpenRecent(i as u8));
                                    ui.close();
                                }
                            }
                            ui.separator();
                            if ui.add_enabled(!state.recent.is_empty(), egui::Button::new("Clear Menu")).clicked() {
                                chosen = Some(MenuCommand::ClearRecent);
                                ui.close();
                            }
                        });
                        continue;
                    }
                    let mut button = egui::Button::new(state.label(cmd));
                    if let Some(s) = cmd.shortcut_text() {
                        button = button.shortcut_text(s);
                    }
                    if let Some(on) = state.checked(cmd) {
                        button = button.selected(on);
                    }
                    if ui.add_enabled(state.enabled(cmd), button).clicked() {
                        chosen = Some(cmd);
                        ui.close();
                    }
                }
            });
        }
    });
    chosen
}

#[cfg(target_os = "macos")]
pub use native::NativeMenu;

#[cfg(target_os = "macos")]
mod native {
    use std::collections::HashMap;

    use muda::accelerator::Accelerator;
    use muda::{AboutMetadata, CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};

    use super::{MENUS, MenuCommand, MenuState};

    enum Item {
        Normal(MenuItem),
        Check(CheckMenuItem),
    }

    /// The macOS menu bar. Must be created on the main thread after the app has launched.
    pub struct NativeMenu {
        _menu: Menu,
        items: HashMap<MenuCommand, Item>,
        recent: Option<Submenu>,
        recent_shown: Option<Vec<String>>,
        last_state: Option<MenuState>,
    }

    impl NativeMenu {
        pub fn install() -> muda::Result<Self> {
            let menu = Menu::new();
            let about = AboutMetadata {
                name: Some("OpenTake".into()),
                version: Some(env!("CARGO_PKG_VERSION").into()),
                comments: Some("A small DAW you can drive from an AI agent over MCP.".into()),
                ..Default::default()
            };
            let app = Submenu::with_items(
                "OpenTake",
                true,
                &[
                    &PredefinedMenuItem::about(None, Some(about)),
                    &PredefinedMenuItem::separator(),
                    &PredefinedMenuItem::services(None),
                    &PredefinedMenuItem::separator(),
                    &PredefinedMenuItem::hide(None),
                    &PredefinedMenuItem::hide_others(None),
                    &PredefinedMenuItem::show_all(None),
                    &PredefinedMenuItem::separator(),
                    &PredefinedMenuItem::quit(None),
                ],
            )?;
            menu.append(&app)?;

            let mut items = HashMap::new();
            let mut recent = None;
            for (title, entries) in MENUS {
                let sub = Submenu::new(*title, true);
                for entry in *entries {
                    match entry {
                        None => sub.append(&PredefinedMenuItem::separator())?,
                        Some(MenuCommand::RecentMenu) => {
                            let sub_recent = Submenu::new(MenuCommand::RecentMenu.label(), true);
                            sub.append(&sub_recent)?;
                            recent = Some(sub_recent);
                        }
                        Some(cmd) => {
                            let accel: Option<Accelerator> = cmd.accelerator().and_then(|a| a.parse().ok());
                            if matches!(
                                cmd,
                                MenuCommand::ToggleInspector
                                    | MenuCommand::ToggleSnap
                                    | MenuCommand::ToggleMetronome
                                    | MenuCommand::ToggleCountIn
                                    | MenuCommand::ToggleCountInOnPlay
                                    | MenuCommand::ToggleAutoMonitor
                                    | MenuCommand::ToggleCycle
                                    | MenuCommand::ToggleMixer
                                    | MenuCommand::TogglePianoRoll
                                    | MenuCommand::ToggleTab
                                    | MenuCommand::ToggleAutomation
                                    | MenuCommand::ToggleMasterAutomation
                            ) {
                                let item = CheckMenuItem::with_id(cmd.id(), cmd.label(), true, true, accel);
                                sub.append(&item)?;
                                items.insert(*cmd, Item::Check(item));
                            } else {
                                let item = MenuItem::with_id(cmd.id(), cmd.label(), true, accel);
                                sub.append(&item)?;
                                items.insert(*cmd, Item::Normal(item));
                            }
                        }
                    }
                }
                menu.append(&sub)?;
            }

            let window = Submenu::with_items(
                "Window",
                true,
                &[
                    &PredefinedMenuItem::minimize(None),
                    &PredefinedMenuItem::maximize(None),
                    &PredefinedMenuItem::separator(),
                    &PredefinedMenuItem::fullscreen(None),
                    &PredefinedMenuItem::separator(),
                    &PredefinedMenuItem::bring_all_to_front(None),
                ],
            )?;
            menu.append(&window)?;
            window.set_as_windows_menu_for_nsapp();

            let help = Submenu::new("Help", true);
            let mcp = MenuItem::with_id(MenuCommand::McpInfo.id(), MenuCommand::McpInfo.label(), true, None);
            help.append(&mcp)?;
            items.insert(MenuCommand::McpInfo, Item::Normal(mcp));
            menu.append(&help)?;
            help.set_as_help_menu_for_nsapp();

            menu.init_for_nsapp();
            Ok(Self { _menu: menu, items, recent, recent_shown: None, last_state: None })
        }

        fn rebuild_recent(&mut self, names: &[String]) {
            let Some(sub) = &self.recent else { return };
            while sub.remove_at(0).is_some() {}
            if names.is_empty() {
                let _ = sub.append(&MenuItem::new("No Recent Projects", false, None));
            }
            for (i, name) in names.iter().enumerate() {
                let _ = sub.append(&MenuItem::with_id(MenuCommand::OpenRecent(i as u8).id(), name, true, None));
            }
            let _ = sub.append(&PredefinedMenuItem::separator());
            let _ = sub.append(&MenuItem::with_id(MenuCommand::ClearRecent.id(), MenuCommand::ClearRecent.label(), !names.is_empty(), None));
            self.recent_shown = Some(names.to_vec());
        }

        /// Commands chosen since the last call.
        pub fn poll(&self) -> Vec<MenuCommand> {
            MenuEvent::receiver().try_iter().filter_map(|e| MenuCommand::from_id(&e.id().0)).collect()
        }

        /// Sync enabled state, check marks and dynamic labels (only when something changed).
        pub fn update(&mut self, state: MenuState) {
            if self.last_state.as_ref() == Some(&state) {
                return;
            }
            if self.recent_shown.as_ref() != Some(&state.recent) {
                self.rebuild_recent(&state.recent);
            }
            for (cmd, item) in &self.items {
                match item {
                    Item::Normal(i) => {
                        i.set_enabled(state.enabled(*cmd));
                        i.set_text(state.label(*cmd));
                    }
                    Item::Check(i) => i.set_checked(state.checked(*cmd).unwrap_or(false)),
                }
            }
            self.last_state = Some(state);
        }
    }
}
