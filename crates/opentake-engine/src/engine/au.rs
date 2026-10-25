//! Audio Unit slots (macOS): listing, loading, saved state and the audio-thread processor.
//! Elsewhere the list is empty and AU slots fail to load with a clear message.
//!
//! An AU slot keeps the codes in `uid` ("aufx:dely:appl"), the system's name in `path`
//! ("Apple: AUDelay") and, once saved, the unit's ClassInfo property list in `state`.

use anyhow::Result;
#[cfg(not(target_os = "macos"))]
use anyhow::bail;
use opentake_project::{PluginSlot, Project};
use serde::Serialize;

use super::{Engine, display_name};
use crate::mixer::Processor;

/// An installed audio unit.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AudioUnitInfo {
    pub name: String,
    pub manufacturer: String,
    /// What insert_plugin / set_instrument take, e.g. "au:aufx:dely:appl".
    pub id: String,
    pub instrument: bool,
}

const PREFIX: &str = "au:";

/// Installed audio units (cheap: nothing is loaded).
pub(super) fn scan() -> Vec<AudioUnitInfo> {
    #[cfg(target_os = "macos")]
    return opentake_au::scan()
        .into_iter()
        .map(|c| AudioUnitInfo { id: format!("{PREFIX}{}", c.id), name: c.name, manufacturer: c.manufacturer, instrument: c.instrument })
        .collect();
    #[cfg(not(target_os = "macos"))]
    Vec::new()
}

impl Engine {
    /// Audio units found by the last scan (always empty off macOS).
    pub fn audio_units(&self) -> &[AudioUnitInfo] {
        &self.audio_units
    }

    /// The audio unit a query names: "au:<codes>", "AU: Name (Manufacturer)", or a plain
    /// name (exact, or contained in the name with `partial`). A VST3 with exactly that
    /// plain name wins, so existing queries keep resolving as before.
    pub(super) fn find_audio_unit(&mut self, query: &str, instrument: bool, partial: bool) -> Option<AudioUnitInfo> {
        if !cfg!(target_os = "macos") {
            return None;
        }
        if self.audio_units.is_empty() {
            self.audio_units = scan();
        }
        let q = query.trim();
        let lower = q.to_lowercase();
        let explicit = lower.starts_with("au: ") || lower.starts_with("aui: ");
        if lower.starts_with(PREFIX) && !explicit {
            return self.audio_units.iter().find(|a| a.id == q || a.id.eq_ignore_ascii_case(q)).cloned();
        }
        let name = if explicit { display_name(q).unwrap_or_default().to_lowercase() } else { lower.clone() };
        let full = |a: &AudioUnitInfo| format!("{}: {} ({})", if a.instrument { "aui" } else { "au" }, a.name, a.manufacturer).to_lowercase();
        let matches: Vec<&AudioUnitInfo> = match self.audio_units.iter().find(|a| full(a) == lower) {
            Some(a) => vec![a],
            None if partial => self.audio_units.iter().filter(|a| a.name.to_lowercase().contains(&name)).collect(),
            None => self.audio_units.iter().filter(|a| a.name.to_lowercase() == name).collect(),
        };
        let pick = matches.iter().find(|a| a.instrument == instrument).or(matches.first()).map(|a| (*a).clone())?;
        if !explicit && !partial {
            if self.scanned.is_empty() {
                self.scan_plugins();
            }
            if self.scanned.iter().any(|p| p.class.name.to_lowercase() == lower) {
                return None;
            }
        }
        Some(pick)
    }

    /// The project with every loaded audio unit's current state written into its slot.
    pub(super) fn project_with_plugin_state(&self) -> Project {
        #[cfg_attr(not(target_os = "macos"), allow(unused_mut))]
        let mut project = self.project.clone();
        #[cfg(target_os = "macos")]
        {
            let slots = project.tracks.iter_mut().flat_map(|t| t.instrument.iter_mut().chain(t.plugins.iter_mut())).chain(project.master_plugins.iter_mut());
            for slot in slots {
                if let Some(super::plugins::Loaded::Au(plugin)) = self.pool.get(&slot.instance) {
                    match plugin.get_state() {
                        Ok(state) => slot.state = mac::encode_state(&state),
                        Err(e) => tracing::warn!("'{}': state not saved: {e}", slot.name),
                    }
                }
            }
        }
        project
    }

    /// Create a slot and pooled instance for an audio unit.
    pub(super) fn new_au_slot(&mut self, info: &AudioUnitInfo) -> Result<PluginSlot> {
        #[cfg(target_os = "macos")]
        return self.new_au_slot_mac(info);
        #[cfg(not(target_os = "macos"))]
        bail!("{}: Audio Units are only available on macOS", info.name)
    }

    /// Load an audio unit slot from a project into the pool. Returns its instance id.
    pub(super) fn load_au_slot(&mut self, slot: &PluginSlot) -> Result<u64> {
        #[cfg(target_os = "macos")]
        return self.load_au_slot_mac(slot);
        #[cfg(not(target_os = "macos"))]
        bail!("{}: Audio Units are only available on macOS", slot.name)
    }

