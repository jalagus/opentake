//! Audio-thread state. Everything in here runs inside the device callback, so it
//! must not block or allocate: structural changes arrive as pre-built boxes over a
//! lock-free queue, and anything removed is sent back to be freed on another thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};

use rtrb::{Consumer, Producer};

use opentake_project::{Fade, FadeCurve, TempoMap};
use opentake_project::automation::{AutomationPoint, envelope_at, pos_to_gain};

use crate::media::AudioData;

/// Internal processing block size; device buffers are split into chunks of this size.
pub const MAX_BLOCK: usize = 512;
pub const MAX_TRACKS: usize = 256;
pub const MAX_INPUT_CHANNELS: usize = 64;
const CLIP_CAPACITY: usize = 1024;
pub const PLUGIN_CAPACITY: usize = 32;
const MIDI_CLIP_CAPACITY: usize = 512;
const EVENT_CAPACITY: usize = 1024;
const LIVE_CAPACITY: usize = 128;
/// Longest plug-in delay that's compensated per track (samples, a power of two).
const MAX_COMP: usize = 16_384;
/// Longest compensation delay: a block is written to the delay line before sends tap it, so
/// a tap must stay a block clear of the samples being overwritten.
const MAX_DELAY: usize = MAX_COMP - MAX_BLOCK;
/// Track id addressing the master bus in plug-in commands.
pub const MASTER: u64 = u64::MAX;
/// `SharedState::io_latency` before anything was measured.
pub const LATENCY_UNKNOWN: u32 = u32::MAX;
/// `SharedState::record_wrap` when a recording didn't cycle.
pub const NO_WRAP: u64 = u64::MAX;
/// While automation plays, blocks are at most this long: plug-in parameters are set once per
/// block, fader and pan ramp across it.
pub const AUTOMATION_BLOCK: usize = 64;

#[derive(Debug, Clone, Copy, Default)]
pub struct Transport {
    pub playing: bool,
    pub recording: bool,
    pub tempo_bpm: f64,
    pub time_sig: (i32, i32),
    pub position_samples: i64,
}

/// A note event inside a block. `velocity` 0 = note-off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MidiEvent {
    pub offset: u32,
    pub pitch: u8,
    pub velocity: u8,
}

/// An instrument or insert effect on the audio thread.
pub trait Processor: Send {
    /// Process a stereo block. `inputs` and `outputs` each hold two channels; `events`
    /// (sorted by offset) are only non-empty for instruments.
    fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], frames: usize, transport: &Transport, events: &[MidiEvent]);
    fn set_param(&mut self, id: u32, normalized: f64);
    /// Processing delay in samples (look-ahead etc.); the mixer compensates for it.
    fn latency(&self) -> usize {
        0
    }
}

pub struct PluginRt {
    /// Engine plugin-instance id (stable across edits/undo).
    pub instance: u64,
    pub processor: Box<dyn Processor>,
    pub bypass: bool,
}

/// A note in absolute timeline samples (end already clamped to its region).
#[derive(Debug, Clone, Copy)]
pub struct NoteRt {
    pub start: u64,
    pub end: u64,
    pub pitch: u8,
    pub velocity: u8,
}

/// A MIDI region; `notes` sorted by start.
#[derive(Debug, Clone)]
pub struct MidiClipRt {
    pub start: u64,
    pub end: u64,
    pub mute: bool,
    pub notes: Vec<NoteRt>,
}

#[derive(Clone)]
pub struct ClipRt {
    pub start: u64,
    pub len: u64,
    pub offset: u64,
    pub gain: f32,
    pub mute: bool,
    pub data: Arc<AudioData>,
    pub fades: ClipFades,
}

/// A region's fade-in and fade-out in timeline samples, evaluated per sample.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClipFades {
    /// The region's timeline span (before any silent lead-in of a later take).
    pub start: u64,
    pub end: u64,
    pub fade_in: u64,
    pub fade_out: u64,
    pub curve_in: FadeCurve,
    pub curve_out: FadeCurve,
}

impl ClipFades {
    pub const NONE: ClipFades = ClipFades { start: 0, end: u64::MAX, fade_in: 0, fade_out: 0, curve_in: FadeCurve::Linear, curve_out: FadeCurve::Linear };

    /// Fades for a region at `start..end` samples with (effective) fades in seconds.
    pub fn new(start: u64, end: u64, (fade_in, fade_out): (Fade, Fade), sr: f64) -> Self {
        let samples = |f: Fade| ((f.secs.max(0.0) * sr).round() as u64).min(end - start);
        ClipFades { start, end, fade_in: samples(fade_in), fade_out: samples(fade_out), curve_in: fade_in.curve, curve_out: fade_out.curve }
    }

    /// Whether `from..to` (timeline samples) plays at full level, so gain needn't be
    /// evaluated per sample.
    pub fn flat(&self, from: u64, to: u64) -> bool {
        from >= self.start + self.fade_in && to <= self.end.saturating_sub(self.fade_out)
    }

    /// Fade gain at timeline sample `t`.
    pub fn gain(&self, t: u64) -> f32 {
        let mut g = 1.0;
        if t < self.start + self.fade_in {
            g = self.curve_in.gain((t.saturating_sub(self.start)) as f32 / self.fade_in as f32);
        }
        if self.fade_out > 0 && t + self.fade_out >= self.end {
            g *= self.curve_out.gain(self.end.saturating_sub(t) as f32 / self.fade_out as f32);
        }
        g
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrackParams {
    pub volume: f32,
    pub pan: f32,
    pub mute: bool,
    pub solo: bool,
    pub armed: bool,
    pub monitor: bool,
    pub input_channel: usize,
    /// The bus track (id) this track feeds, or `MASTER`.
    pub output: u64,
}

/// What an envelope moves on the audio thread.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EnvTarget {
    Volume,
    Pan,
    Mute,
    Param { instance: u64, id: u32 },
    /// A send's level (`SendRt::id`).
    Send { id: u32 },
}

/// A send to a bus, built off-thread.
pub struct SendRt {
    /// Id within the track (what automation names).
    pub id: u32,
    /// The bus track (id) it feeds.
    pub bus: u64,
    pub gain: f32,
    pub pre_fader: bool,
    pub mute: bool,
    /// Resolved on the audio thread: the bus's index, the delay that lines this path up
    /// with everything else reaching the bus, and the gains the last block ended on.
    dest: Option<usize>,
    delay: usize,
    gains: Option<[f32; 2]>,
}

impl SendRt {
    pub fn new(id: u32, bus: u64, gain: f32, pre_fader: bool, mute: bool) -> Self {
        Self { id, bus, gain, pre_fader, mute, dest: None, delay: 0, gains: None }
    }
}

/// An automation envelope, built off-thread (points sorted, in seconds).
pub struct EnvelopeRt {
    pub target: EnvTarget,
    pub points: Vec<AutomationPoint>,
    /// Value last sent to the plug-in (NaN = none yet), so parameters are only set as they move.
    last: f32,
}

impl EnvelopeRt {
    pub fn new(target: EnvTarget, points: Vec<AutomationPoint>) -> Self {
        Self { target, points, last: f32::NAN }
    }

    fn at(&self, secs: f64) -> f32 {
        envelope_at(&self.points, secs, self.target == EnvTarget::Mute).unwrap_or(0.0)
    }
}

pub struct TrackRt {
    pub id: u64,
    /// A bus sums the tracks routed to it (into `buf`) instead of playing regions.
    pub bus: bool,
    pub params: TrackParams,
    pub clips: Vec<ClipRt>,
    pub midi: Vec<MidiClipRt>,
    pub instrument: Option<Box<PluginRt>>,
    pub plugins: Vec<Box<PluginRt>>,
    buf: [Vec<f32>; 2],
    events: Vec<MidiEvent>,
    /// Notes played live (piano-roll audition) waiting for the next block.
    live: Vec<MidiEvent>,
    /// Which pitches are sounding, so they can be released on stop/seek/edit.
    active: [bool; 128],
    flush: bool,
    /// This track's plug-in delay (samples), refreshed every callback.
    latency: usize,
    /// Plug-in delay of everything before this track's output (a bus: its slowest input plus
    /// its own chain), and the slowest arrival among a bus's inputs.
    arrival: usize,
    in_latency: usize,
    /// Delay line that lines the track up with the slowest one reaching the same bus or the
    /// master (plug-in delay compensation), and its length.
    comp: [Vec<f32>; 2],
    comp_write: usize,
    delay: usize,
    /// Routing resolved on the audio thread: the index of the bus this track feeds (`None`
    /// = master), and whether it reaches the output under the current mute/solo state.
    dest: Option<usize>,
    audible: bool,
    /// Active automation lanes, and the channel gains (fader, pan, mute) the last block
    /// ended on, which the next one ramps from.
    pub automation: Vec<EnvelopeRt>,
    gains: Option<[f32; 2]>,
    /// Sends to buses, tapped off the compensation delay line (see `process_block`).
    pub sends: Vec<SendRt>,
    /// Feeds a soloed bus (solo bookkeeping in `rebuild_routing`).
    feeds_solo: bool,
}

impl TrackRt {
    /// Allocates everything the track will need on the audio thread. Call off-thread.
    pub fn new(id: u64, params: TrackParams) -> Self {
        Self {
            id,
            bus: false,
            params,
            clips: Vec::with_capacity(CLIP_CAPACITY),
            midi: Vec::with_capacity(MIDI_CLIP_CAPACITY),
            instrument: None,
            plugins: Vec::with_capacity(PLUGIN_CAPACITY),
            buf: [vec![0.0; MAX_BLOCK], vec![0.0; MAX_BLOCK]],
            events: Vec::with_capacity(EVENT_CAPACITY),
            live: Vec::with_capacity(LIVE_CAPACITY),
            active: [false; 128],
            flush: false,
            latency: 0,
            arrival: 0,
            in_latency: 0,
            comp: [vec![0.0; MAX_COMP], vec![0.0; MAX_COMP]],
            comp_write: 0,
            delay: 0,
            dest: None,
            audible: true,
            automation: Vec::new(),
            gains: None,
            sends: Vec::new(),
            feeds_solo: false,
        }
    }

    /// A bus track.
    pub fn new_bus(id: u64, params: TrackParams) -> Self {
        Self { bus: true, ..Self::new(id, params) }
    }
}

pub enum Command {
    Play,
    Stop,
    Seek(u64),
    Record(bool),
    /// Replace the tempo map (built off-thread).
    SetTempoMap(Box<TempoMap>),
    /// Metronome count-in bars before recording (0 = off), and whether plain playback
    /// counts in too.
    SetCountIn { bars: u32, on_play: bool },
    /// Armed tracks monitor their input while stopped or recording.
    SetAutoMonitor(bool),
    /// End of the last region (samples); playback stops when it passes it (0 = never).
    SetSongEnd(u64),
    SetMasterGain(f32),
    SetMetronome { enabled: bool, gain: f32 },
    /// Cycle playback (and recording) between two sample positions.
    SetLoop { enabled: bool, start: u64, end: u64 },
    AddTrack(Box<TrackRt>),
    RemoveTrack(u64),
    ResetTracks(Box<Vec<Box<TrackRt>>>),
    SetTrack { track: u64, params: TrackParams },
    AddClip { track: u64, clip: ClipRt },
    ReplaceClips { track: u64, clips: Box<Vec<ClipRt>> },
    ReplaceMidi { track: u64, clips: Box<Vec<MidiClipRt>> },
    /// Rebuild a track's effect chain in `order` (instance, bypass), reusing processors
    /// already on the track and taking new ones from `new`. `target` is an empty vec with
    /// enough capacity, so the audio thread never allocates.
    SetPluginChain { track: u64, order: Box<Vec<(u64, bool)>>, new: Box<Vec<Box<PluginRt>>>, target: Box<Vec<Box<PluginRt>>> },
    SetInstrument { track: u64, plugin: Option<Box<PluginRt>> },
    SetPluginBypass { track: u64, instance: u64, bypass: bool },
    SetPluginParam { track: u64, instance: u64, id: u32, value: f64 },
    /// Put tracks in this id order (mixer order = meter order = project order).
    ReorderTracks(Box<Vec<u64>>),
    /// The order to process tracks in (ids): every track before the buses it feeds. Tracks
    /// left out are processed first (plain tracks) or last (buses).
    SetRouting(Box<Vec<u64>>),
    /// Replace a track's sends.
    SetSends { track: u64, sends: Box<Vec<SendRt>> },
    /// Replace a track's (or with `MASTER`, the master's) automation envelopes.
    SetAutomation { track: u64, lanes: Box<Vec<EnvelopeRt>> },
    /// Audition a note on an instrument track (velocity 0 = off).
    LiveNote { track: u64, pitch: u8, velocity: u8 },
    /// Measure the round trip: play the `latency` test signal instead of the mix while
    /// capturing input `channel` until `buf` (empty, with capacity) is full; it comes back as
    /// `Garbage::Probe`.
    Probe { channel: usize, buf: Box<Vec<f32>> },
}

/// Things removed on the audio thread, returned to be dropped elsewhere.
pub enum Garbage {
    Track(Box<TrackRt>),
    Tracks(Box<Vec<Box<TrackRt>>>),
    Clips(Box<Vec<ClipRt>>),
    Midi(Box<Vec<MidiClipRt>>),
    Plugin(Box<PluginRt>),
    PluginChain(Box<Vec<Box<PluginRt>>>),
    Order(Box<Vec<(u64, bool)>>),
    Ids(Box<Vec<u64>>),
    TempoMap(Box<TempoMap>),
    Automation(Box<Vec<EnvelopeRt>>),
    Sends(Box<Vec<SendRt>>),
    /// A finished latency probe's capture.
    Probe(Box<Vec<f32>>),
}

/// State the audio thread publishes for the control side.
pub struct SharedState {
    pub position: AtomicU64,
    pub playing: AtomicBool,
    pub recording: AtomicBool,
    /// Timeline position (samples) where the current/last recording started.
    pub record_start: AtomicU64,
    /// Playhead (samples, negative in a pre-roll) when the first recorded input sample was
    /// processed.
    pub record_origin: AtomicI64,
    /// `io_latency` when the current/last recording started.
    pub record_latency: AtomicU32,
    /// Cycle recording: the recorded frame where the playhead first wrapped back to the
    /// cycle start (`NO_WRAP` if it didn't), and the cycle then (samples).
    pub record_wrap: AtomicU64,
    pub record_cycle: [AtomicU64; 2],
    /// Measured round trip from input capture to output playback (samples, smoothed);
    /// `LATENCY_UNKNOWN` until the device reports timestamps.
    pub io_latency: AtomicU32,
    /// Input samples dropped because the recorder could not keep up.
    pub record_overruns: AtomicU64,
    /// Peak level since last read, per track in mixer order (f32 bits).
    pub track_peaks: Vec<AtomicU32>,
    pub master_peak: [AtomicU32; 2],
    /// Counting in before playback/recording starts.
    pub counting_in: AtomicBool,
    /// Total plug-in delay being compensated (samples).
    pub latency: AtomicU32,
}

impl SharedState {
    pub fn new() -> Self {
        Self {
            position: AtomicU64::new(0),
            playing: AtomicBool::new(false),
            recording: AtomicBool::new(false),
            record_start: AtomicU64::new(0),
            record_origin: AtomicI64::new(0),
            record_latency: AtomicU32::new(LATENCY_UNKNOWN),
            record_wrap: AtomicU64::new(NO_WRAP),
            record_cycle: [AtomicU64::new(0), AtomicU64::new(0)],
            io_latency: AtomicU32::new(LATENCY_UNKNOWN),
            record_overruns: AtomicU64::new(0),
            track_peaks: (0..MAX_TRACKS).map(|_| AtomicU32::new(0)).collect(),
            master_peak: [AtomicU32::new(0), AtomicU32::new(0)],
            counting_in: AtomicBool::new(false),
            latency: AtomicU32::new(0),
        }
    }

