//! Plugin instances: scanning, loading, effect chains and instruments.
//!
//! Loaded instances live in `Engine::pool` keyed by a session-unique instance id that is
//! also stored in the project (`PluginSlot::instance`). Removing a plugin keeps the
//! instance in the pool, so undo can bring back the very same instance with its state.
//!
//! A hosted plug-in's state beyond its parameters (a VST3's component and controller
//! chunks, an AU's ClassInfo) lives in the instance while the project is open: it's written
//! into the slots when saving ([`Engine::project_with_plugin_state`]) and given to each new
//! instance made from a slot (loading, offline rendering). A slot's saved parameter values
//! are applied on top of a restored state: they're the values set here (automation's base
//! line), which a state saved during automation playback needn't hold.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use opentake_project::{Controller, PluginFormat, PluginSlot, Project, VstPreset};
use opentake_vst3::{ClassInfo, Module, NoteEvent, PluginState, ScannedPlugin, Vst3Plugin, Vst3Processor};

use super::{Engine, ParamView, display_name};
use crate::mixer::{Command, KeyRt, MASTER, MAX_BLOCK, MidiData, MidiEvent, PLUGIN_CAPACITY, PluginRt, Processor, Transport};
use crate::drums::{self, DrumSynth};
use crate::delay::{self, Delay};
use crate::dynamics::{self, Compressor, GainMeter, Limiter, comp, limiter};
use crate::eq::{self, Eq};
use crate::multiband::{self, Multiband, MultibandMeter};
use crate::reverb::{self, Reverb};
use crate::gate::{self, Gate};
use crate::deesser::{self, DeEsser};
use crate::chorus::{self, Chorus};
use crate::phaser::{self, Phaser};
use crate::synth::{self, Synth};

pub(super) enum Loaded {
    Vst3(Vst3Plugin),
    #[cfg(target_os = "macos")]
    Au(opentake_au::AuPlugin),
    Synth,
    Drums,
    /// The tap feeds the editor's analyzer (shared by every processor of this instance).
    Eq(std::sync::Arc<eq::Tap>),
    Compressor(std::sync::Arc<GainMeter>),
    Limiter(std::sync::Arc<GainMeter>),
    Multiband(std::sync::Arc<MultibandMeter>),
    Delay,
    Reverb(std::sync::Arc<GainMeter>),
    Gate(std::sync::Arc<GainMeter>),
    DeEsser(std::sync::Arc<GainMeter>),
    Chorus,
    Phaser,
}

impl Loaded {
    /// Tell a hosted plug-in itself about a parameter value (the processor is told
    /// separately); built-ins keep theirs in the project only.
    pub(super) fn set_param(&self, id: u32, value: f64) {
        match self {
            Loaded::Vst3(plugin) => plugin.set_param_on_controller(id, value),
            #[cfg(target_os = "macos")]
            Loaded::Au(plugin) => plugin.set_param(id, value),
            _ => {}
        }
    }
}

/// The built-in effect a query names: (name, path).
fn builtin_effect_for(query: &str) -> Option<(&'static str, &'static str)> {
    let q = query.trim().to_lowercase();
    let q = q.strip_prefix("builtin:").unwrap_or(&q);
    match q {
        "eq" | "equalizer" | "equaliser" | "channel eq" | "parametric eq" => Some((eq::NAME, eq::PATH)),
        "comp" | "compressor" => Some((comp::NAME, comp::PATH)),
        "limiter" | "brickwall" | "brickwall limiter" => Some((limiter::NAME, limiter::PATH)),
        "delay" | "echo" => Some((delay::NAME, delay::PATH)),
        "multiband" | "multiband compressor" | "multiband comp" | "mbc" => Some((multiband::NAME, multiband::PATH)),
        "reverb" | "verb" | "reverb send" | "hall" | "room" => Some((reverb::NAME, reverb::PATH)),
        "gate" | "noise gate" | "expander" => Some((gate::NAME, gate::PATH)),
        "deesser" | "de-esser" | "de esser" | "sibilance" => Some((deesser::NAME, deesser::PATH)),
        "chorus" | "ensemble" => Some((chorus::NAME, chorus::PATH)),
        "phaser" | "phase shifter" => Some((phaser::NAME, phaser::PATH)),
        _ => [eq::NAME, comp::NAME, limiter::NAME, delay::NAME, multiband::NAME, reverb::NAME, gate::NAME, deesser::NAME, chorus::NAME, phaser::NAME]
            .into_iter()
            .zip([eq::PATH, comp::PATH, limiter::PATH, delay::PATH, multiband::PATH, reverb::PATH, gate::PATH, deesser::PATH, chorus::PATH, phaser::PATH])
            .find(|(n, _)| n.to_lowercase() == q),
    }
}

/// Whether `query` names one of OpenTake's built-in effects.
pub(super) fn is_builtin_effect(query: &str) -> bool {
    builtin_effect_for(query).is_some()
}

/// A fresh pooled instance for a built-in effect path.
fn builtin_effect_instance(path: &str) -> Option<Loaded> {
    Some(match path {
        eq::PATH => Loaded::Eq(eq::Tap::new()),
        comp::PATH => Loaded::Compressor(GainMeter::new()),
        limiter::PATH => Loaded::Limiter(GainMeter::new()),
        multiband::PATH => Loaded::Multiband(MultibandMeter::new()),
        delay::PATH => Loaded::Delay,
        reverb::PATH => Loaded::Reverb(GainMeter::new()),
        gate::PATH => Loaded::Gate(GainMeter::new()),
        deesser::PATH => Loaded::DeEsser(GainMeter::new()),
        chorus::PATH => Loaded::Chorus,
        phaser::PATH => Loaded::Phaser,
        _ => return None,
    })
}

/// Parameter list and display function of a built-in effect.
type ParamList = Vec<(u32, String, &'static str, f64, i32)>;
fn builtin_effect_params(loaded: &Loaded) -> Option<(ParamList, fn(u32, f64) -> String)> {
    Some(match loaded {
        Loaded::Eq(_) => (eq::params(), eq::display),
        Loaded::Compressor(_) => (dynamics::comp_params(), dynamics::comp_display),
        Loaded::Limiter(_) => (dynamics::limiter_params(), dynamics::limiter_display),
        Loaded::Multiband(_) => (multiband::params(), multiband::display),
        Loaded::Delay => (delay::params(), delay::display),
        Loaded::Reverb(_) => (reverb::params(), reverb::display),
        Loaded::Gate(_) => (gate::params(), gate::display),
        Loaded::DeEsser(_) => (deesser::params(), deesser::display),
        Loaded::Chorus => (chorus::params(), chorus::display),
        Loaded::Phaser => (phaser::params(), phaser::display),
        _ => return None,
    })
}

/// Which plugin: a track's instrument or effect slot, or a slot on the master bus (the
/// track argument is ignored for those).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SlotRef {
    Instrument,
    Effect(usize),
    Master(usize),
}



struct Vst3Insert {
    processor: Vst3Processor,
    notes: Vec<NoteEvent>,
    /// Reported when the processor was set up (not queried on the audio thread).
    latency: usize,
}