    /// A separate instance for offline rendering, with the live instance's current state.
    pub(super) fn offline_au_processor(&mut self, slot: &PluginSlot, sample_rate: f64) -> Result<Box<dyn Processor>> {
        #[cfg(target_os = "macos")]
        return self.offline_au_processor_mac(slot, sample_rate);
        #[cfg(not(target_os = "macos"))]
        {
            let _ = sample_rate;
            bail!("{}: Audio Units are only available on macOS", slot.name)
        }
    }
}

#[cfg(target_os = "macos")]
pub(super) use mac::{AuInsert, param_views};

#[cfg(target_os = "macos")]
mod mac {
    use anyhow::{Context, Result, anyhow};
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use opentake_au::{AuPlugin, AuProcessor, ComponentInfo, NoteEvent};
    use opentake_project::{PluginFormat, PluginSlot};

    use super::{AudioUnitInfo, Engine, PREFIX};
    use crate::engine::ParamView;
    use crate::engine::plugins::Loaded;
    use crate::mixer::{MAX_BLOCK, MidiEvent, Processor, Transport};

    /// Base64 in lines of 128 characters, the way Reaper stores plug-in state.
    pub(super) fn encode_state(state: &[u8]) -> Vec<String> {
        STANDARD.encode(state).as_bytes().chunks(128).map(|c| String::from_utf8_lossy(c).into_owned()).collect()
    }

    fn decode_state(lines: &[String]) -> Result<Vec<u8>> {
        Ok(STANDARD.decode(lines.concat().trim())?)
    }

    pub(in crate::engine) struct AuInsert {
        processor: AuProcessor,
        notes: Vec<NoteEvent>,
        /// Reported when the processor was set up (not queried on the audio thread).
        latency: usize,
    }

    impl AuInsert {
        pub(in crate::engine) fn new(plugin: &AuPlugin, sample_rate: f64) -> Result<Self> {
            let processor = plugin.create_processor(sample_rate, MAX_BLOCK)?;
            let latency = processor.latency_samples() as usize;
            Ok(Self { processor, notes: Vec::with_capacity(1024), latency })
        }
    }