    /// Read and reset a peak meter.
    pub fn take_peak(meter: &AtomicU32) -> f32 {
        f32::from_bits(meter.swap(0, Ordering::Relaxed))
    }
}

impl Default for SharedState {
    fn default() -> Self {
        Self::new()
    }
}

fn update_peak(meter: &AtomicU32, value: f32) {
    let _ = meter.try_update(Ordering::Relaxed, Ordering::Relaxed, |old| {
        (value > f32::from_bits(old)).then_some(value.to_bits())
    });
}

pub struct Mixer {
    tracks: Vec<Box<TrackRt>>,
    commands: Consumer<Command>,
    garbage: Producer<Garbage>,
    record: Option<Producer<f32>>,
    shared: Arc<SharedState>,
    transport: Transport,
    master_gain: f32,
    metronome: Metronome,
    tempo: Box<TempoMap>,
    count_in: CountInSettings,
    auto_monitor: bool,
    counting: Option<CountIn>,
    song_end: u64,
    master_plugins: Vec<Box<PluginRt>>,
    /// Processing order as sent (ids), resolved to track indices in `order` whenever the
    /// tracks or their routing change (`routing_dirty`).
    route_ids: Box<Vec<u64>>,
    order: Vec<usize>,
    routing_dirty: bool,
    /// The timeline read-ahead that hides all plug-in delays (slowest path + master chain),
    /// and the part of it before the master inserts.
    lookahead: u64,
    master_arrival: usize,
    master_automation: Vec<EnvelopeRt>,
    /// Master gain the last block ended on.
    master_level: Option<f32>,
    /// Any automation anywhere (blocks get shorter while it plays).
    automated: bool,
    sample_rate: f64,
    loop_range: Option<(u64, u64)>,
    /// Smoothed `io_latency` (samples), whether the last block was recorded, and frames
    /// recorded since recording started.
    io_latency: Option<f64>,
    was_recording: bool,
    recorded: u64,
    probe: Option<Probe>,
    input: Vec<Vec<f32>>,
    master: [Vec<f32>; 2],
    scratch: [Vec<f32>; 2],
}

impl Mixer {
    pub fn new(
        commands: Consumer<Command>,
        garbage: Producer<Garbage>,
        record: Option<Producer<f32>>,
        shared: Arc<SharedState>,
        sample_rate: u32,
    ) -> Self {
        Self {
            tracks: Vec::with_capacity(MAX_TRACKS),
            commands,
            garbage,
            record,
            transport: Transport {
                tempo_bpm: 120.0,
                time_sig: (4, 4),
                position_samples: shared.position.load(Ordering::Relaxed) as i64,
                ..Default::default()
            },
            shared,
            master_gain: 1.0,
            metronome: Metronome::new(sample_rate),
            tempo: Box::new(TempoMap::new(120.0, (4, 4), &[])),
            count_in: CountInSettings::default(),
            auto_monitor: true,
            counting: None,
            song_end: 0,
            master_plugins: Vec::with_capacity(PLUGIN_CAPACITY),
            route_ids: Box::new(Vec::new()),
            order: Vec::with_capacity(MAX_TRACKS),
            routing_dirty: false,
            lookahead: 0,
            master_arrival: 0,
            master_automation: Vec::new(),
            master_level: None,
            automated: false,
            sample_rate: sample_rate.max(1) as f64,
            loop_range: None,
            io_latency: None,
            was_recording: false,
            recorded: 0,
            probe: None,
            input: (0..MAX_INPUT_CHANNELS).map(|_| vec![0.0; MAX_BLOCK]).collect(),
            master: [vec![0.0; MAX_BLOCK], vec![0.0; MAX_BLOCK]],
            scratch: [vec![0.0; MAX_BLOCK], vec![0.0; MAX_BLOCK]],
        }
    }

    fn discard(&mut self, g: Garbage) {
        // If the queue is full the item is dropped here; better than leaking.
        let _ = self.garbage.push(g);
    }

    /// Test view of the audio-thread state: (track id, instrument instance, effect
    /// instances with bypass, audio clip count, MIDI region count).
    #[cfg(test)]
    pub(crate) fn debug_tracks(&self) -> Vec<(u64, Option<u64>, Vec<(u64, bool)>, usize, usize)> {
        self.tracks
            .iter()
            .map(|t| {
                let chain = t.plugins.iter().map(|p| (p.instance, p.bypass)).collect();
                (t.id, t.instrument.as_ref().map(|p| p.instance), chain, t.clips.len(), t.midi.len())
            })
            .collect()
    }

    /// Test view of the automation: (track id or `MASTER`, envelope targets).
    #[cfg(test)]
    pub(crate) fn debug_automation(&self) -> Vec<(u64, Vec<EnvTarget>)> {
        let tracks = self.tracks.iter().map(|t| (t.id, t.automation.iter().map(|e| e.target).collect()));
        tracks.chain(std::iter::once((MASTER, self.master_automation.iter().map(|e| e.target).collect()))).collect()
    }

    /// The loop applies while playing (and recording: each pass becomes a take).
    fn active_loop(&self) -> Option<(u64, u64)> {
        self.loop_range.filter(|_| self.transport.playing)
    }

    fn flush_all_notes(&mut self) {
        for t in &mut self.tracks {
            t.flush = true;
        }
    }

    /// Plug-in delay compensation. Each track's output arrives `arrival` samples late: its
    /// own chain, plus for a bus the slowest of its inputs. Everything summed at a bus or the
    /// master (outputs and sends alike) is delayed to match the slowest arrival there, so
    /// every path ends up as long as the slowest one; tracks read the timeline that far ahead
    /// (plus the master chain) and the output lines up with the playhead.
    fn update_latency(&mut self) {
        if self.routing_dirty {
            self.rebuild_routing();
        }
        let chain = |plugins: &[Box<PluginRt>]| plugins.iter().filter(|p| !p.bypass).map(|p| p.processor.latency()).sum::<usize>();
        for t in &mut self.tracks {
            let inst = t.instrument.as_ref().filter(|p| !p.bypass).map_or(0, |p| p.processor.latency());
            t.latency = (inst + chain(&t.plugins)).min(MAX_COMP - 1);
            t.in_latency = 0;
        }
        // In processing order, so a bus has heard from all its inputs before its own turn.
        let mut master = 0;
        for &ti in &self.order {
            let t = &mut self.tracks[ti];
            t.arrival = if t.bus { t.in_latency } else { 0 } + t.latency;
            let arrival = t.arrival;
            match t.dest {
                Some(d) => self.tracks[d].in_latency = self.tracks[d].in_latency.max(arrival),
                None => master = master.max(arrival),
            }
            for k in 0..self.tracks[ti].sends.len() {
                if let Some(d) = self.tracks[ti].sends[k].dest {
                    self.tracks[d].in_latency = self.tracks[d].in_latency.max(arrival);
                }
            }
        }
        for &ti in &self.order {
            let target = self.tracks[ti].dest.map_or(master, |d| self.tracks[d].in_latency);
            let t = &mut self.tracks[ti];
            t.delay = target.saturating_sub(t.arrival).min(MAX_DELAY);
            for k in 0..self.tracks[ti].sends.len() {
                let target = self.tracks[ti].sends[k].dest.map_or(0, |d| self.tracks[d].in_latency);
                let delay = target.saturating_sub(self.tracks[ti].arrival).min(MAX_DELAY);
                self.tracks[ti].sends[k].delay = delay;
            }
        }
        self.master_arrival = master;
        self.lookahead = (master + chain(&self.master_plugins)) as u64;
        self.shared.latency.store(self.lookahead as u32, Ordering::Relaxed);
    }

    /// Resolve the routing to track indices after the tracks, their outputs, sends or solos
    /// changed: the processing order (plain tracks missing from it first, buses missing from
    /// it last), each track's and send's destination, and who's audible. Allocation-free.
    fn rebuild_routing(&mut self) {
        self.routing_dirty = false;
        let n = self.tracks.len();
        self.order.clear();
        let listed = |id: u64, ids: &[u64]| ids.contains(&id);
        for (i, t) in self.tracks.iter().enumerate() {
            if !t.bus && !listed(t.id, &self.route_ids) {
                self.order.push(i);
            }
        }
        for &id in self.route_ids.iter() {
            if let Some(i) = self.tracks.iter().position(|t| t.id == id).filter(|i| !self.order.contains(i)) {
                self.order.push(i);
            }
        }
        for (i, t) in self.tracks.iter().enumerate() {
            if t.bus && !listed(t.id, &self.route_ids) {
                self.order.push(i);
            }
        }
        for i in 0..n {
            let out = self.tracks[i].params.output;
            self.tracks[i].dest = (out != MASTER).then(|| self.tracks.iter().position(|t| t.id == out && t.bus)).flatten().filter(|&d| d != i);
            for k in 0..self.tracks[i].sends.len() {
                let bus = self.tracks[i].sends[k].bus;
                self.tracks[i].sends[k].dest = self.tracks.iter().position(|t| t.id == bus && t.bus).filter(|&d| d != i);
            }
        }
        // Solo: a soloed track keeps the buses it feeds (through outputs and sends) audible,
        // and a soloed bus keeps what feeds it. In processing order (senders before receivers)
        // one pass each way covers whole chains.
        let any_solo = self.tracks.iter().any(|t| t.params.solo);
        for t in &mut self.tracks {
            t.audible = t.params.solo;
            t.feeds_solo = false;
        }
        for k in 0..self.order.len() {
            let i = self.order[k];
            if self.tracks[i].audible {
                self.for_each_target(i, |t| t.audible = true);
            }
        }
        for k in (0..self.order.len()).rev() {
            let i = self.order[k];
            let mut feeds = false;
            self.for_each_target(i, |t| feeds |= t.params.solo || t.feeds_solo);
            self.tracks[i].feeds_solo = feeds;
        }
        for t in &mut self.tracks {
            t.audible = !t.params.mute && (!any_solo || t.audible || t.feeds_solo);
        }
    }

    /// Run `f` on every bus track `i` feeds (its output and its sends' buses).
    fn for_each_target(&mut self, i: usize, mut f: impl FnMut(&mut TrackRt)) {
        if let Some(d) = self.tracks[i].dest {
            f(&mut self.tracks[d]);
        }
        for k in 0..self.tracks[i].sends.len() {
            if let Some(d) = self.tracks[i].sends[k].dest {
                f(&mut self.tracks[d]);
            }
        }
    }

    /// Where the tracks read the timeline for output at playhead `pos`: `lookahead` ahead,
    /// wrapping at the cycle end. (`pos` is negative during a start's pre-roll.)
    fn read_position(&self, pos: i64) -> u64 {
        timeline_at(pos, self.lookahead, self.active_loop())
    }