impl Processor for Vst3Insert {
    fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], frames: usize, t: &Transport, events: &[MidiEvent]) {
        self.process_keyed(inputs, None, outputs, frames, t, events);
    }

    fn process_keyed(&mut self, inputs: &[&[f32]], key: Option<&[&[f32]]>, outputs: &mut [&mut [f32]], frames: usize, t: &Transport, events: &[MidiEvent]) {
        let t = opentake_vst3::Transport {
            playing: t.playing,
            recording: t.recording,
            tempo_bpm: t.tempo_bpm,
            time_sig: t.time_sig,
            position_samples: t.position_samples,
        };
        // Controllers become changes of the parameters the plug-in maps them to.
        self.notes.clear();
        for e in events {
            match e.data {
                MidiData::Note { pitch, velocity } if self.notes.len() < self.notes.capacity() => self.notes.push(NoteEvent { offset: e.offset, pitch, velocity: velocity as f32 / 127.0 }),
                MidiData::Note { .. } => {}
                MidiData::Control { kind, value } => {
                    let (number, normalized) = vst3_control(kind, value);
                    self.processor.queue_midi_control(e.offset.min(frames.saturating_sub(1) as u32), number, normalized);
                }
            }
        }
        self.processor.process_keyed(inputs, key, outputs, frames, &t, &self.notes);
    }

    fn set_param(&mut self, id: u32, normalized: f64) {
        self.processor.queue_param(id, normalized);
    }

    fn latency(&self) -> usize {
        self.latency
    }
}

/// VST3's number for a controller (CCs 0..=127, then channel pressure and pitch bend) and
/// its raw value as 0..1.
fn vst3_control(kind: Controller, value: u16) -> (u8, f64) {
    match kind {
        Controller::PitchBend => (opentake_vst3::host::CTRL_PITCH_BEND, value.min(16383) as f64 / 16383.0),
        Controller::ChannelPressure => (opentake_vst3::host::CTRL_AFTERTOUCH, value.min(127) as f64 / 127.0),
        Controller::Cc(n) => (n & 0x7F, value.min(127) as f64 / 127.0),
    }
}

fn is_builtin_synth(query: &str) -> bool {
    let q = query.trim().to_lowercase();
    matches!(q.as_str(), "synth" | "builtin" | "builtin:synth" | "builtin synth")
        || q == synth::NAME.to_lowercase()
        || q == synth::OLD_NAME.to_lowercase()
}

fn is_builtin_drums(query: &str) -> bool {
    let q = query.trim().to_lowercase();
    matches!(q.as_str(), "drums" | "drum" | "drum kit" | "drum synth" | "builtin:drums") || q == drums::NAME.to_lowercase()
}

fn builtin_slot(name: &str, path: &str) -> PluginSlot {
    PluginSlot {
        name: name.into(),
        format: PluginFormat::Builtin,
        path: path.into(),
        uid: path.into(),
        bypass: false,
        state: Vec::new(),
        params: Vec::new(),
        id: 0,
        side_chain: None,
        instance: 0,
    }
}

/// A VST3 instance's current state as a `.vstpreset`.
fn vst3_preset(plugin: &Vst3Plugin) -> Result<VstPreset> {
    let state = plugin.save_state()?;
    Ok(VstPreset::new(&plugin.class.cid, state.component, state.controller))
}

/// Apply stored parameter values to a fresh processor.
fn with_params(mut p: Box<dyn Processor>, params: &[(u32, f64)]) -> Box<dyn Processor> {
    for &(id, value) in params {
        p.set_param(id, value);
    }
    p
}

impl Engine {
    // ---------------------------------------------------------------- scanning

    /// Plugins found by the last scan.
    pub fn scanned_plugins(&self) -> &[ScannedPlugin] {
        &self.scanned
    }

    pub fn plugin_search_paths(&self) -> &[PathBuf] {
        &self.plugin_paths
    }

    pub fn add_plugin_search_path(&mut self, path: PathBuf) {
        if !self.plugin_paths.contains(&path) {
            self.plugin_paths.push(path);
        }
    }

    pub fn remove_plugin_search_path(&mut self, path: &Path) {
        self.plugin_paths.retain(|p| p != path);
    }

    pub fn set_plugin_search_paths(&mut self, paths: Vec<PathBuf>) {
        self.plugin_paths.clear();
        for p in paths {
            self.add_plugin_search_path(p);
        }
    }

    /// Per-bundle errors from the last scan.
    pub fn scan_errors(&self) -> &[String] {
        &self.scan_errors
    }

    /// Scan plugin folders. Returns the plugins found plus per-bundle errors.
    pub fn scan_plugins(&mut self) -> (Vec<ScannedPlugin>, Vec<String>) {
        let mut errors = Vec::new();
        self.scanned = opentake_vst3::scan_plugins(&self.plugin_paths)
            .into_iter()
            .flat_map(|(bundle, result)| match result {
                Ok(classes) => classes.into_iter().map(|class| ScannedPlugin { bundle: bundle.clone(), class }).collect(),
                Err(e) => {
                    errors.push(format!("{}: {e}", bundle.display()));
                    Vec::new()
                }
            })
            .collect();
        self.scan_errors = errors.clone();
        self.audio_units = super::au::scan();
        (self.scanned.clone(), errors)
    }

    fn module(&mut self, bundle: &Path) -> Result<std::sync::Arc<Module>> {
        if let Some(m) = self.modules.get(bundle) {
            return Ok(m.clone());
        }
        let m = Module::load(bundle).with_context(|| format!("loading {}", bundle.display()))?;
        self.modules.insert(bundle.to_path_buf(), m.clone());
        Ok(m)
    }

    /// Find a plugin by bundle path or (partial, case-insensitive) name, preferring
    /// instruments or effects as requested.
    fn resolve_plugin(&mut self, query: &str, instrument: bool) -> Result<(PathBuf, Option<String>)> {
        let path = Path::new(query);
        if path.exists() {
            return Ok((path.to_path_buf(), None));
        }
        if self.scanned.is_empty() {
            self.scan_plugins();
        }
        let q = query.to_lowercase();
        let kind_first = |list: Vec<&ScannedPlugin>| -> Option<(PathBuf, Option<String>)> {
            let pick = list.iter().find(|p| p.class.is_instrument() == instrument).or(list.first());
            pick.map(|p| (p.bundle.clone(), Some(p.class.cid.clone())))
        };
        let exact: Vec<_> = self.scanned.iter().filter(|p| p.class.name.to_lowercase() == q).collect();
        let partial: Vec<_> = self.scanned.iter().filter(|p| p.class.name.to_lowercase().contains(&q)).collect();
        kind_first(exact)
            .or_else(|| kind_first(partial))
            .ok_or_else(|| anyhow!("no plugin matching '{query}' (try scan_plugins)"))
    }

    fn instantiate(&mut self, bundle: &Path, cid: Option<&str>, name: Option<&str>) -> Result<Vst3Plugin> {
        let module = self.module(bundle)?;
        let classes = module.audio_classes();
        let pick = |c: &&ClassInfo| {
            cid.is_some_and(|id| c.cid.eq_ignore_ascii_case(id)) || name.is_some_and(|n| c.name.eq_ignore_ascii_case(n))
        };
        let class = classes
            .iter()
            .find(pick)
            .or(classes.first())
            .cloned()
            .ok_or_else(|| anyhow!("{} has no audio classes", bundle.display()))?;
        Ok(Vst3Plugin::new(module, class)?)
    }

    pub(super) fn add_to_pool(&mut self, loaded: Loaded) -> u64 {
        let id = self.next_id();
        self.pool.insert(id, loaded);
        id
    }

