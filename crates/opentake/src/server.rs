//! MCP tool surface over the engine.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use opentake_engine::project::Note;
use opentake_engine::{AudioConfig, Engine, MidiClipUpdate, SlotRef, TrackUpdate};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

const INSTRUCTIONS: &str = "\
Controls OpenTake, a running digital audio workstation.

Typical flow: list_audio_devices -> start_audio -> add_track -> update_track (armed, input_channel, \
monitor_input) -> record -> stop (recordings become clips) -> save_project.

Tracks, clips and plugin slots are addressed by zero-based index as shown by get_project. \
Plugin parameters are normalized 0..1; list_plugin_params shows ids, current values and display strings. \
Projects can be saved/loaded as Reaper .rpp or native .json files.

MIDI: add_instrument_track (\"synth\" = built-in synth, or a VST3 instrument from list_instruments) -> \
add_midi_clip -> set_midi_notes / add_midi_notes (times in seconds relative to the region start, \
pitch 60 = middle C, velocity 1..127). Every edit — yours or the user's — can be undone with undo/redo; \
get_history lists the steps.

Tempo: set_tempo sets the opening tempo/signature; set_tempo_map adds tempo and time-signature changes. \
Region and note times are always seconds; get_tempo_map and convert_time translate between seconds and \
bars/beats. Effects: insert_plugin with \"eq\" adds the built-in 6-band EQ to a track; \
insert_master_plugin puts effects on the master bus (address them with master: true).";

pub type SharedEngine = Arc<Mutex<Engine>>;

#[derive(Clone)]
pub struct DawServer {
    engine: SharedEngine,
    tool_router: ToolRouter<Self>,
}

type ToolResult = Result<String, String>;

impl DawServer {
    pub fn new(engine: SharedEngine) -> Self {
        Self { engine, tool_router: Self::tool_router() }
    }

    /// Run `f` against the engine on a blocking thread and render the result as JSON.
    async fn with_engine<T, F>(&self, f: F) -> ToolResult
    where
        T: Serialize + Send + 'static,
        F: FnOnce(&mut Engine) -> anyhow::Result<T> + Send + 'static,
    {
        let engine = self.engine.clone();
        let result = tokio::task::spawn_blocking(move || {
            let mut e = engine.lock().map_err(|_| anyhow::anyhow!("engine lock poisoned"))?;
            f(&mut e)
        })
        .await
        .map_err(|e| format!("engine task failed: {e}"))?;
        let value = result.map_err(|e| format!("{e:#}"))?;
        serde_json::to_string_pretty(&value).map_err(|e| e.to_string())
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct StartAudioArgs {
    /// Input device name (substring) or id. Omit for the system default.
    pub input_device: Option<String>,
    /// Output device name (substring) or id. Omit for the system default.
    pub output_device: Option<String>,
    /// Sample rate in Hz. Omit for the output device default.
    pub sample_rate: Option<u32>,
    /// Buffer size in frames (smaller = lower latency). Omit for the device default.
    pub buffer_size: Option<u32>,
    /// Open output only (no monitoring or recording).
    #[serde(default)]
    pub disable_input: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NameArgs {
    pub name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PathArgs {
    /// File path. `.rpp` = Reaper project, anything else = native JSON.
    pub path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct OptionalPathArgs {
    /// File path (`.rpp` for Reaper, otherwise JSON). Omit to save to the current path.
    pub path: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TempoArgs {
    pub bpm: f64,
    pub numerator: Option<u32>,
    pub denominator: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TempoChangeArg {
    /// Where the change happens, in seconds (> 0).
    pub at_secs: f64,
    /// Quarter notes per minute from here on.
    pub bpm: f64,
    /// New time signature from here (starts a new bar); omit to keep the current one.
    pub numerator: Option<u32>,
    pub denominator: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TempoMapArgs {
    /// All tempo/time-signature changes after the start (replaces the existing ones; [] clears).
    pub changes: Vec<TempoChangeArg>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TempoMapQuery {
    /// Also list bar lines (number, time, signature, tempo) from this time...
    pub bars_from_secs: Option<f64>,
    /// ...to this time (seconds).
    pub bars_to_secs: Option<f64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ConvertTimeArgs {
    /// Seconds to convert to bar/beat; or give bar (and beat) to get seconds.
    pub secs: Option<f64>,
    /// 1-based bar number.
    pub bar: Option<i64>,
    /// 1-based beat (fractions allowed, e.g. 2.5); defaults to 1.
    pub beat: Option<f64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CountInArgs {
    /// Bars of count-in before recording (0 = off, max 4); omit to keep.
    pub bars: Option<u32>,
    /// Also count in before plain playback; omit to keep.
    pub on_play: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct InsertMasterPluginArgs {
    /// "eq" for the built-in EQ, or a VST3 effect name / .vst3 bundle path.
    pub plugin: String,
    /// Position in the master chain; omit to append.
    pub position: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MasterSlotArgs {
    /// Zero-based position in the master effect chain.
    pub slot: usize,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MasterVolumeArgs {
    /// Master level in dB (0 = unity, max +12).
    pub volume_db: f32,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MetronomeArgs {
    /// Turn the click on or off; omit to keep.
    pub enabled: Option<bool>,
    /// Click level in dB (0 = full); omit to keep.
    pub volume_db: Option<f32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GuitarProPathArgs {
    /// Path to a Guitar Pro 7/8 file (.gp) or a raw score.gpif.
    pub path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GuitarProImportArgs {
    /// Path to a Guitar Pro 7/8 file (.gp) or a raw score.gpif.
    pub path: String,
    /// Guitar Pro track indices to import (see inspect_guitar_pro); omit for all tracks with notes.
    pub tracks: Option<Vec<usize>>,
    /// Instrument for pitched tracks: "synth" (default) or a VST3 instrument (see list_instruments).
    pub instrument: Option<String>,
    /// Instrument for drum tracks (GM drum map): "drums" (built-in kit, default), "synth" or a VST3 instrument.
    pub drum_instrument: Option<String>,
    /// Adopt the song's opening tempo and time signature (default true).
    pub set_tempo: Option<bool>,
    /// Timeline position for the song start in seconds (default 0).
    pub position_secs: Option<f64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LoopArgs {
    /// Turn cycle (loop) playback on or off; omit to keep.
    pub enabled: Option<bool>,
    /// Loop start in seconds; omit to keep.
    pub start_secs: Option<f64>,
    /// Loop end in seconds; omit to keep.
    pub end_secs: Option<f64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TrackArgs {
    /// Zero-based track index.
    pub track: usize,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdateTrackArgs {
    /// Zero-based track index.
    pub track: usize,
    pub name: Option<String>,
    /// Fader level in dB (0 = unity, -inf..+12).
    pub volume_db: Option<f32>,
    /// -1 (left) .. 1 (right).
    pub pan: Option<f32>,
    pub mute: Option<bool>,
    pub solo: Option<bool>,
    /// Arm for recording.
    pub armed: Option<bool>,
    /// Hear the live input through the track (and its plugins).
    pub monitor_input: Option<bool>,
    /// Zero-based input channel on the audio interface.
    pub input_channel: Option<u16>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AddClipArgs {
    pub track: usize,
    /// Path to a WAV file.
    pub file: String,
    /// Timeline position in seconds.
    #[serde(default)]
    pub position_secs: f64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ClipArgs {
    pub track: usize,
    pub clip: usize,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MoveClipArgs {
    pub track: usize,
    pub clip: usize,
    /// New timeline position in seconds.
    pub position_secs: f64,
    /// Move to this track instead (index); omit to stay on the same track.
    pub to_track: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CopyClipArgs {
    pub track: usize,
    pub clip: usize,
    /// Timeline position (seconds) for the copy.
    pub position_secs: f64,
    /// Put the copy on this track instead; omit for the same track.
    pub to_track: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SplitClipArgs {
    pub track: usize,
    pub clip: usize,
    /// Timeline position (seconds) to split at; must be inside the clip.
    pub position_secs: f64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TrimClipArgs {
    pub track: usize,
    pub clip: usize,
    /// New start on the timeline (seconds). The audio stays in place; omit to keep.
    pub start_secs: Option<f64>,
    /// New end on the timeline (seconds); omit to keep.
    pub end_secs: Option<f64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct InstrumentTrackArgs {
    pub name: String,
    /// "synth" for the built-in synth, "drums" for the built-in GM drum kit, or a VST3 instrument name / bundle path (see list_instruments).
    pub instrument: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetInstrumentArgs {
    pub track: usize,
    /// "synth", "drums", a VST3 instrument name / bundle path, or omit to remove the instrument.
    pub instrument: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AddMidiClipArgs {
    pub track: usize,
    pub position_secs: f64,
    pub length_secs: f64,
    pub name: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NoteArg {
    /// Seconds from the start of the region.
    pub start_secs: f64,
    pub length_secs: f64,
    /// MIDI note number, 60 = middle C.
    pub pitch: u8,
    /// 1..=127 (default 100).
    pub velocity: Option<u8>,
}

impl From<&NoteArg> for Note {
    fn from(n: &NoteArg) -> Self {
        Note { start_secs: n.start_secs, length_secs: n.length_secs, pitch: n.pitch, velocity: n.velocity.unwrap_or(100), tab: None }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MidiNotesArgs {
    pub track: usize,
    /// MIDI region index on the track (see get_project → midi_clips).
    pub clip: usize,
    pub notes: Vec<NoteArg>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdateMidiClipArgs {
    pub track: usize,
    pub clip: usize,
    pub name: Option<String>,
    pub position_secs: Option<f64>,
    pub length_secs: Option<f64>,
    pub mute: Option<bool>,
    /// Move the region to this track.
    pub to_track: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CopyMidiClipArgs {
    pub track: usize,
    pub clip: usize,
    pub position_secs: f64,
    pub to_track: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct InstrumentParamArgs {
    pub track: usize,
    pub param_id: u32,
    /// Normalized value 0..1.
    pub value: f64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SeekArgs {
    pub position_secs: f64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ScanArgs {
    /// Extra folders to scan in addition to the standard VST3 locations.
    #[serde(default)]
    pub extra_paths: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct InsertPluginArgs {
    pub track: usize,
    /// Plugin name (as listed by scan_plugins, partial match ok) or path to a .vst3 bundle.
    pub plugin: String,
    /// Position in the effect chain; omit to append.
    pub position: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SlotArgs {
    pub track: usize,
    /// Zero-based position in the track's effect chain.
    pub slot: usize,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListParamsArgs {
    #[serde(default)]
    pub track: usize,
    pub slot: usize,
    /// Address the master bus's effect chain instead of a track (track is ignored).
    #[serde(default)]
    pub master: bool,
    /// Only return parameters whose title contains this text (case-insensitive).
    pub filter: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetParamArgs {
    #[serde(default)]
    pub track: usize,
    pub slot: usize,
    /// Address the master bus's effect chain instead of a track (track is ignored).
    #[serde(default)]
    pub master: bool,
    pub param_id: u32,
    /// Normalized value 0..1.
    pub value: f64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BypassArgs {
    #[serde(default)]
    pub track: usize,
    pub slot: usize,
    /// Address the master bus's effect chain instead of a track (track is ignored).
    #[serde(default)]
    pub master: bool,
    pub bypass: bool,
}

fn db_to_gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

#[tool_router]
impl DawServer {
    #[tool(description = "Transport position, play/record state, audio device info and peak meters (dBFS).")]
    async fn get_status(&self) -> ToolResult {
        self.with_engine(|e| Ok(e.status())).await
    }

    #[tool(description = "List audio input and output devices (interfaces) with channel counts.")]
    async fn list_audio_devices(&self) -> ToolResult {
        self.with_engine(|e| e.list_devices()).await
    }

    #[tool(description = "Open audio devices and start the engine. Required before play/record.")]
    async fn start_audio(&self, Parameters(a): Parameters<StartAudioArgs>) -> ToolResult {
        let cfg = AudioConfig {
            input_device: a.input_device,
            output_device: a.output_device,
            sample_rate: a.sample_rate,
            buffer_size: a.buffer_size,
            disable_input: a.disable_input,
        };
        self.with_engine(move |e| e.start_audio(&cfg)).await
    }

    #[tool(description = "Stop the audio engine and close devices.")]
    async fn stop_audio(&self) -> ToolResult {
        self.with_engine(|e| e.stop_audio().map(|_| "audio stopped")).await
    }

    #[tool(description = "Full project: tempo, tracks (with index), clips and plugin chains.")]
    async fn get_project(&self) -> ToolResult {
        self.with_engine(|e| Ok(json!({ "path": e.project_path(), "media_dir": e.media_dir(), "project": e.project() })))
            .await
    }

    #[tool(description = "Start a new project with one empty audio track.")]
    async fn new_project(&self, Parameters(a): Parameters<NameArgs>) -> ToolResult {
        self.with_engine(move |e| e.new_project(&a.name).map(|_| "created")).await
    }

    #[tool(description = "Load a project from a Reaper .rpp or native .json file. Returns warnings for missing media/plugins.")]
    async fn load_project(&self, Parameters(a): Parameters<PathArgs>) -> ToolResult {
        self.with_engine(move |e| e.load_project(&PathBuf::from(a.path)).map(|w| json!({ "warnings": w }))).await
    }

    #[tool(description = "Save the project. Use .otp (OpenTake project; added when no extension is given) or .rpp for a Reaper project. Plugin parameter values, MIDI and built-in plug-ins are only preserved in .otp.")]
    async fn save_project(&self, Parameters(a): Parameters<OptionalPathArgs>) -> ToolResult {
        self.with_engine(move |e| e.save_project(a.path.as_deref().map(std::path::Path::new))).await
    }

    #[tool(description = "Set the opening tempo and optionally the time signature (see set_tempo_map for changes later in the song).")]
    async fn set_tempo(&self, Parameters(a): Parameters<TempoArgs>) -> ToolResult {
        let sig = a.numerator.zip(a.denominator);
        self.with_engine(move |e| e.set_tempo(a.bpm, sig).map(|_| "ok")).await
    }

    #[tool(description = "Replace the tempo map's changes: tempo and/or time-signature changes after the start. Bars, the grid, snapping, the metronome and plug-ins follow it; region times stay in seconds. Undoable.")]
    async fn set_tempo_map(&self, Parameters(a): Parameters<TempoMapArgs>) -> ToolResult {
        let changes = a
            .changes
            .into_iter()
            .map(|c| opentake_engine::project::TempoChange { at_secs: c.at_secs, bpm: c.bpm, time_signature: c.numerator.zip(c.denominator) })
            .collect();
        self.with_engine(move |e| e.set_tempo_changes(changes, None).map(|_| "ok")).await
    }

    #[tool(description = "The tempo map: opening tempo/signature, the changes, and optionally the bar lines in a time range (bar number, start time, signature, tempo).")]
    async fn get_tempo_map(&self, Parameters(a): Parameters<TempoMapQuery>) -> ToolResult {
        self.with_engine(move |e| {
            let p = e.project();
            let map = p.tempo_map();
            let bars = match (a.bars_from_secs, a.bars_to_secs) {
                (Some(from), Some(to)) if to >= from => map
                    .bars(from, to.min(from + 3600.0))
                    .iter()
                    .take(2000)
                    .map(|b| json!({ "bar": b.index + 1, "secs": b.secs, "time_signature": [b.sig.0, b.sig.1], "bpm": map.tempo_at(b.secs) }))
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            };
            Ok(json!({
                "tempo_bpm": p.tempo_bpm,
                "time_signature": p.time_signature,
                "changes": p.tempo_changes,
                "bars": bars,
            }))
        })
        .await
    }

    #[tool(description = "Convert between seconds and bar/beat using the tempo map: give secs to get bar, beat and the tempo there, or bar (and beat) to get secs.")]
    async fn convert_time(&self, Parameters(a): Parameters<ConvertTimeArgs>) -> ToolResult {
        self.with_engine(move |e| {
            let map = e.project().tempo_map();
            match (a.secs, a.bar) {
                (Some(secs), _) => {
                    let pos = map.position(secs.max(0.0));
                    let sig = map.signature_at(secs.max(0.0));
                    Ok(json!({ "secs": secs, "bar": pos.bar, "beat": pos.beat as f64 + pos.fraction, "bpm": map.tempo_at(secs.max(0.0)), "time_signature": [sig.0, sig.1] }))
                }
                (None, Some(bar)) => {
                    let beat = a.beat.unwrap_or(1.0);
                    Ok(json!({ "bar": bar, "beat": beat, "secs": map.secs_of(bar.max(1), beat) }))
                }
                _ => anyhow::bail!("give either secs or bar"),
            }
        })
        .await
    }

    #[tool(description = "Make the project's tempo map follow an imported song's tempo and time-signature changes (for a Guitar Pro song imported without set_tempo). Undoable.")]
    async fn adopt_song_tempo(&self) -> ToolResult {
        self.with_engine(move |e| {
            let adopted = e.adopt_song_tempo()?;
            Ok(json!({ "adopted": adopted, "changes": e.project().tempo_changes.len() }))
        })
        .await
    }

    #[tool(description = "Metronome count-in: bars of clicks before recording starts (0 = off), and whether plain playback counts in too.")]
    async fn set_count_in(&self, Parameters(a): Parameters<CountInArgs>) -> ToolResult {
        self.with_engine(move |e| {
            e.set_count_in(a.bars, a.on_play)?;
            Ok(json!({ "bars": e.project().count_in_bars, "on_play": e.project().count_in_on_play }))
        })
        .await
    }

    #[tool(description = "Set the master output level in dB.")]
    async fn set_master_volume(&self, Parameters(a): Parameters<MasterVolumeArgs>) -> ToolResult {
        self.with_engine(move |e| e.set_master_volume(db_to_gain(a.volume_db)).map(|_| "ok")).await
    }

    #[tool(description = "Metronome: turn the click on/off and/or set its level. It follows tempo and time signature and plays while the transport runs.")]
    async fn set_metronome(&self, Parameters(a): Parameters<MetronomeArgs>) -> ToolResult {
        self.with_engine(move |e| e.set_metronome(a.enabled, a.volume_db.map(db_to_gain)).map(|_| "ok")).await
    }

    #[tool(description = "Cycle (loop) playback: set the loop range in seconds and/or turn it on or off. Playback wraps from the end back to the start (not while recording).")]
    async fn set_loop(&self, Parameters(a): Parameters<LoopArgs>) -> ToolResult {
        self.with_engine(move |e| {
            e.set_loop(a.enabled, a.start_secs, a.end_secs)?;
            let p = e.project();
            Ok(json!({ "enabled": p.loop_enabled, "start_secs": p.loop_start_secs, "end_secs": p.loop_end_secs }))
        })
        .await
    }

    #[tool(description = "Read a Guitar Pro 7/8 file (.gp) and list its tracks, tempo and length, without importing.")]
    async fn inspect_guitar_pro(&self, Parameters(a): Parameters<GuitarProPathArgs>) -> ToolResult {
        let path = PathBuf::from(a.path);
        self.with_engine(move |_| opentake_engine::inspect_guitar_pro(&path)).await
    }

    #[tool(description = "Import a Guitar Pro 7/8 song (.gp): each Guitar Pro track becomes an instrument track with a MIDI region (repeats, tempo changes, ties and tuplets applied). One undo step. Returns the new track indices.")]
    async fn import_guitar_pro(&self, Parameters(a): Parameters<GuitarProImportArgs>) -> ToolResult {
        let path = PathBuf::from(&a.path);
        let options = opentake_engine::GuitarProImport {
            tracks: a.tracks,
            instrument: a.instrument.unwrap_or_else(|| "synth".into()),
            drum_instrument: a.drum_instrument,
            set_tempo: a.set_tempo.unwrap_or(true),
            position_secs: a.position_secs.unwrap_or(0.0),
        };
        self.with_engine(move |e| e.import_guitar_pro(&path, &options).map(|t| json!({ "tracks": t }))).await
    }

    #[tool(description = "Add a track. Returns its index.")]
    async fn add_track(&self, Parameters(a): Parameters<NameArgs>) -> ToolResult {
        self.with_engine(move |e| e.add_track(&a.name).map(|i| json!({ "track": i }))).await
    }

    #[tool(description = "Remove a track (indices of later tracks shift down).")]
    async fn remove_track(&self, Parameters(a): Parameters<TrackArgs>) -> ToolResult {
        self.with_engine(move |e| e.remove_track(a.track)).await
    }

    #[tool(description = "Change track settings: name, volume (dB), pan, mute, solo, record arm, input monitoring, input channel.")]
    async fn update_track(&self, Parameters(a): Parameters<UpdateTrackArgs>) -> ToolResult {
        let update = TrackUpdate {
            name: a.name,
            volume: a.volume_db.map(db_to_gain),
            pan: a.pan,
            mute: a.mute,
            solo: a.solo,
            armed: a.armed,
            monitor_input: a.monitor_input,
            input_channel: a.input_channel,
        };
        self.with_engine(move |e| e.update_track(a.track, update)).await
    }

    #[tool(description = "Place a WAV file on a track at a position (seconds).")]
    async fn add_clip(&self, Parameters(a): Parameters<AddClipArgs>) -> ToolResult {
        self.with_engine(move |e| e.add_clip(a.track, &PathBuf::from(a.file), a.position_secs)).await
    }

    #[tool(description = "Remove a clip from a track.")]
    async fn remove_clip(&self, Parameters(a): Parameters<ClipArgs>) -> ToolResult {
        self.with_engine(move |e| e.remove_clip(a.track, a.clip)).await
    }

    #[tool(description = "Move a clip to a new position, optionally onto another track. The clip is appended to the target track's clip list.")]
    async fn move_clip(&self, Parameters(a): Parameters<MoveClipArgs>) -> ToolResult {
        self.with_engine(move |e| e.move_clip(a.track, a.clip, a.position_secs, a.to_track)).await
    }

    #[tool(description = "Copy a clip (including its trim) to another position, optionally on another track. The original stays. Returns the copy's track and clip index.")]
    async fn copy_clip(&self, Parameters(a): Parameters<CopyClipArgs>) -> ToolResult {
        self.with_engine(move |e| {
            e.copy_clip(a.track, a.clip, a.position_secs, a.to_track).map(|(t, c)| json!({ "track": t, "clip": c }))
        })
        .await
    }

    #[tool(description = "Split a clip in two at a timeline position. The left part keeps the clip index; the right part is inserted at index + 1.")]
    async fn split_clip(&self, Parameters(a): Parameters<SplitClipArgs>) -> ToolResult {
        self.with_engine(move |e| e.split_clip(a.track, a.clip, a.position_secs).map(|(l, r)| json!({ "left": l, "right": r })))
            .await
    }

    #[tool(description = "Trim a clip by moving its start and/or end edge (timeline seconds) without moving the audio. Edges are clamped to the source file.")]
    async fn trim_clip(&self, Parameters(a): Parameters<TrimClipArgs>) -> ToolResult {
        self.with_engine(move |e| e.trim_clip(a.track, a.clip, a.start_secs, a.end_secs)).await
    }

    #[tool(description = "Undo the last edit (made by the user or an agent). Returns what was undone.")]
    async fn undo(&self) -> ToolResult {
        self.with_engine(|e| e.undo().map(|l| json!({ "undone": l }))).await
    }

    #[tool(description = "Redo the last undone edit.")]
    async fn redo(&self) -> ToolResult {
        self.with_engine(|e| e.redo().map(|l| json!({ "redone": l }))).await
    }

    #[tool(description = "Edit history: the steps that can be undone and redone, most recent first.")]
    async fn get_history(&self) -> ToolResult {
        self.with_engine(|e| Ok(e.history_info())).await
    }

    #[tool(description = "Instruments available for instrument tracks: the built-in synth plus scanned VST3 instruments.")]
    async fn list_instruments(&self) -> ToolResult {
        self.with_engine(|e| {
            if e.scanned_plugins().is_empty() {
                e.scan_plugins();
            }
            Ok(e.available_instruments().into_iter().map(|(name, id)| json!({ "name": name, "instrument": id })).collect::<Vec<_>>())
        })
        .await
    }

    #[tool(description = "Create a software-instrument track that plays MIDI regions. Returns its index.")]
    async fn add_instrument_track(&self, Parameters(a): Parameters<InstrumentTrackArgs>) -> ToolResult {
        self.with_engine(move |e| e.add_instrument_track(&a.name, &a.instrument).map(|i| json!({ "track": i }))).await
    }

    #[tool(description = "Set or remove a track's instrument.")]
    async fn set_instrument(&self, Parameters(a): Parameters<SetInstrumentArgs>) -> ToolResult {
        self.with_engine(move |e| e.set_instrument(a.track, a.instrument.as_deref())).await
    }

    #[tool(description = "Add an empty MIDI region to an instrument track. Returns its index.")]
    async fn add_midi_clip(&self, Parameters(a): Parameters<AddMidiClipArgs>) -> ToolResult {
        self.with_engine(move |e| e.add_midi_clip(a.track, a.position_secs, a.length_secs, a.name.as_deref()).map(|i| json!({ "clip": i })))
            .await
    }

    #[tool(description = "Replace all notes in a MIDI region.")]
    async fn set_midi_notes(&self, Parameters(a): Parameters<MidiNotesArgs>) -> ToolResult {
        let notes: Vec<Note> = a.notes.iter().map(Note::from).collect();
        self.with_engine(move |e| e.set_midi_notes(a.track, a.clip, notes).map(|_| "ok")).await
    }

    #[tool(description = "Append notes to a MIDI region. Returns the new note count.")]
    async fn add_midi_notes(&self, Parameters(a): Parameters<MidiNotesArgs>) -> ToolResult {
        let notes: Vec<Note> = a.notes.iter().map(Note::from).collect();
        self.with_engine(move |e| e.add_midi_notes(a.track, a.clip, notes).map(|n| json!({ "note_count": n }))).await
    }

    #[tool(description = "Change a MIDI region: rename, move (optionally to another track), resize or mute.")]
    async fn update_midi_clip(&self, Parameters(a): Parameters<UpdateMidiClipArgs>) -> ToolResult {
        self.with_engine(move |e| {
            let mut clip = a.clip;
            let mut track = a.track;
            if let Some(to) = a.to_track.filter(|&to| to != a.track) {
                let pos = a.position_secs.unwrap_or(e.project().tracks.get(track).and_then(|t| t.midi_clips.get(clip)).map_or(0.0, |c| c.position_secs));
                e.move_midi_clip(track, clip, pos, Some(to))?;
                track = to;
                clip = e.project().tracks[to].midi_clips.len() - 1;
            }
            let u = MidiClipUpdate { name: a.name, position_secs: a.position_secs, length_secs: a.length_secs, mute: a.mute };
            e.update_midi_clip(track, clip, u).map(|c| json!({ "track": track, "clip": clip, "region": c }))
        })
        .await
    }

    #[tool(description = "Copy a MIDI region (with its notes) to another position or track.")]
    async fn copy_midi_clip(&self, Parameters(a): Parameters<CopyMidiClipArgs>) -> ToolResult {
        self.with_engine(move |e| e.copy_midi_clip(a.track, a.clip, a.position_secs, a.to_track).map(|(t, c)| json!({ "track": t, "clip": c })))
            .await
    }

    #[tool(description = "Delete a MIDI region.")]
    async fn remove_midi_clip(&self, Parameters(a): Parameters<ClipArgs>) -> ToolResult {
        self.with_engine(move |e| e.remove_midi_clip(a.track, a.clip)).await
    }

    #[tool(description = "List the instrument's parameters (ids, normalized values, display strings).")]
    async fn list_instrument_params(&self, Parameters(a): Parameters<TrackArgs>) -> ToolResult {
        self.with_engine(move |e| e.slot_params(a.track, SlotRef::Instrument)).await
    }

    #[tool(description = "Set an instrument parameter (normalized 0..1).")]
    async fn set_instrument_param(&self, Parameters(a): Parameters<InstrumentParamArgs>) -> ToolResult {
        self.with_engine(move |e| {
            e.set_slot_param(a.track, SlotRef::Instrument, a.param_id, a.value).map(|d| json!({ "value": a.value, "display": d }))
        })
        .await
    }

    #[tool(description = "Start playback from the current position.")]
    async fn play(&self) -> ToolResult {
        self.with_engine(|e| e.play().map(|_| "playing")).await
    }

    #[tool(description = "Stop playback/recording. If recording, returns the new clips created from the recorded audio.")]
    async fn stop(&self) -> ToolResult {
        self.with_engine(|e| e.stop().map(|clips| json!({ "recorded_clips": clips }))).await
    }

    #[tool(description = "Move the playhead (seconds).")]
    async fn seek(&self, Parameters(a): Parameters<SeekArgs>) -> ToolResult {
        self.with_engine(move |e| e.seek(a.position_secs).map(|_| "ok")).await
    }

    #[tool(description = "Start recording all armed tracks from the current position (also starts playback). Call stop to finish.")]
    async fn record(&self) -> ToolResult {
        self.with_engine(|e| e.record_audio().map(|files| json!({ "recording_to": files }))).await
    }

    #[tool(description = "Scan VST3 folders and list available plugins.")]
    async fn scan_plugins(&self, Parameters(a): Parameters<ScanArgs>) -> ToolResult {
        self.with_engine(move |e| {
            for p in a.extra_paths {
                e.add_plugin_search_path(p.into());
            }
            let (plugins, errors) = e.scan_plugins();
            Ok(json!({ "search_paths": e.plugin_search_paths(), "plugins": plugins, "errors": errors }))
        })
        .await
    }

    #[tool(description = "Insert an effect into a track's chain: \"eq\" for the built-in OpenTake EQ (low cut, low shelf, two bells, high shelf, high cut, output — see list_plugin_params), or a VST3 effect. Returns the slot index.")]
    async fn insert_plugin(&self, Parameters(a): Parameters<InsertPluginArgs>) -> ToolResult {
        self.with_engine(move |e| e.insert_plugin(a.track, &a.plugin, a.position).map(|s| json!({ "slot": s }))).await
    }

    #[tool(description = "Insert an effect on the master bus (\"eq\" or a VST3 effect). Returns the slot index; use master: true with list_plugin_params / set_plugin_param / set_plugin_bypass.")]
    async fn insert_master_plugin(&self, Parameters(a): Parameters<InsertMasterPluginArgs>) -> ToolResult {
        self.with_engine(move |e| e.insert_master_plugin(&a.plugin, a.position).map(|s| json!({ "slot": s }))).await
    }

    #[tool(description = "Remove an effect from the master bus.")]
    async fn remove_master_plugin(&self, Parameters(a): Parameters<MasterSlotArgs>) -> ToolResult {
        self.with_engine(move |e| e.remove_master_plugin(a.slot)).await
    }

    #[tool(description = "Remove a plugin from a track's effect chain.")]
    async fn remove_plugin(&self, Parameters(a): Parameters<SlotArgs>) -> ToolResult {
        self.with_engine(move |e| e.remove_plugin(a.track, a.slot)).await
    }

    #[tool(description = "Bypass or enable a plugin.")]
    async fn set_plugin_bypass(&self, Parameters(a): Parameters<BypassArgs>) -> ToolResult {
        let which = if a.master { SlotRef::Master(a.slot) } else { SlotRef::Effect(a.slot) };
        self.with_engine(move |e| e.set_slot_bypass(a.track, which, a.bypass).map(|_| "ok")).await
    }

    #[tool(description = "List a plugin's parameters with ids, normalized values and display strings.")]
    async fn list_plugin_params(&self, Parameters(a): Parameters<ListParamsArgs>) -> ToolResult {
        self.with_engine(move |e| {
            let which = if a.master { SlotRef::Master(a.slot) } else { SlotRef::Effect(a.slot) };
            let mut params = e.slot_params(a.track, which)?;
            if let Some(f) = a.filter.map(|f| f.to_lowercase()) {
                params.retain(|p| p.title.to_lowercase().contains(&f));
            }
            Ok(params)
        })
        .await
    }

    #[tool(description = "Set a plugin parameter (normalized 0..1). Returns the plugin's display string for the new value.")]
    async fn set_plugin_param(&self, Parameters(a): Parameters<SetParamArgs>) -> ToolResult {
        self.with_engine(move |e| {
            let which = if a.master { SlotRef::Master(a.slot) } else { SlotRef::Effect(a.slot) };
            e.set_slot_param(a.track, which, a.param_id, a.value).map(|d| json!({ "value": a.value, "display": d }))
        })
        .await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for DawServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("opentake", env!("CARGO_PKG_VERSION")).with_title("OpenTake"))
            .with_instructions(INSTRUCTIONS)
    }
}