    /// Start the transport. It starts `lookahead` samples early (a few milliseconds), so
    /// plug-ins with latency are already filled when the playhead reaches the start and
    /// nothing at the start is skipped.
    fn start_rolling(&mut self) {
        if !self.transport.playing {
            self.update_latency(); // a plug-in may have arrived in this same callback
            self.transport.position_samples -= self.lookahead as i64;
        }
        self.transport.playing = true;
    }

    fn plugin_mut(&mut self, track: u64, instance: u64) -> Option<&mut Box<PluginRt>> {
        if track == MASTER {
            return self.master_plugins.iter_mut().find(|p| p.instance == instance);
        }
        self.track_mut(track).and_then(|t| t.plugin_mut(instance))
    }

    /// Start counting in: the bars at the tempo and signature at the playhead.
    fn start_count_in(&mut self, then_record: bool) {
        let secs = self.transport.position_samples.max(0) as f64 / self.sample_rate;
        let seg = *self.tempo.segment_at(secs);
        let beat = self.sample_rate * 60.0 / seg.bpm * seg.beat_len_q();
        let total = (beat * seg.sig.0 as f64 * self.count_in.bars as f64).round() as u64;
        self.counting = Some(CountIn { total, done: 0, beat, num: seg.sig.0, then_record });
    }

    fn track_mut(&mut self, id: u64) -> Option<&mut TrackRt> {
        self.tracks.iter_mut().find(|t| t.id == id).map(|t| &mut **t)
    }

    fn handle_commands(&mut self) {
        while let Ok(cmd) = self.commands.pop() {
            match cmd {
                Command::Play => {
                    if !self.transport.playing && self.counting.is_none() && self.count_in.on_play && self.count_in.bars > 0 {
                        self.start_count_in(false);
                    } else if self.counting.is_none() {
                        self.start_rolling();
                    }
                }
                Command::Stop => {
                    self.transport.playing = false;
                    self.transport.recording = false;
                    self.was_recording = false; // a Record in this same callback starts afresh
                    self.counting = None;
                    self.flush_all_notes();
                }
                Command::Seek(pos) => {
                    self.transport.position_samples = pos as i64;
                    if self.transport.playing {
                        // Pre-roll so the compensated tracks don't skip the first samples.
                        self.update_latency();
                        self.transport.position_samples -= self.lookahead as i64;
                    }
                    self.flush_all_notes();
                }
                // Recording from a stop counts in first (if set); punching in while playing doesn't.
                Command::Record(true) if !self.transport.playing && self.count_in.bars > 0 => {
                    if self.counting.is_none() {
                        self.flush_all_notes();
                        self.start_count_in(true);
                    } else if let Some(c) = &mut self.counting {
                        c.then_record = true;
                    }
                }
                Command::Record(on) => {
                    self.flush_all_notes();
                    self.was_recording &= on;
                    if on && !self.transport.recording {
                        self.shared.record_start.store(self.transport.position_samples.max(0) as u64, Ordering::Relaxed);
                    }
                    self.transport.recording = on;
                    if on {
                        self.start_rolling();
                    }
                }
                Command::SetMasterGain(g) => self.master_gain = g,
                Command::SetLoop { enabled, start, end } => {
                    self.loop_range = (enabled && end > start).then_some((start, end));
                }
                Command::SetMetronome { enabled, gain } => {
                    self.metronome.enabled = enabled;
                    self.metronome.gain = gain;
                }
                Command::SetTempoMap(mut map) => {
                    std::mem::swap(&mut self.tempo, &mut map);
                    self.discard(Garbage::TempoMap(map));
                }
                Command::SetCountIn { bars, on_play } => self.count_in = CountInSettings { bars, on_play },
                Command::SetAutoMonitor(on) => self.auto_monitor = on,
                Command::SetSongEnd(end) => self.song_end = end,
                Command::AddTrack(t) => {
                    self.routing_dirty = true;
                    if self.tracks.len() < MAX_TRACKS {
                        self.tracks.push(t);
                    } else {
                        self.discard(Garbage::Track(t));
                    }
                }
                Command::RemoveTrack(id) => {
                    self.routing_dirty = true;
                    if let Some(i) = self.tracks.iter().position(|t| t.id == id) {
                        let t = self.tracks.remove(i);
                        self.discard(Garbage::Track(t));
                    }
                }
                Command::ResetTracks(mut new) => {
                    self.routing_dirty = true;
                    std::mem::swap(&mut self.tracks, &mut *new);
                    self.discard(Garbage::Tracks(new));
                }
                Command::SetTrack { track, params } => {
                    self.routing_dirty = true;
                    if let Some(t) = self.track_mut(track) {
                        t.params = params;
                    }
                }
                Command::SetRouting(mut ids) => {
                    self.routing_dirty = true;
                    std::mem::swap(&mut self.route_ids, &mut ids);
                    self.discard(Garbage::Ids(ids));
                }
                Command::AddClip { track, clip } => {
                    if let Some(t) = self.track_mut(track) {
                        if t.clips.len() < t.clips.capacity() {
                            t.clips.push(clip);
                        }
                    }
                }
                Command::ReplaceClips { track, mut clips } => {
                    if let Some(t) = self.track_mut(track) {
                        std::mem::swap(&mut t.clips, &mut *clips);
                    }
                    self.discard(Garbage::Clips(clips));
                }
                Command::ReplaceMidi { track, mut clips } => {
                    if let Some(t) = self.track_mut(track) {
                        std::mem::swap(&mut t.midi, &mut *clips);
                        t.flush = true;
                    }
                    self.discard(Garbage::Midi(clips));
                }
                Command::SetPluginChain { track, order, mut new, mut target } => {
                    let chain = if track == MASTER { Some(&mut self.master_plugins) } else { self.tracks.iter_mut().find(|t| t.id == track).map(|t| &mut t.plugins) };
                    if let Some(plugins) = chain {
                        for &(instance, bypass) in order.iter() {
                            let found = match plugins.iter().position(|p| p.instance == instance) {
                                Some(i) => Some(plugins.remove(i)),
                                None => new.iter().position(|p| p.instance == instance).map(|i| new.swap_remove(i)),
                            };
                            if let Some(mut p) = found {
                                p.bypass = bypass;
                                if target.len() < target.capacity() {
                                    target.push(p);
                                } else {
                                    new.push(p); // over capacity: drop it off-thread rather than allocate
                                }
                            }
                        }
                        // `target` now receives whatever was removed from the chain.
                        std::mem::swap(plugins, &mut *target);
                    }
                    self.discard(Garbage::PluginChain(target));
                    self.discard(Garbage::PluginChain(new));
                    self.discard(Garbage::Order(order));
                }
                Command::SetInstrument { track, plugin } => {
                    let old = match self.track_mut(track) {
                        Some(t) => {
                            t.active = [false; 128];
                            std::mem::replace(&mut t.instrument, plugin)
                        }
                        None => plugin,
                    };
                    if let Some(p) = old {
                        self.discard(Garbage::Plugin(p));
                    }
                }
                Command::SetPluginBypass { track, instance, bypass } => {
                    if let Some(p) = self.plugin_mut(track, instance) {
                        p.bypass = bypass;
                    }
                }
                Command::SetPluginParam { track, instance, id, value } => {
                    if let Some(p) = self.plugin_mut(track, instance) {
                        p.processor.set_param(id, value);
                    }
                    // An automated parameter goes back to its envelope on the next block.
                    let envs = if track == MASTER { Some(&mut self.master_automation) } else { self.track_mut(track).map(|t| &mut t.automation) };
                    for e in envs.into_iter().flatten().filter(|e| e.target == EnvTarget::Param { instance, id }) {
                        e.last = f32::NAN;
                    }
                }
                Command::ReorderTracks(order) => {
                    self.routing_dirty = true;
                    self.tracks.sort_unstable_by_key(|t| order.iter().position(|&id| id == t.id).unwrap_or(usize::MAX));
                    self.discard(Garbage::Ids(order));
                }
                Command::Probe { channel, buf } => {
                    if let Some(old) = self.probe.replace(Probe { channel, buf, at: 0 }) {
                        self.discard(Garbage::Probe(old.buf));
                    }
                }
                Command::SetAutomation { track, mut lanes } => {
                    let current = if track == MASTER { Some(&mut self.master_automation) } else { self.track_mut(track).map(|t| &mut t.automation) };
                    if let Some(current) = current {
                        std::mem::swap(current, &mut *lanes);
                    }
                    self.discard(Garbage::Automation(lanes));
                }
                Command::SetSends { track, mut sends } => {
                    self.routing_dirty = true;
                    if let Some(t) = self.track_mut(track) {
                        std::mem::swap(&mut t.sends, &mut *sends);
                        // A level change keeps ramping from where the send was.
                        for new in t.sends.iter_mut() {
                            new.gains = sends.iter().find(|old| old.id == new.id).and_then(|old| old.gains);
                        }
                    }
                    self.discard(Garbage::Sends(sends));
                }
                Command::LiveNote { track, pitch, velocity } => {
                    if let Some(t) = self.track_mut(track) {
                        if t.live.len() < t.live.capacity() {
                            t.live.push(MidiEvent { offset: 0, pitch: pitch.min(127), velocity });
                        }
                    }
                }
            }
        }
    }

    /// The round trip measured for this callback's first input frame (see `audio`).
    /// Callbacks jitter a little, so it's smoothed.
    pub fn report_io_latency(&mut self, frames: f64) {
        let smoothed = match self.io_latency {
            Some(v) if (frames - v).abs() < self.sample_rate * 0.005 => v + (frames - v) * 0.05,
            _ => frames, // first reading, or the device changed buffering
        };
        self.io_latency = Some(smoothed);
        self.shared.io_latency.store(smoothed.round() as u32, Ordering::Relaxed);
    }

    /// Process interleaved device buffers. `input` may be empty (no input device).
    pub fn process(&mut self, input: &[f32], in_channels: usize, output: &mut [f32], out_channels: usize) {
        self.handle_commands();
        self.update_latency();
        self.automated = !self.master_automation.is_empty() || self.tracks.iter().any(|t| !t.automation.is_empty());
        let frames = output.len() / out_channels.max(1);
        let in_frames = if in_channels > 0 { input.len() / in_channels } else { 0 };
        let mut done = 0;
        while done < frames {
            let mut n = (frames - done).min(MAX_BLOCK);
            // End the block exactly where a count-in finishes, so playback starts on time.
            if let Some(c) = &self.counting {
                n = n.min((c.total - c.done) as usize).max(1);
            }
            if self.automated && self.transport.playing {
                n = n.min(AUTOMATION_BLOCK);
            }
            // End the block exactly at the loop end so the wrap is sample-accurate.
            let loop_range = self.active_loop();
            if let Some((_, end)) = loop_range {
                let pos = self.transport.position_samples.max(0) as u64;
                if pos < end {
                    n = n.min((end - pos) as usize).max(1);
                }
                // ...and where the read-ahead crosses it, so the tracks wrap exactly too.
                let ahead = (self.transport.position_samples + self.lookahead as i64).max(0) as u64;
                if ahead < end {
                    n = n.min((end - ahead) as usize).max(1);
                }
            }
            let in_chunk = if done < in_frames {
                let end = (done + n).min(in_frames);
                &input[done * in_channels..end * in_channels]
            } else {
                &[]
            };
            let out_chunk = &mut output[done * out_channels..(done + n) * out_channels];
            let before = self.transport.position_samples.max(0) as u64;
            self.process_block(in_chunk, in_channels, out_chunk, out_channels, n);
            done += n;
            // Plain playback stops when it runs past the last region (not while recording
            // or cycling).
            let after = self.transport.position_samples.max(0) as u64;
            if self.song_end > 0 && self.transport.playing && !self.transport.recording && loop_range.is_none() && before < self.song_end && after >= self.song_end {
                self.transport.playing = false;
                self.flush_all_notes();
            }
            if let Some((start, end)) = loop_range {
                let pos = self.transport.position_samples.max(0) as u64;
                if pos >= end && pos - n as u64 <= end {
                    self.transport.position_samples = start as i64;
                    self.flush_all_notes();
                    if self.transport.recording && self.shared.record_wrap.load(Ordering::Relaxed) == NO_WRAP {
                        self.shared.record_cycle[0].store(start, Ordering::Relaxed);
                        self.shared.record_cycle[1].store(end, Ordering::Relaxed);
                        self.shared.record_wrap.store(self.recorded, Ordering::Relaxed);
                    }
                }
            }
        }
        self.shared.position.store(self.transport.position_samples.max(0) as u64, Ordering::Relaxed);
        self.shared.counting_in.store(self.counting.is_some(), Ordering::Relaxed);
        self.shared.playing.store(self.transport.playing, Ordering::Relaxed);
        self.shared.recording.store(self.transport.recording, Ordering::Relaxed);
    }