    /// Create a slot (and pooled instance) for a plugin query.
    pub(super) fn new_slot(&mut self, query: &str, instrument: bool) -> Result<PluginSlot> {
        if is_builtin_synth(query) {
            let mut slot = builtin_slot(synth::NAME, synth::PATH);
            slot.instance = self.add_to_pool(Loaded::Synth);
            return Ok(slot);
        }
        if is_builtin_drums(query) {
            let mut slot = builtin_slot(drums::NAME, drums::PATH);
            slot.instance = self.add_to_pool(Loaded::Drums);
            return Ok(slot);
        }
        if let Some((name, path)) = builtin_effect_for(query) {
            if instrument {
                bail!("{name} is an effect, not an instrument");
            }
            let mut slot = builtin_slot(name, path);
            slot.instance = self.add_to_pool(builtin_effect_instance(path).expect("known built-in"));
            return Ok(slot);
        }
        if let Some(au) = self.find_audio_unit(query, instrument, false) {
            return self.new_au_slot(&au);
        }
        let (bundle, cid) = match self.resolve_plugin(query, instrument) {
            Ok(found) => found,
            Err(e) => return self.find_audio_unit(query, instrument, true).map_or(Err(e), |au| self.new_au_slot(&au)),
        };
        let plugin = self.instantiate(&bundle, cid.as_deref(), None)?;
        let kind = if plugin.is_instrument() { "VST3i" } else { "VST3" };
        let mut slot = PluginSlot {
            name: format!("{kind}: {} ({})", plugin.class.name, plugin.class.vendor),
            format: PluginFormat::Vst3,
            path: bundle.to_string_lossy().into_owned(),
            uid: plugin.class.cid.clone(),
            bypass: false,
            state: Vec::new(),
            params: Vec::new(),
            id: 0,
            side_chain: None,
            instance: 0,
        };
        slot.instance = self.add_to_pool(Loaded::Vst3(plugin));
        Ok(slot)
    }

    /// A loaded hosted plug-in's current state as its slot stores it (`None` for built-ins
    /// and slots that aren't loaded).
    pub(super) fn capture_state(&self, slot: &PluginSlot) -> Option<Result<Vec<String>>> {
        match self.pool.get(&slot.instance)? {
            Loaded::Vst3(plugin) => Some(vst3_preset(plugin).map(|p| p.to_lines())),
            #[cfg(target_os = "macos")]
            Loaded::Au(plugin) => Some(super::au::capture_state(plugin)),
            _ => None,
        }
    }

    /// The project with every loaded plug-in's current state written into its slot. A state
    /// that can't be had keeps what the slot had (with a warning in the log).
    pub(super) fn project_with_plugin_state(&self) -> Project {
        let mut project = self.project.clone();
        let slots = project.tracks.iter_mut().flat_map(|t| t.instrument.iter_mut().chain(t.plugins.iter_mut())).chain(project.master_plugins.iter_mut());
        for slot in slots {
            match self.capture_state(slot) {
                Some(Ok(state)) => slot.state = state,
                Some(Err(e)) => tracing::warn!("'{}': state not saved: {e:#}", slot.name),
                None => {}
            }
        }
        project
    }

    /// Save a VST3 slot's current state as a `.vstpreset` file (what VST3 hosts load as a
    /// preset). Returns its size in bytes.
    pub fn save_plugin_preset(&self, track: usize, which: SlotRef, path: &Path) -> Result<usize> {
        let slot = self.slot_ref(track, which)?;
        let Some(Loaded::Vst3(plugin)) = self.pool.get(&slot.instance) else { bail!("'{}' isn't a loaded VST3 plug-in", slot.name) };
        let bytes = vst3_preset(plugin)?.to_bytes();
        std::fs::write(path, &bytes).with_context(|| format!("writing {}", path.display()))?;
        Ok(bytes.len())
    }

    /// Load a `.vstpreset` file into a VST3 slot. Parameter values the project holds follow
    /// the preset as one undo step; the rest of the plug-in's state isn't part of undo.
    pub fn load_plugin_preset(&mut self, track: usize, which: SlotRef, path: &Path) -> Result<()> {
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let preset = VstPreset::from_bytes(&bytes).with_context(|| path.display().to_string())?;
        let slot = self.slot_ref(track, which)?;
        let Some(Loaded::Vst3(plugin)) = self.pool.get(&slot.instance) else { bail!("'{}' isn't a loaded VST3 plug-in", slot.name) };
        if !preset.class_id.eq_ignore_ascii_case(&plugin.class.cid) {
            bail!("the preset is for another plug-in (class {})", preset.class_id);
        }
        plugin.load_state(&PluginState { component: preset.component().to_vec(), controller: preset.controller().map(<[u8]>::to_vec) })?;
        self.reload_params(track, which)
    }

    /// Locate and instantiate a plugin referenced by a project file. Returns its instance id
    /// and a warning if its saved state couldn't be restored. A VST3's state then lives in
    /// the instance, so the slot drops its copy (it can be large, and every undo step holds
    /// the project).
    pub(super) fn load_slot(&mut self, slot: &mut PluginSlot) -> Result<(u64, Option<String>)> {
        match slot.format {
            PluginFormat::Builtin if slot.path == synth::PATH => return Ok((self.add_to_pool(Loaded::Synth), None)),
            PluginFormat::Builtin if slot.path == drums::PATH => return Ok((self.add_to_pool(Loaded::Drums), None)),
            PluginFormat::Builtin if builtin_effect_instance(&slot.path).is_some() => {
                return Ok((self.add_to_pool(builtin_effect_instance(&slot.path).expect("known built-in")), None));
            }
            PluginFormat::Vst3 => {}
            PluginFormat::AudioUnit => return Ok((self.load_au_slot(slot)?, None)),
            _ => bail!("unsupported plugin format"),
        }
        let (plugin, warning) = self.instantiate_vst3_slot(slot)?;
        slot.state = Vec::new();
        Ok((self.add_to_pool(Loaded::Vst3(plugin)), warning))
    }

    /// A new VST3 instance for a slot, with its saved state and parameter values. A state it
    /// can't take (damaged, another host's format, refused) is skipped, which the warning
    /// returned says: the parameters still apply.
    fn instantiate_vst3_slot(&mut self, slot: &PluginSlot) -> Result<(Vst3Plugin, Option<String>)> {
        let bundle = if Path::new(&slot.path).exists() {
            PathBuf::from(&slot.path)
        } else {
            let file_name = Path::new(&slot.path).file_name().map(|f| f.to_string_lossy().to_lowercase());
            opentake_vst3::scan::find_bundles(&self.plugin_paths)
                .into_iter()
                .find(|b| b.file_name().map(|f| f.to_string_lossy().to_lowercase()) == file_name)
                .ok_or_else(|| anyhow!("bundle '{}' not found in plugin search paths", slot.path))?
        };
        // Reaper's uid is "<id>{<cid>}"; ours is the bare cid.
        let cid = slot.uid.rsplit('{').next().map(|s| s.trim_end_matches('}').to_string());
        let name = display_name(&slot.name);
        let plugin = self.instantiate(&bundle, cid.as_deref(), name.as_deref())?;
        let mut warning = None;
        if !slot.state.is_empty() {
            let restored = VstPreset::from_lines(&slot.state).map_err(anyhow::Error::from).and_then(|p| {
                if !p.class_id.eq_ignore_ascii_case(&plugin.class.cid) {
                    tracing::warn!("'{}': state saved for class {}, loading into {}", slot.name, p.class_id, plugin.class.cid);
                }
                let state = PluginState { component: p.component().to_vec(), controller: p.controller().map(<[u8]>::to_vec) };
                Ok(plugin.load_state(&state)?)
            });
            warning = restored.err().map(|e| format!("plugin '{}': saved state not restored, using its parameters: {e:#}", slot.name));
        }
        for &(id, value) in &slot.params {
            plugin.set_param_on_controller(id, value);
        }
        Ok((plugin, warning))
    }

