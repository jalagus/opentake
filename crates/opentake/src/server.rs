//! MCP tool surface over the engine.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use opentake_engine::project::{AutomationMode, AutomationPoint, AutomationTarget, Curve, Note};
use opentake_engine::{AudioConfig, Engine, MidiClipUpdate, SendUpdate, SlotRef, TrackUpdate};
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
monitor_input) -> record -> stop (recordings become clips) -> save_project. With the cycle on \
(set_loop), recording loops and each pass becomes a take in one take folder region; select_take \
switches which take plays.

Tracks, clips and plugin slots are addressed by zero-based index as shown by get_project. \
Plugin parameters are normalized 0..1; list_plugin_params shows ids, current values and display strings. \
Projects can be saved/loaded as native .otp, Reaper .rpp or .dawproject files; Ableton .als, Logic (via .fcpxml) and Ardour sessions can be imported.

MIDI: add_instrument_track (\"synth\" = built-in synth, or a VST3 / AU instrument from list_instruments) -> \
add_midi_clip -> set_midi_notes / add_midi_notes (times in seconds relative to the region start, \
pitch 60 = middle C, velocity 1..127). Every edit — yours or the user's — can be undone with undo/redo; \
get_history lists the steps. Songs: import_guitar_pro (.gp) and import_midi_file (.mid) create \
instrument tracks; export_midi_file writes the MIDI regions back out.

Tempo: set_tempo sets the opening tempo/signature; set_tempo_map adds tempo and time-signature changes. \
Region and note times are always seconds; get_tempo_map and convert_time translate between seconds and \
bars/beats. Effects: insert_plugin with \"eq\", \"comp\", \"multiband\", \"limiter\", \"delay\" or \"reverb\" adds a built-in effect to a track (a reverb usually goes on a bus at 100% wet); \
insert_master_plugin puts effects on the master bus (address them with master: true); VST3 and, on macOS, Audio Unit plug-ins work too (scan_plugins lists them). Plug-in delay \
(e.g. the limiter's look-ahead) is compensated automatically. Buses: add_bus (with tracks to group them) \
creates a bus track; update_track output routes a track to a bus index or \"master\"; effects on the bus \
(insert_plugin with its index) process the whole group. Sends feed buses besides the output, at their own \
level, pre- or post-fader: add_send (without bus it makes a \"Reverb\" bus with the built-in reverb), \
update_send, remove_send. get_project shows each track's kind, output and sends.

Regions: set_clip_fades gives an audio region fade-in/out curves; overlapping regions on a track \
crossfade automatically, and crossfade_clips makes a short crossfade between two neighbours. \
merge_clips joins regions into one new audio file without clicks at the joins.

Automation: list_automation_params shows what a track (or master: true) can automate — volume, \
pan, mute, send levels and every plug-in parameter; add_automation_lane makes a lane, set_automation_points \
draws it (times in seconds; values normalized 0..1, or db for volume and sends, pan -1..1), \
get_automation reads lanes back. Lanes play in mode \"read\" and are shown under the track in the \
GUI; analyze_mix and export_audio render them.

Mixing: analyze_mix renders the mix offline and reports peak, LUFS, band energy and phone-speaker \
translation per track and for the master — balance with update_track (volume_db, pan) and EQs, \
re-analyze, repeat; keep the master peak below -1 dBFS.";

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
pub struct GetProjectArgs {
    /// Include every MIDI note (can be very large); otherwise regions show note counts.
    /// Use get_midi_notes for one region's notes.
    #[serde(default)]
    pub include_notes: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetMidiNotesArgs {
    pub track: usize,
    /// Zero-based MIDI region index on the track.
    pub clip: usize,
    /// Only notes starting at or after this time (seconds, relative to the region start).
    pub from_secs: Option<f64>,
    /// Only notes starting before this time (seconds, relative to the region start).
    pub to_secs: Option<f64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExportAudioArgs {
    /// Where to write the full mix (the extension is added when missing). Omit to skip it.
    pub mix_path: Option<String>,
    /// Folder for one file per audible track ("01 Drums.wav", ...). Omit to skip stems.
    pub stems_dir: Option<String>,
    /// "wav" (default) or "flac".
    pub format: Option<String>,
    /// 16 (dithered), 24 (default) or 32 (float, WAV only).
    pub bit_depth: Option<u16>,
    /// 44100, 48000 (default), 88200, 96000, 176400 or 192000.
    pub sample_rate: Option<u32>,
    /// Start of the range (seconds); with to_secs. Omit both for the whole song.
    pub from_secs: Option<f64>,
    pub to_secs: Option<f64>,
    /// Export the cycle range instead of the whole song.
    #[serde(default)]
    pub cycle: bool,
    /// Extra seconds after the end for delay repeats and other tails (default 1).
    pub tail_secs: Option<f64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AnalyzeMixArgs {
    /// Start of the range to analyze (seconds); default the song start.
    pub from_secs: Option<f64>,
    /// End of the range (seconds); default the end of the last region.
    pub to_secs: Option<f64>,
}

/// The project as JSON for agents: MIDI notes replaced by counts (unless asked for) and
/// tablature bars summarized, so the answer stays small.
fn project_json(project: &opentake_engine::project::Project, include_notes: bool) -> serde_json::Value {
    let mut v = serde_json::to_value(project).unwrap_or_default();
    compact_lanes(v.get_mut("master_automation"));
    if let Some(tracks) = v.get_mut("tracks").and_then(|t| t.as_array_mut()) {
        for (i, t) in tracks.iter_mut().enumerate() {
            compact_track(t, include_notes);
            t["index"] = json!(i);
            // Kind and output by index (the model stores buses by session id).
            let track = &project.tracks[i];
            t["kind"] = json!(if track.bus { "bus" } else if track.is_instrument() { "instrument" } else { "audio" });
            t["output"] = project.output_of(i).map_or(json!("master"), |b| json!(b));
            t.as_object_mut().map(|o| o.remove("bus"));
            if !track.sends.is_empty() {
                t["sends"] = json!((0..track.sends.len()).map(|k| send_json(project, i, k)).collect::<Vec<_>>());
            }
        }
    }
    v
}

/// A send as agents see it: bus by index, level in dB.
fn send_json(project: &opentake_engine::project::Project, track: usize, k: usize) -> serde_json::Value {
    let s = &project.tracks[track].sends[k];
    let db = if s.level > 0.0 { ((20.0 * s.level.log10()) * 10.0).round() / 10.0 } else { -144.0 };
    json!({ "send": k, "bus": project.send_dest(track, k), "level_db": db, "pre_fader": s.pre_fader, "mute": s.mute })
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AddSendArgs {
    pub track: usize,
    /// Bus track index. Leave out to create a new bus for it (new_bus_name, default "Reverb").
    pub bus: Option<usize>,
    /// Send level in dB (default 0; max +6).
    pub level_db: Option<f32>,
    /// Tap before the fader and pan (default: after).
    #[serde(default)]
    pub pre_fader: bool,
    /// Name of the new bus when bus is left out.
    pub new_bus_name: Option<String>,
    /// Built-in effect for the new bus (default "reverb"; "none" for no effect).
    pub new_bus_effect: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdateSendArgs {
    pub track: usize,
    /// Send index on the track (as in get_project).
    pub send: usize,
    /// Another bus track index.
    pub bus: Option<usize>,
    pub level_db: Option<f32>,
    pub pre_fader: Option<bool>,
    pub mute: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SendArgs {
    pub track: usize,
    pub send: usize,
}

/// A track output: a bus track's index or "master".
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum OutputArg {
    Bus(usize),
    Named(String),
}

impl OutputArg {
    fn bus(&self) -> anyhow::Result<Option<usize>> {
        match self {
            OutputArg::Bus(i) => Ok(Some(*i)),
            OutputArg::Named(s) if s.eq_ignore_ascii_case("master") => Ok(None),
            OutputArg::Named(s) => s.trim().parse().map(Some).map_err(|_| anyhow::anyhow!("output must be a bus track index or \"master\", not '{s}'")),
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AddBusArgs {
    pub name: String,
    /// Tracks to route into the new bus (it then feeds where the first of them went).
    pub tracks: Option<Vec<usize>>,
}

/// Automation lanes say how many points they have (get_automation lists them).
fn compact_lanes(lanes: Option<&mut serde_json::Value>) {
    for l in lanes.and_then(|a| a.as_array_mut()).into_iter().flatten() {
        let count = l.get("points").and_then(|n| n.as_array()).map_or(0, |n| n.len());
        l.as_object_mut().map(|o| o.remove("points"));
        l["point_count"] = json!(count);
    }
}

fn compact_track(t: &mut serde_json::Value, include_notes: bool) {
    compact_lanes(t.get_mut("automation"));
    // Take folders always say which take plays (serde leaves out 0).
    for c in t.get_mut("clips").and_then(|c| c.as_array_mut()).into_iter().flatten() {
        if c.get("takes").is_some() && c.get("active_take").is_none() {
            c["active_take"] = json!(0);
        }
    }
    if let Some(clips) = t.get_mut("midi_clips").and_then(|c| c.as_array_mut()) {
        for c in clips {
            if !include_notes {
                let count = c.get("notes").and_then(|n| n.as_array()).map_or(0, |n| n.len());
                c.as_object_mut().map(|o| o.remove("notes"));
                c["note_count"] = json!(count);
            }
            if let Some(tab) = c.get_mut("tab").filter(|t| t.is_object()) {
                let bars = tab.get("bars").and_then(|b| b.as_array()).map_or(0, |b| b.len());
                tab.as_object_mut().map(|o| o.remove("bars"));
                tab["bar_count"] = json!(bars);
            }
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NameArgs {
    pub name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PathArgs {
    /// File path. `.rpp` = Reaper, `.dawproject`/`.als`/`.fcpxml`/`.ardour` = import, anything else = native JSON.
    pub path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct OptionalPathArgs {
    /// File path (`.rpp` for Reaper, `.dawproject`, otherwise native). Omit to save to the current path.
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
    /// "eq" for the built-in EQ, or a VST3 / AU effect name, .vst3 bundle path or AU id ("au:...").
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
pub struct MeasureLatencyArgs {
    /// Zero-based input channel that hears the output (default 0).
    #[serde(default)]
    pub input_channel: usize,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RecordOffsetArgs {
    /// Milliseconds added to the measured round trip (-500..500). Positive moves new
    /// recordings earlier, negative later.
    pub offset_ms: f64,
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
    /// Instrument for pitched tracks: "synth" (default) or a VST3 / AU instrument (see list_instruments).
    pub instrument: Option<String>,
    /// Instrument for drum tracks (GM drum map): "drums" (built-in kit, default), "synth" or a VST3 / AU instrument.
    pub drum_instrument: Option<String>,
    /// Adopt the song's opening tempo and time signature (default true).
    pub set_tempo: Option<bool>,
    /// Timeline position for the song start in seconds (default 0).
    pub position_secs: Option<f64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MidiFilePathArgs {
    /// Path to a Standard MIDI File (.mid, .midi).
    pub path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MidiFileImportArgs {
    /// Path to a Standard MIDI File (.mid, .midi).
    pub path: String,
    /// Part indices to import (see inspect_midi_file); omit for all.
    pub tracks: Option<Vec<usize>>,
    /// Instrument for pitched parts: "synth" (default) or a VST3 instrument (see list_instruments).
    pub instrument: Option<String>,
    /// Instrument for drum parts (channel 10): "drums" (built-in kit, default), "synth" or a VST3 instrument.
    pub drum_instrument: Option<String>,
    /// Adopt the file's tempo map and time signatures (default true).
    pub set_tempo: Option<bool>,
    /// Timeline position for the file start in seconds (default 0).
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
    /// Where the track goes: a bus track's index, or "master".
    pub output: Option<OutputArg>,
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
pub struct EnabledArgs {
    pub enabled: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MoveTrackArgs {
    pub track: usize,
    /// Its new index (past the end = last).
    pub to_index: usize,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ClipGainArgs {
    pub track: usize,
    pub clip: usize,
    pub gain_db: f32,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ClipFadesArgs {
    pub track: usize,
    pub clip: usize,
    /// Fade-in length in seconds (0 = none); omit to keep.
    pub fade_in_secs: Option<f64>,
    /// Fade-out length in seconds (0 = none); omit to keep.
    pub fade_out_secs: Option<f64>,
    /// Fade-in shape: "linear", "equal_power", "exponential", "logarithmic" or "s_curve"; omit to keep.
    pub curve_in: Option<String>,
    /// Fade-out shape (same names); omit to keep.
    pub curve_out: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CrossfadeArgs {
    pub track: usize,
    /// The two regions (indices on the track) to crossfade, in either order.
    pub clip_a: usize,
    pub clip_b: usize,
    /// Crossfade length in seconds (default 0.02).
    pub length_secs: Option<f64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MergeClipsArgs {
    pub track: usize,
    /// Indices of the audio regions on the track to merge (two or more).
    pub clips: Vec<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NormalizeClipArgs {
    pub track: usize,
    pub clip: usize,
    /// Peak level to reach in dBFS (default 0, max 0).
    pub target_db: Option<f32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TakeArgs {
    pub track: usize,
    pub clip: usize,
    /// Zero-based index into the region's `takes`.
    pub take: usize,
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
pub struct PitchStretchArgs {
    pub track: usize,
    pub clip: usize,
    /// Pitch shift in semitones (-24..24); omit to keep.
    pub semitones: Option<i32>,
    /// Fine pitch in cents (-100..100); omit to keep.
    pub cents: Option<i32>,
    /// Time stretch ratio, new length / original length (0.25..4; 2 = half speed, 1 = off). The region's length scales with it.
    pub stretch: Option<f64>,
    /// Instead of stretch: fit the region to this length in seconds.
    pub length_secs: Option<f64>,
    /// Instead of stretch: fit the region to this many bars (at the tempo where it starts).
    pub length_bars: Option<f64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct InstrumentTrackArgs {
    pub name: String,
    /// "synth" for the built-in synth, "drums" for the built-in GM drum kit, or a VST3 / AU instrument name, bundle path or AU id (see list_instruments).
    pub instrument: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetInstrumentArgs {
    pub track: usize,
    /// "synth", "drums", a VST3 / AU instrument name, bundle path or AU id, or omit to remove the instrument.
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
    /// Plugin name (as listed by scan_plugins, partial match ok), path to a .vst3 bundle, or an AU id ("au:aufx:dely:appl"); "AU: Name" picks the Audio Unit when a VST3 has the same name.
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

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AutomationOwnerArgs {
    #[serde(default)]
    pub track: usize,
    /// The master bus's automation instead of a track's (track is ignored).
    #[serde(default)]
    pub master: bool,
}

/// What a lane automates.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct AutomationTargetArgs {
    /// "volume", "pan", "mute", "send" (tracks only) or "plugin".
    pub parameter: String,
    /// For "send": the send index (as in get_project).
    pub send: Option<usize>,
    /// For "plugin": the effect slot (as in get_project; with master: true, the master's).
    pub slot: Option<usize>,
    /// For "plugin": the track's instrument instead of an effect slot.
    #[serde(default)]
    pub instrument: bool,
    /// For "plugin": the parameter id (see list_automation_params / list_plugin_params).
    pub param_id: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AddLaneArgs {
    #[serde(flatten)]
    pub owner: AutomationOwnerArgs,
    #[serde(flatten)]
    pub target: AutomationTargetArgs,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LaneArgs {
    #[serde(flatten)]
    pub owner: AutomationOwnerArgs,
    /// Lane index (see get_automation).
    pub lane: usize,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PointArg {
    /// Timeline position (seconds).
    pub time: f64,
    /// Normalized 0..1 (volume: fader travel; pan: 0 left, 0.5 centre, 1 right; mute: 1 muted).
    pub value: Option<f32>,
    /// Volume and send lanes: the level in dB instead of value.
    pub db: Option<f32>,
    /// Pan lanes: -1 (left) .. 1 (right) instead of value.
    pub pan: Option<f32>,
    /// Shape of the segment to the next point: "linear" (default), "hold" (step), "smooth", "fast_start", "fast_end".
    pub curve: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetPointsArgs {
    #[serde(flatten)]
    pub owner: AutomationOwnerArgs,
    pub lane: usize,
    pub points: Vec<PointArg>,
    /// Replace the lane's points (default) or add to them.
    #[serde(default = "yes")]
    pub replace: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ClearAutomationArgs {
    #[serde(flatten)]
    pub owner: AutomationOwnerArgs,
    pub lane: usize,
    /// Only clear from/to these times (seconds); everything by default.
    pub from: Option<f64>,
    pub to: Option<f64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LaneSettingsArgs {
    #[serde(flatten)]
    pub owner: AutomationOwnerArgs,
    pub lane: usize,
    /// Show the lane under its track in the GUI (also opens the track's automation view).
    pub shown: Option<bool>,
    /// "read" (play it), "off", or "touch"/"latch" (record GUI control moves while playing).
    pub mode: Option<String>,
}

impl AutomationOwnerArgs {
    fn owner(&self) -> Option<usize> {
        (!self.master).then_some(self.track)
    }
}

fn parse_name<T: serde::de::DeserializeOwned>(what: &str, s: &str) -> anyhow::Result<T> {
    serde_json::from_value(json!(s.trim().to_lowercase())).map_err(|_| anyhow::anyhow!("unknown {what} '{s}'"))
}

/// A target as agents name it.
fn target_json(e: &Engine, owner: Option<usize>, t: AutomationTarget) -> serde_json::Value {
    match (t, e.target_slot(owner, t)) {
        (AutomationTarget::Param { param, .. }, Some(SlotRef::Instrument)) => json!({ "parameter": "plugin", "instrument": true, "param_id": param }),
        (AutomationTarget::Param { param, .. }, Some(SlotRef::Effect(s) | SlotRef::Master(s))) => json!({ "parameter": "plugin", "slot": s, "param_id": param }),
        (AutomationTarget::Param { param, .. }, None) => json!({ "parameter": "plugin", "param_id": param }),
        (AutomationTarget::Send { send }, _) => json!({ "parameter": "send", "send": owner.and_then(|t| e.project().tracks.get(t)?.sends.iter().position(|s| s.id == send)) }),
        (other, _) => json!({ "parameter": format!("{other:?}").to_lowercase() }),
    }
}

fn resolve_target(e: &Engine, owner: Option<usize>, a: &AutomationTargetArgs) -> anyhow::Result<AutomationTarget> {
    Ok(match a.parameter.trim().to_lowercase().as_str() {
        "volume" => AutomationTarget::Volume,
        "pan" => AutomationTarget::Pan,
        "mute" => AutomationTarget::Mute,
        "send" => {
            let (Some(t), Some(k)) = (owner, a.send) else { anyhow::bail!("give the track's send index") };
            let s = e.project().tracks.get(t).and_then(|t| t.sends.get(k)).ok_or_else(|| anyhow::anyhow!("track {t} has no send {k}"))?;
            AutomationTarget::Send { send: s.id }
        }
        "plugin" | "param" | "parameter" => {
            let param = a.param_id.ok_or_else(|| anyhow::anyhow!("give param_id for a plug-in parameter"))?;
            let which = match (owner, a.instrument, a.slot) {
                (Some(_), true, _) => SlotRef::Instrument,
                (None, _, Some(s)) => SlotRef::Master(s),
                (Some(_), false, Some(s)) => SlotRef::Effect(s),
                _ => anyhow::bail!("give the plug-in's slot (or instrument: true)"),
            };
            e.param_target(owner, which, param)?
        }
        other => anyhow::bail!("parameter must be volume, pan, mute, send or plugin, not '{other}'"),
    })
}

fn lane_json(e: &Engine, owner: Option<usize>, i: usize) -> anyhow::Result<serde_json::Value> {
    let l = e.automation(owner)?.get(i).ok_or_else(|| anyhow::anyhow!("no lane {i}"))?;
    let points: Vec<_> = l
        .points
        .iter()
        .map(|p| {
            let mut v = json!({ "time": (p.time_secs * 1e4).round() / 1e4, "value": (p.value * 1e4).round() / 1e4, "display": e.format_automation_value(owner, l.target, p.value) });
            if p.curve != Curve::Linear {
                v["curve"] = serde_json::to_value(p.curve).unwrap_or_default();
            }
            v
        })
        .collect();
    Ok(json!({ "lane": i, "name": e.target_name(owner, l.target), "target": target_json(e, owner, l.target), "mode": l.mode, "shown": l.shown, "points": points }))
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

    #[tool(description = "The project: tempo map, tracks (with index), regions and plugin chains. MIDI regions show note counts; pass include_notes for every note, or use get_midi_notes for one region.")]
    async fn get_project(&self, Parameters(a): Parameters<GetProjectArgs>) -> ToolResult {
        self.with_engine(move |e| {
            Ok(json!({ "path": e.project_path(), "media_dir": e.media_dir(), "project": project_json(e.project(), a.include_notes) }))
        })
        .await
    }

    #[tool(description = "The notes of one MIDI region (times in seconds relative to the region start), optionally only a time range.")]
    async fn get_midi_notes(&self, Parameters(a): Parameters<GetMidiNotesArgs>) -> ToolResult {
        self.with_engine(move |e| {
            let track = e.project().tracks.get(a.track).ok_or_else(|| anyhow::anyhow!("no track {}", a.track))?;
            let clip = track.midi_clips.get(a.clip).ok_or_else(|| anyhow::anyhow!("track {} has no MIDI region {}", a.track, a.clip))?;
            let (from, to) = (a.from_secs.unwrap_or(f64::NEG_INFINITY), a.to_secs.unwrap_or(f64::INFINITY));
            let notes: Vec<_> = clip.notes.iter().filter(|n| n.start_secs >= from && n.start_secs < to).collect();
            Ok(json!({ "region": clip.name, "position_secs": clip.position_secs, "length_secs": clip.length_secs, "count": notes.len(), "notes": notes }))
        })
        .await
    }

    #[tool(description = "Export audio: the full mix and/or one stem per audible track, as WAV (16-bit dithered, 24-bit, 32-bit float) or FLAC (16/24-bit), at a chosen sample rate, for the whole song, the cycle or a time range, with a tail for delay repeats. Rendered offline (faster than real time) with all instruments, effects, faders and master processing. Returns the files with their peak, loudness and any clipped samples.")]
    async fn export_audio(&self, Parameters(a): Parameters<ExportAudioArgs>) -> ToolResult {
        let format = match a.format.as_deref().map(str::to_lowercase).as_deref() {
            None | Some("wav") => opentake_engine::AudioFormat::Wav,
            Some("flac") => opentake_engine::AudioFormat::Flac,
            Some(other) => return Err(format!("unknown format '{other}' (use wav or flac)")),
        };
        let engine = self.engine.clone();
        let result = tokio::task::spawn_blocking(move || {
            let (job, options) = {
                let mut e = engine.lock().map_err(|_| anyhow::anyhow!("engine lock poisoned"))?;
                let p = e.project();
                let range = if a.cycle {
                    anyhow::ensure!(p.loop_end_secs > p.loop_start_secs, "there's no cycle range set");
                    Some((p.loop_start_secs, p.loop_end_secs))
                } else {
                    match (a.from_secs, a.to_secs) {
                        (Some(f), Some(t)) => Some((f, t)),
                        (None, None) => None,
                        _ => anyhow::bail!("give both from_secs and to_secs (or neither)"),
                    }
                };
                let options = opentake_engine::ExportOptions {
                    format,
                    bit_depth: a.bit_depth.unwrap_or(24),
                    sample_rate: a.sample_rate.unwrap_or(48_000),
                    mix_path: a.mix_path.map(PathBuf::from),
                    stems_dir: a.stems_dir.map(PathBuf::from),
                    range,
                    tail_secs: a.tail_secs.unwrap_or(1.0),
                };
                (e.export_job(&options)?, options)
            };
            // Render and write without holding the engine.
            job.export(&options)
        })
        .await
        .map_err(|e| format!("export failed: {e}"))?;
        let report = result.map_err(|e| format!("{e:#}"))?;
        serde_json::to_string_pretty(&report).map_err(|e| e.to_string())
    }

    #[tool(description = "Render the mix offline and measure it: per track and for the master — peak (dBFS), integrated loudness (LUFS, BS.1770), energy per frequency band, and loudness through a simulated phone speaker. Use it to balance levels/EQ by numbers: similar phone_drop across tracks means the balance holds on small speakers; keep the master peak below -1 dBFS (clipped_samples 0). Built-in instruments and effects, VST3 and AU plug-ins are rendered; plug-ins that fail to load are listed in warnings.")]
    async fn analyze_mix(&self, Parameters(a): Parameters<AnalyzeMixArgs>) -> ToolResult {
        // Snapshot under the engine lock, then render without holding it.
        let engine = self.engine.clone();
        let result = tokio::task::spawn_blocking(move || {
            let job = engine.lock().map_err(|_| anyhow::anyhow!("engine lock poisoned"))?.mix_job();
            job.analyze(a.from_secs, a.to_secs)
        })
        .await
        .map_err(|e| format!("analysis failed: {e}"))?;
        let report = result.map_err(|e| format!("{e:#}"))?;
        serde_json::to_string_pretty(&report).map_err(|e| e.to_string())
    }

    #[tool(description = "Start a new project with one empty audio track.")]
    async fn new_project(&self, Parameters(a): Parameters<NameArgs>) -> ToolResult {
        self.with_engine(move |e| e.new_project(&a.name).map(|_| "created")).await
    }

    #[tool(description = "Load a project: native .otp/.json, Reaper .rpp, or import from another DAW — .dawproject (Bitwig, Studio One, Cubase), Ableton .als, .fcpxml (Logic Pro via File > Export > Project to Final Cut Pro XML), Ardour .ardour. Returns warnings for missing media/plugins and anything the import couldn't carry over. Imported projects have no path until saved.")]
    async fn load_project(&self, Parameters(a): Parameters<PathArgs>) -> ToolResult {
        self.with_engine(move |e| e.load_project(&PathBuf::from(a.path)).map(|w| json!({ "warnings": w }))).await
    }

    #[tool(description = "Save the project. Use .otp (OpenTake project; added when no extension is given), .rpp for a Reaper project, or .dawproject to open it in Bitwig/Studio One/Cubase (audio embedded, MIDI included). Plugin parameter values and built-in plug-ins are only preserved in .otp.")]
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

    #[tool(description = "Measure the true recording latency: plays test chirps (about 2 s, replacing the mix; transport stops) and finds them on an input that hears the output — a cable from an output to that input, or speakers near a microphone. Sets the recording offset for this device setup to the difference from what the device reports. Ask the user to set up the loopback first.")]
    async fn measure_latency(&self, Parameters(a): Parameters<MeasureLatencyArgs>) -> ToolResult {
        self.with_engine(move |e| e.measure_latency(a.input_channel)).await
    }

    #[tool(description = "Recording latency correction for the current device setup (remembered per setup). New recordings are moved earlier by the interface round trip the device reports (get_status io_latency_ms) plus this offset, so they line up with what the performer heard. measure_latency sets it automatically.")]
    async fn set_record_offset(&self, Parameters(a): Parameters<RecordOffsetArgs>) -> ToolResult {
        self.with_engine(move |e| e.set_record_offset(a.offset_ms).map(|_| "ok")).await
    }

    #[tool(description = "Auto input monitoring (on by default): record-armed tracks monitor their input while stopped and while recording, and play back their regions during plain playback. Turn it off when the audio interface does its own direct monitoring; the I (monitor_input) button always monitors.")]
    async fn set_auto_input_monitoring(&self, Parameters(a): Parameters<EnabledArgs>) -> ToolResult {
        self.with_engine(move |e| e.set_auto_input_monitoring(a.enabled).map(|_| "ok")).await
    }

    #[tool(description = "Set the master output level in dB.")]
    async fn set_master_volume(&self, Parameters(a): Parameters<MasterVolumeArgs>) -> ToolResult {
        self.with_engine(move |e| e.set_master_volume(db_to_gain(a.volume_db)).map(|_| "ok")).await
    }

    #[tool(description = "Metronome: turn the click on/off and/or set its level. It follows tempo and time signature and plays while the transport runs.")]
    async fn set_metronome(&self, Parameters(a): Parameters<MetronomeArgs>) -> ToolResult {
        self.with_engine(move |e| e.set_metronome(a.enabled, a.volume_db.map(db_to_gain)).map(|_| "ok")).await
    }

    #[tool(description = "Cycle (loop) playback: set the loop range in seconds and/or turn it on or off. Playback wraps from the end back to the start; recording does too, making each pass a take.")]
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

    #[tool(description = "Read a Standard MIDI File (.mid) and list its parts (one per track, or per channel where a track mixes channels) with channel, GM program and note count, plus tempo and length, without importing.")]
    async fn inspect_midi_file(&self, Parameters(a): Parameters<MidiFilePathArgs>) -> ToolResult {
        let path = PathBuf::from(a.path);
        self.with_engine(move |_| opentake_engine::inspect_midi_file(&path)).await
    }

    #[tool(description = "Import a Standard MIDI File (format 0/1): each part becomes an instrument track with a MIDI region, timed through the file's tempo map; channel 10 gets the drum kit. One undo step. Returns the new track indices.")]
    async fn import_midi_file(&self, Parameters(a): Parameters<MidiFileImportArgs>) -> ToolResult {
        let path = PathBuf::from(&a.path);
        let options = opentake_engine::MidiFileImport {
            tracks: a.tracks,
            instrument: a.instrument.unwrap_or_else(|| "synth".into()),
            drum_instrument: a.drum_instrument,
            set_tempo: a.set_tempo.unwrap_or(true),
            position_secs: a.position_secs.unwrap_or(0.0),
        };
        self.with_engine(move |e| e.import_midi_file(&path, &options).map(|t| json!({ "tracks": t }))).await
    }

    #[tool(description = "Export the project's MIDI regions as a Standard MIDI File (format 1, with the tempo map; one track per instrument track, drum kits on channel 10). Muted regions are left out.")]
    async fn export_midi_file(&self, Parameters(a): Parameters<MidiFilePathArgs>) -> ToolResult {
        let path = PathBuf::from(a.path);
        self.with_engine(move |e| e.export_midi_file(&path).map(|n| json!({ "tracks": n }))).await
    }

    #[tool(description = "Add a track. Returns its index.")]
    async fn add_track(&self, Parameters(a): Parameters<NameArgs>) -> ToolResult {
        self.with_engine(move |e| e.add_track(&a.name).map(|i| json!({ "track": i }))).await
    }

    #[tool(description = "Move a track to another position in the track list (indices of the tracks in between shift). Undoable.")]
    async fn move_track(&self, Parameters(a): Parameters<MoveTrackArgs>) -> ToolResult {
        self.with_engine(move |e| e.move_track(a.track, a.to_index).map(|_| "ok")).await
    }

    #[tool(description = "Remove a track (indices of later tracks shift down).")]
    async fn remove_track(&self, Parameters(a): Parameters<TrackArgs>) -> ToolResult {
        self.with_engine(move |e| e.remove_track(a.track)).await
    }

    #[tool(description = "Add a bus (aux) track: it sums the tracks routed to it (pass tracks, or set their output with update_track), runs them through its own effects (insert_plugin with the bus's index) and fader, and feeds the master or another bus. Returns its index.")]
    async fn add_bus(&self, Parameters(a): Parameters<AddBusArgs>) -> ToolResult {
        self.with_engine(move |e| match &a.tracks {
            Some(tracks) => e.create_bus_from(tracks, &a.name),
            None => e.add_bus(&a.name),
        }
        .map(|i| json!({ "track": i })))
        .await
    }

    #[tool(description = "Add a send from a track (or bus) to a bus, e.g. several tracks into one reverb bus. Without bus, creates a new bus (default \"Reverb\" with the built-in reverb). Returns the send and bus indices. Undoable.")]
    async fn add_send(&self, Parameters(a): Parameters<AddSendArgs>) -> ToolResult {
        self.with_engine(move |e| {
            let level = db_to_gain(a.level_db.unwrap_or(0.0));
            let (bus, send) = match a.bus {
                Some(bus) => (bus, e.add_send(a.track, bus, level, a.pre_fader)?),
                None => {
                    let effect = a.new_bus_effect.unwrap_or_else(|| "reverb".into());
                    let effect = (effect != "none").then_some(effect.as_str());
                    e.send_to_new_bus(a.track, a.new_bus_name.as_deref().unwrap_or("Reverb"), effect, level, a.pre_fader)?
                }
            };
            Ok(json!({ "track": a.track, "send": send, "bus": bus, "bus_effects": e.project().tracks[bus].plugins.len() }))
        })
        .await
    }

    #[tool(description = "Change a send: bus, level (dB), pre/post-fader, mute. Undoable.")]
    async fn update_send(&self, Parameters(a): Parameters<UpdateSendArgs>) -> ToolResult {
        let update = SendUpdate { bus: a.bus, level: a.level_db.map(db_to_gain), pre_fader: a.pre_fader, mute: a.mute };
        self.with_engine(move |e| {
            e.update_send(a.track, a.send, update)?;
            Ok(send_json(e.project(), a.track, a.send))
        })
        .await
    }

    #[tool(description = "Remove a send (later sends' indices shift down). Undoable.")]
    async fn remove_send(&self, Parameters(a): Parameters<SendArgs>) -> ToolResult {
        self.with_engine(move |e| e.remove_send(a.track, a.send).map(|_| "ok")).await
    }

    #[tool(description = "Change track settings: name, volume (dB), pan, mute, solo, record arm, input monitoring, input channel, output (a bus track index or \"master\").")]
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
        self.with_engine(move |e| {
            if let Some(output) = &a.output {
                e.set_track_output(a.track, output.bus()?)?;
            }
            let track = e.update_track(a.track, update)?;
            // The settings, not every region and note.
            Ok(json!({
                "output": e.project().output_of(a.track).map_or(json!("master"), |b| json!(b)),
                "track": a.track,
                "name": track.name,
                "volume_db": 20.0 * track.volume.max(1e-6).log10(),
                "pan": track.pan,
                "mute": track.mute,
                "solo": track.solo,
                "armed": track.armed,
                "monitor_input": track.monitor_input,
                "input_channel": track.input_channel,
            }))
        })
        .await
    }

    #[tool(description = "Place a WAV file on a track at a position (seconds).")]
    async fn add_clip(&self, Parameters(a): Parameters<AddClipArgs>) -> ToolResult {
        self.with_engine(move |e| e.add_clip(a.track, &PathBuf::from(a.file), a.position_secs)).await
    }

    #[tool(description = "Choose which take of a take folder (a region with `takes`, from cycle recording) plays. Undoable.")]
    async fn select_take(&self, Parameters(a): Parameters<TakeArgs>) -> ToolResult {
        self.with_engine(move |e| e.select_take(a.track, a.clip, a.take)).await
    }

    #[tool(description = "Delete a take from a take folder (the audio file stays on disk). A folder left with one take becomes a plain region. Undoable.")]
    async fn delete_take(&self, Parameters(a): Parameters<TakeArgs>) -> ToolResult {
        self.with_engine(move |e| e.remove_take(a.track, a.clip, a.take)).await
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

    #[tool(description = "Set an audio region's gain in dB (before the track fader; 0 = unchanged, -60..+24). Undoable.")]
    async fn set_clip_gain(&self, Parameters(a): Parameters<ClipGainArgs>) -> ToolResult {
        self.with_engine(move |e| e.set_clip_gain(a.track, a.clip, a.gain_db)).await
    }

    #[tool(description = "Normalize an audio region: set its gain so its loudest sample (after pitch/stretch) reaches target_db dBFS (default 0). Returns the peak found and the gain set. Undoable.")]
    async fn normalize_clip(&self, Parameters(a): Parameters<NormalizeClipArgs>) -> ToolResult {
        self.with_engine(move |e| {
            let peak = e.clip_peak(a.track, a.clip)?;
            let c = e.normalize_clip(a.track, a.clip, a.target_db.unwrap_or(0.0))?;
            let db = |v: f32| (20.0 * v.log10() * 100.0).round() / 100.0;
            Ok(json!({ "peak_db": db(peak), "gain_db": db(c.gain) }))
        })
        .await
    }

    #[tool(description = "Pitch-shift and/or time-stretch an audio region (independently; formants aren't preserved). Stretch by a ratio, or fit the region to a length in seconds or bars (e.g. to match a loop to the tempo). The source offset stays in source seconds. Long regions take a moment to render. Undoable.")]
    async fn set_clip_pitch_stretch(&self, Parameters(a): Parameters<PitchStretchArgs>) -> ToolResult {
        let engine = self.engine.clone();
        let result = tokio::task::spawn_blocking(move || {
            let lock = || engine.lock().map_err(|_| anyhow::anyhow!("engine lock poisoned"));
            let (ps, job) = {
                let mut e = lock()?;
                let clip = e.project().tracks.get(a.track).and_then(|t| t.clips.get(a.clip)).ok_or_else(|| anyhow::anyhow!("no clip {} on track {}", a.clip, a.track))?;
                let length_secs = match a.length_bars {
                    Some(bars) => {
                        let map = e.project().tempo_map();
                        let q0 = map.quarters_at(clip.position_secs);
                        Some(map.secs_at(q0 + bars * opentake_engine::project::tempo::bar_len_q(map.signature_at(clip.position_secs))) - clip.position_secs)
                    }
                    None => a.length_secs,
                };
                let ps = opentake_engine::PitchStretch { semitones: a.semitones, cents: a.cents, stretch: a.stretch, length_secs };
                (ps, e.pitch_stretch_job(a.track, a.clip, ps)?)
            };
            // Render without holding the engine, then apply the edit (now cached).
            let render = job.map(|j| j.render());
            let mut e = lock()?;
            if let Some(r) = render {
                e.add_flex_render(r);
            }
            e.set_clip_pitch_stretch(a.track, a.clip, ps)
        })
        .await
        .map_err(|e| format!("pitch/stretch failed: {e}"))?;
        let clip = result.map_err(|e| format!("{e:#}"))?;
        serde_json::to_string_pretty(&clip).map_err(|e| e.to_string())
    }

    #[tool(description = "Set an audio region's fade-in and/or fade-out (lengths in timeline seconds) and their curves: linear, equal_power, exponential, logarithmic or s_curve. Overlapping regions crossfade automatically (equal power over the overlap) whether or not they have fades; a region's own longer fade wins. Undoable.")]
    async fn set_clip_fades(&self, Parameters(a): Parameters<ClipFadesArgs>) -> ToolResult {
        self.with_engine(move |e| {
            let curve = |c: Option<String>| c.map(|c| parse_name::<opentake_engine::project::FadeCurve>("fade curve", &c.replace(['-', ' '], "_"))).transpose();
            let update = opentake_engine::FadeUpdate { fade_in_secs: a.fade_in_secs, fade_out_secs: a.fade_out_secs, curve_in: curve(a.curve_in)?, curve_out: curve(a.curve_out)? };
            e.set_clip_fades(a.track, a.clip, update)
        })
        .await
    }

    #[tool(description = "Crossfade two neighbouring audio regions on a track: overlaps them by length_secs (default 0.02), centred on their boundary, by extending/trimming their facing edges as far as their source audio allows, and gives both matching equal-power fades. Regions without audio beyond their edges get a short fade-out/fade-in pair instead. Returns the (earlier, later) regions. Undoable.")]
    async fn crossfade_clips(&self, Parameters(a): Parameters<CrossfadeArgs>) -> ToolResult {
        self.with_engine(move |e| e.crossfade_clips(a.track, a.clip_a, a.clip_b, a.length_secs).map(|(x, y)| json!({ "earlier": x, "later": y })))
            .await
    }

    #[tool(description = "Merge (join) two or more audio regions on one track into one region: renders what they play (gain, fades, crossfades, pitch/stretch, active takes) into a new WAV in the project's Audio folder, spanning all of them; gaps become silence and butted regions get a 5 ms crossfade at the join so it doesn't click. One undo step (undo brings the original regions back). Returns the new region's index and the region.")]
    async fn merge_clips(&self, Parameters(a): Parameters<MergeClipsArgs>) -> ToolResult {
        self.with_engine(move |e| e.merge_clips(a.track, &a.clips).map(|(i, c)| json!({ "clip": i, "region": c }))).await
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

    #[tool(description = "Instruments available for instrument tracks: the built-in synth and drums plus scanned VST3 and Audio Unit (macOS) instruments.")]
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

    #[tool(description = "Start recording all armed tracks from the current position (also starts playback). Call stop to finish. With the cycle on it loops, and each pass through the cycle becomes a take (a pass under half the cycle is dropped); recording the same cycle again adds takes to the same folder.")]
    async fn record(&self) -> ToolResult {
        self.with_engine(|e| e.record_audio().map(|files| json!({ "recording_to": files }))).await
    }

    #[tool(description = "Scan VST3 folders and list available plugins, plus installed Audio Units on macOS (audio_units; insert them by id).")]
    async fn scan_plugins(&self, Parameters(a): Parameters<ScanArgs>) -> ToolResult {
        self.with_engine(move |e| {
            for p in a.extra_paths {
                e.add_plugin_search_path(p.into());
            }
            let (plugins, errors) = e.scan_plugins();
            Ok(json!({ "search_paths": e.plugin_search_paths(), "plugins": plugins, "audio_units": e.audio_units(), "errors": errors }))
        })
        .await
    }

    #[tool(description = "Insert an effect into a track's chain. Built-ins: \"eq\" (6-band parametric EQ), \"comp\" (compressor: threshold, ratio, attack, release, knee, makeup, mix), \"multiband\" (3-band compressor: two crossovers, then threshold, ratio, attack, release, gain, bypass and solo per band, and output), \"limiter\" (look-ahead brickwall: gain, ceiling, release), \"delay\" (ms or tempo-synced note values, feedback, mix, repeat filters, ping-pong), \"reverb\" (algorithmic stereo reverb: pre-delay, size, decay RT60, damping, low cut, width, diffusion, modulation, mix (default 100% wet, for a bus), level); or a VST3 / AU effect. list_plugin_params shows ids and values. Plug-in latency is compensated automatically. Returns the slot index.")]
    async fn insert_plugin(&self, Parameters(a): Parameters<InsertPluginArgs>) -> ToolResult {
        self.with_engine(move |e| e.insert_plugin(a.track, &a.plugin, a.position).map(|s| json!({ "slot": s }))).await
    }

    #[tool(description = "Insert an effect on the master bus (\"eq\", \"comp\", \"multiband\", \"limiter\", \"delay\", \"reverb\" or a VST3 / AU effect; a limiter last in the chain sets the final peak ceiling and loudness). Returns the slot index; use master: true with list_plugin_params / set_plugin_param / set_plugin_bypass.")]
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

    #[tool(description = "What a track (or master: true) can automate: volume, pan, mute, send levels and each plug-in parameter, with the target to pass to add_automation_lane.")]
    async fn list_automation_params(&self, Parameters(a): Parameters<AutomationOwnerArgs>) -> ToolResult {
        self.with_engine(move |e| {
            let owner = a.owner();
            let list: Vec<_> = e.automation_params(owner)?.into_iter().map(|p| json!({ "name": p.name, "group": p.group, "target": target_json(e, owner, p.target) })).collect();
            Ok(list)
        })
        .await
    }

    #[tool(description = "A track's (or the master's) automation lanes with their points (time s, normalized value, display, curve), mode and visibility.")]
    async fn get_automation(&self, Parameters(a): Parameters<AutomationOwnerArgs>) -> ToolResult {
        self.with_engine(move |e| {
            let owner = a.owner();
            (0..e.automation(owner)?.len()).map(|i| lane_json(e, owner, i)).collect::<anyhow::Result<Vec<_>>>()
        })
        .await
    }

    #[tool(description = "Add an automation lane (shown under the track) for volume, pan, mute, a send level or a plug-in parameter; returns its index (an existing lane for the same target is reused). Draw it with set_automation_points. Undoable.")]
    async fn add_automation_lane(&self, Parameters(a): Parameters<AddLaneArgs>) -> ToolResult {
        self.with_engine(move |e| {
            let owner = a.owner.owner();
            let target = resolve_target(e, owner, &a.target)?;
            let lane = e.add_lane(owner, target)?;
            Ok(json!({ "lane": lane, "name": e.target_name(owner, target) }))
        })
        .await
    }

    #[tool(description = "Set a lane's points (replace, or add with replace: false). Values are normalized 0..1, or give db (volume) or pan (-1..1). Between points the value ramps (curve per point). Undoable.")]
    async fn set_automation_points(&self, Parameters(a): Parameters<SetPointsArgs>) -> ToolResult {
        self.with_engine(move |e| {
            let owner = a.owner.owner();
            let target = e.automation(owner)?.get(a.lane).ok_or_else(|| anyhow::anyhow!("no lane {}", a.lane))?.target;
            let mut points = Vec::new();
            for p in &a.points {
                let value = match (p.value, p.db, p.pan, target) {
                    (Some(v), ..) => v,
                    (None, Some(db), _, AutomationTarget::Volume | AutomationTarget::Send { .. }) => target.from_units(db),
                    (None, _, Some(pan), AutomationTarget::Pan) => target.from_units(pan),
                    _ => anyhow::bail!("point at {} s needs a value (or db for volume, pan for pan lanes)", p.time),
                };
                let curve = p.curve.as_deref().map(|c| parse_name::<Curve>("curve", c)).transpose()?.unwrap_or_default();
                points.push(AutomationPoint { curve, ..AutomationPoint::new(p.time, value) });
            }
            e.set_automation_points(owner, a.lane, points, a.replace)?;
            lane_json(e, owner, a.lane)
        })
        .await
    }

    #[tool(description = "Remove a lane's points, all or between from and to (seconds). Undoable.")]
    async fn clear_automation(&self, Parameters(a): Parameters<ClearAutomationArgs>) -> ToolResult {
        self.with_engine(move |e| e.clear_automation(a.owner.owner(), a.lane, a.from, a.to).map(|_| "ok")).await
    }

    #[tool(description = "Show/hide a lane and set its mode: read (plays), off, touch or latch (record GUI control moves while playing). Undoable.")]
    async fn set_automation_lane(&self, Parameters(a): Parameters<LaneSettingsArgs>) -> ToolResult {
        self.with_engine(move |e| {
            let owner = a.owner.owner();
            if let Some(m) = &a.mode {
                e.set_lane_mode(owner, a.lane, parse_name::<AutomationMode>("mode", m)?)?;
            }
            if let Some(shown) = a.shown {
                e.set_lane_shown(owner, a.lane, shown)?;
                if let (Some(t), true) = (owner, shown) {
                    e.set_automation_shown(Some(t), true)?;
                }
            }
            lane_json(e, owner, a.lane)
        })
        .await
    }

    #[tool(description = "Delete an automation lane (later lanes' indices shift down). Undoable.")]
    async fn remove_automation_lane(&self, Parameters(a): Parameters<LaneArgs>) -> ToolResult {
        self.with_engine(move |e| e.remove_lane(a.owner.owner(), a.lane).map(|_| "ok")).await
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

#[cfg(test)]
mod compact_tests {
    use super::*;
    use opentake_engine::project::{MidiClip, Project, Track};

    #[test]
    fn project_json_replaces_notes_with_counts_unless_asked() {
        let mut p = Project::default();
        let mut t = Track::new("Keys");
        t.midi_clips.push(MidiClip {
            name: "r".into(),
            position_secs: 0.0,
            length_secs: 1.0,
            mute: false,
            notes: vec![Note { start_secs: 0.0, length_secs: 0.5, pitch: 60, velocity: 100, tab: None }; 3],
            tab: None,
            tempos: Vec::new(),
        });
        p.tracks.push(t);
        let compact = project_json(&p, false);
        let clip = &compact["tracks"][0]["midi_clips"][0];
        assert!(clip.get("notes").is_none());
        assert_eq!(clip["note_count"], 3);
        assert_eq!(compact["tracks"][0]["index"], 0);
        let full = project_json(&p, true);
        assert_eq!(full["tracks"][0]["midi_clips"][0]["notes"].as_array().map(|n| n.len()), Some(3));
    }

    #[test]
    fn automation_targets_points_and_compact_lanes() {
        let mut e = Engine::new();
        e.add_track("Gtr").unwrap();
        e.insert_plugin(0, "comp", None).unwrap();
        let args: AddLaneArgs = serde_json::from_value(json!({ "track": 0, "parameter": "plugin", "slot": 0, "param_id": 1 })).unwrap();
        let target = resolve_target(&e, args.owner.owner(), &args.target).unwrap();
        let lane = e.add_lane(Some(0), target).unwrap();
        assert_eq!(target_json(&e, Some(0), target), json!({ "parameter": "plugin", "slot": 0, "param_id": 1 }));
        let vol = e.add_lane(Some(0), AutomationTarget::Volume).unwrap();
        e.set_automation_points(Some(0), vol, vec![AutomationPoint::new(1.0, AutomationTarget::Volume.from_units(-6.0))], true).unwrap();
        let v = lane_json(&e, Some(0), vol).unwrap();
        assert_eq!(v["points"][0]["display"], json!("-6.0 dB"));
        assert_eq!(lane_json(&e, Some(0), lane).unwrap()["name"], json!("OpenTake Compressor: Ratio"));
        assert!(parse_name::<Curve>("curve", "Fast_Start").is_ok() && parse_name::<AutomationMode>("mode", "bogus").is_err());
        let p = project_json(e.project(), false);
        assert_eq!(p["tracks"][0]["automation"][1]["point_count"], json!(1));
        assert!(p["tracks"][0]["automation"][1].get("points").is_none());
        let bad: AddLaneArgs = serde_json::from_value(json!({ "master": true, "parameter": "pan" })).unwrap();
        assert!(e.add_lane(bad.owner.owner(), resolve_target(&e, None, &bad.target).unwrap()).is_err());
    }

    #[test]
    fn project_json_shows_kind_and_output_by_index() {
        let mut e = Engine::new();
        e.add_track("Gtr").unwrap();
        e.create_bus_from(&[0], "Bus").unwrap();
        let v = project_json(e.project(), false);
        assert_eq!((&v["tracks"][0]["kind"], &v["tracks"][0]["output"]), (&json!("audio"), &json!(1)));
        assert_eq!((&v["tracks"][1]["kind"], &v["tracks"][1]["output"]), (&json!("bus"), &json!("master")));
        let arg: UpdateTrackArgs = serde_json::from_value(json!({ "track": 0, "output": "master" })).unwrap();
        assert_eq!(arg.output.unwrap().bus().unwrap(), None);
        let arg: UpdateTrackArgs = serde_json::from_value(json!({ "track": 0, "output": 1 })).unwrap();
        assert_eq!(arg.output.unwrap().bus().unwrap(), Some(1));
    }

    #[test]
    fn project_json_lists_sends_and_send_lanes_resolve() {
        let mut e = Engine::new();
        e.add_track("Gtr").unwrap();
        let (bus, _) = e.send_to_new_bus(0, "Reverb", Some("reverb"), db_to_gain(-6.0), true).unwrap();
        let v = project_json(e.project(), false);
        assert_eq!(v["tracks"][0]["sends"], json!([{ "send": 0, "bus": bus, "level_db": -6.0, "pre_fader": true, "mute": false }]));
        assert!(v["tracks"][1].get("sends").is_none());
        let args: AddLaneArgs = serde_json::from_value(json!({ "track": 0, "parameter": "send", "send": 0 })).unwrap();
        let target = resolve_target(&e, Some(0), &args.target).unwrap();
        let lane = e.add_lane(Some(0), target).unwrap();
        assert_eq!(lane_json(&e, Some(0), lane).unwrap()["target"], json!({ "parameter": "send", "send": 0 }));
        assert_eq!(lane_json(&e, Some(0), lane).unwrap()["name"], json!("Send to Reverb"));
    }
}