    fn process_block(&mut self, input: &[f32], in_channels: usize, output: &mut [f32], out_channels: usize, n: usize) {
        // De-interleave input; missing frames (e.g. input underrun) are silent.
        let in_ch = in_channels.min(MAX_INPUT_CHANNELS);
        let in_frames = if in_channels > 0 { input.len() / in_channels } else { 0 };
        for (ch, buf) in self.input.iter_mut().enumerate().take(in_ch) {
            for (i, s) in buf[..n].iter_mut().enumerate() {
                *s = if i < in_frames { input[i * in_channels + ch] } else { 0.0 };
            }
        }

        if self.transport.recording && !self.was_recording {
            self.shared.record_origin.store(self.transport.position_samples, Ordering::Relaxed);
            self.shared.record_latency.store(self.shared.io_latency.load(Ordering::Relaxed), Ordering::Relaxed);
            self.shared.record_wrap.store(NO_WRAP, Ordering::Relaxed);
            self.recorded = 0;
        }
        self.was_recording = self.transport.recording;
        if self.transport.recording {
            self.recorded += n as u64;
            if let Some(rec) = &mut self.record {
                let wanted = n * in_channels;
                match rec.write_chunk_uninit(wanted) {
                    Ok(chunk) => {
                        let it = input.iter().copied().chain(std::iter::repeat(0.0));
                        chunk.fill_from_iter(it);
                    }
                    Err(_) => {
                        self.shared.record_overruns.fetch_add(wanted as u64, Ordering::Relaxed);
                    }
                }
            }
        }

        for m in &mut self.master {
            m[..n].fill(0.0);
        }
        // Plug-ins see the tempo and signature at the block start.
        let seg = *self.tempo.segment_at(self.transport.position_samples.max(0) as f64 / self.sample_rate);
        self.transport.tempo_bpm = seg.bpm;
        self.transport.time_sig = (seg.sig.0 as i32, seg.sig.1 as i32);
        // Tracks render the timeline `lookahead` ahead; plug-ins see that position.
        let pos = self.read_position(self.transport.position_samples);
        let block_transport = Transport { position_samples: pos as i64, ..self.transport };
        let clock = Clock::new(self, n);

        // Buses collect their inputs in `buf`; everything else starts it from silence.
        for t in self.tracks.iter_mut().filter(|t| t.bus) {
            t.buf.iter_mut().for_each(|b| b[..n].fill(0.0));
        }
        // Auto input monitoring: armed tracks hear their input except during plain playback,
        // where they play what was recorded.
        let auto_monitor = self.auto_monitor && (!self.transport.playing || self.transport.recording || self.counting.is_some());
        for k in 0..self.order.len() {
            let ti = self.order[k];
            let track = &mut *self.tracks[ti];
            let [l, r] = &mut track.buf;
            if !track.bus {
                l[..n].fill(0.0);
                r[..n].fill(0.0);
            }

            if self.transport.playing {
                for clip in track.clips.iter().filter(|c| !c.mute) {
                    let (from, to) = (pos.max(clip.start), (pos + n as u64).min(clip.start + clip.len));
                    if from >= to {
                        continue;
                    }
                    let src_start = (clip.offset + from - clip.start) as usize;
                    let dst_start = (from - pos) as usize;
                    let count = ((to - from) as usize).min(clip.data.frames().saturating_sub(src_start));
                    let src_l = &clip.data.channels[0];
                    let src_r = clip.data.channels.get(1).unwrap_or(src_l);
                    if clip.fades.flat(from, to) {
                        for i in 0..count {
                            l[dst_start + i] += src_l[src_start + i] * clip.gain;
                            r[dst_start + i] += src_r[src_start + i] * clip.gain;
                        }
                    } else {
                        for i in 0..count {
                            let g = clip.gain * clip.fades.gain(from + i as u64);
                            l[dst_start + i] += src_l[src_start + i] * g;
                            r[dst_start + i] += src_r[src_start + i] * g;
                        }
                    }
                }
            }

            let p = &track.params;
            if (p.monitor || (p.armed && auto_monitor)) && p.input_channel < in_ch {
                for i in 0..n {
                    let s = self.input[p.input_channel][i];
                    l[i] += s;
                    r[i] += s;
                }
            }

            // Plug-in parameters follow their envelopes at the timeline position each plug-in
            // is processing: behind the read-ahead by the plug-in delay before it.
            let mut back = if track.bus { track.in_latency } else { 0 };
            if let Some(inst) = track.instrument.as_mut() {
                apply_params(&mut track.automation, inst, clock.at(clock.start, back));
                back += if inst.bypass { 0 } else { inst.processor.latency() };
                collect_events(&mut track.events, &mut track.live, &mut track.active, &mut track.flush, &track.midi, pos, n, self.transport.playing);
                let [sl, sr] = &mut self.scratch;
                sl[..n].fill(0.0);
                sr[..n].fill(0.0);
                if !inst.bypass {
                    let inputs: [&[f32]; 2] = [&l[..n], &r[..n]];
                    let mut outputs: [&mut [f32]; 2] = [&mut sl[..n], &mut sr[..n]];
                    inst.processor.process(&inputs, &mut outputs, n, &block_transport, &track.events);
                }
                for i in 0..n {
                    l[i] += sl[i];
                    r[i] += sr[i];
                }
            }

            for plugin in track.plugins.iter_mut() {
                apply_params(&mut track.automation, plugin, clock.at(clock.start, back));
                if plugin.bypass {
                    continue;
                }
                back += plugin.processor.latency();
                let [sl, sr] = &mut self.scratch;
                {
                    let inputs: [&[f32]; 2] = [&l[..n], &r[..n]];
                    let mut outputs: [&mut [f32]; 2] = [&mut sl[..n], &mut sr[..n]];
                    plugin.processor.process(&inputs, &mut outputs, n, &block_transport, &[]);
                }
                l[..n].copy_from_slice(&sl[..n]);
                r[..n].copy_from_slice(&sr[..n]);
            }

            // Line the track up with the slowest one reaching the same place (plug-in delay
            // compensation). The delay line keeps the undelayed signal for the sends.
            let delay = track.delay;
            let w0 = track.comp_write;
            {
                let [cl, cr] = &mut track.comp;
                let mut w = track.comp_write;
                for i in 0..n {
                    cl[w] = l[i];
                    cr[w] = r[i];
                    let read = (w + MAX_COMP - delay) & (MAX_COMP - 1);
                    l[i] = cl[read];
                    r[i] = cr[read];
                    w = (w + 1) & (MAX_COMP - 1);
                }
                track.comp_write = w;
            }
            // Fader, pan and mute (automated or not) ramp across the block, so moves don't click.
            let g1 = fader_gains(p, &track.automation, clock.at(clock.end, track.arrival + track.delay));
            let g0 = track.gains.replace(g1).unwrap_or(g1);
            let mut peak = 0.0f32;
            for i in 0..n {
                let f = (i + 1) as f32 / n as f32;
                l[i] *= g0[0] + (g1[0] - g0[0]) * f;
                r[i] *= g0[1] + (g1[1] - g0[1]) * f;
                peak = peak.max(l[i].abs()).max(r[i].abs());
            }
            update_peak(&self.shared.track_peaks[ti], peak);
            if !track.audible {
                continue;
            }
            // Into the bus it feeds (processed later, see `order`) or the master.
            let (src, dst) = match track.dest {
                Some(d) if d < ti => {
                    let (a, b) = self.tracks.split_at_mut(ti);
                    (&b[0].buf, &mut a[d].buf)
                }
                Some(d) => {
                    let (a, b) = self.tracks.split_at_mut(d);
                    (&a[ti].buf, &mut b[0].buf)
                }
                None => (&self.tracks[ti].buf, &mut self.master),
            };
            for ch in 0..2 {
                for i in 0..n {
                    dst[ch][i] += src[ch][i];
                }
            }
            // Sends tap the delay line, each at the delay that lines it up with the bus it
            // feeds, and ramp their gain (level, plus the fader and pan when post-fader).
            for k in 0..self.tracks[ti].sends.len() {
                let t = &mut *self.tracks[ti];
                let Some(d) = t.sends[k].dest else { continue };
                let g1 = send_gains(&t.params, &t.automation, &t.sends[k], clock.at(clock.end, t.arrival + t.sends[k].delay));
                let send = &mut t.sends[k];
                let g0 = send.gains.replace(g1).unwrap_or(g1);
                if g0 == [0.0; 2] && g1 == [0.0; 2] {
                    continue;
                }
                let delay = send.delay;
                let (src, dst) = pair_mut(&mut self.tracks, ti, d);
                for ch in 0..2 {
                    for i in 0..n {
                        let read = (w0 + i + MAX_COMP - delay) & (MAX_COMP - 1);
                        dst.buf[ch][i] += src.comp[ch][read] * (g0[ch] + (g1[ch] - g0[ch]) * (i + 1) as f32 / n as f32);
                    }
                }
            }
        }

        // Master inserts, then the master fader.
        let mut back = self.master_arrival;
        for plugin in self.master_plugins.iter_mut() {
            apply_params(&mut self.master_automation, plugin, clock.at(clock.start, back));
            if plugin.bypass {
                continue;
            }
            back += plugin.processor.latency();
            let [sl, sr] = &mut self.scratch;
            {
                let [ml, mr] = &self.master;
                let inputs: [&[f32]; 2] = [&ml[..n], &mr[..n]];
                let mut outputs: [&mut [f32]; 2] = [&mut sl[..n], &mut sr[..n]];
                plugin.processor.process(&inputs, &mut outputs, n, &block_transport, &[]);
            }
            self.master[0][..n].copy_from_slice(&sl[..n]);
            self.master[1][..n].copy_from_slice(&sr[..n]);
        }
        let volume = self.master_automation.iter().find(|e| e.target == EnvTarget::Volume);
        let g1 = volume.map_or(self.master_gain, |e| pos_to_gain(e.at(clock.at(clock.end, self.lookahead as usize))));
        let g0 = self.master_level.replace(g1).unwrap_or(g1);
        for (ch, m) in self.master.iter_mut().enumerate() {
            for (i, s) in m[..n].iter_mut().enumerate() {
                *s *= g0 + (g1 - g0) * (i + 1) as f32 / n as f32;
            }
            let peak = m[..n].iter().fold(0.0f32, |a, s| a.max(s.abs()));
            update_peak(&self.shared.master_peak[ch], peak);
        }

        // The click goes straight to the output: not affected by the master fader,
        // not metered, never recorded.
        if self.transport.playing && self.metronome.enabled {
            let pos = self.transport.position_samples.max(0);
            let [l, r] = &mut self.master;
            self.metronome.render(pos, &self.tempo, &mut l[..n], &mut r[..n]);
        }
        // Count-in clicks play whether or not the metronome is on.
        let mut count_in_done = None;
        if let Some(c) = &mut self.counting {
            let [l, r] = &mut self.master;
            self.metronome.render_count_in(c, &mut l[..n], &mut r[..n]);
            c.done += n as u64;
            if c.done >= c.total {
                count_in_done = Some(c.then_record);
            }
        }

        // A latency probe replaces everything else, and captures the input processed with
        // each output sample, exactly as a recording would.
        if let Some(p) = &mut self.probe {
            for i in 0..n {
                let v = crate::latency::signal(p.at + i, self.sample_rate);
                self.master[0][i] = v;
                self.master[1][i] = v;
                if p.buf.len() < p.buf.capacity() {
                    p.buf.push(if p.channel < in_ch { self.input[p.channel][i] } else { 0.0 });
                }
            }
            p.at += n;
            if p.buf.len() >= p.buf.capacity() {
                if let Some(p) = self.probe.take() {
                    self.discard(Garbage::Probe(p.buf));
                }
            }
        }

        for (i, frame) in output.chunks_exact_mut(out_channels).enumerate() {
            let (l, r) = (self.master[0][i], self.master[1][i]);
            match frame {
                [mono] => *mono = ((l + r) * 0.5).clamp(-1.0, 1.0),
                [a, b, rest @ ..] => {
                    *a = l.clamp(-1.0, 1.0);
                    *b = r.clamp(-1.0, 1.0);
                    rest.fill(0.0);
                }
                [] => {}
            }
        }

        if self.transport.playing {
            self.transport.position_samples += n as i64;
        }
        // Start after this block, so playback begins exactly where the count-in ends.
        if let Some(record) = count_in_done {
            self.counting = None;
            if record {
                // The recording starts at the playhead (before the pre-roll shift).
                self.shared.record_start.store(self.transport.position_samples.max(0) as u64, Ordering::Relaxed);
                self.transport.recording = true;
            }
            self.start_rolling();
        }
    }
}

/// The timeline position at playhead `pos` for a stage `ahead` samples before the output,
/// wrapping at the cycle end like the playhead will. (`pos` is negative in a pre-roll.)
fn timeline_at(pos: i64, ahead: u64, cycle: Option<(u64, u64)>) -> u64 {
    let at = (pos + ahead as i64).max(0) as u64;
    match cycle {
        Some((start, end)) if pos >= 0 && (pos as u64) < end && at >= end => start + (at - end),
        _ => at,
    }
}

/// Where automation reads the timeline in a block: for a stage of the mix `back` samples of
/// plug-in delay before the output, the position whose audio it is processing (so envelopes
/// line up with what the tracks render, read-ahead and cycle wrap included). Stopped: the
/// playhead, so moving it moves the automated controls.
#[derive(Clone, Copy)]
struct Clock {
    start: i64,
    /// Playhead after the block (wrapped at the cycle end).
    end: i64,
    playing: bool,
    lookahead: u64,
    cycle: Option<(u64, u64)>,
    sample_rate: f64,
}

impl Clock {
    fn new(m: &Mixer, n: usize) -> Self {
        let (start, playing, cycle) = (m.transport.position_samples, m.transport.playing, m.active_loop());
        let mut end = if playing { start + n as i64 } else { start };
        if let Some((s, e)) = cycle.filter(|&(_, e)| start < e as i64 && end >= e as i64) {
            end = s as i64 + (end - e as i64);
        }
        Self { start, end, playing, lookahead: m.lookahead, cycle, sample_rate: m.sample_rate }
    }