    /// A separate processor for offline rendering (analysis, export): built-ins are created
    /// fresh, VST3s get a new instance with the live one's current state (or the slot's, if
    /// it isn't loaded), so the live audio thread is never touched. Saved parameter values
    /// are applied.
    pub(super) fn offline_processor(&mut self, slot: &PluginSlot, sample_rate: f64) -> Result<Box<dyn Processor>> {
        let sr = sample_rate;
        let p: Box<dyn Processor> = match (slot.format.clone(), slot.path.as_str()) {
            (PluginFormat::Builtin, synth::PATH) => Box::new(Synth::new(sr)),
            (PluginFormat::Builtin, drums::PATH) => Box::new(DrumSynth::new(sr)),
            (PluginFormat::Builtin, eq::PATH) => Box::new(Eq::new(sr)),
            (PluginFormat::Builtin, comp::PATH) => Box::new(Compressor::new(sr, None)),
            (PluginFormat::Builtin, limiter::PATH) => Box::new(Limiter::new(sr, None)),
            (PluginFormat::Builtin, multiband::PATH) => Box::new(Multiband::new(sr, None)),
            (PluginFormat::Builtin, delay::PATH) => Box::new(Delay::new(sr)),
            (PluginFormat::Builtin, reverb::PATH) => Box::new(Reverb::new(sr, None)),
            (PluginFormat::Builtin, gate::PATH) => Box::new(Gate::new(sr, None)),
            (PluginFormat::Builtin, deesser::PATH) => Box::new(DeEsser::new(sr, None)),
            (PluginFormat::Builtin, chorus::PATH) => Box::new(Chorus::new(sr)),
            (PluginFormat::Builtin, phaser::PATH) => Box::new(Phaser::new(sr)),
            (PluginFormat::Vst3, _) => {
                let mut slot = slot.clone();
                match self.capture_state(&slot) {
                    Some(Ok(state)) => slot.state = state,
                    Some(Err(e)) => tracing::warn!("'{}': state not captured for rendering: {e:#}", slot.name),
                    None => {}
                }
                let (plugin, warning) = self.instantiate_vst3_slot(&slot)?;
                if let Some(w) = warning {
                    tracing::warn!("{w}");
                }
                let mut p = plugin.create_processor(sr, MAX_BLOCK, 2)?;
                for &(id, value) in &slot.params {
                    p.queue_param(id, value);
                }
                let latency = p.latency_samples() as usize;
                return Ok(Box::new(Vst3Insert { processor: p, notes: Vec::with_capacity(1024), latency }));
            }
            (PluginFormat::AudioUnit, _) => return self.offline_au_processor(slot, sr),
            _ => bail!("'{}' can't be rendered offline", slot.name),
        };
        Ok(with_params(p, &slot.params))
    }

    fn is_loaded(&self, slot: &PluginSlot) -> bool {
        slot.instance != 0 && self.pool.contains_key(&slot.instance)
    }

    /// Audio-thread processor for a slot (`None` if the instance isn't loaded).
    pub(super) fn make_processor(&self, slot: &PluginSlot, sr: u32) -> Result<Option<Box<PluginRt>>> {
        let processor: Box<dyn Processor> = match self.pool.get(&slot.instance) {
            Some(Loaded::Vst3(plugin)) => {
                let mut p = plugin.create_processor(sr as f64, MAX_BLOCK, 2)?;
                for &(id, value) in &slot.params {
                    p.queue_param(id, value);
                }
                let latency = p.latency_samples() as usize;
                Box::new(Vst3Insert { processor: p, notes: Vec::with_capacity(1024), latency })
            }
            #[cfg(target_os = "macos")]
            Some(Loaded::Au(plugin)) => Box::new(super::au::AuInsert::new(plugin, sr as f64)?),
            Some(Loaded::Synth) => {
                let mut s = Synth::new(sr as f64);
                for &(id, value) in &slot.params {
                    s.set_param(id, value);
                }
                Box::new(s)
            }
            Some(Loaded::Drums) => {
                let mut d = DrumSynth::new(sr as f64);
                for &(id, value) in &slot.params {
                    d.set_param(id, value);
                }
                Box::new(d)
            }
            Some(Loaded::Eq(tap)) => with_params(Box::new(Eq::with_tap(sr as f64, Some(tap.clone()))), &slot.params),
            Some(Loaded::Compressor(meter)) => with_params(Box::new(Compressor::new(sr as f64, Some(meter.clone()))), &slot.params),
            Some(Loaded::Limiter(meter)) => with_params(Box::new(Limiter::new(sr as f64, Some(meter.clone()))), &slot.params),
            Some(Loaded::Multiband(meter)) => with_params(Box::new(Multiband::new(sr as f64, Some(meter.clone()))), &slot.params),
            Some(Loaded::Delay) => with_params(Box::new(Delay::new(sr as f64)), &slot.params),
            Some(Loaded::Reverb(meter)) => with_params(Box::new(Reverb::new(sr as f64, Some(meter.clone()))), &slot.params),
            Some(Loaded::Gate(meter)) => with_params(Box::new(Gate::new(sr as f64, Some(meter.clone()))), &slot.params),
            Some(Loaded::DeEsser(meter)) => with_params(Box::new(DeEsser::new(sr as f64, Some(meter.clone()))), &slot.params),
            Some(Loaded::Chorus) => with_params(Box::new(Chorus::new(sr as f64)), &slot.params),
            Some(Loaded::Phaser) => with_params(Box::new(Phaser::new(sr as f64)), &slot.params),
            None => return Ok(None),
        };
        Ok(Some(Box::new(PluginRt { instance: slot.instance, processor, bypass: slot.bypass, key: slot.side_chain.map(|k| KeyRt::new(k.track, k.pre_fader)) })))
    }

    /// Bring the audio thread's effect chain for a track in line with the project,
    /// reusing processors it already has.
    pub(super) fn sync_chain(&mut self, index: usize) -> Result<()> {
        if self.audio.is_none() {
            return Ok(());
        }
        let id = self.track_id(index);
        let live = self.live_chain.get(&id).cloned().unwrap_or_default();
        let slots: Vec<PluginSlot> = self.project.tracks[index].plugins.iter().filter(|s| self.is_loaded(s)).cloned().collect();
        let sr = self.sample_rate();
        let mut new = Vec::new();
        for slot in slots.iter().filter(|s| !live.contains(&s.instance)) {
            if let Some(p) = self.make_processor(slot, sr)? {
                new.push(p);
            }
        }
        let order: Vec<(u64, bool)> = slots.iter().map(|s| (s.instance, s.bypass)).collect();
        self.live_chain.insert(id, order.iter().map(|o| o.0).collect());
        self.send(Command::SetPluginChain {
            track: id,
            order: Box::new(order),
            new: Box::new(new),
            target: Box::new(Vec::with_capacity(PLUGIN_CAPACITY)),
        })?;
        // Envelopes address processors by instance.
        self.sync_automation(Some(index))
    }