    impl Processor for AuInsert {
        fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], frames: usize, t: &Transport, events: &[MidiEvent]) {
            let t = opentake_au::Transport {
                playing: t.playing,
                recording: t.recording,
                tempo_bpm: t.tempo_bpm,
                time_sig: t.time_sig,
                position_samples: t.position_samples,
            };
            self.notes.clear();
            for e in events.iter().take(self.notes.capacity()) {
                self.notes.push(NoteEvent { offset: e.offset, pitch: e.pitch, velocity: e.velocity });
            }
            self.processor.process(inputs, outputs, frames, &t, &self.notes);
        }

        fn set_param(&mut self, id: u32, normalized: f64) {
            self.processor.set_param(id, normalized);
        }

        fn latency(&self) -> usize {
            self.latency
        }
    }

    pub(in crate::engine) fn param_views(plugin: &AuPlugin) -> Vec<ParamView> {
        plugin
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
            .collect()
    }

    impl Engine {
        pub(super) fn new_au_slot_mac(&mut self, info: &AudioUnitInfo) -> Result<PluginSlot> {
            let component = opentake_au::find(info.id.trim_start_matches(PREFIX)).ok_or_else(|| anyhow!("audio unit '{}' is not installed", info.name))?;
            let plugin = AuPlugin::new(&component, self.sample_rate() as f64, MAX_BLOCK).with_context(|| format!("loading {}", info.name))?;
            let mut slot = PluginSlot {
                name: component.display_name(),
                format: PluginFormat::AudioUnit,
                path: component.component_name(),
                uid: component.id.clone(),
                bypass: false,
                state: Vec::new(),
                params: Vec::new(),
                id: 0,
                instance: 0,
            };
            slot.instance = self.add_to_pool(Loaded::Au(plugin));
            Ok(slot)
        }

        pub(super) fn load_au_slot_mac(&mut self, slot: &PluginSlot) -> Result<u64> {
            let plugin = self.instantiate_au_slot(slot, self.sample_rate() as f64)?;
            Ok(self.add_to_pool(Loaded::Au(plugin)))
        }

        pub(super) fn offline_au_processor_mac(&mut self, slot: &PluginSlot, sample_rate: f64) -> Result<Box<dyn Processor>> {
            let mut slot = slot.clone();
            if let Some(Loaded::Au(live)) = self.pool.get(&slot.instance) {
                slot.state = encode_state(&live.get_state()?);
            }
            let plugin = self.instantiate_au_slot(&slot, sample_rate)?;
            Ok(Box::new(AuInsert::new(&plugin, sample_rate)?))
        }

        /// A new instance for a saved slot, with its state and parameter values. A state
        /// this host can't read (e.g. another host's) is skipped with a warning.
        fn instantiate_au_slot(&mut self, slot: &PluginSlot, sample_rate: f64) -> Result<AuPlugin> {
            let name = super::display_name(&slot.name).unwrap_or_default();
            let component: ComponentInfo = opentake_au::find(&slot.uid)
                .or_else(|| opentake_au::scan().into_iter().find(|c| c.component_name() == slot.path || c.name.eq_ignore_ascii_case(&name)))
                .ok_or_else(|| anyhow!("audio unit '{}' is not installed", slot.path))?;
            let plugin = AuPlugin::new(&component, sample_rate, MAX_BLOCK)?;
            if !slot.state.is_empty()
                && let Err(e) = decode_state(&slot.state).and_then(|s| Ok(plugin.set_state(&s)?))
            {
                tracing::warn!("'{}': saved state not restored: {e:#}", slot.name);
            }
            for &(id, value) in &slot.params {
                plugin.set_param(id, value);
            }
            Ok(plugin)
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use crate::engine::{Engine, SlotRef};
    use opentake_project::PluginFormat;

    #[test]
    fn audio_unit_slots_save_restore_and_undo() {
        let mut e = Engine::new();
        assert!(e.audio_units().is_empty());
        e.scan_plugins();
        assert!(e.audio_units().iter().any(|a| a.id == "au:aufx:dely:appl"));
        assert!(e.available_effects().iter().any(|(_, id)| id == "au:aufx:dely:appl"));
        assert!(e.available_instruments().iter().any(|(_, id)| id == "au:aumu:dls :appl"));

        let t = e.add_track("Gtr").unwrap();
        e.insert_plugin(t, "au:aufx:dely:appl", None).unwrap();
        let slot = e.project().tracks[t].plugins[0].clone();
        assert_eq!((slot.name.as_str(), slot.format, slot.uid.as_str()), ("AU: AUDelay (Apple)", PluginFormat::AudioUnit, "aufx:dely:appl"));
        assert_eq!(e.insert_plugin(t, "AU: AUPeakLimiter (Apple)", None).unwrap(), 1);
        assert_eq!(e.project().tracks[t].plugins[1].uid, "aufx:lmtr:appl");

        // A parameter change and its undo reach the unit itself.
        let mix = e.plugin_params(t, 0).unwrap().into_iter().find(|p| p.title.contains("Wet")).unwrap();
        e.set_plugin_param(t, 0, mix.id, 0.2).unwrap();
        let value = |e: &Engine| e.plugin_params(t, 0).unwrap().into_iter().find(|p| p.id == mix.id).unwrap().value;
        assert!((value(&e) - 0.2).abs() < 1e-3);
        e.undo().unwrap();
        assert!((value(&e) - mix.value).abs() < 1e-3, "undo restores the unit's value");
        e.redo().unwrap();

        // An instrument by name; save and reload keep the state.
        let keys = e.add_track("Keys").unwrap();
        e.set_instrument(keys, Some("DLSMusicDevice")).unwrap();
        assert!(!e.slot_params(keys, SlotRef::Instrument).unwrap().is_empty());
        let path = std::env::temp_dir().join(format!("opentake-au-{}.otp", std::process::id()));
        e.save_project(Some(&path)).unwrap();
        let mut back = Engine::new();
        let warnings = back.load_project(&path).unwrap();
        let rpp = path.with_extension("rpp");
        e.save_project(Some(&rpp)).unwrap();
        let rpp_warnings = Engine::new().load_project(&rpp).unwrap();
        let _ = (std::fs::remove_file(&path), std::fs::remove_file(&rpp));
        assert!(warnings.is_empty() && rpp_warnings.is_empty(), "{warnings:?} {rpp_warnings:?}");
        assert!(!back.project().tracks[t].plugins[0].state.is_empty(), "the state was saved");
        assert!((value(&back) - 0.2).abs() < 1e-3, "state and parameters restore");
        assert!(back.project().tracks[keys].is_instrument());
    }

    #[test]
    fn audio_units_play_through_the_mixer() {
        let mut e = Engine::new();
        let mut mixer = e.attach_test_mixer(48_000);
        let k = e.add_instrument_track("Keys", "au:aumu:dls :appl").unwrap();
        let clip = e.add_midi_clip(k, 0.0, 2.0, None).unwrap();
        let note = opentake_project::Note { start_secs: 0.0, length_secs: 1.0, pitch: 60, velocity: 110, tab: None };
        e.set_midi_notes(k, clip, vec![note]).unwrap();
        e.insert_plugin(k, "au:aufx:dely:appl", None).unwrap();
        e.play().unwrap();
        let mut out = vec![0.0f32; 2 * 256];
        let mut peak = 0.0f32;
        for _ in 0..60 {
            mixer.process(&[], 0, &mut out, 2);
            peak = out.iter().fold(peak, |m, s| m.max(s.abs()));
        }
        assert!(peak > 1e-3, "the AU instrument sounds through an AU effect: {peak}");
    }
}
