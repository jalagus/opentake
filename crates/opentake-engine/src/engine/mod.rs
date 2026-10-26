//! Control-side engine: owns the project model and keeps the audio thread in sync.
//!
//! Every edit goes through [`Engine::record`], which snapshots the project so it can be
//! undone (see `history.rs`). Tracks and plugin instances carry stable ids, so undo can
//! diff two project states and update only what changed on the audio thread.

mod analysis;
mod au;
mod automation;
mod calibration;
mod comp;
mod export;
mod fades;
mod flex;
mod gain;
mod guitarpro;
mod history;
mod live_midi;
mod midi;
mod midi_file;
mod plugins;
mod recording;
mod regions;
mod routing;
mod sends;
mod sidechain;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use opentake_project::{Clip, Fade, Project, TempoChange, Track, played_clips};
use opentake_vst3::{Module, ScannedPlugin};
use rtrb::{Consumer, Producer};
use serde::Serialize;

use crate::audio::{self, AudioConfig, AudioStreams};
use crate::media::{self, AudioData};
use crate::midi_input::{self, MidiSelection};
use crate::mixer::{ClipFades, ClipRt, Command, EnvTarget, Garbage, LATENCY_UNKNOWN, Mixer, RecMidi, SharedState, TrackParams, TrackRt};
use crate::recorder::{RecordTarget, Recorder};

pub use guitarpro::{GuitarProImport, GuitarProSummary, GuitarProTrackInfo, inspect as inspect_guitar_pro};
pub use analysis::{LevelReport, MixJob, MixReport};
pub use au::AudioUnitInfo;
pub use automation::{AutomationParam, LaneOwner};
pub use calibration::LatencyReport;
pub use export::{AudioFormat, ExportOptions, ExportReport, ExportedFile, SAMPLE_RATES};
pub use fades::{CROSSFADE_SECS, FadeUpdate, JOIN_SECS};
pub use flex::{FlexJob, FlexRender, MAX_SEMITONES, PitchStretch, STRETCH_RANGE};
pub use gain::CLIP_GAIN_DB;
pub use history::HistoryInfo;
pub use live_midi::{MidiInputs, RecordedMidi};
pub use midi::MidiClipUpdate;
pub use midi_file::{MidiFileImport, MidiFileSummary, MidiFileTrackInfo, gm_program_name, inspect as inspect_midi_file, is_midi_file_path};
pub use plugins::SlotRef;
pub use regions::{RegionData, RegionKind, RegionRef};
pub use sends::SendUpdate;

const SILENCE_DB: f32 = -120.0;

/// Optional changes to a track; `None` leaves a field unchanged.
#[derive(Debug, Clone, Default)]
pub struct TrackUpdate {
    pub name: Option<String>,
    pub volume: Option<f32>,
    pub pan: Option<f32>,
    pub mute: Option<bool>,
    pub solo: Option<bool>,
    pub armed: Option<bool>,
    pub monitor_input: Option<bool>,
    pub input_channel: Option<u16>,
}