    /// Bring the audio thread's master insert chain in line with the project.
    pub(super) fn sync_master_chain(&mut self) -> Result<()> {
        if self.audio.is_none() {
            return Ok(());
        }
        let live = self.live_chain.get(&MASTER).cloned().unwrap_or_default();
        let slots: Vec<PluginSlot> = self.project.master_plugins.iter().filter(|s| self.is_loaded(s)).cloned().collect();
        let sr = self.sample_rate();
        let mut new = Vec::new();
        for slot in slots.iter().filter(|s| !live.contains(&s.instance)) {
            if let Some(p) = self.make_processor(slot, sr)? {
                new.push(p);
            }
        }
        let order: Vec<(u64, bool)> = slots.iter().map(|s| (s.instance, s.bypass)).collect();
        self.live_chain.insert(MASTER, order.iter().map(|o| o.0).collect());
        self.send(Command::SetPluginChain {
            track: MASTER,
            order: Box::new(order),
            new: Box::new(new),
            target: Box::new(Vec::with_capacity(PLUGIN_CAPACITY)),
        })?;
        self.sync_automation(None)
    }

    /// Mixer id for a slot's commands: the master bus or the track.
    fn slot_target(&self, track: usize, which: SlotRef) -> u64 {
        match which {
            SlotRef::Master(_) => MASTER,
            _ => self.track_id(track),
        }
    }

    /// Bring the audio thread's instrument for a track in line with the project.
    pub(super) fn sync_instrument(&mut self, index: usize) -> Result<()> {
        if self.audio.is_none() {
            return Ok(());
        }
        let id = self.track_id(index);
        let slot = self.project.tracks[index].instrument.clone().filter(|s| self.is_loaded(s));
        let want = slot.as_ref().map(|s| s.instance);
        if self.live_instrument.get(&id).copied().flatten() != want {
            let processor = match &slot {
                Some(s) => self.make_processor(s, self.sample_rate())?,
                None => None,
            };
            self.live_instrument.insert(id, want);
            self.send(Command::SetInstrument { track: id, plugin: processor })?;
        }
        if let Some(s) = slot {
            self.send(Command::SetPluginBypass { track: id, instance: s.instance, bypass: s.bypass })?;
        }
        self.sync_automation(Some(index))
    }

    // ---------------------------------------------------------------- slots

    pub(super) fn slot_ref(&self, track: usize, which: SlotRef) -> Result<&PluginSlot> {
        if let SlotRef::Master(s) = which {
            return self.project.master_plugins.get(s).ok_or_else(|| anyhow!("the master bus has no plugin at slot {s}"));
        }
        let t = &self.project.tracks[self.track_index(track)?];
        match which {
            SlotRef::Instrument => t.instrument.as_ref().ok_or_else(|| anyhow!("track {track} has no instrument")),
            SlotRef::Effect(s) => t.plugins.get(s).ok_or_else(|| anyhow!("track {track} has no plugin at slot {s}")),
            SlotRef::Master(_) => unreachable!(),
        }
    }

    pub(super) fn slot_mut(&mut self, track: usize, which: SlotRef) -> Result<&mut PluginSlot> {
        if let SlotRef::Master(s) = which {
            return self.project.master_plugins.get_mut(s).ok_or_else(|| anyhow!("the master bus has no plugin at slot {s}"));
        }
        let index = self.track_index(track)?;
        let t = &mut self.project.tracks[index];
        match which {
            SlotRef::Instrument => t.instrument.as_mut().ok_or_else(|| anyhow!("track {track} has no instrument")),
            SlotRef::Effect(s) => t.plugins.get_mut(s).ok_or_else(|| anyhow!("track {track} has no plugin at slot {s}")),
            SlotRef::Master(_) => unreachable!(),
        }
    }

    /// Insert an effect on the master bus. `position` defaults to the end of the chain.
    pub fn insert_master_plugin(&mut self, query: &str, position: Option<usize>) -> Result<usize> {
        let slot = self.new_slot(query, false)?;
        self.record("Insert Master Plug-in", None, |e| {
            let pos = position.unwrap_or(usize::MAX).min(e.project.master_plugins.len());
            e.project.master_plugins.insert(pos, slot);
            e.sync_master_chain()?;
            Ok(pos)
        })
    }

    pub fn remove_master_plugin(&mut self, slot: usize) -> Result<PluginSlot> {
        self.slot_ref(0, SlotRef::Master(slot))?;
        self.record("Remove Master Plug-in", None, |e| {
            let removed = e.project.master_plugins.remove(slot);
            e.sync_master_chain()?;
            Ok(removed)
        })
    }

    /// The gain-reduction meter of a built-in compressor or limiter slot.
    pub fn fx_meter(&self, track: usize, which: SlotRef) -> Option<std::sync::Arc<GainMeter>> {
        let slot = self.slot_ref(track, which).ok()?;
        match self.pool.get(&slot.instance) {
            Some(Loaded::Compressor(m) | Loaded::Limiter(m) | Loaded::Reverb(m) | Loaded::Gate(m) | Loaded::DeEsser(m)) => Some(m.clone()),
            _ => None,
        }
    }

    /// The band meters of a built-in multiband compressor slot.
    pub fn multiband_meter(&self, track: usize, which: SlotRef) -> Option<std::sync::Arc<MultibandMeter>> {
        let slot = self.slot_ref(track, which).ok()?;
        match self.pool.get(&slot.instance) {
            Some(Loaded::Multiband(m)) => Some(m.clone()),
            _ => None,
        }
    }

    /// The analyzer tap of a built-in EQ slot.
    pub fn eq_tap(&self, track: usize, which: SlotRef) -> Option<std::sync::Arc<eq::Tap>> {
        let slot = self.slot_ref(track, which).ok()?;
        match self.pool.get(&slot.instance) {
            Some(Loaded::Eq(tap)) => Some(tap.clone()),
            _ => None,
        }
    }

    /// Effects available to insert: the built-ins plus scanned VST3 and AU effects.
    pub fn available_effects(&self) -> Vec<(String, String)> {
        let mut list: Vec<(String, String)> =
            [(eq::NAME, eq::PATH), (comp::NAME, comp::PATH), (multiband::NAME, multiband::PATH), (limiter::NAME, limiter::PATH), (delay::NAME, delay::PATH), (reverb::NAME, reverb::PATH), (gate::NAME, gate::PATH), (deesser::NAME, deesser::PATH), (chorus::NAME, chorus::PATH), (phaser::NAME, phaser::PATH)].iter().map(|(n, p)| (n.to_string(), p.to_string())).collect();
        for p in self.scanned.iter().filter(|p| !p.class.is_instrument()) {
            list.push((format!("{} — {}", p.class.name, p.class.vendor), p.bundle.to_string_lossy().into_owned()));
        }
        list.extend(self.audio_units.iter().filter(|a| !a.instrument).map(|a| (format!("{} — {} (AU)", a.name, a.manufacturer), a.id.clone())));
        list
    }

    // ---------------------------------------------------------------- effects