    /// Seconds for a stage `back` samples before the output at playhead `pos`.
    fn at(&self, pos: i64, back: usize) -> f64 {
        let at = if self.playing { timeline_at(pos, self.lookahead.saturating_sub(back as u64), self.cycle) } else { self.start.max(0) as u64 };
        at as f64 / self.sample_rate
    }
}

/// Set a plug-in's automated parameters to their envelope values at `secs` (only those
/// that moved).
fn apply_params(envs: &mut [EnvelopeRt], plugin: &mut PluginRt, secs: f64) {
    for env in envs.iter_mut() {
        match env.target {
            EnvTarget::Param { instance, id } if instance == plugin.instance => {
                let v = env.at(secs);
                if v != env.last {
                    env.last = v;
                    plugin.processor.set_param(id, v as f64);
                }
            }
            _ => {}
        }
    }
}

/// Left/right gains from the fader, pan and mute, or their envelopes at `secs`.
fn fader_gains(p: &TrackParams, envs: &[EnvelopeRt], secs: f64) -> [f32; 2] {
    let (mut volume, mut pan, mut mute) = (p.volume, p.pan, false);
    for env in envs {
        match env.target {
            EnvTarget::Volume => volume = pos_to_gain(env.at(secs)),
            EnvTarget::Pan => pan = env.at(secs) * 2.0 - 1.0,
            EnvTarget::Mute => mute = env.at(secs) >= 0.5,
            EnvTarget::Param { .. } | EnvTarget::Send { .. } => {}
        }
    }
    if mute { [0.0; 2] } else { pan_gains(volume, pan).into() }
}

/// Left/right gains of a send at `secs`: its level (or envelope), times the fader and pan
/// when post-fader. Pre-fader it ignores them, but a muted track still sends nothing.
fn send_gains(p: &TrackParams, envs: &[EnvelopeRt], send: &SendRt, secs: f64) -> [f32; 2] {
    if send.mute {
        return [0.0; 2];
    }
    let level = envs.iter().find(|e| e.target == EnvTarget::Send { id: send.id }).map_or(send.gain, |e| pos_to_gain(e.at(secs)));
    let tap = if !send.pre_fader {
        fader_gains(p, envs, secs)
    } else if envs.iter().any(|e| e.target == EnvTarget::Mute && e.at(secs) >= 0.5) {
        [0.0; 2]
    } else {
        [1.0; 2]
    };
    tap.map(|g| g * level)
}

/// Track `a` to read from and track `b` to add into (`a != b`).
fn pair_mut(tracks: &mut [Box<TrackRt>], a: usize, b: usize) -> (&TrackRt, &mut TrackRt) {
    if a < b {
        let (x, y) = tracks.split_at_mut(b);
        (&x[a], &mut y[0])
    } else {
        let (x, y) = tracks.split_at_mut(a);
        (&y[0], &mut x[b])
    }
}

struct Probe {
    channel: usize,
    buf: Box<Vec<f32>>,
    /// Samples of the test signal played so far.
    at: usize,
}

impl TrackRt {
    fn plugin_mut(&mut self, instance: u64) -> Option<&mut Box<PluginRt>> {
        if self.instrument.as_ref().is_some_and(|p| p.instance == instance) {
            return self.instrument.as_mut();
        }
        self.plugins.iter_mut().find(|p| p.instance == instance)
    }
}

/// Gather this block's note events for an instrument track: pending note-offs (after
/// stop/seek/edits), live audition notes, then notes from the MIDI regions. Allocation-free.
#[allow(clippy::too_many_arguments)]
fn collect_events(
    events: &mut Vec<MidiEvent>,
    live: &mut Vec<MidiEvent>,
    active: &mut [bool; 128],
    flush: &mut bool,
    midi: &[MidiClipRt],
    pos: u64,
    n: usize,
    playing: bool,
) {
    events.clear();
    let push = |events: &mut Vec<MidiEvent>, e: MidiEvent| {
        if events.len() < events.capacity() {
            events.push(e);
        }
    };
    if *flush {
        for (pitch, on) in active.iter_mut().enumerate() {
            if *on {
                push(events, MidiEvent { offset: 0, pitch: pitch as u8, velocity: 0 });
                *on = false;
            }
        }
        *flush = false;
    }
    for e in live.drain(..) {
        active[e.pitch as usize] = e.velocity > 0;
        push(events, e);
    }
    if playing {
        let end = pos + n as u64;
        for clip in midi.iter().filter(|c| !c.mute && c.start < end && c.end > pos) {
            for note in &clip.notes {
                if note.start >= end {
                    break;
                }
                if note.start >= pos && note.end > note.start {
                    push(events, MidiEvent { offset: (note.start - pos) as u32, pitch: note.pitch, velocity: note.velocity.max(1) });
                    active[note.pitch as usize] = true;
                }
                if note.end > pos && note.end <= end && active[note.pitch as usize] {
                    let offset = ((note.end - pos) as u32).min(n as u32 - 1);
                    push(events, MidiEvent { offset, pitch: note.pitch, velocity: 0 });
                    active[note.pitch as usize] = false;
                }
            }
        }
    }
    // Stable insertion sort by offset (tiny lists, no allocation).
    for i in 1..events.len() {
        let mut j = i;
        while j > 0 && events[j - 1].offset > events[j].offset {
            events.swap(j - 1, j);
            j -= 1;
        }
    }
}

/// Click generator. The two click sounds are rendered once at construction (off the
/// audio thread); playback is a table lookup per sample.
#[derive(Debug, Clone, Copy, Default)]
struct CountInSettings {
    bars: u32,
    on_play: bool,
}

/// A count-in in progress (samples).
#[derive(Debug, Clone, Copy)]
struct CountIn {
    total: u64,
    done: u64,
    beat: f64,
    num: u32,
    then_record: bool,
}

struct Metronome {
    enabled: bool,
    gain: f32,
    sample_rate: f64,
    accent: Vec<f32>,
    normal: Vec<f32>,
}

impl Metronome {
    fn new(sample_rate: u32) -> Self {
        let sr = sample_rate.max(1) as f64;
        let click = |freq: f64, amp: f32| -> Vec<f32> {
            let len = (sr * 0.035) as usize;
            (0..len)
                .map(|i| {
                    let t = i as f64 / sr;
                    let attack = (t / 0.0008).min(1.0); // avoid a hard edge
                    ((t * freq * std::f64::consts::TAU).sin() * (-t / 0.008).exp() * attack) as f32 * amp
                })
                .collect()
        };
        Self { enabled: false, gain: 0.5, sample_rate: sr, accent: click(1760.0, 1.0), normal: click(1320.0, 0.7) }
    }

    /// Add clicks for the block starting at timeline sample `pos`, following the tempo map
    /// (accent on each bar's first beat).
    fn render(&self, pos: i64, map: &TempoMap, l: &mut [f32], r: &mut [f32]) {
        let segments = map.segments();
        let sr = self.sample_rate;
        let mut si = segments.partition_point(|s| s.start_secs * sr <= pos as f64).saturating_sub(1);
        for (i, (a, b)) in l.iter_mut().zip(r.iter_mut()).enumerate() {
            let secs = (pos + i as i64) as f64 / sr;
            while si + 1 < segments.len() && segments[si + 1].start_secs <= secs {
                si += 1;
            }
            let seg = &segments[si];
            let q = seg.start_q + (secs - seg.start_secs) * seg.bpm / 60.0;
            let beat_q = seg.beat_len_q();
            let k = ((q - seg.bar_q) / beat_q + 1e-9).floor();
            let beat_start = seg.bar_q + k * beat_q;
            // Only beats that start inside this segment (a tempo change mid-beat skips one).
            if beat_start + 1e-9 < seg.start_q {
                continue;
            }
            let phase = ((q - beat_start) * 60.0 / seg.bpm * sr) as usize;
            let table = if (k as i64).rem_euclid(seg.sig.0.max(1) as i64) == 0 { &self.accent } else { &self.normal };
            if let Some(&v) = table.get(phase) {
                let v = v * self.gain;
                *a += v;
                *b += v;
            }
        }
    }