impl TrackUpdate {
    /// Undo label and, for continuous controls, a merge key so a fader drag is one step.
    fn describe(&self, track_id: u64) -> (&'static str, Option<String>) {
        let set = [
            self.name.is_some(),
            self.volume.is_some(),
            self.pan.is_some(),
            self.mute.is_some(),
            self.solo.is_some(),
            self.armed.is_some(),
            self.monitor_input.is_some(),
            self.input_channel.is_some(),
        ];
        if set.iter().filter(|&&s| s).count() != 1 {
            return ("Track Settings", None);
        }
        match set.iter().position(|&s| s) {
            Some(0) => ("Rename Track", None),
            Some(1) => ("Volume", Some(format!("vol:{track_id}"))),
            Some(2) => ("Pan", Some(format!("pan:{track_id}"))),
            Some(3) => ("Mute", None),
            Some(4) => ("Solo", None),
            Some(5) => ("Record Enable", None),
            Some(6) => ("Input Monitoring", None),
            _ => ("Input Channel", None),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct EngineStatus {
    pub audio_running: bool,
    pub sample_rate: u32,
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub input_channels: usize,
    pub output_channels: usize,
    pub playing: bool,
    pub recording: bool,
    /// Counting in before playback or recording starts.
    pub counting_in: bool,
    /// Plug-in delay being compensated (milliseconds).
    pub latency_ms: f64,
    /// Interface round trip, input capture to output playback, as the device reports it
    /// (milliseconds; `None` if it doesn't). Recordings are moved earlier by this plus
    /// `record_offset_ms`.
    pub io_latency_ms: Option<f64>,
    /// Manual correction added to the measured round trip (milliseconds).
    pub record_offset_ms: f64,
    pub position_secs: f64,
    /// Where the current recording started (seconds), while recording.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record_start_secs: Option<f64>,
    pub record_overrun_samples: u64,
    /// Peak level in dBFS with a 30 dB/s fall (floored at -120).
    pub master_peak_db: [f32; 2],
    pub track_peak_db: Vec<f32>,
    /// From the engine processing a moment to it being heard, as the device reports it
    /// (milliseconds). Recorded MIDI is moved earlier by this (plus the buffer it waited for).
    pub output_latency_ms: Option<f64>,
    /// MIDI input ports open now, the note-ons received from them so far, the keys held down
    /// (pitches), and the tracks (indices) that play them.
    pub midi_inputs: Vec<String>,
    pub midi_notes_received: u64,
    pub midi_keys_down: Vec<u8>,
    pub midi_input_tracks: Vec<usize>,
}

/// What `start_recording` armed: audio files being written, and instrument tracks recording
/// MIDI (indices).
#[derive(Debug, Clone, Serialize)]
pub struct RecordingStarted {
    pub files: Vec<PathBuf>,
    pub midi_tracks: Vec<usize>,
}

/// What a recording added: audio regions and MIDI regions.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Recorded {
    pub clips: Vec<Clip>,
    pub midi_regions: Vec<RecordedMidi>,
}

impl Recorded {
    pub fn count(&self) -> usize {
        self.clips.len() + self.midi_regions.len()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ParamView {
    pub id: u32,
    pub title: String,
    pub units: String,
    pub value: f64,
    pub display: Option<String>,
    pub step_count: i32,
    pub read_only: bool,
}

/// Min/max overview of an audio file for drawing.
#[derive(Debug)]
pub struct Waveform {
    /// Length of the source file in frames.
    pub frames: usize,
    pub frames_per_peak: usize,
    pub sample_rate: u32,
    /// (min, max) per bucket, mixed down to mono.
    pub peaks: Vec<(f32, f32)>,
}

impl Waveform {
    const FRAMES_PER_PEAK: usize = 256;

    fn from_audio(data: &AudioData) -> Self {
        let n = data.frames();
        let peaks = (0..n.div_ceil(Self::FRAMES_PER_PEAK))
            .map(|b| {
                let range = b * Self::FRAMES_PER_PEAK..((b + 1) * Self::FRAMES_PER_PEAK).min(n);
                let (mut lo, mut hi) = (0.0f32, 0.0f32);
                for ch in &data.channels {
                    for &s in &ch[range.clone()] {
                        lo = lo.min(s);
                        hi = hi.max(s);
                    }
                }
                (lo, hi)
            })
            .collect();
        Self { frames: n, frames_per_peak: Self::FRAMES_PER_PEAK, sample_rate: data.sample_rate, peaks }
    }
}

/// Peak meters with a falling decay, so several readers (GUI, agent) see the same levels.
struct Meters {
    last: Instant,
    master: [f32; 2],
    tracks: Vec<f32>,
}

/// Meter fall rate in dB per second.
const METER_DECAY_DB_PER_SEC: f32 = 30.0;

struct AudioRuntime {
    // Field order matters: streams (and the mixer inside them) are dropped first.
    /// `None` in tests, where the mixer is driven directly instead of by a device.
    streams: Option<AudioStreams>,
    sample_rate: u32,
    input_channels: usize,
    commands: Producer<Command>,
    recorder: Option<Recorder>,
    /// Open MIDI input ports, and the MIDI the audio thread captures while recording.
    midi: midi_input::Connections,
    midi_record: Consumer<RecMidi>,
    /// Identifies the device setup (for its stored recording offset).
    setup: String,
    /// Latency probe captures, handed on by the garbage collector.
    probe_results: crossbeam_channel::Receiver<Box<Vec<f32>>>,
    _gc: GarbageCollector,
}

/// Drops whatever the audio thread hands back, off the audio thread (except latency probe
/// captures, which it passes on).
struct GarbageCollector {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl GarbageCollector {
    fn spawn(mut rx: Consumer<Garbage>, probes: crossbeam_channel::Sender<Box<Vec<f32>>>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let handle = std::thread::Builder::new()
            .name("daw-gc".into())
            .spawn(move || {
                while !flag.load(Ordering::Relaxed) {
                    while let Ok(g) = rx.pop() {
                        match g {
                            Garbage::Probe(buf) => drop(probes.send(buf)),
                            g => drop(g),
                        }
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            })
            .expect("spawn gc thread");
        Self { stop, handle: Some(handle) }
    }
}

impl Drop for GarbageCollector {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

pub struct Engine {
    project: Project,
    project_path: Option<PathBuf>,
    next_id: u64,
    shared: Arc<SharedState>,
    audio: Option<AudioRuntime>,
    media: HashMap<(PathBuf, u32), Arc<AudioData>>,
    /// Pitch-shifted/stretched region audio (see flex.rs).
    flexed: HashMap<flex::FlexKey, Arc<AudioData>>,
    waveforms: HashMap<PathBuf, Arc<Waveform>>,
    meters: Meters,
    // Plugins (see plugins.rs).
    modules: HashMap<PathBuf, Arc<Module>>,
    plugin_paths: Vec<PathBuf>,
    scanned: Vec<ScannedPlugin>,
    scan_errors: Vec<String>,
    audio_units: Vec<au::AudioUnitInfo>,
    /// Every plugin instance loaded this session, by instance id. Instances removed from
    /// the project stay here while undo history may still refer to them.
    pool: HashMap<u64, plugins::Loaded>,
    /// What the audio thread currently has, per track id (to diff against).
    live_chain: HashMap<u64, Vec<u64>>,
    live_instrument: HashMap<u64, Option<u64>>,
    /// Automation targets the audio thread plays, per track id (or `MASTER`), and the
    /// controls being written into lanes (see automation.rs).
    live_automation: HashMap<u64, Vec<EnvTarget>>,
    writing: Vec<automation::Writing>,
    history: history::History,
    /// Added to the measured round trip when placing recordings (milliseconds), and the
    /// stored offsets per device setup (see calibration.rs).
    record_offset_ms: f64,
    latency_store: calibration::LatencyStore,
    /// MIDI input: the chosen ports, the selected track (id), the tracks the audio thread
    /// plays input on, and the recording in progress (see live_midi.rs).
    midi_selection: MidiSelection,
    selected_track: Option<u64>,
    live_midi_targets: Vec<u64>,
    midi_take: Option<live_midi::MidiTake>,
    /// A recording was started (it may still be counting in).
    taking: bool,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine {
    pub fn new() -> Self {
        Self {
            project: Project::default(),
            project_path: None,
            next_id: 1,
            shared: Arc::new(SharedState::new()),
            audio: None,
            media: HashMap::new(),
            flexed: HashMap::new(),
            waveforms: HashMap::new(),
            meters: Meters { last: Instant::now(), master: [0.0; 2], tracks: Vec::new() },
            modules: HashMap::new(),
            plugin_paths: opentake_vst3::scan::default_search_paths(),
            scanned: Vec::new(),
            scan_errors: Vec::new(),
            audio_units: Vec::new(),
            pool: HashMap::new(),
            live_chain: HashMap::new(),
            live_instrument: HashMap::new(),
            live_automation: HashMap::new(),
            writing: Vec::new(),
            history: history::History::default(),
            record_offset_ms: 0.0,
            latency_store: calibration::LatencyStore::default(),
            midi_selection: MidiSelection::All,
            selected_track: None,
            live_midi_targets: Vec::new(),
            midi_take: None,
            taking: false,
        }
    }

    pub fn project(&self) -> &Project {
        &self.project
    }

    pub fn project_path(&self) -> Option<&Path> {
        self.project_path.as_deref()
    }

    fn sample_rate(&self) -> u32 {
        self.audio.as_ref().map_or(self.project.sample_rate, |a| a.sample_rate)
    }

    fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Where recordings are written: `<project dir>/Audio`.
    pub fn media_dir(&self) -> PathBuf {
        let base = match &self.project_path {
            Some(p) => p.parent().map(Path::to_path_buf).unwrap_or_default(),
            None => std::env::current_dir().unwrap_or_default().join(&self.project.name),
        };
        base.join("Audio")
    }

    fn send(&mut self, cmd: Command) -> Result<()> {
        match &mut self.audio {
            Some(a) => a.commands.push(cmd).map_err(|_| anyhow!("audio command queue is full")),
            None => Ok(()),
        }
    }

    fn track_index(&self, index: usize) -> Result<usize> {
        if index < self.project.tracks.len() {
            Ok(index)
        } else {
            bail!("no track at index {index} (project has {} tracks)", self.project.tracks.len())
        }
    }

    fn track_id(&self, index: usize) -> u64 {
        self.project.tracks[index].id
    }

    // ---------------------------------------------------------------- audio devices

    pub fn list_devices(&self) -> Result<audio::DeviceList> {
        audio::list_devices()
    }

    pub fn start_audio(&mut self, config: &AudioConfig) -> Result<EngineStatus> {
        self.stop_audio()?;
        midi_input::now_ns(); // start the shared clock before the audio thread reads it
        let resolved = audio::resolve(config)?;
        let sample_rate = resolved.sample_rate();
        let in_channels = resolved.input_channels();
        self.project.sample_rate = sample_rate;

        let (mut cmd_tx, cmd_rx) = rtrb::RingBuffer::new(1024);
        let (g_tx, g_rx) = rtrb::RingBuffer::new(1024);
        let (rec_tx, recorder) = if in_channels > 0 {
            let (tx, rx) = rtrb::RingBuffer::new(in_channels * sample_rate as usize * 4);
            (Some(tx), Some(Recorder::spawn(rx, in_channels)))
        } else {
            (None, None)
        };

        self.live_chain.clear();
        self.live_instrument.clear();
        self.live_automation.clear();
        let tracks = (0..self.project.tracks.len()).map(|i| self.build_track(i, sample_rate)).collect::<Result<Vec<_>>>()?;
        // Queued before the stream starts, so the first callback picks them up.
        let _ = cmd_tx.push(Command::ResetTracks(Box::new(tracks)));
        let _ = cmd_tx.push(self.routing_command());
        let _ = cmd_tx.push(self.tempo_command());
        let _ = cmd_tx.push(Command::SetMasterGain(self.project.master_volume));
        let _ = cmd_tx.push(self.metronome_command());
        let _ = cmd_tx.push(self.count_in_command());
        let _ = cmd_tx.push(Command::SetAutoMonitor(self.project.auto_input_monitoring));
        let _ = cmd_tx.push(Command::SetSongEnd((self.project.length_secs() * sample_rate as f64).round() as u64));
        let _ = cmd_tx.push(self.loop_command_at(sample_rate));

        let mut mixer = Mixer::new(cmd_rx, g_tx, rec_tx, self.shared.clone(), sample_rate);
        let (midi_tx, midi_record) = rtrb::RingBuffer::new(MIDI_RECORD_RING);
        mixer.set_midi_record(midi_tx);
        let streams = audio::start(resolved, mixer)?;
        tracing::info!(
            "audio started: {} Hz, out '{}' ({} ch), in {:?} ({} ch)",
            streams.sample_rate,
            streams.output_name,
            streams.output_channels,
            streams.input_name,
            streams.input_channels
        );
        let setup = calibration::setup_key(streams.input_name.as_deref(), &streams.output_name, streams.sample_rate, config.buffer_size);
        self.record_offset_ms = self.latency_store.get(&setup);
        let (probe_tx, probe_results) = crossbeam_channel::bounded(2);
        self.audio = Some(AudioRuntime {
            sample_rate: streams.sample_rate,
            input_channels: streams.input_channels,
            streams: Some(streams),
            commands: cmd_tx,
            recorder,
            midi: midi_input::Connections::default(),
            midi_record,
            setup,
            probe_results,
            _gc: GarbageCollector::spawn(g_rx, probe_tx),
        });
        // The master bus's inserts too (the tracks went with ResetTracks).
        self.sync_master_chain()?;
        self.live_midi_targets.clear();
        self.sync_midi_targets()?;
        self.open_midi_inputs();
        Ok(self.status())
    }

    /// Test hook: run "audio" with a mixer the test drives by calling `process`.
    #[cfg(test)]
    pub(crate) fn attach_test_mixer(&mut self, sample_rate: u32) -> Mixer {
        self.attach_test_mixer_with_input(sample_rate, 0)
    }

    /// Test hook: as `attach_test_mixer`, with `in_channels` of input to record.
    #[cfg(test)]
    pub(crate) fn attach_test_mixer_with_input(&mut self, sample_rate: u32, in_channels: usize) -> Mixer {
        let (mut cmd_tx, cmd_rx) = rtrb::RingBuffer::new(4096);
        let (g_tx, g_rx) = rtrb::RingBuffer::new(4096);
        self.project.sample_rate = sample_rate;
        self.live_chain.clear();
        self.live_instrument.clear();
        self.live_automation.clear();
        let tracks = (0..self.project.tracks.len()).map(|i| self.build_track(i, sample_rate)).collect::<Result<Vec<_>>>().unwrap();
        let _ = cmd_tx.push(Command::ResetTracks(Box::new(tracks)));
        let _ = cmd_tx.push(self.routing_command());
        let (probe_tx, probe_results) = crossbeam_channel::bounded(2);
        let (rec_tx, recorder) = match in_channels {
            0 => (None, None),
            n => {
                let (tx, rx) = rtrb::RingBuffer::new(n * sample_rate as usize * 4);
                (Some(tx), Some(Recorder::spawn(rx, n)))
            }
        };
        let (midi_tx, midi_record) = rtrb::RingBuffer::new(MIDI_RECORD_RING);
        self.audio = Some(AudioRuntime {
            streams: None,
            sample_rate,
            input_channels: in_channels,
            commands: cmd_tx,
            recorder,
            midi: midi_input::Connections::default(),
            midi_record,
            setup: "test".into(),
            probe_results,
            _gc: GarbageCollector::spawn(g_rx, probe_tx),
        });
        self.sync_master_chain().unwrap();
        self.live_midi_targets.clear();
        self.sync_midi_targets().unwrap();
        let mut mixer = Mixer::new(cmd_rx, g_tx, rec_tx, self.shared.clone(), sample_rate);
        mixer.set_midi_record(midi_tx);
        mixer
    }

    pub fn stop_audio(&mut self) -> Result<()> {
        if self.taking {
            self.stop()?;
        }
        self.audio = None;
        self.live_chain.clear();
        self.live_instrument.clear();
        self.live_automation.clear();
        self.writing.clear();
        self.shared.playing.store(false, Ordering::Relaxed);
        Ok(())
    }

    fn tempo_command(&self) -> Command {
        Command::SetTempoMap(Box::new(self.project.tempo_map()))
    }

    /// Tell the audio thread where the last region ends, so playback stops there.
    pub(super) fn sync_song_end(&mut self) -> Result<()> {
        if self.audio.is_none() {
            return Ok(());
        }
        let end = (self.project.length_secs() * self.sample_rate() as f64).round() as u64;
        self.send(Command::SetSongEnd(end))
    }

    fn count_in_command(&self) -> Command {
        Command::SetCountIn { bars: self.project.count_in_bars, on_play: self.project.count_in_on_play }
    }

    // ---------------------------------------------------------------- runtime builders

    fn load_media(&mut self, path: &Path, sample_rate: u32) -> Result<Arc<AudioData>> {
        let key = (path.to_path_buf(), sample_rate);
        if let Some(d) = self.media.get(&key) {
            return Ok(d.clone());
        }
        let data = Arc::new(media::load_wav(path, sample_rate)?);
        self.media.insert(key, data.clone());
        Ok(data)
    }

    fn build_clip(&mut self, clip: &Clip, sr: u32) -> Result<ClipRt> {
        self.build_clip_with(clip, (clip.fade_in, clip.fade_out), sr)
    }

    /// A clip with the given (effective) fades.
    fn build_clip_with(&mut self, clip: &Clip, fades: (Fade, Fade), sr: u32) -> Result<ClipRt> {
        let data = self.clip_media(clip, sr)?;
        let secs = |s: f64| (s.max(0.0) * sr as f64).round() as u64;
        // The processed audio is already stretched, so the source offset scales with it. A
        // negative offset (a take that starts later than its folder) is silence first.
        let offset = clip.source_offset_secs * clip.stretch;
        let lead = (-offset).max(0.0).min(clip.length_secs);
        let (start, end) = (secs(clip.position_secs), secs(clip.position_secs + clip.length_secs));
        Ok(ClipRt {
            start: secs(clip.position_secs + lead),
            len: secs(clip.length_secs - lead),
            offset: secs(offset),
            gain: clip.gain,
            mute: clip.mute,
            data,
            fades: ClipFades::new(start, end.max(start), fades, sr as f64),
        })
    }

    /// A track's audio clips, crossfaded where they overlap; comped take folders play one
    /// clip per comp segment, crossfaded at the joins.
    fn build_clips(&mut self, index: usize, sr: u32) -> Vec<ClipRt> {
        let played = played_clips(&self.project.tracks[index].clips);
        played
            .iter()
            .filter_map(|(c, f)| match self.build_clip_with(c, *f, sr) {
                Ok(rt) => Some(rt),
                Err(e) => {
                    tracing::warn!("skipping clip '{}': {e:#}", c.name);
                    None
                }
            })
            .collect()
    }

    /// Everything the audio thread needs for one track. Records what was sent so later
    /// edits can be diffed against it.
    fn build_track(&mut self, index: usize, sr: u32) -> Result<Box<TrackRt>> {
        let id = self.track_id(index);
        let params = track_params(&self.project.tracks[index]);
        let mut rt = Box::new(if self.project.tracks[index].bus { TrackRt::new_bus(id, params) } else { TrackRt::new(id, params) });
        for clip in self.build_clips(index, sr) {
            rt.clips.push(clip);
        }
        for clip in self.build_midi(index, sr) {
            rt.midi.push(clip);
        }
        let track = self.project.tracks[index].clone();
        if let Some(slot) = &track.instrument {
            rt.instrument = self.make_processor(slot, sr)?;
        }
        self.live_instrument.insert(id, rt.instrument.as_ref().map(|p| p.instance));
        for slot in &track.plugins {
            if let Some(p) = self.make_processor(slot, sr)? {
                rt.plugins.push(p);
            }
        }
        self.live_chain.insert(id, rt.plugins.iter().map(|p| p.instance).collect());
        rt.sends = self.send_rts(index);
        rt.automation = self.envelopes(Some(index));
        self.live_automation.insert(id, rt.automation.iter().map(|e| e.target).collect());
        Ok(rt)
    }

    /// Push a track's audio clips to the audio thread after an edit.
    fn resync_clips(&mut self, index: usize) -> Result<()> {
        if self.audio.is_some() {
            let rt = self.build_clips(index, self.sample_rate());
            self.send(Command::ReplaceClips { track: self.track_id(index), clips: Box::new(rt) })?;
        }
        Ok(())
    }

    // ---------------------------------------------------------------- transport

    pub fn play(&mut self) -> Result<()> {
        self.send(Command::Play)
    }

    /// Stop playback; if recording, finalize files and add them as audio regions, and the
    /// MIDI played as MIDI regions on the armed instrument tracks (one undo step).
    pub fn stop(&mut self) -> Result<Recorded> {
        let taking = std::mem::take(&mut self.taking);
        self.send(Command::Stop)?;
        if !taking {
            return Ok(Recorded::default());
        }

        // Wait for the audio thread to acknowledge before closing files.
        let deadline = Instant::now() + Duration::from_millis(500);
        while self.shared.recording.load(Ordering::Relaxed) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        let files = match self.audio.as_ref().and_then(|a| a.recorder.as_ref()) {
            Some(r) => r.stop()?,
            None => Vec::new(),
        };
        self.drain_midi_take();
        let take = self.midi_take.take().unwrap_or_default();

        let placement = self.record_placement();
        // Undoing a take also takes the playhead back to where recording began.
        self.history.playhead = Some(placement.start as f64 / self.sample_rate() as f64);
        self.record("Record", None, |e| {
            let clips = e.add_recordings(files, &placement)?;
            let midi_regions = e.add_midi_take(take, &placement)?;
            Ok(Recorded { clips, midi_regions })
        })
    }

    pub fn seek(&mut self, secs: f64) -> Result<()> {
        let pos = (secs.max(0.0) * self.sample_rate() as f64) as u64;
        if self.audio.is_none() {
            self.shared.position.store(pos, Ordering::Relaxed);
        }
        self.send(Command::Seek(pos))
    }

    /// Start recording on all armed tracks (also starts playback, after any count-in): audio
    /// tracks record their input, instrument tracks the MIDI played (see live_midi.rs).
    pub fn start_recording(&mut self) -> Result<RecordingStarted> {
        let audio = self.audio.as_ref().ok_or_else(|| anyhow!("audio is not running; call start_audio first"))?;
        if self.taking || self.shared.recording.load(Ordering::Relaxed) {
            bail!("already recording");
        }
        let armed = |t: &&Track| t.armed && !t.bus;
        let audio_tracks: Vec<&Track> = self.project.tracks.iter().filter(armed).filter(|t| !t.is_instrument()).collect();
        let midi_tracks: Vec<usize> = (0..self.project.tracks.len()).filter(|&i| self.project.tracks[i].armed && self.project.tracks[i].is_instrument()).collect();
        if audio_tracks.is_empty() && midi_tracks.is_empty() {
            bail!("no tracks are armed for recording");
        }
        let mut paths = Vec::new();
        if !audio_tracks.is_empty() {
            let recorder = audio.recorder.as_ref().ok_or_else(|| anyhow!("no input device is open (disarm the audio tracks to record only MIDI)"))?;
            let in_channels = audio.input_channels;
            let dir = self.media_dir();
            let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            let targets: Vec<RecordTarget> = audio_tracks
                .iter()
                .map(|t| RecordTarget {
                    track_id: t.id,
                    channel: (t.input_channel as usize).min(in_channels - 1),
                    path: dir.join(format!("{}-{stamp}.wav", sanitize(&t.name))),
                })
                .collect();
            paths = targets.iter().map(|t| t.path.clone()).collect();
            recorder.start(targets, audio.sample_rate)?;
        }
        // Anything left from before belongs to no take.
        self.drain_midi_take();
        self.midi_take = Some(live_midi::MidiTake { tracks: midi_tracks.iter().map(|&i| self.project.tracks[i].id).collect(), events: Vec::new() });
        self.send(Command::Record(true))?;
        self.taking = true;
        Ok(RecordingStarted { files: paths, midi_tracks })
    }

    fn update_meters(&mut self) {
        let now = Instant::now();
        let dt = now.duration_since(self.meters.last).as_secs_f32();
        self.meters.last = now;
        let decay = 10f32.powf(-METER_DECAY_DB_PER_SEC * dt / 20.0);
        let m = &mut self.meters;
        for (i, v) in m.master.iter_mut().enumerate() {
            *v = SharedState::take_peak(&self.shared.master_peak[i]).max(*v * decay);
        }
        m.tracks.resize(self.project.tracks.len(), 0.0);
        for (i, v) in m.tracks.iter_mut().enumerate() {
            *v = SharedState::take_peak(&self.shared.track_peaks[i]).max(*v * decay);
        }
    }

    /// Linear peak levels (master L/R, per track) with decay applied.
    pub fn meters(&mut self) -> ([f32; 2], Vec<f32>) {
        self.update_meters();
        (self.meters.master, self.meters.tracks.clone())
    }

    pub fn status(&mut self) -> EngineStatus {
        self.update_meters();
        let sr = self.sample_rate();
        self.drain_midi_take();
        let db = |v: f32| if v > 0.0 { (20.0 * v.log10()).max(SILENCE_DB) } else { SILENCE_DB };
        let ms = |samples: u32| (samples as f64 * 1000.0 / sr.max(1) as f64 * 100.0).round() / 100.0;
        let keys = self.shared.midi_keys.each_ref().map(|k| k.load(Ordering::Relaxed));
        let streams = self.audio.as_ref().and_then(|a| a.streams.as_ref());
        EngineStatus {
            audio_running: self.audio.is_some(),
            sample_rate: sr,
            input_device: streams.and_then(|s| s.input_name.clone()),
            output_device: streams.map(|s| s.output_name.clone()),
            input_channels: streams.map_or(0, |s| s.input_channels),
            output_channels: streams.map_or(0, |s| s.output_channels),
            playing: self.shared.playing.load(Ordering::Relaxed),
            recording: self.shared.recording.load(Ordering::Relaxed),
            counting_in: self.shared.counting_in.load(Ordering::Relaxed),
            latency_ms: self.shared.latency.load(Ordering::Relaxed) as f64 * 1000.0 / sr.max(1) as f64,
            io_latency_ms: match self.shared.io_latency.load(Ordering::Relaxed) {
                _ if self.audio.is_none() => None,
                LATENCY_UNKNOWN => None,
                v => Some((v as f64 * 1000.0 / sr.max(1) as f64 * 100.0).round() / 100.0),
            },
            record_offset_ms: self.record_offset_ms,
            position_secs: self.shared.position.load(Ordering::Relaxed) as f64 / sr as f64,
            record_start_secs: self
                .shared
                .recording
                .load(Ordering::Relaxed)
                .then(|| self.shared.record_start.load(Ordering::Relaxed) as f64 / sr as f64),
            record_overrun_samples: self.shared.record_overruns.load(Ordering::Relaxed),
            master_peak_db: self.meters.master.map(db),
            track_peak_db: self.meters.tracks.iter().map(|&v| db(v)).collect(),
            output_latency_ms: match self.shared.output_latency.load(Ordering::Relaxed) {
                _ if self.audio.is_none() => None,
                LATENCY_UNKNOWN => None,
                v => Some(ms(v)),
            },
            midi_inputs: self.audio.as_ref().map(|a| a.midi.names()).unwrap_or_default(),
            midi_notes_received: self.shared.midi_notes.load(Ordering::Relaxed),
            midi_keys_down: (0..128u8).filter(|&p| keys[p as usize / 64] & (1 << (p % 64)) != 0).collect(),
            midi_input_tracks: self.midi_input_tracks(),
        }
    }

    pub fn set_tempo(&mut self, bpm: f64, time_signature: Option<(u32, u32)>) -> Result<()> {
        if !(20.0..=999.0).contains(&bpm) {
            bail!("tempo must be between 20 and 999 BPM");
        }
        self.record("Tempo", Some("tempo".into()), |e| {
            e.project.tempo_bpm = bpm;
            if let Some(sig) = time_signature {
                e.project.time_signature = sig;
            }
            let cmd = e.tempo_command();
            e.send(cmd)
        })
    }

    /// Master output gain (linear, 1.0 = 0 dB, max 4.0).
    pub fn set_master_volume(&mut self, gain: f32) -> Result<()> {
        self.record("Master Volume", Some("master".into()), |e| {
            let gain = gain.clamp(0.0, 4.0);
            e.project.master_volume = gain;
            e.send(Command::SetMasterGain(gain))
        })
    }

    fn metronome_command(&self) -> Command {
        Command::SetMetronome { enabled: self.project.metronome_enabled, gain: self.project.metronome_volume }
    }

    /// Metronome count-in: bars before recording (0 = off, max 4), and whether plain
    /// playback counts in too. Not an undoable edit.
    pub fn set_count_in(&mut self, bars: Option<u32>, on_play: Option<bool>) -> Result<()> {
        if let Some(b) = bars {
            self.project.count_in_bars = b.min(4);
        }
        if let Some(p) = on_play {
            self.project.count_in_on_play = p;
        }
        let cmd = self.count_in_command();
        self.send(cmd)
    }

    /// Auto input monitoring: armed tracks monitor their input while stopped or recording.
    /// A setting, not an undoable edit.
    pub fn set_auto_input_monitoring(&mut self, on: bool) -> Result<()> {
        self.project.auto_input_monitoring = on;
        self.send(Command::SetAutoMonitor(on))
    }

    /// Replace the tempo changes after the start (the opening tempo and signature are set
    /// with `set_tempo`). `merge` groups repeated edits (e.g. a drag) into one undo step.
    pub fn set_tempo_changes(&mut self, mut changes: Vec<TempoChange>, merge: Option<String>) -> Result<()> {
        for c in &changes {
            if !(c.at_secs.is_finite() && c.at_secs > 0.0) {
                bail!("tempo changes must be after the start (got {} s)", c.at_secs);
            }
            if !(20.0..=999.0).contains(&c.bpm) {
                bail!("tempo must be between 20 and 999 BPM (got {})", c.bpm);
            }
            if let Some((n, d)) = c.time_signature {
                if n == 0 || n > 32 || !matches!(d, 1 | 2 | 4 | 8 | 16 | 32) {
                    bail!("invalid time signature {n}/{d}");
                }
            }
        }
        changes.sort_by(|a, b| a.at_secs.total_cmp(&b.at_secs));
        self.record("Tempo Change", merge, |e| {
            e.project.tempo_changes = changes;
            let cmd = e.tempo_command();
            e.send(cmd)
        })
    }

    /// Turn the click on/off and/or set its level (linear, max 2.0). Not an undoable edit.
    pub fn set_metronome(&mut self, enabled: Option<bool>, volume: Option<f32>) -> Result<()> {
        if let Some(on) = enabled {
            self.project.metronome_enabled = on;
        }
        if let Some(v) = volume {
            self.project.metronome_volume = v.clamp(0.0, 2.0);
        }
        let cmd = self.metronome_command();
        self.send(cmd)
    }

    fn loop_command_at(&self, sr: u32) -> Command {
        let s = |secs: f64| (secs.max(0.0) * sr as f64).round() as u64;
        let p = &self.project;
        Command::SetLoop { enabled: p.loop_enabled, start: s(p.loop_start_secs), end: s(p.loop_end_secs) }
    }

    /// Cycle playback: turn it on/off and/or set its range (seconds). Like the metronome,
    /// this is a transport setting, not an undoable edit.
    pub fn set_loop(&mut self, enabled: Option<bool>, start_secs: Option<f64>, end_secs: Option<f64>) -> Result<()> {
        let start = start_secs.unwrap_or(self.project.loop_start_secs).max(0.0);
        let end = end_secs.unwrap_or(self.project.loop_end_secs).max(0.0);
        let (start, end) = (start.min(end), start.max(end));
        let enabled = enabled.unwrap_or(self.project.loop_enabled);
        if enabled && end - start < Self::MIN_CLIP_SECS {
            bail!("set a loop range first (start < end)");
        }
        self.project.loop_enabled = enabled;
        self.project.loop_start_secs = start;
        self.project.loop_end_secs = end;
        let cmd = self.loop_command_at(self.sample_rate());
        self.send(cmd)
    }

    // ---------------------------------------------------------------- tracks

    pub fn add_track(&mut self, name: &str) -> Result<usize> {
        self.record("New Track", None, |e| e.add_track_impl(name))
    }

    fn add_track_impl(&mut self, name: &str) -> Result<usize> {
        let mut track = Track::new(name);
        track.id = self.next_id();
        self.project.tracks.push(track);
        let index = self.project.tracks.len() - 1;
        if self.audio.is_some() {
            let rt = self.build_track(index, self.sample_rate())?;
            self.send(Command::AddTrack(rt))?;
        }
        Ok(index)
    }

    /// Move a track to position `to` in the track list (the others shift to make room).
    pub fn move_track(&mut self, from: usize, to: usize) -> Result<()> {
        let from = self.track_index(from)?;
        let to = to.min(self.project.tracks.len() - 1);
        if from == to {
            return Ok(());
        }
        self.record("Move Track", None, |e| {
            let track = e.project.tracks.remove(from);
            e.project.tracks.insert(to, track);
            // Meters are reported in this order, so the audio thread follows it.
            e.send(Command::ReorderTracks(Box::new(e.project.tracks.iter().map(|t| t.id).collect())))?;
            e.sync_routing()
        })
    }

    pub fn remove_track(&mut self, index: usize) -> Result<Track> {
        let index = self.track_index(index)?;
        self.record("Delete Track", None, |e| {
            e.unroute_bus(index)?;
            e.drop_keys_from(index)?;
            let track = e.project.tracks.remove(index);
            e.live_chain.remove(&track.id);
            e.live_instrument.remove(&track.id);
            e.send(Command::RemoveTrack(track.id))?;
            e.sync_routing()?;
            Ok(track)
        })
    }

    pub fn update_track(&mut self, index: usize, u: TrackUpdate) -> Result<Track> {
        let index = self.track_index(index)?;
        let (label, key) = u.describe(self.track_id(index));
        self.record(label, key, |e| {
            let t = &mut e.project.tracks[index];
            if let Some(v) = u.name {
                t.name = v;
            }
            if let Some(v) = u.volume {
                t.volume = v.clamp(0.0, 4.0);
            }
            if let Some(v) = u.pan {
                t.pan = v.clamp(-1.0, 1.0);
            }
            if let Some(v) = u.mute {
                t.mute = v;
            }
            if let Some(v) = u.solo {
                t.solo = v;
            }
            if let Some(v) = u.armed {
                t.armed = v;
            }
            if let Some(v) = u.monitor_input {
                t.monitor_input = v;
            }
            if let Some(v) = u.input_channel {
                t.input_channel = v;
            }
            let track = t.clone();
            e.send(Command::SetTrack { track: track.id, params: track_params(&track) })?;
            Ok(track)
        })
    }

    // ---------------------------------------------------------------- audio clips

    /// Place an audio file on a track. Length defaults to the whole file.
    pub fn add_clip(&mut self, track: usize, file: &Path, position_secs: f64) -> Result<Clip> {
        let index = self.track_index(track)?;
        let file = std::path::absolute(file)?;
        let sr = self.sample_rate();
        let data = self.load_media(&file, sr)?;
        let clip = Clip {
            name: file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
            file,
            position_secs: position_secs.max(0.0),
            length_secs: data.frames() as f64 / sr as f64,
            source_offset_secs: 0.0,
            gain: 1.0,
            mute: false,
            takes: Vec::new(),
            active_take: 0,
            pitch_semitones: 0,
            pitch_cents: 0,
            stretch: 1.0,
            fade_in: Fade::default(),
            fade_out: Fade::default(),
            comp: Vec::new(),
            show_takes: false,
        };
        self.record("Add Region", None, |e| {
            e.project.tracks[index].clips.push(clip.clone());
            // The whole track: an overlap changes its neighbours' crossfades too.
            e.resync_clips(index)?;
            Ok(clip)
        })
    }

    /// Place an existing clip (e.g. from a copy) on a track. Returns its new index.
    pub fn insert_clip(&mut self, track: usize, clip: Clip) -> Result<usize> {
        self.record("Paste Region", None, |e| e.insert_clip_impl(track, clip))
    }

    fn insert_clip_impl(&mut self, track: usize, clip: Clip) -> Result<usize> {
        let index = self.track_index(track)?;
        let sr = self.sample_rate();
        let clip = Clip { position_secs: clip.position_secs.max(0.0), ..clip };
        // Validates that the media is loadable before touching the project.
        self.build_clip(&clip, sr)?;
        self.project.tracks[index].clips.push(clip);
        self.resync_clips(index)?;
        Ok(self.project.tracks[index].clips.len() - 1)
    }

    /// Copy a clip to a new position, optionally on another track. Returns the copy's
    /// (track, index).
    pub fn copy_clip(&mut self, track: usize, clip: usize, position_secs: f64, to_track: Option<usize>) -> Result<(usize, usize)> {
        let from = self.clip_index(track, clip)?;
        let to = self.track_index(to_track.unwrap_or(track))?;
        let copy = Clip { position_secs, ..self.project.tracks[from].clips[clip].clone() };
        self.record("Copy Region", None, |e| Ok((to, e.insert_clip_impl(to, copy)?)))
    }

    pub fn remove_clip(&mut self, track: usize, clip: usize) -> Result<Clip> {
        let index = self.clip_index(track, clip)?;
        self.record("Delete Region", None, |e| {
            let removed = e.project.tracks[index].clips.remove(clip);
            e.resync_clips(index)?;
            Ok(removed)
        })
    }

    /// Move a clip on the timeline (and optionally to another track).
    pub fn move_clip(&mut self, track: usize, clip: usize, position_secs: f64, to_track: Option<usize>) -> Result<Clip> {
        let from = self.clip_index(track, clip)?;
        let to = self.track_index(to_track.unwrap_or(track))?;
        self.record("Move Region", None, |e| {
            let mut c = e.project.tracks[from].clips.remove(clip);
            c.position_secs = position_secs.max(0.0);
            e.project.tracks[to].clips.push(c.clone());
            e.resync_clips(from)?;
            if to != from {
                e.resync_clips(to)?;
            }
            Ok(c)
        })
    }

    /// Shortest region that split/trim will produce (seconds).
    pub const MIN_CLIP_SECS: f64 = 0.01;

    fn clip_index(&self, track: usize, clip: usize) -> Result<usize> {
        let index = self.track_index(track)?;
        if clip >= self.project.tracks[index].clips.len() {
            bail!("track {track} has no clip at index {clip}");
        }
        Ok(index)
    }

    /// Length of a clip's source file in seconds.
    fn source_secs(&mut self, clip: &Clip) -> Result<f64> {
        let sr = self.sample_rate();
        let data = self.load_media(&clip.file, sr)?;
        Ok(data.frames() as f64 / sr as f64)
    }

    /// Split a clip at a timeline position. The left part keeps the index, the right
    /// part is inserted right after it. Returns (left, right).
    pub fn split_clip(&mut self, track: usize, clip: usize, at_secs: f64) -> Result<(Clip, Clip)> {
        let index = self.clip_index(track, clip)?;
        let c = self.project.tracks[index].clips[clip].clone();
        let (start, end) = (c.position_secs, c.position_secs + c.length_secs);
        if at_secs <= start + Self::MIN_CLIP_SECS || at_secs >= end - Self::MIN_CLIP_SECS {
            bail!("split point {at_secs:.3}s is outside clip '{}' ({start:.3}s – {end:.3}s)", c.name);
        }
        let (left, right) = c.split(at_secs - start);
        self.record("Split Region", None, |e| {
            let clips = &mut e.project.tracks[index].clips;
            clips[clip] = left.clone();
            clips.insert(clip + 1, right.clone());
            e.resync_clips(index)?;
            Ok((left, right))
        })
    }

    /// Move a clip's start and/or end edge (timeline seconds) without moving its audio.
    /// Edges are clamped to the source file and a minimum length.
    pub fn trim_clip(&mut self, track: usize, clip: usize, start_secs: Option<f64>, end_secs: Option<f64>) -> Result<Clip> {
        let index = self.clip_index(track, clip)?;
        let c = self.project.tracks[index].clips[clip].clone();
        let source = self.source_secs(&c)?;
        // Timeline time where the source file starts / ends for this clip (offsets are in
        // source seconds; the timeline plays them `stretch` times as long).
        let source_start = c.position_secs - c.source_offset_secs * c.stretch;
        let source_end = source_start + source * c.stretch;
        let end = end_secs.unwrap_or(c.position_secs + c.length_secs).min(source_end);
        let start = start_secs.unwrap_or(c.position_secs).max(source_start).max(0.0);
        if end - start < Self::MIN_CLIP_SECS {
            bail!("trim would leave clip '{}' shorter than {} ms", c.name, Self::MIN_CLIP_SECS * 1000.0);
        }
        let moved = start - c.position_secs;
        let mut trimmed = Clip {
            position_secs: start,
            length_secs: end - start,
            source_offset_secs: (start - source_start) / c.stretch,
            ..c
        };
        trimmed.clamp_fades();
        trimmed.crop_comp(moved);
        self.record("Trim Region", None, |e| {
            e.project.tracks[index].clips[clip] = trimmed.clone();
            e.resync_clips(index)?;
            Ok(trimmed)
        })
    }

    /// Waveform overview of a media file (cached).
    pub fn waveform(&mut self, file: &Path) -> Result<Arc<Waveform>> {
        if let Some(w) = self.waveforms.get(file) {
            return Ok(w.clone());
        }
        let sr = self.sample_rate();
        let data = self.load_media(file, sr)?;
        let w = Arc::new(Waveform::from_audio(&data));
        self.waveforms.insert(file.to_path_buf(), w.clone());
        Ok(w)
    }

    // ---------------------------------------------------------------- project files

    /// Start a new project with one empty audio track.
    pub fn new_project(&mut self, name: &str) -> Result<()> {
        let project = Project { name: name.to_string(), sample_rate: self.sample_rate(), ..Project::default() };
        self.replace_project(project, None)?;
        self.add_track_impl("Audio 1")?;
        Ok(())
    }

    /// Save as Reaper `.rpp`, DAWproject or native `.otp`/`.json`, chosen by extension.
    pub fn save_project(&mut self, path: Option<&Path>) -> Result<PathBuf> {
        let mut path = match path {
            Some(p) => std::path::absolute(p)?,
            None => self.project_path.clone().ok_or_else(|| anyhow!("project has never been saved; give a path"))?,
        };
        if path.extension().is_none() {
            path.set_extension(PROJECT_EXTENSION);
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // Files refer to buses by track index (ids are per session).
        let mut file = self.project_with_plugin_state();
        file.outputs_to_indices();
        if is_rpp(&path) {
            opentake_project::rpp::save(&file, &path)?;
        } else if is_ext(&path, "dawproject") {
            opentake_project::dawproject::save(&file, &path)?;
        } else {
            std::fs::write(&path, serde_json::to_string_pretty(&file)?)?;
        }
        self.project_path = Some(path.clone());
        Ok(path)
    }

    /// Load a project (`.otp`/`.json`, `.rpp`, or another DAW's: see `import`). Returns
    /// warnings (missing media/plugins, what an import couldn't carry over).
    pub fn load_project(&mut self, path: &Path) -> Result<Vec<String>> {
        let path = std::path::absolute(path)?;
        if opentake_project::import::is_import_path(&path) {
            // Imported projects get no path, so Save asks where to put the .otp instead of
            // overwriting the other DAW's file.
            let imported = opentake_project::import::load(&path).with_context(|| format!("importing {}", path.display()))?;
            let mut warnings = imported.warnings;
            warnings.extend(self.replace_project(imported.project, None)?);
            return Ok(warnings);
        }
        let project = if is_rpp(&path) {
            opentake_project::rpp::load(&path)?
        } else {
            serde_json::from_str(&std::fs::read_to_string(&path)?).context("parsing project JSON")?
        };
        self.replace_project(project, Some(path))
    }

    fn replace_project(&mut self, mut project: Project, path: Option<PathBuf>) -> Result<Vec<String>> {
        if self.taking {
            self.stop()?;
        }
        let mut warnings = Vec::new();
        for track in &mut project.tracks {
            track.id = self.next_id();
            if track.bus {
                // A bus only mixes (see `record`).
                (track.clips, track.midi_clips, track.instrument, track.armed, track.monitor_input) = (Vec::new(), Vec::new(), None, false, false);
            }
            let name = track.name.clone();
            for slot in track.instrument.iter_mut().chain(track.plugins.iter_mut()) {
                slot.instance = match self.load_slot(slot) {
                    Ok(instance) => instance,
                    Err(e) => {
                        warnings.push(format!("track '{name}': plugin '{}' not loaded: {e:#}", slot.name));
                        0
                    }
                };
            }
            for clip in &track.clips {
                if !clip.file.exists() {
                    warnings.push(format!("track '{name}': missing media {}", clip.file.display()));
                }
            }
        }
        project.outputs_from_indices();
        project.assign_slot_ids();
        project.assign_send_ids();
        project.prune_automation();

        if guitarpro::upgrade_song_tempo(&mut project) {
            warnings.push("The imported song's tempo changes were added to the project's tempo map".into());
        }
        for slot in &mut project.master_plugins {
            slot.instance = match self.load_slot(slot) {
                Ok(instance) => instance,
                Err(e) => {
                    warnings.push(format!("master: plugin '{}' not loaded: {e:#}", slot.name));
                    0
                }
            };
        }

        self.project = project;
        self.project_path = path;
        self.writing.clear();
        self.media.clear();
        self.flexed.clear();
        self.waveforms.clear();
        self.history.clear();
        self.prune_pool();
        self.seek(0.0)?;

        if self.audio.is_some() {
            let sr = self.sample_rate();
            self.project.sample_rate = sr;
            self.live_chain.clear();
            self.live_instrument.clear();
            self.live_automation.clear();
            let tracks = (0..self.project.tracks.len()).map(|i| self.build_track(i, sr)).collect::<Result<Vec<_>>>()?;
            self.send(Command::ResetTracks(Box::new(tracks)))?;
            self.sync_routing()?;
            let tempo = self.tempo_command();
            self.send(tempo)?;
            self.send(Command::SetMasterGain(self.project.master_volume))?;
            let metronome = self.metronome_command();
            self.send(metronome)?;
            let count_in = self.count_in_command();
            self.send(count_in)?;
            self.send(Command::SetAutoMonitor(self.project.auto_input_monitoring))?;
            self.sync_song_end()?;
            let looping = self.loop_command_at(sr);
            self.send(looping)?;
            self.sync_master_chain()?;
            self.live_midi_targets.clear();
        }
        self.selected_track = None;
        self.sync_midi_targets()?;
        Ok(warnings)
    }
}

fn track_params(t: &Track) -> TrackParams {
    TrackParams {
        volume: t.volume,
        pan: t.pan,
        mute: t.mute,
        solo: t.solo,
        armed: t.armed,
        monitor: t.monitor_input,
        input_channel: t.input_channel as usize,
        output: t.output.unwrap_or(crate::mixer::MASTER),
    }
}

/// Messages recorded live between two drains (the GUI drains every frame; an agent's
/// recording is drained on stop, so this holds a long take).
const MIDI_RECORD_RING: usize = 1 << 16;

/// OpenTake's own project file extension (JSON inside).
pub const PROJECT_EXTENSION: &str = "otp";

/// Files `load_project` opens: OpenTake projects (`.otp`, or `.json` from earlier versions),
/// Reaper projects (`.rpp`) and other DAWs' projects it imports.
pub fn is_project_path(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "otp" | "json" | "rpp"))
        || opentake_project::import::is_import_path(path)
}

fn is_rpp(path: &Path) -> bool {
    is_ext(path, "rpp")
}

fn is_ext(path: &Path, ext: &str) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case(ext))
}

/// "VST3: ReaEQ (Cockos)" -> "ReaEQ"
fn display_name(full: &str) -> Option<String> {
    let rest = full.split_once(": ").map_or(full, |(_, r)| r);
    let name = rest.rsplit_once(" (").map_or(rest, |(n, _)| n);
    (!name.is_empty()).then(|| name.to_string())
}

fn sanitize(name: &str) -> String {
    let s: String = name.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    if s.is_empty() { "track".into() } else { s }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn tone_file(dir: &Path, secs: f32) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let wav = dir.join("tone.wav");
        let spec = hound::WavSpec { channels: 1, sample_rate: 48_000, bits_per_sample: 32, sample_format: hound::SampleFormat::Float };
        let mut w = hound::WavWriter::create(&wav, spec).unwrap();
        for i in 0..(48_000.0 * secs) as usize {
            w.write_sample((i as f32 * 0.05).sin() * 0.5).unwrap();
        }
        w.finalize().unwrap();
        wav
    }

    #[test]
    fn dynamics_meters_follow_the_audio() {
        let dir = std::env::temp_dir().join(format!("opentake-meter-{}", std::process::id()));
        let wav = tone_file(&dir, 1.0); // peaks at -6 dBFS
        let mut e = Engine::new();
        e.add_track("Drums").unwrap();
        e.add_clip(0, &wav, 0.0).unwrap();
        e.insert_plugin(0, "comp", None).unwrap();
        e.insert_plugin(0, "limiter", None).unwrap();
        e.insert_master_plugin("comp", None).unwrap();
        e.insert_master_plugin("limiter", None).unwrap();
        let mut mixer = e.attach_test_mixer(48_000);
        e.play().unwrap();
        let mut out = vec![0.0f32; 2 * 512];
        for _ in 0..20 {
            mixer.process(&[], 0, &mut out, 2);
        }
        for s in [SlotRef::Effect(0), SlotRef::Effect(1), SlotRef::Master(0), SlotRef::Master(1)] {
            let (_, input, output) = e.fx_meter(0, s).expect("meter").take();
            assert!(input > -10.0 && output > -20.0, "{s:?}: in {input} out {output}");
        }
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn move_track_reorders_and_undoes() {
        let mut e = Engine::new();
        for name in ["A", "B", "C"] {
            e.add_track(name).unwrap();
        }
        let names = |e: &Engine| e.project().tracks.iter().map(|t| t.name.clone()).collect::<Vec<_>>();
        e.move_track(0, 2).unwrap();
        assert_eq!(names(&e), ["B", "C", "A"]);
        e.move_track(2, 0).unwrap();
        e.move_track(1, 9).unwrap(); // past the end = last
        assert_eq!(names(&e), ["A", "C", "B"]);
        e.undo().unwrap();
        assert_eq!(names(&e), ["A", "B", "C"]);
        assert!(e.move_track(5, 0).is_err());
    }

    #[test]
    fn split_and_trim_clips() {
        let dir = std::env::temp_dir().join(format!("opentake-edit-{}", std::process::id()));
        let wav = tone_file(&dir, 4.0);
        let mut e = Engine::new();
        let t = e.add_track("T").unwrap();
        e.add_clip(t, &wav, 1.0).unwrap(); // 1.0 .. 5.0

        let (l, r) = e.split_clip(t, 0, 2.5).unwrap();
        assert_eq!((l.position_secs, l.length_secs, l.source_offset_secs), (1.0, 1.5, 0.0));
        assert_eq!((r.position_secs, r.length_secs, r.source_offset_secs), (2.5, 2.5, 1.5));
        assert!(e.split_clip(t, 0, 0.5).is_err(), "split outside clip must fail");

        // Trim the right part's start later and its end beyond the source (clamped).
        let c = e.trim_clip(t, 1, Some(3.0), Some(99.0)).unwrap();
        assert_eq!((c.position_secs, c.source_offset_secs), (3.0, 2.0));
        assert!((c.position_secs + c.length_secs - 5.0).abs() < 1e-9, "end clamped to source end");
        // Extend the start back before the source start: clamped to where the file begins.
        let c = e.trim_clip(t, 1, Some(0.0), None).unwrap();
        assert_eq!((c.position_secs, c.source_offset_secs), (1.0, 0.0));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn copy_clip_keeps_edits_and_original() {
        let dir = std::env::temp_dir().join(format!("opentake-copy-{}", std::process::id()));
        let wav = tone_file(&dir, 4.0);
        let mut e = Engine::new();
        let a = e.add_track("A").unwrap();
        let b = e.add_track("B").unwrap();
        e.add_clip(a, &wav, 0.0).unwrap();
        e.trim_clip(a, 0, Some(1.0), Some(3.0)).unwrap();
        let (t, i) = e.copy_clip(a, 0, 10.0, Some(b)).unwrap();
        assert_eq!((t, i), (b, 0));
        let orig = &e.project().tracks[a].clips[0];
        let copy = &e.project().tracks[b].clips[0];
        assert_eq!(orig.position_secs, 1.0);
        assert_eq!((copy.position_secs, copy.length_secs, copy.source_offset_secs), (10.0, 2.0, 1.0));
        assert!(e.copy_clip(a, 5, 0.0, None).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn new_project_has_one_track() {
        let mut e = Engine::new();
        e.new_project("Song").unwrap();
        assert_eq!(e.project().tracks.len(), 1);
        assert_eq!(e.project().tracks[0].name, "Audio 1");
        assert!(e.history_info().undo.is_none(), "a fresh project has nothing to undo");
    }

    #[test]
    fn display_name_parses_reaper_names() {
        assert_eq!(display_name("VST3: ReaEQ (Cockos)").as_deref(), Some("ReaEQ"));
        assert_eq!(display_name("Plain").as_deref(), Some("Plain"));
    }

    #[test]
    fn project_editing_without_audio() {
        let dir = std::env::temp_dir().join(format!("opentake-test-{}", std::process::id()));
        let wav = tone_file(&dir, 1.0);
        let mut e = Engine::new();
        let t = e.add_track("Guitar").unwrap();
        e.update_track(t, TrackUpdate { volume: Some(0.5), armed: Some(true), ..Default::default() }).unwrap();
        let clip = e.add_clip(t, &wav, 2.0).unwrap();
        assert!((clip.length_secs - 1.0).abs() < 1e-9);

        let rpp = dir.join("song.rpp");
        e.save_project(Some(&rpp)).unwrap();
        let mut e2 = Engine::new();
        let warnings = e2.load_project(&rpp).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        let tr = &e2.project().tracks[0];
        assert_eq!((tr.name.as_str(), tr.volume, tr.armed), ("Guitar", 0.5, true));
        assert_eq!(tr.clips[0].file, wav);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dawproject_export_reimports_without_a_path() {
        let dir = std::env::temp_dir().join(format!("opentake-test-daw-{}", std::process::id()));
        let wav = tone_file(&dir, 1.0);
        let mut e = Engine::new();
        let t = e.add_track("Guitar").unwrap();
        e.add_clip(t, &wav, 2.0).unwrap();
        let file = dir.join("song.dawproject");
        e.save_project(Some(&file)).unwrap();
        let mut e2 = Engine::new();
        let warnings = e2.load_project(&file).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        // Saving must not overwrite the imported file in place.
        assert_eq!(e2.project_path(), None);
        let c = &e2.project().tracks[0].clips[0];
        assert_eq!(c.file, dir.join("song Media/audio").join(wav.file_name().unwrap()));
        assert!(c.file.exists() && (c.position_secs - 2.0).abs() < 1e-9);
        std::fs::remove_dir_all(&dir).ok();
    }
}