    /// Insert an effect on a track. `position` defaults to the end of the chain.
    pub fn insert_plugin(&mut self, track: usize, query: &str, position: Option<usize>) -> Result<usize> {
        let index = self.track_index(track)?;
        let slot = self.new_slot(query, false)?;
        self.record("Insert Plug-in", None, |e| {
            let pos = position.unwrap_or(usize::MAX).min(e.project.tracks[index].plugins.len());
            e.project.tracks[index].plugins.insert(pos, slot);
            e.sync_chain(index)?;
            Ok(pos)
        })
    }

    pub fn remove_plugin(&mut self, track: usize, slot: usize) -> Result<PluginSlot> {
        let index = self.track_index(track)?;
        self.slot_ref(track, SlotRef::Effect(slot))?;
        self.record("Remove Plug-in", None, |e| {
            let removed = e.project.tracks[index].plugins.remove(slot);
            e.sync_chain(index)?;
            Ok(removed)
        })
    }

    pub fn set_plugin_bypass(&mut self, track: usize, slot: usize, bypass: bool) -> Result<()> {
        self.set_slot_bypass(track, SlotRef::Effect(slot), bypass)
    }

    pub fn set_slot_bypass(&mut self, track: usize, which: SlotRef, bypass: bool) -> Result<()> {
        self.slot_ref(track, which)?;
        self.record(if bypass { "Bypass Plug-in" } else { "Enable Plug-in" }, None, |e| {
            let slot = e.slot_mut(track, which)?;
            slot.bypass = bypass;
            let instance = slot.instance;
            let target = e.slot_target(track, which);
            e.send(Command::SetPluginBypass { track: target, instance, bypass })
        })
    }

    pub fn plugin_params(&self, track: usize, slot: usize) -> Result<Vec<ParamView>> {
        self.slot_params(track, SlotRef::Effect(slot))
    }

    /// A plug-in's parameters; automated ones show their value at the playhead.
    pub fn slot_params(&self, track: usize, which: SlotRef) -> Result<Vec<ParamView>> {
        let mut params = self.slot_params_static(track, which)?;
        let owner = if matches!(which, SlotRef::Master(_)) { None } else { Some(track) };
        let slot = self.slot_ref(track, which)?;
        for p in &mut params {
            let target = opentake_project::AutomationTarget::Param { slot: slot.id, param: p.id };
            if let Some(v) = self.automated_value(owner, target) {
                p.value = v as f64;
                if let Some(d) = self.param_display(slot, p.id, p.value) {
                    p.display = Some(d);
                }
            }
        }
        Ok(params)
    }

    /// How a plug-in shows a parameter value (`None` if it can't say).
    pub(super) fn param_display(&self, slot: &PluginSlot, id: u32, value: f64) -> Option<String> {
        match self.pool.get(&slot.instance)? {
            Loaded::Vst3(plugin) => plugin.param_display(id, value),
            Loaded::Synth => Some(synth::display(id, value)),
            Loaded::Drums => Some(drums::display(id, value)),
            loaded => builtin_effect_params(loaded).map(|(_, display)| display(id, value)),
        }
    }

    /// A plug-in's parameters with their set values (automation aside).
    pub(super) fn slot_params_static(&self, track: usize, which: SlotRef) -> Result<Vec<ParamView>> {
        let slot = self.slot_ref(track, which)?;
        match self.pool.get(&slot.instance) {
            Some(Loaded::Vst3(plugin)) => Ok(plugin
                .parameters()
                .iter()
                .map(|p| {
                    let value = plugin.get_param(p.id).unwrap_or(p.default_normalized);
                    ParamView {
                        id: p.id,
                        title: p.title.clone(),
                        units: p.units.clone(),
                        value,
                        display: plugin.param_display(p.id, value),
                        step_count: p.step_count,
                        read_only: p.read_only,
                    }
                })
                .collect()),
            #[cfg(target_os = "macos")]
            Some(Loaded::Au(plugin)) => Ok(super::au::param_views(plugin)),
            Some(Loaded::Synth) => Ok(synth::PARAMS
                .iter()
                .map(|&(id, title, units, default, steps)| {
                    let value = slot.params.iter().find(|(pid, _)| *pid == id).map_or(default, |p| p.1);
                    ParamView {
                        id,
                        title: title.into(),
                        units: units.into(),
                        value,
                        display: Some(synth::display(id, value)),
                        step_count: steps,
                        read_only: false,
                    }
                })
                .collect()),
            Some(Loaded::Drums) => Ok(drums::PARAMS
                .iter()
                .map(|&(id, title, units, default, steps)| {
                    let value = slot.params.iter().find(|(pid, _)| *pid == id).map_or(default, |p| p.1);
                    ParamView {
                        id,
                        title: title.into(),
                        units: units.into(),
                        value,
                        display: Some(drums::display(id, value)),
                        step_count: steps,
                        read_only: false,
                    }
                })
                .collect()),
            Some(builtin @ (Loaded::Eq(_) | Loaded::Compressor(_) | Loaded::Limiter(_) | Loaded::Multiband(_) | Loaded::Delay | Loaded::Reverb(_) | Loaded::Gate(_) | Loaded::DeEsser(_) | Loaded::Chorus | Loaded::Phaser)) => {
                let (list, display) = builtin_effect_params(builtin).expect("checked");
                Ok(list
                    .into_iter()
                    .map(|(id, title, units, default, steps)| {
                        let value = slot.params.iter().find(|(pid, _)| *pid == id).map_or(default, |p| p.1);
                        ParamView { id, title, units: units.into(), value, display: Some(display(id, value)), step_count: steps, read_only: false }
                    })
                    .collect())
            }
            None => bail!("plugin '{}' is not loaded", slot.name),
        }
    }

    /// Set an effect parameter (normalized 0..1). Returns the display string.
    pub fn set_plugin_param(&mut self, track: usize, slot: usize, id: u32, value: f64) -> Result<Option<String>> {
        self.set_slot_param(track, SlotRef::Effect(slot), id, value)
    }

    pub fn set_slot_param(&mut self, track: usize, which: SlotRef, id: u32, value: f64) -> Result<Option<String>> {
        let instance = self.slot_ref(track, which)?.instance;
        self.set_slot_param_keyed(track, which, id, value, format!("param:{instance}:{id}"))
    }

    /// [`set_slot_param`](Self::set_slot_param) merging into the undo step with `key`.
    pub(super) fn set_slot_param_keyed(&mut self, track: usize, which: SlotRef, id: u32, value: f64, key: String) -> Result<Option<String>> {
        let index = if matches!(which, SlotRef::Master(_)) { track } else { self.track_index(track)? };
        let value = value.clamp(0.0, 1.0);
        let params = self.slot_params_static(track, which)?;
        let current = params.iter().find(|p| p.id == id).ok_or_else(|| anyhow!("plugin has no parameter with id {id}"))?.value;
        // Store the current value first (outside the undo step) so undo restores it.
        {
            let slot = self.slot_mut(track, which)?;
            if !slot.params.iter().any(|(pid, _)| *pid == id) {
                slot.params.push((id, current));
            }
        }
        let title = params.iter().find(|p| p.id == id).map(|p| p.title.clone()).unwrap_or_default();
        self.record(format!("Change {title}"), Some(key), |e| {
            e.apply_param(index, which, id, value)?;
            Ok(())
        })?;
        Ok(self.slot_params(track, which)?.into_iter().find(|p| p.id == id).and_then(|p| p.display))
    }

