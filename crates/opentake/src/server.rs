//! MCP tool surface over the engine.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use opentake_engine::project::{AutomationMode, AutomationPoint, AutomationTarget, Controller, ControllerEvent, Curve, MidiClip, Note, Quantize, QuantizeGrid, TempoMap};
use opentake_engine::midi_input::MidiSelection;
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
switches which take plays, and comp_take plays another take over a time range (comping the best \
parts together; get_project lists the clip's `comp`, where the rest plays `active_take`); \
flatten_comp renders a comp into one plain region.

MIDI input: list_midi_inputs / set_midi_inputs choose the keyboards and controllers (all by default). \
What's played goes to every record-armed instrument track (update_track armed), or with none armed \
the track selected in the GUI. record also records MIDI on armed instrument tracks (with audio on \
armed audio tracks, or MIDI only without an input device): stop turns it into one MIDI region per \
track, notes placed where the performer heard them; with the cycle on, every pass merges into one \
region. get_status shows midi_inputs, midi_keys_down and midi_input_tracks.

Tracks, clips and plugin slots are addressed by zero-based index as shown by get_project. \
Plugin parameters are normalized 0..1; list_plugin_params shows ids, current values and display strings. \
Projects can be saved/loaded as native .otp, Reaper .rpp or .dawproject files; Ableton .als, Logic (via .fcpxml) and Ardour sessions can be imported.

MIDI: add_instrument_track (\"synth\" = built-in synth, or a VST3 / AU instrument from list_instruments) -> \
add_midi_clip -> set_midi_notes / add_midi_notes (pitch 60 = middle C, velocity 1..127). MIDI lives in \
musical time: regions, notes and controllers are stored in beats (quarter notes; regions from the song start, \
notes from the region start) and follow the tempo map. Give note times as start_beats/length_beats, or as \
start_secs/length_secs (seconds from the region start, converted through the tempo map); get_project and \
get_midi_notes show both, plus each note's bar and beat. quantize_midi snaps notes to a grid (1/16, 1/8T...) \
with strength and swing; set_record_quantize quantizes recorded MIDI. Pitch bend, mod wheel and other controllers: \
get_midi_controllers / set_midi_controllers (events at times relative to the region start; values in MIDI \
units, or semitones for pitch bend with bend_range; the built-in synth bends ±2 semitones by default, set by its \
Bend Range parameter). Every edit — yours or the user's — can be undone with undo/redo; \
get_history lists the steps. Songs: import_guitar_pro (.gp) and import_midi_file (.mid) create \
instrument tracks; export_midi_file writes the MIDI regions back out.

Tempo: set_tempo sets the opening tempo/signature; set_tempo_map adds tempo and time-signature changes. \
MIDI follows tempo changes (it stays on its bars); audio regions, automation, markers and the tempo changes \
themselves are in seconds. get_tempo_map and convert_time translate between seconds and bars/beats. Markers: list_markers, add_marker (seconds or bar/beat; sections like \"Verse\"), update_marker, \
remove_marker; goto_marker moves the playhead to one and cycle_marker loops its section (to the next marker). \
Guitar Pro sections, MIDI file markers and the other DAWs' markers come in as markers. Effects: insert_plugin with \"eq\", \"comp\", \"multiband\", \"limiter\", \"delay\", \"reverb\", \"gate\", \"deesser\", \"chorus\" or \"phaser\" adds a built-in effect to a track (a reverb usually goes on a bus at 100% wet); \
insert_master_plugin puts effects on the master bus (address them with master: true); VST3 and, on macOS, Audio Unit plug-ins work too (scan_plugins lists them). Plug-in delay \
(e.g. the limiter's look-ahead) is compensated automatically. Buses: add_bus (with tracks to group them) \
creates a bus track; update_track output routes a track to a bus index or \"master\"; effects on the bus \
(insert_plugin with its index) process the whole group. Sends feed buses besides the output, at their own \
level, pre- or post-fader: add_send (without bus it makes a \"Reverb\" bus with the built-in reverb), \
update_send, remove_send. get_project shows each track's kind, output and sends. Side chains: \
set_sidechain keys an effect's detector from another track or bus (a kick ducking a pad through a \
\"comp\" for pumping, a key opening a \"gate\"); get_project and list_plugin_params show the key.

Regions: set_clip_fades gives an audio region fade-in/out curves; overlapping regions on a track \
crossfade automatically, and crossfade_clips makes a short crossfade between two neighbours. \
merge_clips joins regions into one new audio file without clicks at the joins. set_clip_loop loops \
an audio or MIDI region: its content repeats until a time, for a number of passes, or up to the next \
region (get_project shows each looped region's loop length and passes).

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
pub struct SetMidiInputsArgs {
    /// MIDI input port names (exact, or a unique part of one) to listen to; ["all"] for every
    /// port (also ones plugged in later), [] for none.
    pub inputs: Vec<String>,
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
    /// Only notes starting at or after this many beats (quarter notes) into the region.
    pub from_beats: Option<f64>,
    /// Only notes starting before this many beats into the region.
    pub to_beats: Option<f64>,
}

/// Round for agents (beats and seconds to 1e-6).
fn r6(x: f64) -> f64 {
    (x * 1e6).round() / 1e6
}

/// A MIDI region's place in both units: beats (as stored) and seconds (through the tempo map),
/// and the bar it starts in.
fn midi_span_json(map: &TempoMap, c: &MidiClip) -> serde_json::Value {
    let (start, end) = (c.start_secs(map), c.end_secs(map));
    let pos = map.position(start);
    json!({ "position_beats": r6(c.position_beats), "length_beats": r6(c.length_beats), "position_secs": r6(start), "length_secs": r6(end - start), "bar": pos.bar, "beat": r6(pos.beat as f64 + pos.fraction) })
}

/// A note in both units, with the bar and beat it starts on (`index` is its place in the region).
fn note_json(map: &TempoMap, c: &MidiClip, index: usize, n: &Note) -> serde_json::Value {
    let (s, e) = (c.secs_of(map, n.start_beats), c.secs_of(map, n.start_beats + n.length_beats));
    let pos = map.position(s);
    let mut v = json!({
        "index": index, "pitch": n.pitch, "velocity": n.velocity,
        "start_beats": r6(n.start_beats), "length_beats": r6(n.length_beats),
        "start_secs": r6(s - c.start_secs(map)), "length_secs": r6(e - s),
        "bar": pos.bar, "beat": r6(pos.beat as f64 + pos.fraction),
    });
    if let Some(t) = n.tab {
        v["tab"] = json!(t);
    }
    v
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
    let map = project.tempo_map();
    if let Some(tracks) = v.get_mut("tracks").and_then(|t| t.as_array_mut()) {
        for (i, t) in tracks.iter_mut().enumerate() {
            compact_track(t, include_notes);
            // MIDI regions in beats and seconds (and notes too, when asked for).
            for (c, clip) in t.get_mut("midi_clips").and_then(|c| c.as_array_mut()).into_iter().flatten().zip(&project.tracks[i].midi_clips) {
                for (k, val) in midi_span_json(&map, clip).as_object().into_iter().flatten() {
                    c[k] = val.clone();
                }
                if include_notes {
                    c["notes"] = json!(clip.notes.iter().enumerate().map(|(k, n)| note_json(&map, clip, k, n)).collect::<Vec<_>>());
                }
            }
            t["index"] = json!(i);
            // Kind and output by index (the model stores buses by session id).
            let track = &project.tracks[i];
            t["kind"] = json!(if track.bus { "bus" } else if track.is_instrument() { "instrument" } else { "audio" });
            t["output"] = project.output_of(i).map_or(json!("master"), |b| json!(b));
            t.as_object_mut().map(|o| o.remove("bus"));
            if !track.sends.is_empty() {
                t["sends"] = json!((0..track.sends.len()).map(|k| send_json(project, i, k)).collect::<Vec<_>>());
            }
            keys_json(project, Some(i), t.get_mut("plugins"));
            loops_json(t, track);
            state_json(t.get_mut("instrument"));
            t.get_mut("plugins").and_then(|p| p.as_array_mut()).into_iter().flatten().for_each(|s| state_json(Some(s)));
        }
    }
    keys_json(project, None, v.get_mut("master_plugins"));
    v.get_mut("master_plugins").and_then(|p| p.as_array_mut()).into_iter().flatten().for_each(|s| state_json(Some(s)));
    v
}

/// A saved plug-in state is binary: shown by its size, if there is one. (A loaded VST3
/// keeps its state in the instance until the project is saved.)
fn state_json(slot: Option<&mut serde_json::Value>) {
    let Some(o) = slot.and_then(|s| s.as_object_mut()) else { return };
    let lines: Vec<String> = o.remove("state").and_then(|s| serde_json::from_value(s).ok()).unwrap_or_default();
    if !lines.is_empty() {
        o.insert("state".into(), json!(format!("{} bytes", opentake_engine::project::vstpreset::state_size(&lines))));
    }
}

/// Looped regions say how long the loop is and how many passes play.
fn loops_json(t: &mut serde_json::Value, track: &opentake_engine::project::Track) {
    let audio = track.clips.iter().map(|c| (c.looped, c.loop_passes().len()));
    let midi = track.midi_clips.iter().map(|c| (c.looped, c.loop_passes().len()));
    // Audio loops are in seconds, MIDI loops in beats.
    for (list, unit, info) in [("clips", "secs", audio.collect::<Vec<_>>()), ("midi_clips", "beats", midi.collect())] {
        for (c, (looped, passes)) in t.get_mut(list).and_then(|c| c.as_array_mut()).into_iter().flatten().zip(info) {
            let (Some(l), Some(o)) = (looped, c.as_object_mut()) else { continue };
            o.remove("looped");
            let mut info = json!({ format!("loop_length_{unit}"): l.len, "passes": passes });
            if l.phase != 0.0 {
                info[format!("starts_into_loop_{unit}")] = json!(l.phase);
            }
            o.insert("loop".into(), info);
        }
    }
}

/// Side chains by track index (the model stores session ids); a key from a gone track goes.
fn keys_json(project: &opentake_engine::project::Project, owner: Option<usize>, slots: Option<&mut serde_json::Value>) {
    for (k, slot) in slots.and_then(|s| s.as_array_mut()).into_iter().flatten().enumerate() {
        let Some(o) = slot.as_object_mut() else { continue };
        if let (Some(key), Some(source)) = (o.remove("side_chain"), project.key_source(owner, k)) {
            let pre = key.get("pre_fader").and_then(|p| p.as_bool()).unwrap_or(false);
            o.insert("side_chain".into(), json!({ "track": source, "name": project.tracks[source].name, "pre_fader": pre }));
        }
    }
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
                // Controller lanes as counts (get_midi_controllers has the events).
                if let Some(lanes) = c.as_object_mut().and_then(|o| o.remove("controllers")) {
                    let lanes = serde_json::from_value::<Vec<opentake_engine::project::ControllerLane>>(lanes).unwrap_or_default();
                    c["controllers"] = json!(lanes.iter().map(|l| json!({ "kind": controller_name(l.kind), "event_count": l.events.len() })).collect::<Vec<_>>());
                }
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
pub struct SideChainArgs {
    /// The track whose effect is keyed (ignored with master).
    #[serde(default)]
    pub track: usize,
    /// Zero-based effect slot.
    pub slot: usize,
    /// Address the master bus's effect chain instead of a track.
    #[serde(default)]
    pub master: bool,
    /// Index of the key track or bus (e.g. the kick); null or omitted removes the side chain.
    pub source: Option<usize>,
    /// Tap the key before the source's fader, pan and mute (so a muted "ghost" kick still
    /// keys); default after them.
    #[serde(default)]
    pub pre_fader: bool,
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
pub struct MarkerRefArgs {
    /// Zero-based marker index (markers are in time order, as list_markers shows them).
    pub index: Option<usize>,
    /// Or the marker's name (case-insensitive; the first one by time).
    pub marker_name: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MarkerTimeArgs {
    /// Position in seconds...
    pub time_secs: Option<f64>,
    /// ...or 1-based bar (and beat) on the tempo map.
    pub bar: Option<i64>,
    /// 1-based beat (fractions allowed); defaults to 1.
    pub beat: Option<f64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AddMarkerArgs {
    #[serde(flatten)]
    pub at: MarkerTimeArgs,
    /// Defaults to "Marker N".
    pub name: Option<String>,
    /// "#rrggbb" or red, orange, yellow, green, teal, blue, purple, pink.
    pub color: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdateMarkerArgs {
    #[serde(flatten)]
    pub marker: MarkerRefArgs,
    /// Move it (seconds, or bar/beat).
    #[serde(flatten)]
    pub at: MarkerTimeArgs,
    /// New name.
    pub name: Option<String>,
    /// "#rrggbb", a colour name, or "none" for the default colour.
    pub color: Option<String>,
}

impl MarkerTimeArgs {
    /// The time given, in seconds (`None` when neither secs nor bar is).
    fn secs(&self, e: &Engine) -> Option<f64> {
        self.time_secs.or_else(|| Some(e.project().tempo_map().secs_of(self.bar?.max(1), self.beat.unwrap_or(1.0))))
    }
}

/// The id of the marker an agent names (by index or name).
fn find_marker(e: &Engine, r: &MarkerRefArgs) -> anyhow::Result<u32> {
    let markers = &e.project().markers;
    let found = match (r.index, r.marker_name.as_deref()) {
        (Some(i), _) => markers.get(i).ok_or_else(|| anyhow::anyhow!("no marker {i} (there are {})", markers.len()))?,
        (None, Some(name)) => markers.iter().find(|m| m.name.eq_ignore_ascii_case(name.trim())).ok_or_else(|| anyhow::anyhow!("no marker named '{name}'"))?,
        _ => anyhow::bail!("give the marker's index or marker_name"),
    };
    Ok(found.id)
}

/// A colour by name or "#rrggbb"; `Ok(None)` for "none"/"default".
fn parse_marker_color(s: &str) -> anyhow::Result<Option<[u8; 3]>> {
    let s = s.trim().to_lowercase();
    if matches!(s.as_str(), "" | "none" | "default") {
        return Ok(None);
    }
    if let Some((_, rgb)) = opentake_engine::project::MARKER_COLORS.iter().find(|(n, _)| *n == s) {
        return Ok(Some(*rgb));
    }
    let hex = s.trim_start_matches('#');
    let byte = |i: usize| hex.get(i..i + 2).and_then(|b| u8::from_str_radix(b, 16).ok());
    match (hex.len(), byte(0), byte(2), byte(4)) {
        (6, Some(r), Some(g), Some(b)) => Ok(Some([r, g, b])),
        _ => anyhow::bail!("color must be #rrggbb, none, or one of red, orange, yellow, green, teal, blue, purple, pink"),
    }
}

fn markers_json(e: &Engine) -> serde_json::Value {
    let p = e.project();
    let map = p.tempo_map();
    let list: Vec<_> = p
        .markers
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let pos = map.position(m.time_secs);
            let mut v = json!({ "index": i, "name": m.name, "time_secs": (m.time_secs * 1e4).round() / 1e4, "bar": pos.bar, "beat": ((pos.beat as f64 + pos.fraction) * 1e3).round() / 1e3 });
            if let Some((_, end)) = p.marker_range(i) {
                v["end_secs"] = json!((end * 1e4).round() / 1e4);
            }
            if let Some([r, g, b]) = m.color {
                v["color"] = json!(format!("#{r:02x}{g:02x}{b:02x}"));
            }
            v
        })
        .collect();
    json!(list)
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
    /// Arm for recording: an audio track records its input, an instrument track the MIDI
    /// played (and plays MIDI input live).
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
pub struct CompTakeArgs {
    pub track: usize,
    pub clip: usize,
    /// Start of the range in timeline seconds (absolute, like `split_clip`); clamped to the region.
    pub from_secs: f64,
    /// End of the range in timeline seconds.
    pub to_secs: f64,
    /// Zero-based index into the region's `takes`: the take that plays over the range.
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
pub struct ClipLoopArgs {
    pub track: usize,
    /// Region index within the track's audio clips or MIDI regions (see kind).
    pub clip: usize,
    /// "audio" (default) or "midi".
    pub kind: Option<String>,
    /// true (default) loops the region; false stops looping (it becomes its first pass).
    pub enabled: Option<bool>,
    /// Loop until this timeline time (seconds).
    pub until_secs: Option<f64>,
    /// Or this many passes in all (the content plus its repeats).
    pub passes: Option<usize>,
    /// Instead: turn the repeats of a looped region into separate regions.
    #[serde(default)]
    pub convert_to_regions: bool,
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
    /// Where it starts, in seconds...
    pub position_secs: Option<f64>,
    /// ...or in beats (quarter notes) from the song start...
    pub position_beats: Option<f64>,
    /// ...or at the start of this bar (1-based).
    pub bar: Option<i64>,
    /// Length in seconds (from where it starts)...
    pub length_secs: Option<f64>,
    /// ...or in beats (quarter notes)...
    pub length_beats: Option<f64>,
    /// ...or in bars (of the time signature where it starts).
    pub length_bars: Option<f64>,
    pub name: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NoteArg {
    /// Start in beats (quarter notes) from the start of the region...
    pub start_beats: Option<f64>,
    /// ...or in seconds from the start of the region (converted through the tempo map).
    pub start_secs: Option<f64>,
    /// Length in beats (quarter notes: 1 = quarter, 0.5 = eighth)...
    pub length_beats: Option<f64>,
    /// ...or in seconds.
    pub length_secs: Option<f64>,
    /// MIDI note number, 60 = middle C.
    pub pitch: u8,
    /// 1..=127 (default 100).
    pub velocity: Option<u8>,
}

/// Notes for region `clip` of `track`, their times given in beats or seconds.
fn notes_from_args(e: &Engine, track: usize, clip: usize, args: &[NoteArg]) -> anyhow::Result<Vec<Note>> {
    let c = midi_clip(e, track, clip)?;
    let map = e.project().tempo_map();
    let origin = c.start_secs(&map);
    args.iter()
        .map(|n| {
            let start = match (n.start_beats, n.start_secs) {
                (Some(b), _) => b,
                (None, Some(s)) => c.beats_at(&map, origin + s),
                (None, None) => anyhow::bail!("each note needs start_beats or start_secs"),
            };
            let length = match (n.length_beats, n.length_secs) {
                (Some(b), _) => b,
                (None, Some(s)) => c.beats_at(&map, c.secs_of(&map, start) + s) - start,
                (None, None) => anyhow::bail!("each note needs length_beats or length_secs"),
            };
            Ok(Note { start_beats: start, length_beats: length, pitch: n.pitch, velocity: n.velocity.unwrap_or(100), tab: None })
        })
        .collect()
}

fn midi_clip(e: &Engine, track: usize, clip: usize) -> anyhow::Result<&MidiClip> {
    let t = e.project().tracks.get(track).ok_or_else(|| anyhow::anyhow!("no track {track}"))?;
    t.midi_clips.get(clip).ok_or_else(|| anyhow::anyhow!("track {track} has no MIDI region {clip}"))
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct QuantizeMidiArgs {
    pub track: usize,
    pub clip: usize,
    /// "1/4", "1/8", "1/16" (default), "1/32", or triplets "1/4T", "1/8T", "1/16T".
    pub grid: Option<String>,
    /// How far notes move towards the grid, percent (default 100; 50 = halfway).
    pub strength: Option<f64>,
    /// Swing percent on straight grids (default 0): every second grid line moves later,
    /// up to a third of a step at 100 (a triplet feel).
    pub swing: Option<f64>,
    /// Quantize note ends too (default false: lengths are kept).
    #[serde(default)]
    pub ends: bool,
    /// Only notes within this percent of half a step from their grid line move (default 100:
    /// every note); e.g. 50 fixes near misses and leaves deliberate pushes.
    pub window: Option<f64>,
    /// Note indices (as get_midi_notes lists them) to quantize; omit for all.
    pub notes: Option<Vec<usize>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RecordQuantizeArgs {
    /// Grid ("1/16", "1/8T"...) to quantize recorded MIDI to, or "off".
    pub grid: String,
    /// Percent (default 100).
    pub strength: Option<f64>,
    /// Percent (default 0).
    pub swing: Option<f64>,
}

/// Quantize settings from tool arguments (percentages).
fn quantize_from(grid: Option<&str>, strength: Option<f64>, swing: Option<f64>) -> anyhow::Result<Quantize> {
    let grid = match grid {
        None => QuantizeGrid::Sixteenth,
        Some(g) => QuantizeGrid::parse(g).ok_or_else(|| anyhow::anyhow!("unknown grid '{g}': use 1/4, 1/8, 1/16, 1/32, 1/4T, 1/8T or 1/16T"))?,
    };
    let pct = |v: Option<f64>, default: f64| v.map_or(default, |p| (p / 100.0).clamp(0.0, 1.0));
    Ok(Quantize { strength: pct(strength, 1.0), swing: pct(swing, 0.0), ..Quantize::new(grid) })
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MidiNotesArgs {
    pub track: usize,
    /// MIDI region index on the track (see get_project → midi_clips).
    pub clip: usize,
    pub notes: Vec<NoteArg>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetMidiControllersArgs {
    pub track: usize,
    /// MIDI region index on the track.
    pub clip: usize,
    /// Only this controller (see set_midi_controllers' kind); omit for every lane.
    pub kind: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ControllerEventArg {
    /// Beats (quarter notes) from the start of the region...
    pub time_beats: Option<f64>,
    /// ...or seconds from the start of the region.
    pub time_secs: Option<f64>,
    /// CC and pressure 0..127; pitch bend -8192..8191 (0 = centre), or semitones when
    /// bend_range is given.
    pub value: f64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetMidiControllersArgs {
    pub track: usize,
    pub clip: usize,
    /// "pitch_bend", "mod_wheel" (CC1), "volume" (CC7), "pan" (CC10), "expression" (CC11),
    /// "sustain" (CC64), "pressure" (channel pressure), or any CC as "cc74" / "74".
    pub kind: String,
    /// The events (each holds until the next). Empty with replace removes the lane.
    pub events: Vec<ControllerEventArg>,
    /// "replace" (default) the lane's events, or "add" to them (an event at the same time
    /// replaces the old one).
    pub mode: Option<String>,
    /// Pitch bend only: values are semitones and the instrument bends ±bend_range (the
    /// built-in synth's Bend Range parameter, 2 by default).
    pub bend_range: Option<f64>,
}

/// A controller from a tool argument.
fn parse_controller(kind: &str) -> anyhow::Result<Controller> {
    let k = kind.trim().to_lowercase().replace([' ', '-'], "_");
    Ok(match k.as_str() {
        "pitch_bend" | "pitchbend" | "bend" => Controller::PitchBend,
        "pressure" | "channel_pressure" | "aftertouch" => Controller::ChannelPressure,
        "mod_wheel" | "modwheel" | "modulation" | "mod" => Controller::MOD_WHEEL,
        "volume" => Controller::VOLUME,
        "pan" => Controller::PAN,
        "expression" => Controller::EXPRESSION,
        "sustain" | "sustain_pedal" | "pedal" => Controller::SUSTAIN,
        _ => match k.trim_start_matches("cc").parse::<u8>() {
            Ok(n) if n < 120 => Controller::Cc(n),
            _ => anyhow::bail!("unknown controller '{kind}': use pitch_bend, mod_wheel, volume, pan, expression, sustain, pressure or cc0..cc119"),
        },
    })
}

fn controller_name(kind: Controller) -> String {
    match kind {
        Controller::PitchBend => "pitch_bend".into(),
        Controller::ChannelPressure => "pressure".into(),
        Controller::Cc(n) => format!("cc{n}"),
    }
}

/// A controller value in MIDI units: CC and pressure 0..127, pitch bend -8192..8191.
fn natural_value(kind: Controller, value: f32) -> i32 {
    kind.raw(value) as i32 - if kind == Controller::PitchBend { 8192 } else { 0 }
}

/// Controller events for a region (times in beats or seconds from its start).
fn controller_events(clip: &MidiClip, map: &TempoMap, kind: Controller, events: &[ControllerEventArg], bend_range: Option<f64>) -> anyhow::Result<Vec<ControllerEvent>> {
    let origin = clip.start_secs(map);
    events
        .iter()
        .map(|e| {
            let time = match (e.time_beats, e.time_secs) {
                (Some(b), _) => b,
                (None, Some(s)) => clip.beats_at(map, origin + s),
                (None, None) => anyhow::bail!("each event needs time_beats or time_secs"),
            };
            let value = match (kind, bend_range) {
                (Controller::PitchBend, Some(range)) if range > 0.0 => (e.value / range).clamp(-1.0, 1.0),
                (Controller::PitchBend, None) => (e.value / 8192.0).clamp(-1.0, 1.0),
                (Controller::PitchBend, Some(_)) => anyhow::bail!("bend_range must be positive"),
                _ if !(0.0..=127.0).contains(&e.value) => anyhow::bail!("{} values are 0..127 (got {})", controller_name(kind), e.value),
                _ => e.value / 127.0,
            };
            Ok(ControllerEvent { time_beats: time, value: value as f32 })
        })
        .collect()
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdateMidiClipArgs {
    pub track: usize,
    pub clip: usize,
    pub name: Option<String>,
    /// New start in seconds (or position_beats: quarter notes from the song start).
    pub position_secs: Option<f64>,
    pub position_beats: Option<f64>,
    /// New length in seconds (or length_beats: quarter notes).
    pub length_secs: Option<f64>,
    pub length_beats: Option<f64>,
    pub mute: Option<bool>,
    /// Move the region to this track.
    pub to_track: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CopyMidiClipArgs {
    pub track: usize,
    pub clip: usize,
    /// Where the copy starts, in seconds (or position_beats: quarter notes from the song start).
    pub position_secs: Option<f64>,
    pub position_beats: Option<f64>,
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
pub struct EditorArgs {
    #[serde(default)]
    pub track: usize,
    /// Effect slot (ignored with instrument: true).
    #[serde(default)]
    pub slot: usize,
    /// The track's instrument instead of an effect.
    #[serde(default)]
    pub instrument: bool,
    /// The master bus's effect chain instead of a track (track is ignored).
    #[serde(default)]
    pub master: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PresetArgs {
    #[serde(default)]
    pub track: usize,
    /// Effect slot (ignored with instrument: true).
    #[serde(default)]
    pub slot: usize,
    /// The track's instrument instead of an effect.
    #[serde(default)]
    pub instrument: bool,
    /// The master bus's effect chain instead of a track (track is ignored).
    #[serde(default)]
    pub master: bool,
    /// The `.vstpreset` file.
    pub path: PathBuf,
}

impl PresetArgs {
    fn which(&self) -> SlotRef {
        match (self.master, self.instrument) {
            (true, _) => SlotRef::Master(self.slot),
            (false, true) => SlotRef::Instrument,
            (false, false) => SlotRef::Effect(self.slot),
        }
    }
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
    #[tool(description = "Transport position, play/record state, audio device info and peak meters (dBFS); MIDI input: open ports (midi_inputs), note-ons received, keys held down now and the tracks that play the input.")]
    async fn get_status(&self) -> ToolResult {
        self.with_engine(|e| Ok(e.status())).await
    }

    #[tool(description = "List MIDI input ports (keyboards, controllers, virtual ports): enabled = chosen, open = connected. Rescans, so a device plugged in since is found (and opened when chosen).")]
    async fn list_midi_inputs(&self) -> ToolResult {
        self.with_engine(|e| e.list_midi_inputs()).await
    }

    #[tool(description = "Choose MIDI inputs: [\"all\"] (the default, includes devices plugged in later), port names (or a unique part of one), or [] for none. A name no connected port matches is kept and opens when that device is plugged in. Remembered by the GUI.")]
    async fn set_midi_inputs(&self, Parameters(a): Parameters<SetMidiInputsArgs>) -> ToolResult {
        let selection = match &a.inputs[..] {
            [all] if all.eq_ignore_ascii_case("all") => MidiSelection::All,
            names => MidiSelection::Ports(names.to_vec()),
        };
        self.with_engine(move |e| e.set_midi_inputs(selection)).await
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

    #[tool(description = "The notes of one MIDI region, each with its index, start and length in beats (quarter notes from the region start, as stored) and in seconds (from the region start, through the tempo map), and the bar and beat it starts on; optionally only a range (seconds or beats into the region).")]
    async fn get_midi_notes(&self, Parameters(a): Parameters<GetMidiNotesArgs>) -> ToolResult {
        self.with_engine(move |e| {
            let clip = midi_clip(e, a.track, a.clip)?;
            let map = e.project().tempo_map();
            let at = |s: f64| clip.beats_at(&map, clip.start_secs(&map) + s);
            let from = a.from_beats.or(a.from_secs.map(at)).unwrap_or(f64::NEG_INFINITY);
            let to = a.to_beats.or(a.to_secs.map(at)).unwrap_or(f64::INFINITY);
            let notes: Vec<_> = clip.notes.iter().enumerate().filter(|(_, n)| n.start_beats >= from - 1e-9 && n.start_beats < to - 1e-9).map(|(i, n)| note_json(&map, clip, i, n)).collect();
            let mut v = midi_span_json(&map, clip);
            v["region"] = json!(clip.name);
            v["count"] = json!(notes.len());
            v["notes"] = json!(notes);
            Ok(v)
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

    #[tool(description = "Set the opening tempo and optionally the time signature (see set_tempo_map for changes later in the song). MIDI regions follow the tempo (they stay on their bars); audio regions keep their times.")]
    async fn set_tempo(&self, Parameters(a): Parameters<TempoArgs>) -> ToolResult {
        let sig = a.numerator.zip(a.denominator);
        self.with_engine(move |e| e.set_tempo(a.bpm, sig).map(|_| "ok")).await
    }

    #[tool(description = "Replace the tempo map's changes: tempo and/or time-signature changes after the start (at_secs). Bars, the grid, snapping, the metronome, plug-ins and MIDI regions (which are in beats) follow it; audio regions, automation and markers stay in seconds. Undoable.")]
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

    #[tool(description = "List the markers in time order: index, name, time (seconds and bar/beat), end_secs (where its section ends: the next marker, or the song end) and colour.")]
    async fn list_markers(&self) -> ToolResult {
        self.with_engine(|e| Ok(markers_json(e))).await
    }

    #[tool(description = "Add a marker (a named point on the marker track, e.g. \"Verse\") at time_secs or bar/beat, or at the playhead. Undoable. Returns the markers.")]
    async fn add_marker(&self, Parameters(a): Parameters<AddMarkerArgs>) -> ToolResult {
        self.with_engine(move |e| {
            let color = a.color.as_deref().map(parse_marker_color).transpose()?.flatten();
            let at = match a.at.secs(e) {
                Some(t) => t,
                None => e.status().position_secs,
            };
            e.add_marker(at, a.name, color)?;
            Ok(markers_json(e))
        })
        .await
    }

    #[tool(description = "Rename, move (time_secs or bar/beat) or recolour a marker, found by index or marker_name. Undoable. Returns the markers.")]
    async fn update_marker(&self, Parameters(a): Parameters<UpdateMarkerArgs>) -> ToolResult {
        self.with_engine(move |e| {
            let id = find_marker(e, &a.marker)?;
            let color = a.color.as_deref().map(parse_marker_color).transpose()?;
            e.update_marker(id, opentake_engine::MarkerUpdate { time_secs: a.at.secs(e), name: a.name, color }, None)?;
            Ok(markers_json(e))
        })
        .await
    }

    #[tool(description = "Delete a marker (by index or marker_name). Undoable.")]
    async fn remove_marker(&self, Parameters(a): Parameters<MarkerRefArgs>) -> ToolResult {
        self.with_engine(move |e| {
            let id = find_marker(e, &a)?;
            e.remove_marker(id)?;
            Ok(markers_json(e))
        })
        .await
    }

    #[tool(description = "Move the playhead to a marker (by index or marker_name).")]
    async fn goto_marker(&self, Parameters(a): Parameters<MarkerRefArgs>) -> ToolResult {
        self.with_engine(move |e| {
            let id = find_marker(e, &a)?;
            let m = e.project().markers.iter().find(|m| m.id == id).cloned().ok_or_else(|| anyhow::anyhow!("no such marker"))?;
            e.seek(m.time_secs)?;
            Ok(json!({ "name": m.name, "position_secs": m.time_secs }))
        })
        .await
    }

    #[tool(description = "Set the cycle (loop) to a marker's section, from the marker to the next one (the song end for the last), and turn cycling on.")]
    async fn cycle_marker(&self, Parameters(a): Parameters<MarkerRefArgs>) -> ToolResult {
        self.with_engine(move |e| {
            let id = find_marker(e, &a)?;
            let (start, end) = e.cycle_marker(id)?;
            Ok(json!({ "enabled": true, "start_secs": start, "end_secs": end }))
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

    #[tool(description = "Comp a take folder: play take `take` from `from_secs` to `to_secs` (timeline seconds), replacing what played there; the rest keeps playing. The region's `comp` lists the ranges (relative to the region start) that play a take other than `active_take`. Joins crossfade over 10 ms so they don't click. select_take clears the comp. Undoable.")]
    async fn comp_take(&self, Parameters(a): Parameters<CompTakeArgs>) -> ToolResult {
        self.with_engine(move |e| {
            let at = e.project().tracks.get(a.track).and_then(|t| t.clips.get(a.clip)).map_or(0.0, |c| c.position_secs);
            e.set_comp_segment(a.track, a.clip, a.from_secs - at, a.to_secs - at, a.take)
        })
        .await
    }

    #[tool(description = "Clear a take folder's comp: its active take plays all through again. Undoable.")]
    async fn clear_comp(&self, Parameters(a): Parameters<ClipArgs>) -> ToolResult {
        self.with_engine(move |e| e.clear_comp(a.track, a.clip)).await
    }

    #[tool(description = "Flatten a take folder: render what it plays (its comp, or the active take) into a new WAV in the project's Audio folder and make it a plain region playing that file, keeping its position, gain and fades (pitch/stretch is rendered in). The take files stay on disk; one undo step.")]
    async fn flatten_comp(&self, Parameters(a): Parameters<ClipArgs>) -> ToolResult {
        self.with_engine(move |e| e.flatten_comp(a.track, a.clip)).await
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

    #[tool(description = "Loop a region (Logic's loop handle): its content — the region as it is, or its current loop — repeats until until_secs, for passes passes in all, or (neither given) until the next region on the track. enabled false stops looping; convert_to_regions turns the repeats into separate regions. Works for audio and MIDI regions (kind). A looped region's end (trim_clip) sets how many repeats play; get_project shows each looped region's loop length and passes. Undoable.")]
    async fn set_clip_loop(&self, Parameters(a): Parameters<ClipLoopArgs>) -> ToolResult {
        use opentake_engine::{LoopTo, RegionKind, RegionRef};
        self.with_engine(move |e| {
            let kind = match a.kind.as_deref().map(str::to_lowercase).as_deref() {
                None | Some("audio") => RegionKind::Audio,
                Some("midi") => RegionKind::Midi,
                Some(other) => anyhow::bail!("unknown kind '{other}' (use audio or midi)"),
            };
            let r = RegionRef { track: a.track, kind, index: a.clip };
            if a.convert_to_regions {
                let made = e.convert_loops(&[r])?;
                return Ok(json!({ "regions": made.iter().map(|m| m.index).collect::<Vec<_>>() }));
            }
            let to = match (a.enabled.unwrap_or(true), a.until_secs, a.passes) {
                (false, ..) => LoopTo::Off,
                (true, Some(t), None) => LoopTo::Until(t),
                (true, None, Some(n)) => LoopTo::Passes(n),
                (true, None, None) => LoopTo::Fill,
                (true, Some(_), Some(_)) => anyhow::bail!("give until_secs or passes, not both"),
            };
            e.set_region_loop(&[r], to)?;
            Ok(json!({ "region": e.region_data(r).map(|d| match d {
                opentake_engine::RegionData::Audio(c) => json!({ "position_secs": c.position_secs, "length_secs": c.length_secs, "looped": c.looped, "passes": c.loop_passes().len() }),
                opentake_engine::RegionData::Midi(c) => {
                    let mut v = midi_span_json(&e.project().tempo_map(), &c);
                    (v["looped"], v["passes"]) = (json!(c.looped), json!(c.loop_passes().len()));
                    v
                }
            })? }))
        })
        .await
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

    #[tool(description = "Add an empty MIDI region to an instrument track: start at position_secs, position_beats (quarter notes from the song start) or bar; length in length_secs, length_beats or length_bars. MIDI regions follow the tempo map (they're stored in beats). Returns its index and where it landed.")]
    async fn add_midi_clip(&self, Parameters(a): Parameters<AddMidiClipArgs>) -> ToolResult {
        self.with_engine(move |e| {
            let map = e.project().tempo_map();
            let start = match (a.position_beats, a.bar, a.position_secs) {
                (Some(b), _, _) => b,
                (None, Some(bar), _) => map.quarters_at(map.secs_of(bar.max(1), 1.0)),
                (None, None, Some(s)) => map.quarters_at(s.max(0.0)),
                _ => anyhow::bail!("give position_secs, position_beats or bar"),
            };
            let length = match (a.length_beats, a.length_bars, a.length_secs) {
                (Some(b), _, _) => b,
                (None, Some(bars), _) => bars * opentake_engine::project::tempo::bar_len_q(map.signature_at(map.secs_at(start))),
                (None, None, Some(s)) => map.quarters_at(map.secs_at(start) + s) - start,
                _ => anyhow::bail!("give length_secs, length_beats or length_bars"),
            };
            let i = e.add_midi_clip_beats(a.track, start, length, a.name.as_deref())?;
            let mut v = midi_span_json(&e.project().tempo_map(), &e.project().tracks[a.track].midi_clips[i]);
            v["clip"] = json!(i);
            Ok(v)
        })
        .await
    }

    #[tool(description = "Replace all notes in a MIDI region. Each note gives start_beats/length_beats (quarter notes from the region start) or start_secs/length_secs (seconds, converted through the tempo map).")]
    async fn set_midi_notes(&self, Parameters(a): Parameters<MidiNotesArgs>) -> ToolResult {
        self.with_engine(move |e| {
            let notes = notes_from_args(e, a.track, a.clip, &a.notes)?;
            e.set_midi_notes(a.track, a.clip, notes).map(|_| "ok")
        })
        .await
    }

    #[tool(description = "Append notes to a MIDI region (times in beats or seconds from the region start, as set_midi_notes). Returns the new note count.")]
    async fn add_midi_notes(&self, Parameters(a): Parameters<MidiNotesArgs>) -> ToolResult {
        self.with_engine(move |e| {
            let notes = notes_from_args(e, a.track, a.clip, &a.notes)?;
            e.add_midi_notes(a.track, a.clip, notes).map(|n| json!({ "note_count": n }))
        })
        .await
    }

    #[tool(description = "Quantize a MIDI region's notes (all, or the indices in notes) to a grid: 1/4, 1/8, 1/16 (default), 1/32 or triplets 1/4T, 1/8T, 1/16T, aligned to the bars. strength (percent, default 100) moves notes part of the way; swing (percent) delays every second line of a straight grid; ends quantizes note ends too; window only moves notes already close to the grid. Undoable. Returns how many notes moved.")]
    async fn quantize_midi(&self, Parameters(a): Parameters<QuantizeMidiArgs>) -> ToolResult {
        self.with_engine(move |e| {
            let mut q = quantize_from(a.grid.as_deref(), a.strength, a.swing)?;
            q.ends = a.ends;
            q.window = a.window.map_or(1.0, |w| (w / 100.0).clamp(0.0, 1.0));
            let moved = e.quantize_midi(a.track, a.clip, &q, a.notes.as_deref())?;
            Ok(json!({ "moved": moved, "grid": q.grid.name() }))
        })
        .await
    }

    #[tool(description = "Quantize on record: MIDI recorded from now on snaps to this grid (\"1/16\", \"1/8T\"...) with the strength and swing given (percent) when recording stops; \"off\" records as played. Saved with the project; not an undoable edit.")]
    async fn set_record_quantize(&self, Parameters(a): Parameters<RecordQuantizeArgs>) -> ToolResult {
        self.with_engine(move |e| {
            let q = if a.grid.eq_ignore_ascii_case("off") { None } else { Some(quantize_from(Some(&a.grid), a.strength, a.swing)?) };
            e.set_record_quantize(q);
            Ok(json!({ "record_quantize": e.project().record_quantize }))
        })
        .await
    }

    #[tool(description = "The controller lanes of a MIDI region: pitch bend (-8192..8191, 0 = centre), CCs and channel pressure (0..127), each event holding until the next; times in beats (quarter notes) and seconds from the region start.")]
    async fn get_midi_controllers(&self, Parameters(a): Parameters<GetMidiControllersArgs>) -> ToolResult {
        self.with_engine(move |e| {
            let only = a.kind.as_deref().map(parse_controller).transpose()?;
            let clip = midi_clip(e, a.track, a.clip)?;
            let map = e.project().tempo_map();
            let origin = clip.start_secs(&map);
            let lanes: Vec<_> = clip
                .controllers
                .iter()
                .filter(|l| only.is_none_or(|k| k == l.kind))
                .map(|l| {
                    let events: Vec<_> = l
                        .events
                        .iter()
                        .map(|ev| json!({ "time_beats": r6(ev.time_beats), "time_secs": r6(clip.secs_of(&map, ev.time_beats) - origin), "value": natural_value(l.kind, ev.value) }))
                        .collect();
                    json!({ "kind": controller_name(l.kind), "name": l.kind.name(), "events": events })
                })
                .collect();
            let mut v = midi_span_json(&map, clip);
            v["region"] = json!(clip.name);
            v["lanes"] = json!(lanes);
            Ok(v)
        })
        .await
    }

    #[tool(description = "Set a MIDI region's pitch bend, CC (mod wheel, volume, expression, sustain, pan, any CC) or channel-pressure events: replace the lane (default) or add to it. Values in MIDI units (CC 0..127, pitch bend -8192..8191) or, for pitch bend with bend_range, semitones. Events are steps, so draw a bend or swell as several events (e.g. every 10-20 ms). Instruments get them sample-accurately, and starting playback mid-region sends each controller's value there. Undoable. Returns the lane's event count.")]
    async fn set_midi_controllers(&self, Parameters(a): Parameters<SetMidiControllersArgs>) -> ToolResult {
        self.with_engine(move |e| {
            let kind = parse_controller(&a.kind)?;
            let replace = match a.mode.as_deref().map(str::to_lowercase).as_deref() {
                None | Some("replace") => true,
                Some("add") => false,
                Some(m) => anyhow::bail!("mode must be replace or add, not '{m}'"),
            };
            let events = controller_events(midi_clip(e, a.track, a.clip)?, &e.project().tempo_map(), kind, &a.events, a.bend_range)?;
            e.set_midi_controllers(a.track, a.clip, kind, events, replace).map(|n| json!({ "kind": controller_name(kind), "event_count": n }))
        })
        .await
    }

    #[tool(description = "Change a MIDI region: rename, move (position_secs or position_beats; optionally to another track), resize (length_secs or length_beats) or mute.")]
    async fn update_midi_clip(&self, Parameters(a): Parameters<UpdateMidiClipArgs>) -> ToolResult {
        self.with_engine(move |e| {
            let mut clip = a.clip;
            let mut track = a.track;
            if let Some(to) = a.to_track.filter(|&to| to != a.track) {
                let current = midi_clip(e, track, clip)?.position_beats;
                let pos = a.position_beats.or(a.position_secs.map(|s| e.beats_at(s.max(0.0)))).unwrap_or(current);
                e.move_midi_clip_beats(track, clip, pos, Some(to))?;
                track = to;
                clip = e.project().tracks[to].midi_clips.len() - 1;
            }
            let u = MidiClipUpdate { name: a.name, position_secs: a.position_secs, length_secs: a.length_secs, position_beats: a.position_beats, length_beats: a.length_beats, mute: a.mute };
            let c = e.update_midi_clip(track, clip, u)?;
            Ok(json!({ "track": track, "clip": clip, "region": c, "span": midi_span_json(&e.project().tempo_map(), &c) }))
        })
        .await
    }

    #[tool(description = "Copy a MIDI region (with its notes) to another position (position_secs, or position_beats from the song start) or track.")]
    async fn copy_midi_clip(&self, Parameters(a): Parameters<CopyMidiClipArgs>) -> ToolResult {
        self.with_engine(move |e| {
            let pos = match (a.position_beats, a.position_secs) {
                (Some(b), _) => b,
                (None, Some(s)) => e.beats_at(s.max(0.0)),
                (None, None) => anyhow::bail!("give position_secs or position_beats"),
            };
            e.copy_midi_clip_beats(a.track, a.clip, pos, a.to_track).map(|(t, c)| json!({ "track": t, "clip": c }))
        })
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

    #[tool(description = "Stop playback/recording. If recording, returns the new audio regions (recorded_clips) and MIDI regions (recorded_midi: track, region index, note count) — one undo step.")]
    async fn stop(&self) -> ToolResult {
        self.with_engine(|e| e.stop().map(|r| json!({ "recorded_clips": r.clips, "recorded_midi": r.midi_regions }))).await
    }

    #[tool(description = "Move the playhead (seconds).")]
    async fn seek(&self, Parameters(a): Parameters<SeekArgs>) -> ToolResult {
        self.with_engine(move |e| e.seek(a.position_secs).map(|_| "ok")).await
    }

    #[tool(description = "Start recording all armed tracks from the current position (also starts playback, after any count-in): audio tracks record their input, instrument tracks the MIDI played on the MIDI inputs (no audio input device needed for MIDI only). Call stop to finish. With the cycle on it loops: each audio pass becomes a take (a pass under half the cycle is dropped; recording the same cycle again adds takes to the same folder), while MIDI from every pass merges into one region spanning the cycle.")]
    async fn record(&self) -> ToolResult {
        self.with_engine(|e| e.start_recording().map(|r| json!({ "recording_to": r.files, "recording_midi_on": r.midi_tracks }))).await
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

    #[tool(description = "Insert an effect into a track's chain. Built-ins: \"eq\" (6-band parametric EQ), \"comp\" (compressor: threshold, ratio, attack, release, knee, makeup, mix, key low cut; set_sidechain keys it from another track), \"multiband\" (3-band compressor: two crossovers, then threshold, ratio, attack, release, gain, bypass and solo per band, and output), \"limiter\" (look-ahead brickwall: gain, ceiling, release), \"delay\" (ms or tempo-synced note values, feedback, mix, repeat filters, ping-pong), \"reverb\" (algorithmic stereo reverb: pre-delay, size, decay RT60, damping, low cut, width, diffusion, modulation, mix (default 100% wet, for a bus), level), \"gate\" (noise gate: threshold, hysteresis, attack, hold, release, range, key low/high cut), \"deesser\" (de-esser: frequency, threshold, range, release, split or wideband, listen), \"chorus\" (rate, depth, delay, 1-4 voices, width, feedback, tone, mix), \"phaser\" (rate, depth in octaves, center, feedback, 2-12 stages, stereo spread, mix); or a VST3 / AU effect. list_plugin_params shows ids and values. Plug-in latency is compensated automatically. Returns the slot index.")]
    async fn insert_plugin(&self, Parameters(a): Parameters<InsertPluginArgs>) -> ToolResult {
        self.with_engine(move |e| e.insert_plugin(a.track, &a.plugin, a.position).map(|s| json!({ "slot": s }))).await
    }

    #[tool(description = "Insert an effect on the master bus (\"eq\", \"comp\", \"multiband\", \"limiter\", \"delay\", \"reverb\", \"gate\", \"deesser\", \"chorus\", \"phaser\" or a VST3 / AU effect; a limiter last in the chain sets the final peak ceiling and loudness). Returns the slot index; use master: true with list_plugin_params / set_plugin_param / set_plugin_bypass.")]
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

    #[tool(description = "Save a VST3 plugin's complete current state (its settings beyond the parameters too) as a .vstpreset file, which VST3 hosts load as a preset. Returns its size.")]
    async fn save_plugin_preset(&self, Parameters(a): Parameters<PresetArgs>) -> ToolResult {
        self.with_engine(move |e| e.save_plugin_preset(a.track, a.which(), &a.path).map(|bytes| json!({ "path": a.path, "bytes": bytes }))).await
    }

    #[tool(description = "Load a .vstpreset file into a VST3 plugin (the preset must be for that plugin). Undo restores the parameter values OpenTake had saved, not the rest of the plugin's state.")]
    async fn load_plugin_preset(&self, Parameters(a): Parameters<PresetArgs>) -> ToolResult {
        self.with_engine(move |e| e.load_plugin_preset(a.track, a.which(), &a.path).map(|_| "ok")).await
    }

    #[tool(description = "Show a VST3 / AU plugin's own editor window to the user in the OpenTake window (built-ins and plugins without one get OpenTake's controls). Needs the GUI; edits made there are undoable and write automation like any control.")]
    async fn open_plugin_editor(&self, Parameters(a): Parameters<EditorArgs>) -> ToolResult {
        let which = match (a.master, a.instrument) {
            (true, _) => SlotRef::Master(a.slot),
            (false, true) => SlotRef::Instrument,
            (false, false) => SlotRef::Effect(a.slot),
        };
        self.with_engine(move |e| e.request_editor(a.track, which).map(|_| "ok")).await
    }

    #[tool(description = "List a plugin's parameters (params: ids, normalized values and display strings), whether it takes a side chain (side_chain_input) and the track keying it (side_chain: track index, name, pre_fader; null when unkeyed).")]
    async fn list_plugin_params(&self, Parameters(a): Parameters<ListParamsArgs>) -> ToolResult {
        self.with_engine(move |e| {
            let which = if a.master { SlotRef::Master(a.slot) } else { SlotRef::Effect(a.slot) };
            let mut params = e.slot_params(a.track, which)?;
            if let Some(f) = a.filter.map(|f| f.to_lowercase()) {
                params.retain(|p| p.title.to_lowercase().contains(&f));
            }
            let key = e.side_chain(a.track, which).map(|(s, pre)| json!({ "track": s, "name": e.project().tracks[s].name, "pre_fader": pre }));
            Ok(json!({ "params": params, "side_chain_input": e.supports_side_chain(a.track, which), "side_chain": key }))
        })
        .await
    }

    #[tool(description = "Side-chain an effect's detector to another track or bus (source index; null removes it), e.g. duck a pad or bass with the kick for pumping: put a \"comp\" on the pad, set_sidechain with the kick as source, then threshold low (-30..-40 dB), ratio high (8..20), attack fast (~1 ms), release 100-200 ms; the comp's Key Low Cut keeps a kick key from being all sub. The built-in comp, multiband, limiter, gate (opened by the key) and de-esser take a key, as do VST3 / AU effects with a side-chain input. The audio processed is still the slot's own track; the key is lined up through plug-in delay. pre_fader taps the key before the source's fader and mute (a muted ghost kick still keys).")]
    async fn set_sidechain(&self, Parameters(a): Parameters<SideChainArgs>) -> ToolResult {
        let which = if a.master { SlotRef::Master(a.slot) } else { SlotRef::Effect(a.slot) };
        self.with_engine(move |e| {
            e.set_side_chain(a.track, which, a.source, a.pre_fader)?;
            Ok(json!({ "side_chain": e.side_chain(a.track, which).map(|(s, pre)| json!({ "track": s, "pre_fader": pre })) }))
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
    fn markers_are_found_by_index_or_name_and_colours_parse() {
        let mut e = Engine::new();
        e.add_marker(4.0, Some("Verse".into()), None).unwrap();
        e.add_marker(0.0, Some("Intro".into()), Some([255, 0, 0])).unwrap();
        let by = |index: Option<usize>, name: Option<&str>| find_marker(&e, &MarkerRefArgs { index, marker_name: name.map(Into::into) });
        assert_eq!(by(Some(1), None).unwrap(), by(None, Some("verse")).unwrap(), "index in time order, names case-insensitive");
        assert!(by(Some(2), None).is_err() && by(None, Some("Bridge")).is_err() && by(None, None).is_err());
        assert_eq!(parse_marker_color("Blue").unwrap(), Some([86, 140, 230]));
        assert_eq!(parse_marker_color("#0a0B0c").unwrap(), Some([10, 11, 12]));
        assert_eq!(parse_marker_color("none").unwrap(), None);
        assert!(parse_marker_color("#12345").is_err());
        let list = markers_json(&e);
        assert_eq!((list[0]["name"].as_str(), list[0]["color"].as_str(), list[0]["end_secs"].as_f64()), (Some("Intro"), Some("#ff0000"), Some(4.0)));
        assert_eq!((list[1]["bar"].as_i64(), list[1]["beat"].as_f64()), (Some(3), Some(1.0)));
    }

    #[test]
    fn project_json_shows_loops() {
        let mut p = Project::default();
        let mut t = Track::new("Keys");
        let mut c = MidiClip::new("r", 0.0, 2.5);
        c.looped = Some(opentake_engine::project::RegionLoop::new(1.0));
        t.midi_clips.push(c);
        t.midi_clips.push(MidiClip::new("plain", 4.0, 1.0));
        p.tracks.push(t);
        let v = project_json(&p, false);
        assert_eq!(v["tracks"][0]["midi_clips"][0]["loop"], json!({ "loop_length_beats": 1.0, "passes": 3 }), "MIDI loops are in beats");
        // Regions in beats and seconds: 4 beats at 120 bpm is 2 s, bar 2.
        let plain = &v["tracks"][0]["midi_clips"][1];
        assert_eq!((plain["position_beats"].as_f64(), plain["position_secs"].as_f64(), plain["length_secs"].as_f64(), plain["bar"].as_i64()), (Some(4.0), Some(2.0), Some(0.5), Some(2)));
        assert!(v["tracks"][0]["midi_clips"][0].get("looped").is_none() && v["tracks"][0]["midi_clips"][1].get("loop").is_none());
    }

    fn t_slot() -> opentake_engine::project::PluginSlot {
        opentake_engine::project::PluginSlot {
            name: "VST3: Synth".into(),
            format: opentake_engine::project::PluginFormat::Vst3,
            path: String::new(),
            uid: String::new(),
            bypass: false,
            state: Vec::new(),
            params: Vec::new(),
            id: 0,
            side_chain: None,
            instance: 0,
        }
    }

    #[test]
    fn project_json_replaces_notes_with_counts_unless_asked() {
        let mut p = Project::default();
        let mut t = Track::new("Keys");
        t.midi_clips.push(MidiClip {
            name: "r".into(),
            position_beats: 0.0,
            length_beats: 2.0,
            mute: false,
            notes: vec![Note { start_beats: 0.0, length_beats: 1.0, pitch: 60, velocity: 100, tab: None }; 3],
            tab: None,
            tempos: Vec::new(),
            controllers: Vec::new(),
            looped: None,
        });
        p.tracks.push(t);
        let mut eq = opentake_engine::project::PluginSlot { state: vec!["AAAAAA==".into()], ..t_slot() };
        eq.name = "VST3: EQ".into();
        p.tracks[0].plugins.push(eq);
        p.tracks[0].instrument = Some(t_slot());
        let compact = project_json(&p, false);
        assert_eq!(compact["tracks"][0]["plugins"][0]["state"], "4 bytes", "states show their size");
        assert!(compact["tracks"][0]["instrument"].get("state").is_none());
        let clip = &compact["tracks"][0]["midi_clips"][0];
        assert!(clip.get("notes").is_none());
        assert_eq!(clip["note_count"], 3);
        assert_eq!(compact["tracks"][0]["index"], 0);
        let full = project_json(&p, true);
        assert_eq!(full["tracks"][0]["midi_clips"][0]["notes"].as_array().map(|n| n.len()), Some(3));
        assert_eq!((full["tracks"][0]["midi_clips"][0]["notes"][0]["length_beats"].as_f64(), full["tracks"][0]["midi_clips"][0]["notes"][0]["length_secs"].as_f64()), (Some(1.0), Some(0.5)));
    }

    #[test]
    fn midi_tools_take_seconds_or_beats_and_show_both() {
        let mut e = Engine::new();
        // 120 bpm, then 60 from 4 s (beat 8).
        e.set_tempo_changes(vec![opentake_engine::project::TempoChange { at_secs: 4.0, bpm: 60.0, time_signature: None }], None).unwrap();
        let t = e.add_instrument_track("Keys", "synth").unwrap();
        let c = e.add_midi_clip_beats(t, 6.0, 4.0, None).unwrap(); // 3 s .. 6 s
        let args: Vec<NoteArg> = serde_json::from_value(json!([
            { "start_secs": 0.5, "length_secs": 1.0, "pitch": 60 },
            { "start_beats": 3.0, "length_beats": 0.5, "pitch": 64, "velocity": 90 },
            { "start_beats": 0.0, "length_secs": 0.25, "pitch": 67 },
        ]))
        .unwrap();
        let notes = notes_from_args(&e, t, c, &args).unwrap();
        // 3.5 s = beat 7, 1 s later = beat 8.5 (across the change); 0.25 s from beat 6 = half a beat.
        assert_eq!(notes.iter().map(|n| (n.start_beats, n.length_beats)).collect::<Vec<_>>(), [(1.0, 1.5), (3.0, 0.5), (0.0, 0.5)]);
        let bad: Vec<NoteArg> = serde_json::from_value(json!([{ "length_beats": 1.0, "pitch": 60 }])).unwrap();
        assert!(notes_from_args(&e, t, c, &bad).is_err());
        e.set_midi_notes(t, c, notes).unwrap();
        let map = e.project().tempo_map();
        let clip = &e.project().tracks[t].midi_clips[c];
        let n = note_json(&map, clip, 1, &clip.notes[1]);
        assert_eq!(n, json!({ "index": 1, "pitch": 60, "velocity": 100, "start_beats": 1.0, "length_beats": 1.5, "start_secs": 0.5, "length_secs": 1.0, "bar": 2, "beat": 4.0 }));
        assert_eq!(midi_span_json(&map, clip), json!({ "position_beats": 6.0, "length_beats": 4.0, "position_secs": 3.0, "length_secs": 3.0, "bar": 2, "beat": 3.0 }));

        // Quantize settings in percent.
        let q = quantize_from(Some("1/8t"), Some(50.0), Some(250.0)).unwrap();
        assert_eq!((q.grid, q.strength, q.swing), (QuantizeGrid::EighthTriplet, 0.5, 1.0));
        assert_eq!(quantize_from(None, None, None).unwrap(), Quantize::new(QuantizeGrid::Sixteenth));
        assert!(quantize_from(Some("1/5"), None, None).is_err());
    }

    #[test]
    fn controller_tools_take_midi_units_or_semitones() {
        assert_eq!(parse_controller("Pitch Bend").unwrap(), Controller::PitchBend);
        assert_eq!(parse_controller("cc74").unwrap(), Controller::Cc(74));
        assert_eq!(parse_controller("64").unwrap(), Controller::SUSTAIN);
        assert!(parse_controller("cc123").is_err(), "channel mode messages aren't controllers");
        let ev = |t: f64, v: f64| ControllerEventArg { time_secs: Some(t), time_beats: None, value: v };
        let (clip, map) = (MidiClip::new("r", 2.0, 4.0), opentake_engine::project::TempoMap::new(120.0, (4, 4), &[]));
        let events = |kind, list: &[ControllerEventArg], range| controller_events(&clip, &map, kind, list, range);
        let bend = events(Controller::PitchBend, &[ev(0.0, -8192.0), ev(0.5, 4096.0)], None).unwrap();
        assert_eq!(bend.iter().map(|e| e.value).collect::<Vec<_>>(), vec![-1.0, 0.5]);
        assert_eq!(bend[1].time_beats, 1.0, "half a second into the region: a beat");
        let semis = events(Controller::PitchBend, &[ControllerEventArg { time_beats: Some(1.5), ..ev(9.0, 1.0) }], Some(2.0)).unwrap();
        assert_eq!((semis[0].value, semis[0].time_beats), (0.5, 1.5), "beats win");
        assert!(events(Controller::VOLUME, &[ev(0.0, 200.0)], None).is_err());

        let mut e = Engine::new();
        let t = e.add_instrument_track("Keys", "synth").unwrap();
        e.add_midi_clip(t, 0.0, 2.0, None).unwrap();
        e.set_midi_controllers(t, 0, Controller::PitchBend, bend, true).unwrap();
        e.set_midi_controllers(t, 0, Controller::VOLUME, events(Controller::VOLUME, &[ev(0.0, 100.0)], None).unwrap(), true).unwrap();
        let v = project_json(e.project(), false);
        assert_eq!(v["tracks"][t]["midi_clips"][0]["controllers"], json!([{ "kind": "pitch_bend", "event_count": 2 }, { "kind": "cc7", "event_count": 1 }]));
        assert_eq!(natural_value(Controller::PitchBend, -1.0), -8192);
        assert_eq!(natural_value(Controller::VOLUME, 100.0 / 127.0), 100);
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

    #[test]
    fn project_json_shows_side_chains_by_index() {
        let mut e = Engine::new();
        let pad = e.add_track("Pad").unwrap();
        let kick = e.add_track("Kick").unwrap();
        e.insert_plugin(pad, "comp", None).unwrap();
        e.insert_master_plugin("limiter", None).unwrap();
        let a: SideChainArgs = serde_json::from_value(json!({ "track": pad, "slot": 0, "source": kick, "pre_fader": true })).unwrap();
        e.set_side_chain(a.track, SlotRef::Effect(a.slot), a.source, a.pre_fader).unwrap();
        e.set_side_chain(0, SlotRef::Master(0), Some(pad), false).unwrap();
        let v = project_json(e.project(), false);
        assert_eq!(v["tracks"][pad]["plugins"][0]["side_chain"], json!({ "track": kick, "name": "Kick", "pre_fader": true }));
        assert_eq!(v["master_plugins"][0]["side_chain"]["track"], json!(pad));
        let clear: SideChainArgs = serde_json::from_value(json!({ "slot": 0, "master": true, "source": null })).unwrap();
        assert!(clear.source.is_none() && clear.master);
    }
}