    fn render_count_in(&self, c: &CountIn, l: &mut [f32], r: &mut [f32]) {
        let gain = self.gain.max(0.35);
        for (i, (a, b)) in l.iter_mut().zip(r.iter_mut()).enumerate() {
            let s = (c.done + i as u64) as f64;
            if s >= c.total as f64 {
                break;
            }
            let k = (s / c.beat).floor();
            let phase = (s - k * c.beat) as usize;
            let table = if (k as i64).rem_euclid(c.num.max(1) as i64) == 0 { &self.accent } else { &self.normal };
            if let Some(&v) = table.get(phase) {
                *a += v * gain;
                *b += v * gain;
            }
        }
    }
}

/// Linear balance pan: centre leaves both channels at `volume`.
fn pan_gains(volume: f32, pan: f32) -> (f32, f32) {
    let pan = pan.clamp(-1.0, 1.0);
    (volume * (1.0 - pan).min(1.0), volume * (1.0 + pan).min(1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mixer() -> (Mixer, Producer<Command>, Consumer<Garbage>, Arc<SharedState>) {
        let (cmd_tx, cmd_rx) = rtrb::RingBuffer::new(64);
        let (g_tx, g_rx) = rtrb::RingBuffer::new(64);
        let shared = Arc::new(SharedState::new());
        (Mixer::new(cmd_rx, g_tx, None, shared.clone(), 48_000), cmd_tx, g_rx, shared)
    }

    fn params() -> TrackParams {
        TrackParams { volume: 1.0, pan: 0.0, mute: false, solo: false, armed: false, monitor: false, input_channel: 0, output: MASTER }
    }

    #[test]
    fn plays_clip_at_position() {
        let (mut m, mut tx, _g, shared) = mixer();
        let data = Arc::new(AudioData { channels: vec![vec![0.5; 100]], sample_rate: 48_000 });
        tx.push(Command::AddTrack(Box::new(TrackRt::new(1, params())))).ok();
        tx.push(Command::AddClip { track: 1, clip: ClipRt { start: 10, len: 100, offset: 0, gain: 1.0, mute: false, data, fades: ClipFades::NONE } }).ok();
        tx.push(Command::Play).ok();

        let mut out = vec![0.0; 2 * 64];
        m.process(&[], 0, &mut out, 2);
        assert_eq!(out[2 * 9], 0.0);
        assert_eq!(out[2 * 10], 0.5);
        assert_eq!(out[2 * 10 + 1], 0.5);
        assert_eq!(shared.position.load(Ordering::Relaxed), 64);
    }

    #[test]
    fn mute_solo_and_monitoring() {
        let (mut m, mut tx, _g, _s) = mixer();
        let mut a = params();
        a.monitor = true;
        let mut b = a;
        b.input_channel = 1;
        b.solo = true;
        tx.push(Command::AddTrack(Box::new(TrackRt::new(1, a)))).ok();
        tx.push(Command::AddTrack(Box::new(TrackRt::new(2, b)))).ok();

        let input: Vec<f32> = (0..32).flat_map(|_| [0.25, 0.5]).collect();
        let mut out = vec![0.0; 64];
        m.process(&input, 2, &mut out, 2);
        // Only the soloed track (input channel 1) is audible.
        assert_eq!(out[0], 0.5);
    }

    #[test]
    fn armed_tracks_monitor_while_stopped_or_recording() {
        let (mut m, mut tx, _g, _s) = mixer();
        let mut a = params();
        a.armed = true;
        tx.push(Command::AddTrack(Box::new(TrackRt::new(1, a)))).ok();
        let input = vec![0.25f32; 64];
        let mut out = vec![0.0; 128];
        let mut heard = |m: &mut Mixer| {
            m.process(&input, 1, &mut out, 2);
            out[2 * 63]
        };
        assert_eq!(heard(&mut m), 0.25, "stopped: hear the input");
        tx.push(Command::Play).ok();
        assert_eq!(heard(&mut m), 0.0, "playing back: hear the recording, not the input");
        tx.push(Command::Record(true)).ok();
        assert_eq!(heard(&mut m), 0.25, "recording: hear the input");
        tx.push(Command::Stop).ok();
        tx.push(Command::SetAutoMonitor(false)).ok();
        assert_eq!(heard(&mut m), 0.0, "off: only the I button monitors");
    }

    #[test]
    fn metronome_clicks_on_beats_only_when_enabled_and_playing() {
        let (mut m, mut tx, _g, _s) = mixer();
        tx.push(Command::SetTempoMap(Box::new(TempoMap::new(120.0, (4, 4), &[])))).ok(); // beat = 24000 samples
        tx.push(Command::SetMetronome { enabled: true, gain: 1.0 }).ok();
        let mut out = vec![0.0; 2 * 512];
        m.process(&[], 0, &mut out, 2);
        assert!(out.iter().all(|&s| s == 0.0), "no click while stopped");

        tx.push(Command::Play).ok();
        let mut out = vec![0.0; 2 * 512];
        m.process(&[], 0, &mut out, 2);
        assert!(out.iter().any(|&s| s.abs() > 0.1), "click at the downbeat");

        // Middle of the first beat: silence.
        let mut out = vec![0.0; 2 * 512];
        for _ in 0..20 {
            m.process(&[], 0, &mut out, 2);
        }
        assert!(out.iter().all(|&s| s == 0.0), "silence between beats");

        tx.push(Command::SetMetronome { enabled: false, gain: 1.0 }).ok();
        tx.push(Command::Seek(0)).ok();
        m.process(&[], 0, &mut out, 2);
        assert!(out.iter().all(|&s| s == 0.0), "no click when disabled");
    }

    /// Run `blocks` blocks of 480 frames, returning the left channel.
    fn run(m: &mut Mixer, blocks: usize) -> Vec<f32> {
        let mut all = Vec::new();
        let mut out = vec![0.0; 2 * 480];
        for _ in 0..blocks {
            m.process(&[], 0, &mut out, 2);
            all.extend(out.chunks(2).map(|f| f[0]));
        }
        all
    }

    fn click_at(signal: &[f32], sample: usize) -> bool {
        signal[sample..(sample + 200).min(signal.len())].iter().any(|s| s.abs() > 0.05)
    }

    #[test]
    fn metronome_follows_the_tempo_map() {
        let (mut m, mut tx, _g, _s) = mixer();
        // 120 bpm (24000-sample beats), then 60 bpm from 1 s (48000-sample beats).
        let map = TempoMap::new(120.0, (4, 4), &[opentake_project::TempoChange { at_secs: 1.0, bpm: 60.0, time_signature: None }]);
        tx.push(Command::SetTempoMap(Box::new(map))).ok();
        tx.push(Command::SetMetronome { enabled: true, gain: 1.0 }).ok();
        tx.push(Command::Play).ok();
        let s = run(&mut m, 250); // 2.5 s
        for beat in [0, 24_000, 48_000, 96_000] {
            assert!(click_at(&s, beat), "click at {beat}");
        }
        assert!(!click_at(&s, 72_000), "no click at the old tempo's beat after the change");
    }

    #[test]
    fn count_in_before_recording_then_records_from_the_playhead() {
        let (mut m, mut tx, _g, shared) = mixer();
        tx.push(Command::SetTempoMap(Box::new(TempoMap::new(120.0, (4, 4), &[])))).ok();
        tx.push(Command::SetCountIn { bars: 1, on_play: false }).ok();
        tx.push(Command::Seek(48_000)).ok();
        tx.push(Command::Record(true)).ok();
        // One 4/4 bar at 120 bpm = 2 s = 96000 samples of clicks, metronome off or not.
        let s = run(&mut m, 100);
        assert!(shared.counting_in.load(Ordering::Relaxed));
        assert!(!shared.recording.load(Ordering::Relaxed));
        assert_eq!(shared.position.load(Ordering::Relaxed), 48_000, "the playhead waits during the count-in");
        assert!(click_at(&s, 0) && click_at(&s, 24_000), "count-in clicks");
        run(&mut m, 101);
        assert!(!shared.counting_in.load(Ordering::Relaxed));
        assert!(shared.recording.load(Ordering::Relaxed) && shared.playing.load(Ordering::Relaxed));
        assert_eq!(shared.record_start.load(Ordering::Relaxed), 48_000);
        assert_eq!(shared.record_origin.load(Ordering::Relaxed), 48_000);
        assert_eq!(shared.position.load(Ordering::Relaxed), 48_000 + (201 * 480 - 96_000) as u64, "starts exactly when the count-in ends");

        // Punching in while playing doesn't count in; plain play only counts in if asked to.
        tx.push(Command::Stop).ok();
        tx.push(Command::Play).ok();
        run(&mut m, 1);
        assert!(shared.playing.load(Ordering::Relaxed) && !shared.counting_in.load(Ordering::Relaxed));
        tx.push(Command::Record(true)).ok();
        run(&mut m, 1);
        assert!(shared.recording.load(Ordering::Relaxed));
        tx.push(Command::Stop).ok();
        tx.push(Command::SetCountIn { bars: 1, on_play: true }).ok();
        tx.push(Command::Play).ok();
        run(&mut m, 1);
        assert!(shared.counting_in.load(Ordering::Relaxed) && !shared.playing.load(Ordering::Relaxed));
    }

    #[test]
    fn playback_stops_at_the_song_end_but_not_while_recording_or_cycling() {
        let (mut m, mut tx, _g, shared) = mixer();
        tx.push(Command::SetSongEnd(10_000)).ok();
        tx.push(Command::Play).ok();
        run(&mut m, 30); // 14400 samples
        assert!(!shared.playing.load(Ordering::Relaxed), "stopped after the end");
        assert!((10_000..10_000 + 480).contains(&shared.position.load(Ordering::Relaxed)));
        // Starting beyond the end (e.g. to hear the metronome) keeps playing.
        tx.push(Command::Play).ok();
        run(&mut m, 5);
        assert!(shared.playing.load(Ordering::Relaxed));
        // Recording runs past the end.
        tx.push(Command::Stop).ok();
        tx.push(Command::Seek(0)).ok();
        tx.push(Command::Record(true)).ok();
        run(&mut m, 30);
        assert!(shared.recording.load(Ordering::Relaxed));
    }

    /// Delays its input by its latency and says so.
    struct Latent(usize, Vec<f32>, Vec<f32>);

    impl Latent {
        fn new(samples: usize) -> Self {
            Self(samples, vec![0.0; samples], vec![0.0; samples])
        }
    }

    impl Processor for Latent {
        fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], frames: usize, _: &Transport, _: &[MidiEvent]) {
            for i in 0..frames {
                // Shift through a FIFO of `latency` samples.
                self.1.push(inputs[0][i]);
                self.2.push(inputs[1][i]);
                outputs[0][i] = self.1.remove(0);
                outputs[1][i] = self.2.remove(0);
            }
        }
        fn set_param(&mut self, _: u32, _: f64) {}
        fn latency(&self) -> usize {
            self.0
        }
    }

    #[test]
    fn plugin_delay_is_compensated_across_tracks_and_the_master() {
        let (mut m, mut tx, _g, shared) = mixer();
        let mut impulse = vec![0.0f32; 4];
        impulse[0] = 0.4; // two of them sum to 0.8, below the output clamp
        let data = Arc::new(AudioData { channels: vec![impulse], sample_rate: 48_000 });
        for id in [1, 2] {
            tx.push(Command::AddTrack(Box::new(TrackRt::new(id, params())))).ok();
            tx.push(Command::AddClip { track: id, clip: ClipRt { start: 1000, len: 4, offset: 0, gain: 1.0, mute: false, data: data.clone(), fades: ClipFades::NONE } }).ok();
        }
        // Track 1 has a 300-sample plug-in, the master a 200-sample one.
        let slow = Box::new(PluginRt { instance: 9, processor: Box::new(Latent::new(300)), bypass: false });
        tx.push(Command::SetPluginChain { track: 1, order: Box::new(vec![(9, false)]), new: Box::new(vec![slow]), target: Box::new(Vec::with_capacity(PLUGIN_CAPACITY)) }).ok();
        let master = Box::new(PluginRt { instance: 10, processor: Box::new(Latent::new(200)), bypass: false });
        tx.push(Command::SetPluginChain { track: MASTER, order: Box::new(vec![(10, false)]), new: Box::new(vec![master]), target: Box::new(Vec::with_capacity(PLUGIN_CAPACITY)) }).ok();
        tx.push(Command::Play).ok();
        let out = run(&mut m, 10);
        assert_eq!(shared.latency.load(Ordering::Relaxed), 500);
        // Both impulses land together at the clips' timeline position. The transport started
        // 500 samples early (pre-roll), so output sample i is timeline position i - 500.
        let hits: Vec<(usize, f32)> = out.iter().enumerate().filter(|(_, v)| v.abs() > 1e-6).map(|(i, v)| (i, *v)).collect();
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].0 - 500, 1000);
        assert!((hits[0].1 - 0.8).abs() < 1e-6);
    }

    fn chain(tx: &mut Producer<Command>, track: u64, instance: u64, processor: Box<dyn Processor>) {
        let p = Box::new(PluginRt { instance, processor, bypass: false });
        tx.push(Command::SetPluginChain { track, order: Box::new(vec![(instance, false)]), new: Box::new(vec![p]), target: Box::new(Vec::with_capacity(PLUGIN_CAPACITY)) }).ok();
    }

    /// Tracks 1 and 2 feed bus 3 (x3 effect, fader 0.5); track 4 goes straight to the master.
    /// All monitor a 0.1 input.
    fn bus_mixer() -> (Mixer, Producer<Command>, Consumer<Garbage>) {
        let (m, mut tx, g, _) = mixer();
        let monitor = TrackParams { monitor: true, ..params() };
        for (id, output) in [(1, 3), (2, 3), (4, MASTER)] {
            tx.push(Command::AddTrack(Box::new(TrackRt::new(id, TrackParams { output, ..monitor })))).ok();
        }
        tx.push(Command::AddTrack(Box::new(TrackRt::new_bus(3, TrackParams { volume: 0.5, ..params() })))).ok();
        chain(&mut tx, 3, 9, Box::new(Gain(3.0)));
        (m, tx, g)
    }

    fn level(m: &mut Mixer) -> f32 {
        let mut out = vec![0.0; 64];
        m.process(&[0.1; 32], 1, &mut out, 2);
        out[62]
    }

    #[test]
    fn buses_sum_their_sources_through_their_effects_and_fader() {
        let (mut m, mut tx, _g) = bus_mixer();
        assert!((level(&mut m) - 0.4).abs() < 1e-6, "(0.1 + 0.1) * 3 * 0.5 + 0.1");
        // Re-routing track 1 to the master takes it out of the bus.
        tx.push(Command::SetTrack { track: 1, params: TrackParams { monitor: true, ..params() } }).ok();
        assert!((level(&mut m) - 0.35).abs() < 1e-6, "0.1 * 3 * 0.5 + 0.1 + 0.1");
        tx.push(Command::SetTrack { track: 3, params: TrackParams { volume: 0.5, mute: true, ..params() } }).ok();
        assert!((level(&mut m) - 0.2).abs() < 1e-6, "a muted bus silences what feeds it");
        assert!(SharedState::take_peak(&m.shared.track_peaks[3]) > 0.0, "buses are metered");
    }

    #[test]
    fn solo_keeps_a_tracks_buses_and_a_buses_sources() {
        let (mut m, mut tx, _g) = bus_mixer();
        let monitor = TrackParams { monitor: true, output: 3, ..params() };
        tx.push(Command::SetTrack { track: 1, params: TrackParams { solo: true, ..monitor } }).ok();
        assert!((level(&mut m) - 0.15).abs() < 1e-6, "track 1 through its bus only");
        tx.push(Command::SetTrack { track: 1, params: monitor }).ok();
        tx.push(Command::SetTrack { track: 3, params: TrackParams { volume: 0.5, solo: true, ..params() } }).ok();
        assert!((level(&mut m) - 0.3).abs() < 1e-6, "the bus with both sources, not track 4");
    }

    #[test]
    fn nested_buses_follow_the_routing_order() {
        let (mut m, mut tx, _g, _s) = mixer();
        // Bus 5 is first in the track list but fed by bus 3, which track 1 feeds.
        tx.push(Command::AddTrack(Box::new(TrackRt::new_bus(5, TrackParams { volume: 0.5, ..params() })))).ok();
        tx.push(Command::AddTrack(Box::new(TrackRt::new_bus(3, TrackParams { output: 5, ..params() })))).ok();
        tx.push(Command::AddTrack(Box::new(TrackRt::new(1, TrackParams { monitor: true, output: 3, ..params() })))).ok();
        tx.push(Command::SetRouting(Box::new(vec![1, 3, 5]))).ok();
        assert!((level(&mut m) - 0.05).abs() < 1e-6, "{}", level(&mut m));
    }

    #[test]
    fn plugin_delay_is_compensated_through_buses() {
        let (mut m, mut tx, _g, shared) = mixer();
        let mut impulse = vec![0.0f32; 4];
        impulse[0] = 0.25;
        let data = Arc::new(AudioData { channels: vec![impulse], sample_rate: 48_000 });
        // 1 (dry) and 4 (120) feed bus 3 (300); 2 (100) goes to the master (50).
        for (id, output) in [(1, 3), (2, MASTER), (4, 3)] {
            tx.push(Command::AddTrack(Box::new(TrackRt::new(id, TrackParams { output, ..params() })))).ok();
            tx.push(Command::AddClip { track: id, clip: ClipRt { start: 1000, len: 4, offset: 0, gain: 1.0, mute: false, data: data.clone(), fades: ClipFades::NONE } }).ok();
        }
        tx.push(Command::AddTrack(Box::new(TrackRt::new_bus(3, params())))).ok();
        chain(&mut tx, 4, 10, Box::new(Latent::new(120)));
        chain(&mut tx, 3, 11, Box::new(Latent::new(300)));
        chain(&mut tx, 2, 12, Box::new(Latent::new(100)));
        chain(&mut tx, MASTER, 13, Box::new(Latent::new(50)));
        tx.push(Command::Play).ok();
        let out = run(&mut m, 10);
        // Slowest path: 120 + 300 to the master, plus the master's 50.
        assert_eq!(shared.latency.load(Ordering::Relaxed), 470);
        let hits: Vec<(usize, f32)> = out.iter().enumerate().filter(|(_, v)| v.abs() > 1e-6).map(|(i, v)| (i, *v)).collect();
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].0 - 470, 1000);
        assert!((hits[0].1 - 0.75).abs() < 1e-6);
    }

    fn sends(tx: &mut Producer<Command>, track: u64, sends: Vec<SendRt>) {
        tx.push(Command::SetSends { track, sends: Box::new(sends) }).ok();
    }

    #[test]
    fn sends_tap_before_or_after_the_fader() {
        let (mut m, mut tx, _g, _s) = mixer();
        // Track 1 (fader 0.5) to the master, with a send at 0.5 to bus 3 (x2 effect).
        let track = TrackParams { monitor: true, volume: 0.5, ..params() };
        tx.push(Command::AddTrack(Box::new(TrackRt::new(1, track)))).ok();
        tx.push(Command::AddTrack(Box::new(TrackRt::new_bus(3, params())))).ok();
        chain(&mut tx, 3, 9, Box::new(Gain(2.0)));
        sends(&mut tx, 1, vec![SendRt::new(1, 3, 0.5, false, false)]);
        assert!((level(&mut m) - 0.1).abs() < 1e-6, "post-fader: 0.05 dry + 0.05 * 0.5 * 2");
        sends(&mut tx, 1, vec![SendRt::new(1, 3, 0.5, true, false)]);
        assert!((level(&mut m) - 0.15).abs() < 1e-6, "pre-fader: 0.05 + 0.1 * 0.5 * 2");
        sends(&mut tx, 1, vec![SendRt::new(1, 3, 0.5, true, true)]);
        assert!((level(&mut m) - 0.05).abs() < 1e-6, "muted send");
        sends(&mut tx, 1, vec![SendRt::new(1, 3, 0.5, true, false)]);
        tx.push(Command::SetTrack { track: 1, params: TrackParams { mute: true, ..track } }).ok();
        assert_eq!(level(&mut m), 0.0, "a muted track sends nothing, pre-fader or not");
        // Send level automation (silent from the start) overrides the level.
        tx.push(Command::SetTrack { track: 1, params: track }).ok();
        tx.push(Command::SetAutomation { track: 1, lanes: Box::new(vec![env(EnvTarget::Send { id: 1 }, &[(0, 0.0, opentake_project::Curve::Linear)])]) }).ok();
        level(&mut m);
        assert!((level(&mut m) - 0.05).abs() < 1e-6, "automated send level");
    }

    #[test]
    fn solo_keeps_the_buses_a_track_sends_to() {
        let (mut m, mut tx, _g, _s) = mixer();
        // Tracks 1 and 2 to the master, both sending to bus 3 at unity.
        let monitor = TrackParams { monitor: true, ..params() };
        for id in [1, 2] {
            tx.push(Command::AddTrack(Box::new(TrackRt::new(id, monitor)))).ok();
            sends(&mut tx, id, vec![SendRt::new(1, 3, 1.0, false, false)]);
        }
        tx.push(Command::AddTrack(Box::new(TrackRt::new_bus(3, params())))).ok();
        assert!((level(&mut m) - 0.4).abs() < 1e-6);
        tx.push(Command::SetTrack { track: 1, params: TrackParams { solo: true, ..monitor } }).ok();
        assert!((level(&mut m) - 0.2).abs() < 1e-6, "track 1 dry and through the bus it sends to");
        tx.push(Command::SetTrack { track: 1, params: monitor }).ok();
        tx.push(Command::SetTrack { track: 3, params: TrackParams { solo: true, ..params() } }).ok();
        assert!((level(&mut m) - 0.4).abs() < 1e-6, "a soloed bus keeps the tracks sending to it");
    }

    #[test]
    fn plugin_delay_is_compensated_through_sends() {
        let (mut m, mut tx, _g, shared) = mixer();
        let mut impulse = vec![0.0f32; 4];
        impulse[0] = 0.25;
        let data = Arc::new(AudioData { channels: vec![impulse], sample_rate: 48_000 });
        // 1 (120) to the master, 2 (dry) to bus 5 (80); both send at 0.5 to bus 3 (300), 2
        // pre-fader. The master has 50.
        for (id, output) in [(1, MASTER), (2, 5)] {
            tx.push(Command::AddTrack(Box::new(TrackRt::new(id, TrackParams { output, ..params() })))).ok();
            tx.push(Command::AddClip { track: id, clip: ClipRt { start: 1000, len: 4, offset: 0, gain: 1.0, mute: false, data: data.clone(), fades: ClipFades::NONE } }).ok();
        }
        sends(&mut tx, 1, vec![SendRt::new(1, 3, 0.5, false, false)]);
        sends(&mut tx, 2, vec![SendRt::new(1, 3, 0.5, true, false)]);
        tx.push(Command::AddTrack(Box::new(TrackRt::new_bus(3, params())))).ok();
        tx.push(Command::AddTrack(Box::new(TrackRt::new_bus(5, params())))).ok();
        chain(&mut tx, 1, 10, Box::new(Latent::new(120)));
        chain(&mut tx, 3, 11, Box::new(Latent::new(300)));
        chain(&mut tx, 5, 12, Box::new(Latent::new(80)));
        chain(&mut tx, MASTER, 13, Box::new(Latent::new(50)));
        tx.push(Command::Play).ok();
        let out = run(&mut m, 10);
        // Slowest path: track 1 through the reverb bus (120 + 300), plus the master's 50.
        assert_eq!(shared.latency.load(Ordering::Relaxed), 470);
        let hits: Vec<(usize, f32)> = out.iter().enumerate().filter(|(_, v)| v.abs() > 1e-6).map(|(i, v)| (i, *v)).collect();
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].0 - 470, 1000);
        assert!((hits[0].1 - 0.75).abs() < 1e-6, "two dry paths and both sends");
    }

    #[test]
    fn recording_notes_its_pre_roll_and_the_measured_round_trip() {
        let (mut m, mut tx, _g, shared) = mixer();
        tx.push(Command::AddTrack(Box::new(TrackRt::new(1, params())))).ok();
        let slow = Box::new(PluginRt { instance: 9, processor: Box::new(Latent::new(300)), bypass: false });
        tx.push(Command::SetPluginChain { track: 1, order: Box::new(vec![(9, false)]), new: Box::new(vec![slow]), target: Box::new(Vec::with_capacity(PLUGIN_CAPACITY)) }).ok();
        assert_eq!(shared.io_latency.load(Ordering::Relaxed), LATENCY_UNKNOWN);
        m.report_io_latency(240.0);
        m.report_io_latency(250.0);
        assert_eq!(shared.io_latency.load(Ordering::Relaxed), 241, "smoothed");
        tx.push(Command::Seek(10_000)).ok();
        tx.push(Command::Record(true)).ok();
        run(&mut m, 2);
        assert_eq!(shared.record_start.load(Ordering::Relaxed), 10_000);
        assert_eq!(shared.record_origin.load(Ordering::Relaxed), 10_000 - 300, "recording starts in the pre-roll");
        assert_eq!(shared.record_latency.load(Ordering::Relaxed), 241);
        // A big jump (new buffer size) is taken as is.
        m.report_io_latency(2000.0);
        assert_eq!(shared.io_latency.load(Ordering::Relaxed), 2000);
    }

    #[test]
    fn cycle_recording_loops_and_notes_the_first_wrap() {
        let (mut m, mut tx, _g, shared) = mixer();
        tx.push(Command::SetLoop { enabled: true, start: 1000, end: 2000 }).ok();
        tx.push(Command::Seek(1000)).ok();
        tx.push(Command::Record(true)).ok();
        run(&mut m, 2); // 960 frames: not round yet
        assert_eq!(shared.record_wrap.load(Ordering::Relaxed), NO_WRAP);
        run(&mut m, 4);
        assert!(shared.recording.load(Ordering::Relaxed));
        assert_eq!(shared.record_wrap.load(Ordering::Relaxed), 1000, "wrapped after one cycle of recording");
        assert_eq!([0, 1].map(|i| shared.record_cycle[i].load(Ordering::Relaxed)), [1000, 2000]);
        assert_eq!(shared.position.load(Ordering::Relaxed), 1000 + (6 * 480 - 2000) as u64, "still cycling");
        // A new recording starts afresh.
        tx.push(Command::Stop).ok();
        tx.push(Command::Seek(1000)).ok();
        tx.push(Command::Record(true)).ok();
        run(&mut m, 1);
        assert_eq!(shared.record_wrap.load(Ordering::Relaxed), NO_WRAP);
    }

    #[test]
    fn compensated_playback_wraps_exactly_at_the_cycle() {
        let (mut m, mut tx, _g, _s) = mixer();
        let mut impulse = vec![0.0f32; 4];
        impulse[0] = 0.5;
        let data = Arc::new(AudioData { channels: vec![impulse], sample_rate: 48_000 });
        tx.push(Command::AddTrack(Box::new(TrackRt::new(1, params())))).ok();
        tx.push(Command::AddClip { track: 1, clip: ClipRt { start: 100, len: 4, offset: 0, gain: 1.0, mute: false, data, fades: ClipFades::NONE } }).ok();
        let slow = Box::new(PluginRt { instance: 9, processor: Box::new(Latent::new(300)), bypass: false });
        tx.push(Command::SetPluginChain { track: 1, order: Box::new(vec![(9, false)]), new: Box::new(vec![slow]), target: Box::new(Vec::with_capacity(PLUGIN_CAPACITY)) }).ok();
        tx.push(Command::SetLoop { enabled: true, start: 0, end: 2000 }).ok();
        tx.push(Command::Play).ok();
        let out = run(&mut m, 10); // 4800 samples = 2.4 passes
        let hits: Vec<usize> = out.iter().enumerate().filter(|(_, v)| v.abs() > 1e-6).map(|(i, _)| i).collect();
        // Every pass hears the impulse 100 samples in, including the first (nothing is skipped
        // at the start). Output index = timeline position + the 300-sample pre-roll.
        assert_eq!(hits, vec![100 + 300, 2100 + 300, 4100 + 300], "{hits:?}");
    }

    struct Gain(f32);

    impl Processor for Gain {
        fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], frames: usize, _: &Transport, _: &[MidiEvent]) {
            for ch in 0..2 {
                for i in 0..frames {
                    outputs[ch][i] = inputs[ch][i] * self.0;
                }
            }
        }
        fn set_param(&mut self, _: u32, v: f64) {
            self.0 = v as f32;
        }
    }

    #[test]
    fn master_inserts_process_the_master_bus() {
        let (mut m, mut tx, _g, _s) = mixer();
        tx.push(Command::AddTrack(Box::new(TrackRt::new(1, TrackParams { monitor: true, ..params() })))).ok();
        let plugin = Box::new(PluginRt { instance: 7, processor: Box::new(Gain(0.5)), bypass: false });
        tx.push(Command::SetPluginChain { track: MASTER, order: Box::new(vec![(7, false)]), new: Box::new(vec![plugin]), target: Box::new(Vec::with_capacity(PLUGIN_CAPACITY)) }).ok();
        let input = vec![0.8f32; 64];
        let mut out = vec![0.0; 64];
        m.process(&input, 1, &mut out, 2);
        assert!((out[0] - 0.4).abs() < 1e-6, "{}", out[0]);
        tx.push(Command::SetPluginParam { track: MASTER, instance: 7, id: 0, value: 0.25 }).ok();
        m.process(&input, 1, &mut out, 2);
        assert!((out[0] - 0.2).abs() < 1e-6);
        tx.push(Command::SetPluginBypass { track: MASTER, instance: 7, bypass: true }).ok();
        m.process(&input, 1, &mut out, 2);
        assert!((out[0] - 0.8).abs() < 1e-6);
    }

    /// Records the events an instrument receives.
    struct Probe(std::sync::Arc<std::sync::Mutex<Vec<(u64, MidiEvent)>>>, u64);

    impl Processor for Probe {
        fn process(&mut self, _: &[&[f32]], outputs: &mut [&mut [f32]], frames: usize, t: &Transport, events: &[MidiEvent]) {
            for e in events {
                self.0.lock().unwrap().push((t.position_samples as u64 + e.offset as u64, *e));
            }
            for o in outputs.iter_mut() {
                o[..frames].fill(0.25);
            }
            self.1 += 1;
        }
        fn set_param(&mut self, _: u32, _: f64) {}
    }

    #[test]
    fn midi_notes_reach_instrument_sample_accurately() {
        let (mut m, mut tx, _g, _s) = mixer();
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut t = TrackRt::new(1, params());
        t.instrument = Some(Box::new(PluginRt { instance: 9, processor: Box::new(Probe(log.clone(), 0)), bypass: false }));
        t.midi.push(MidiClipRt {
            start: 0,
            end: 2000,
            mute: false,
            notes: vec![NoteRt { start: 100, end: 700, pitch: 60, velocity: 100 }, NoteRt { start: 600, end: 5000, pitch: 64, velocity: 90 }],
        });
        tx.push(Command::AddTrack(Box::new(t))).ok();
        tx.push(Command::Play).ok();
        let mut out = vec![0.0; 2 * 512];
        m.process(&[], 0, &mut out, 2);
        m.process(&[], 0, &mut out, 2);
        m.process(&[], 0, &mut out, 2);
        assert!(out.iter().all(|&s| (s - 0.25).abs() < 1e-6), "instrument output is mixed in");
        let ev = log.lock().unwrap().clone();
        assert!(ev.contains(&(100, MidiEvent { offset: 100, pitch: 60, velocity: 100 })), "{ev:?}");
        assert!(ev.iter().any(|&(at, e)| at == 700 && e.pitch == 60 && e.velocity == 0), "note-off at 700: {ev:?}");
        assert!(ev.iter().any(|&(at, e)| at == 600 && e.pitch == 64 && e.velocity == 90));
        // Second note is clamped to its region end (2000 → block containing 2000 is not reached yet).
        // Stopping releases the still-sounding note.
        tx.push(Command::Stop).ok();
        m.process(&[], 0, &mut out, 2);
        let ev = log.lock().unwrap().clone();
        assert!(ev.iter().any(|&(_, e)| e.pitch == 64 && e.velocity == 0), "stop sends note-off: {ev:?}");
    }

    struct Tag(u64);
    impl Processor for Tag {
        fn process(&mut self, _: &[&[f32]], outputs: &mut [&mut [f32]], frames: usize, _: &Transport, _: &[MidiEvent]) {
            for o in outputs.iter_mut() {
                for s in &mut o[..frames] {
                    *s += self.0 as f32;
                }
            }
        }
        fn set_param(&mut self, _: u32, _: f64) {}
    }

    #[test]
    fn plugin_chain_is_reordered_without_recreating() {
        let (mut m, mut tx, mut g, _s) = mixer();
        tx.push(Command::AddTrack(Box::new(TrackRt::new(1, params())))).ok();
        let new = vec![
            Box::new(PluginRt { instance: 1, processor: Box::new(Tag(1)), bypass: false }),
            Box::new(PluginRt { instance: 2, processor: Box::new(Tag(2)), bypass: false }),
        ];
        let target = || Box::new(Vec::with_capacity(PLUGIN_CAPACITY));
        tx.push(Command::SetPluginChain { track: 1, order: Box::new(vec![(1, false), (2, false)]), new: Box::new(new), target: target() }).ok();
        m.process(&[], 0, &mut [0.0; 4], 2);
        assert_eq!(m.tracks[0].plugins.iter().map(|p| p.instance).collect::<Vec<_>>(), vec![1, 2]);
        // Drop instance 1, keep 2 (same processor object), add nothing new.
        tx.push(Command::SetPluginChain { track: 1, order: Box::new(vec![(2, true)]), new: Box::new(Vec::new()), target: target() }).ok();
        m.process(&[], 0, &mut [0.0; 4], 2);
        assert_eq!(m.tracks[0].plugins.len(), 1);
        assert!(m.tracks[0].plugins[0].bypass);
        let mut freed = 0;
        while let Ok(gb) = g.pop() {
            if let Garbage::PluginChain(v) = gb {
                freed += v.len();
            }
        }
        assert_eq!(freed, 1, "removed plugin is handed back for freeing");
    }

    #[test]
    fn loop_wraps_sample_accurately() {
        let (mut m, mut tx, _g, shared) = mixer();
        tx.push(Command::SetLoop { enabled: true, start: 100, end: 300 }).ok();
        tx.push(Command::Seek(250)).ok();
        tx.push(Command::Play).ok();
        let mut out = vec![0.0; 2 * 128];
        m.process(&[], 0, &mut out, 2); // 250 -> 300 (wrap to 100) -> 178
        assert_eq!(shared.position.load(Ordering::Relaxed), 100 + (128 - 50));
        // Starting after the loop end plays straight through.
        tx.push(Command::Seek(1000)).ok();
        m.process(&[], 0, &mut out, 2);
        assert_eq!(shared.position.load(Ordering::Relaxed), 1128);
        // Disabled: no wrap.
        tx.push(Command::SetLoop { enabled: false, start: 100, end: 300 }).ok();
        tx.push(Command::Seek(250)).ok();
        m.process(&[], 0, &mut out, 2);
        assert_eq!(shared.position.load(Ordering::Relaxed), 378);
    }

    fn dc_clip(start: u64, len: usize, v: f32) -> ClipRt {
        ClipRt { start, len: len as u64, offset: 0, gain: 1.0, mute: false, data: Arc::new(AudioData { channels: vec![vec![v; len]], sample_rate: 48_000 }), fades: ClipFades::NONE }
    }

    fn faded(start: u64, len: usize, fade_in: Fade, fade_out: Fade) -> ClipRt {
        let end = start + len as u64;
        ClipRt { fades: ClipFades::new(start, end, (fade_in, fade_out), 48_000.0), ..dc_clip(start, len, 0.5) }
    }

    #[test]
    fn clip_fades_play_sample_accurately() {
        let (mut m, mut tx, _g, _s) = mixer();
        tx.push(Command::AddTrack(Box::new(TrackRt::new(1, params())))).ok();
        // 100 samples of linear fade-in, 200 of S-curve fade-out, across block boundaries.
        let ms = |samples: f64| samples / 48_000.0;
        let clip = faded(400, 1000, Fade::new(ms(100.0), FadeCurve::Linear), Fade::new(ms(200.0), FadeCurve::SCurve));
        tx.push(Command::AddClip { track: 1, clip }).ok();
        tx.push(Command::Play).ok();
        let out = run(&mut m, 4);
        assert_eq!(out[400], 0.0, "starts from silence");
        assert!((out[450] - 0.25).abs() < 1e-6, "halfway through the fade-in");
        assert!(out[500..1200].iter().all(|&v| v == 0.5), "full level between the fades");
        assert!((out[1300] - 0.25).abs() < 1e-6 && out[1399] < 1e-3 && out[1400] == 0.0);
    }

    #[test]
    fn overlapping_clips_crossfade_instead_of_doubling() {
        // Two identical signals overlapping over 1000..1400.
        for (curve, bound) in [(FadeCurve::Linear, 0.5), (FadeCurve::EqualPower, 0.5 * std::f32::consts::SQRT_2)] {
            let (mut m, mut tx, _g, _s) = mixer();
            tx.push(Command::AddTrack(Box::new(TrackRt::new(1, params())))).ok();
            let x = Fade::new(400.0 / 48_000.0, curve);
            tx.push(Command::AddClip { track: 1, clip: faded(0, 1400, Fade::default(), x) }).ok();
            tx.push(Command::AddClip { track: 1, clip: faded(1000, 1000, x, Fade::default()) }).ok();
            tx.push(Command::Play).ok();
            let out = run(&mut m, 5);
            let peak = out[..2000].iter().fold(0.0f32, |a, &v| a.max(v));
            let dip = out[..2000].iter().fold(1.0f32, |a, &v| a.min(v));
            assert!(peak <= bound + 1e-5 && dip >= 0.5 - 1e-5, "{curve:?}: {dip}..{peak}");
            if curve == FadeCurve::Linear {
                assert!(out[..2000].iter().all(|&v| (v - 0.5).abs() < 1e-5), "equal gain keeps unity through the overlap");
            }
        }
    }

    fn env(target: EnvTarget, points: &[(u64, f32, opentake_project::Curve)]) -> EnvelopeRt {
        EnvelopeRt::new(target, points.iter().map(|&(at, value, curve)| AutomationPoint { time_secs: at as f64 / 48_000.0, value, curve }).collect())
    }

    #[test]
    fn volume_automation_lines_up_through_plugin_delay_and_the_cycle() {
        use opentake_project::Curve::{Hold, Linear};
        use opentake_project::automation::gain_to_pos;
        let (mut m, mut tx, _g, _s) = mixer();
        tx.push(Command::AddTrack(Box::new(TrackRt::new(1, params())))).ok();
        tx.push(Command::AddClip { track: 1, clip: dc_clip(0, 2000, 0.5) }).ok();
        // 300 samples of delay on the track and 200 on the master: the fader sits 200 samples
        // before the output, the tracks read 500 ahead.
        chain(&mut tx, 1, 9, Box::new(Latent::new(300)));
        chain(&mut tx, MASTER, 10, Box::new(Latent::new(200)));
        // Unity until 1000, then silent (a step).
        let lanes = vec![env(EnvTarget::Volume, &[(0, gain_to_pos(1.0), Hold), (1000, 0.0, Linear)])];
        tx.push(Command::SetAutomation { track: 1, lanes: Box::new(lanes) }).ok();
        tx.push(Command::SetLoop { enabled: true, start: 0, end: 2000 }).ok();
        tx.push(Command::Play).ok();
        let out = run(&mut m, 12);
        // Output sample i plays timeline (i - 500) in the cycle (after the 500-sample pre-roll).
        // Changes land within a block of where they are, also around the wrap.
        for (i, &v) in out.iter().enumerate().skip(500 + 2) {
            let t = (i - 500) % 2000;
            let expect = if (AUTOMATION_BLOCK..1000 - AUTOMATION_BLOCK).contains(&t) { 0.5 } else if (1000 + AUTOMATION_BLOCK..2000 - AUTOMATION_BLOCK).contains(&t) { 0.0 } else { continue };
            assert!((v - expect).abs() < 1e-4, "output {i} (timeline {t}): {v}");
        }
        // The step ramps within one block, not before it.
        assert!(out[500 + 1000 - AUTOMATION_BLOCK - 1] > 0.49 && out[500 + 2000 + 1000 - AUTOMATION_BLOCK - 1] > 0.49);
    }

    #[test]
    fn plugin_parameter_automation_reaches_the_processor_ahead_of_its_delay() {
        use opentake_project::Curve::{Hold, Linear};
        let (mut m, mut tx, _g, _s) = mixer();
        tx.push(Command::AddTrack(Box::new(TrackRt::new(1, TrackParams { monitor: true, ..params() })))).ok();
        tx.push(Command::AddClip { track: 1, clip: dc_clip(0, 4000, 0.5) }).ok();
        // A gain, then 300 samples of delay: the gain processes timeline audio 300 ahead.
        let gain = Box::new(PluginRt { instance: 7, processor: Box::new(Gain(1.0)), bypass: false });
        let late = Box::new(PluginRt { instance: 8, processor: Box::new(Latent::new(300)), bypass: false });
        tx.push(Command::SetPluginChain { track: 1, order: Box::new(vec![(7, false), (8, false)]), new: Box::new(vec![gain, late]), target: Box::new(Vec::with_capacity(PLUGIN_CAPACITY)) }).ok();
        let lanes = vec![env(EnvTarget::Param { instance: 7, id: 0 }, &[(0, 1.0, Hold), (1000, 0.25, Linear)])];
        tx.push(Command::SetAutomation { track: 1, lanes: Box::new(lanes) }).ok();
        // Stopped, the value at the playhead applies (to the monitored input here, once it's
        // through the delay).
        let monitored = |m: &mut Mixer| {
            let mut out = vec![0.0; 64];
            for _ in 0..12 {
                m.process(&[0.8; 32], 1, &mut out, 2);
            }
            out[62]
        };
        tx.push(Command::Seek(1500)).ok();
        assert!((monitored(&mut m) - 0.2).abs() < 1e-6);
        tx.push(Command::Seek(0)).ok();
        assert!((monitored(&mut m) - 0.8).abs() < 1e-6);

        tx.push(Command::Play).ok();
        let out = run(&mut m, 6);
        // Output i = timeline i - 300; the gain drops at 1000 to the block.
        let first_low = out.iter().skip(600).position(|&v| (v - 0.125).abs() < 1e-6).map(|i| i + 600 - 300).unwrap();
        assert!((1000..1000 + AUTOMATION_BLOCK).contains(&first_low), "{first_low}");
        assert!(out[300 + 990] > 0.49);
    }

    #[test]
    fn master_volume_automation_and_garbage() {
        use opentake_project::Curve::Linear;
        let (mut m, mut tx, mut g, _s) = mixer();
        tx.push(Command::AddTrack(Box::new(TrackRt::new(1, params())))).ok();
        tx.push(Command::AddClip { track: 1, clip: dc_clip(0, 4000, 0.5) }).ok();
        let lanes = vec![env(EnvTarget::Volume, &[(0, 0.0, Linear)])];
        tx.push(Command::SetAutomation { track: MASTER, lanes: Box::new(lanes) }).ok();
        tx.push(Command::Play).ok();
        let out = run(&mut m, 2);
        assert!(out[100..].iter().all(|&v| v == 0.0), "master fader automated down");
        tx.push(Command::SetAutomation { track: MASTER, lanes: Box::new(Vec::new()) }).ok();
        let out = run(&mut m, 2);
        assert!((out[600] - 0.5).abs() < 1e-6, "lanes removed: back to the fader");
        let mut freed = false;
        while let Ok(gb) = g.pop() {
            freed |= matches!(gb, Garbage::Automation(v) if v.len() == 1);
        }
        assert!(freed, "replaced envelopes are freed off the audio thread");
    }

    #[test]
    fn removed_tracks_go_to_garbage() {
        let (mut m, mut tx, mut g, _s) = mixer();
        tx.push(Command::AddTrack(Box::new(TrackRt::new(7, params())))).ok();
        tx.push(Command::RemoveTrack(7)).ok();
        m.process(&[], 0, &mut [0.0; 8], 2);
        assert!(matches!(g.pop(), Ok(Garbage::Track(t)) if t.id == 7));
    }
}