    /// Set several parameters as one undo step (e.g. an EQ band's frequency and gain while
    /// dragging it). Repeated calls with the same `label` and ids merge into one step.
    pub fn set_slot_params(&mut self, track: usize, which: SlotRef, values: &[(u32, f64)], label: &str) -> Result<()> {
        let index = if matches!(which, SlotRef::Master(_)) { track } else { self.track_index(track)? };
        let params = self.slot_params_static(track, which)?;
        for &(id, _) in values {
            let current = params.iter().find(|p| p.id == id).ok_or_else(|| anyhow!("plugin has no parameter with id {id}"))?.value;
            let slot = self.slot_mut(track, which)?;
            if !slot.params.iter().any(|(pid, _)| *pid == id) {
                slot.params.push((id, current));
            }
        }
        let instance = self.slot_ref(track, which)?.instance;
        let ids: Vec<String> = values.iter().map(|v| v.0.to_string()).collect();
        self.record(label.to_string(), Some(format!("params:{instance}:{}", ids.join(","))), |e| {
            for &(id, value) in values {
                e.apply_param(index, which, id, value.clamp(0.0, 1.0))?;
            }
            Ok(())
        })
    }

    /// Write a parameter into the project, the plugin controller and the processor.
    pub(super) fn apply_param(&mut self, index: usize, which: SlotRef, id: u32, value: f64) -> Result<()> {
        let slot = self.slot_mut(index, which)?;
        match slot.params.iter_mut().find(|(pid, _)| *pid == id) {
            Some(p) => p.1 = value,
            None => slot.params.push((id, value)),
        }
        let instance = slot.instance;
        if let Some(plugin) = self.pool.get(&instance) {
            plugin.set_param(id, value);
        }
        let target = self.slot_target(index, which);
        self.send(Command::SetPluginParam { track: target, instance, id, value })
    }

    // ---------------------------------------------------------------- instruments

    /// Set (or with `None`, remove) a track's instrument: "synth" for the built-in synth,
    /// or a VST3 / AU instrument name, bundle path or AU id ("au:aumu:dls :appl").
    pub fn set_instrument(&mut self, track: usize, query: Option<&str>) -> Result<Option<PluginSlot>> {
        let index = self.track_index(track)?;
        let slot = match query {
            Some(q) => Some(self.new_slot(q, true)?),
            None => None,
        };
        let label = if slot.is_some() { "Set Instrument" } else { "Remove Instrument" };
        self.record(label, None, |e| {
            e.project.tracks[index].instrument = slot.clone();
            e.sync_instrument(index)?;
            Ok(slot)
        })
    }

    /// Instruments available: the built-ins plus scanned VST3 and AU instruments.
    pub fn available_instruments(&self) -> Vec<(String, String)> {
        let mut list = vec![(synth::NAME.to_string(), "synth".to_string()), (drums::NAME.to_string(), "drums".to_string())];
        for p in self.scanned.iter().filter(|p| p.class.is_instrument()) {
            list.push((format!("{} — {}", p.class.name, p.class.vendor), p.bundle.to_string_lossy().into_owned()));
        }
        list.extend(self.audio_units.iter().filter(|a| a.instrument).map(|a| (format!("{} — {} (AU)", a.name, a.manufacturer), a.id.clone())));
        list
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controllers_map_to_vst3_controller_numbers() {
        use opentake_vst3::host::{CTRL_AFTERTOUCH, CTRL_PITCH_BEND};
        assert_eq!(vst3_control(Controller::PitchBend, 0), (CTRL_PITCH_BEND, 0.0));
        let (n, centre) = vst3_control(Controller::PitchBend, opentake_project::BEND_CENTRE);
        assert!(n == CTRL_PITCH_BEND && (centre - 0.5).abs() < 1e-4);
        assert_eq!(vst3_control(Controller::MOD_WHEEL, 127), (1, 1.0));
        assert_eq!(vst3_control(Controller::ChannelPressure, 0), (CTRL_AFTERTOUCH, 0.0));
    }

    /// A plug-in that isn't installed keeps its saved state through loading and saving.
    #[test]
    fn missing_vst3_keeps_its_state_in_the_project_file() {
        let dir = std::env::temp_dir().join(format!("opentake-vst3-missing-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut project = Project::default();
        let mut track = opentake_project::Track::new("Gtr");
        let preset = VstPreset::new("00112233445566778899AABBCCDDEEFF", vec![3; 1000], Some(vec![4; 10]));
        track.plugins.push(PluginSlot { state: preset.to_lines(), params: vec![(7, 0.25)], ..builtin_slot("VST3: Gone (Nobody)", "/nowhere/Gone.vst3") });
        track.plugins[0].format = PluginFormat::Vst3;
        project.tracks.push(track);
        let path = dir.join("song.otp");
        std::fs::write(&path, serde_json::to_string(&project).unwrap()).unwrap();

        let mut e = Engine::new();
        let warnings = e.load_project(&path).unwrap();
        assert!(warnings.iter().any(|w| w.contains("Gone")), "{warnings:?}");
        let again = dir.join("again.otp");
        e.save_project(Some(&again)).unwrap();
        let back: Project = serde_json::from_str(&std::fs::read_to_string(&again).unwrap()).unwrap();
        assert_eq!(VstPreset::from_lines(&back.tracks[0].plugins[0].state), Ok(preset));
        assert_eq!(back.tracks[0].plugins[0].params, vec![(7, 0.25)]);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The first installed VST3 effect, if any (tests with a real plug-in need one).
    fn installed_vst3_effect() -> Option<(PathBuf, ClassInfo)> {
        let roots = opentake_vst3::scan::default_search_paths();
        opentake_vst3::scan_plugins(&roots).into_iter().find_map(|(bundle, classes)| Some((bundle, classes.ok()?.into_iter().find(|c| !c.is_instrument())?)))
    }

    fn vst3_state(e: &Engine, t: usize) -> VstPreset {
        let Some(Loaded::Vst3(plugin)) = e.pool.get(&e.project().tracks[t].plugins[0].instance) else { panic!("a loaded VST3") };
        vst3_preset(plugin).unwrap()
    }

    /// With a real plug-in (skipped when none is installed): its full state survives saving
    /// and loading in every format, undo after loading, and goes to offline instances.
    #[test]
    fn vst3_state_survives_save_load_undo_and_offline_rendering() {
        let Some((bundle, class)) = installed_vst3_effect() else {
            eprintln!("no VST3 effect installed; skipping");
            return;
        };
        let mut e = Engine::new();
        let mut mixer = e.attach_test_mixer(48_000);
        let t = e.add_track("Gtr").unwrap();
        e.insert_plugin(t, &bundle.to_string_lossy(), None).unwrap();
        let initial = vst3_state(&e, t);
        // Change a parameter the plug-in stores (one that takes the value); the processor
        // gets it with the next block.
        let params = e.plugin_params(t, 0).unwrap();
        let changed = params.iter().filter(|p| !p.read_only).find_map(|p| {
            let target = if p.value > 0.5 { 0.1 } else { 0.9 };
            e.set_plugin_param(t, 0, p.id, target).ok()?;
            let Some(Loaded::Vst3(plugin)) = e.pool.get(&e.project().tracks[t].plugins[0].instance) else { return None };
            plugin.get_param(p.id).filter(|v| (v - p.value).abs() > 1e-3).map(|v| (p.id, v))
        });
        mixer.process(&[], 0, &mut [0.0; 2 * 64], 2);
        let before = vst3_state(&e, t);
        assert!(before.class_id.eq_ignore_ascii_case(&class.cid) && !before.component().is_empty());
        if changed.is_some() {
            assert_ne!(before.component(), initial.component(), "the component state holds the change");
        }

        let dir = std::env::temp_dir().join(format!("opentake-vst3-state-{}", std::process::id()));
        for ext in ["otp", "rpp", "dawproject"] {
            let path = dir.join(format!("song.{ext}"));
            e.save_project(Some(&path)).unwrap();
            let mut back = Engine::new();
            let warnings = back.load_project(&path).unwrap();
            assert!(warnings.is_empty(), "{ext}: {warnings:?}");
            assert_eq!(vst3_state(&back, t).component(), before.component(), "{ext}: the component state comes back");
            assert!(back.project().tracks[t].plugins[0].state.is_empty(), "{ext}: the instance holds it");
            if let Some((id, v)) = changed {
                let now = back.plugin_params(t, 0).unwrap().into_iter().find(|p| p.id == id).unwrap().value;
                assert!((now - v).abs() < 1e-3, "{ext}: {now} != {v}");
            }
            // Undo after loading: removing and bringing back the plug-in keeps its state.
            back.remove_plugin(t, 0).unwrap();
            back.undo().unwrap();
            assert_eq!(vst3_state(&back, t).component(), before.component(), "{ext}: after undo");
        }
        // An offline instance gets the live one's state: the state alone sets the controller.
        let mut slot = e.project().tracks[t].plugins[0].clone();
        slot.state = e.capture_state(&slot).unwrap().unwrap();
        slot.params.clear();
        let (copy, warning) = e.instantiate_vst3_slot(&slot).unwrap();
        assert_eq!(warning, None);
        assert_eq!(vst3_preset(&copy).unwrap().component(), before.component());
        if let Some((id, v)) = changed {
            assert!(copy.get_param(id).is_some_and(|now| (now - v).abs() < 1e-3), "the controller follows the state");
        }
        assert!(e.offline_processor(&e.project().tracks[t].plugins[0].clone(), 48_000.0).is_ok());
        // Presets: saved from one instance, loaded into another (the saved values follow).
        let preset = dir.join("tone.vstpreset");
        assert!(e.save_plugin_preset(t, SlotRef::Effect(0), &preset).unwrap() > 48);
        let fresh = e.add_track("Fresh").unwrap();
        e.insert_plugin(fresh, &bundle.to_string_lossy(), None).unwrap();
        e.load_plugin_preset(fresh, SlotRef::Effect(0), &preset).unwrap();
        assert_eq!(vst3_state(&e, fresh).component(), before.component());
        assert!(e.load_plugin_preset(fresh, SlotRef::Effect(0), &dir.join("song.otp")).is_err(), "not a preset");
        // A state the plug-in can't use leaves it on its parameters.
        slot.state = vec!["bm90IGEgcHJlc2V0".into()];
        let warning = e.instantiate_vst3_slot(&slot).unwrap().1;
        assert!(warning.is_some_and(|w| w.contains("not a VST3 preset")));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn builtin_synth_as_instrument_with_params_and_undo() {
        let mut e = Engine::new();
        let t = e.add_track("Keys").unwrap();
        let slot = e.set_instrument(t, Some("synth")).unwrap().unwrap();
        assert_eq!(slot.format, PluginFormat::Builtin);
        assert!(e.project().tracks[t].is_instrument());

        let params = e.slot_params(t, SlotRef::Instrument).unwrap();
        assert_eq!(params.len(), synth::PARAMS.len());
        let display = e.set_slot_param(t, SlotRef::Instrument, synth::param::WAVE, 0.0).unwrap();
        assert_eq!(display.as_deref(), Some("Sine"));

        e.undo().unwrap();
        let wave = e.slot_params(t, SlotRef::Instrument).unwrap().into_iter().find(|p| p.id == synth::param::WAVE).unwrap();
        assert_eq!(wave.display.as_deref(), Some("Saw"), "undo restores the previous value");
        e.undo().unwrap();
        assert!(!e.project().tracks[t].is_instrument(), "undo removes the instrument");
        e.redo().unwrap();
        assert!(e.project().tracks[t].is_instrument());
    }

    #[test]
    fn builtin_chorus_and_phaser_by_name() {
        let mut e = Engine::new();
        let t = e.add_track("Gtr").unwrap();
        for (query, path, count) in [("chorus", chorus::PATH, 8), ("phaser", phaser::PATH, 7)] {
            let slot = e.insert_plugin(t, query, None).unwrap();
            assert_eq!(e.project().tracks[t].plugins[slot].path, path, "{query}");
            assert_eq!(e.slot_params(t, SlotRef::Effect(slot)).unwrap().len(), count);
        }
        assert_eq!(e.set_plugin_param(t, 1, phaser::STAGES, 1.0).unwrap().as_deref(), Some("12"));
        assert!(e.available_effects().iter().any(|(n, _)| n == chorus::NAME));
    }

    #[test]
    fn builtin_gate_and_deesser_by_name() {
        let mut e = Engine::new();
        let t = e.add_track("Vox").unwrap();
        for (query, path, count) in [("gate", gate::PATH, 8), ("noise gate", gate::PATH, 8), ("de-esser", deesser::PATH, 6), ("deesser", deesser::PATH, 6)] {
            let slot = e.insert_plugin(t, query, None).unwrap();
            assert_eq!(e.project().tracks[t].plugins[slot].path, path, "{query}");
            assert_eq!(e.slot_params(t, SlotRef::Effect(slot)).unwrap().len(), count);
            assert!(e.fx_meter(t, SlotRef::Effect(slot)).is_some(), "metered");
        }
        assert_eq!(e.set_plugin_param(t, 0, gate::THRESHOLD, gate::norm_threshold(-50.0)).unwrap().as_deref(), Some("-50.0"));
        assert!(e.available_effects().iter().any(|(n, _)| n == deesser::NAME));
    }

    #[test]
    fn builtin_reverb_by_name_with_params() {
        let mut e = Engine::new();
        let t = e.add_track("Reverb").unwrap();
        let slot = e.insert_plugin(t, "reverb", None).unwrap();
        let p = &e.project().tracks[t].plugins[slot];
        assert_eq!((p.name.as_str(), p.path.as_str(), p.format), (reverb::NAME, reverb::PATH, PluginFormat::Builtin));
        let params = e.slot_params(t, SlotRef::Effect(slot)).unwrap();
        let titles: Vec<&str> = params.iter().map(|p| p.title.as_str()).collect();
        assert_eq!(titles, ["Pre-Delay", "Size", "Decay", "Damping", "Low Cut", "Width", "Diffusion", "Modulation", "Mix", "Level"]);
        let display = e.set_plugin_param(t, slot, reverb::DECAY, reverb::norm_decay(4.0)).unwrap();
        assert_eq!(display.as_deref(), Some("4.00"));
        assert!(e.fx_meter(t, SlotRef::Effect(slot)).is_some(), "the editor gets in/out meters");
        assert!(e.available_effects().iter().any(|(n, _)| n == reverb::NAME));
        let m = e.insert_master_plugin("OpenTake Reverb", None).unwrap();
        assert_eq!(e.project().master_plugins[m].path, reverb::PATH);
    }
}
