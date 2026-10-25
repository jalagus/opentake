//! Audio-thread state. Everything in here runs inside the device callback, so it
//! must not block or allocate: structural changes arrive as pre-built boxes over a
//! lock-free queue, and anything removed is sent back to be freed on another thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use rtrb::{Consumer, Producer};

use opentake_project::TempoMap;

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
/// Track id addressing the master bus in plug-in commands.
pub const MASTER: u64 = u64::MAX;

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
}

pub struct TrackRt {
    pub id: u64,
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
    /// Delay line that lines the track up with the slowest track (plug-in delay compensation).
    comp: [Vec<f32>; 2],
    comp_write: usize,
}

impl TrackRt {
    /// Allocates everything the track will need on the audio thread. Call off-thread.
    pub fn new(id: u64, params: TrackParams) -> Self {
        Self {
            id,
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
            comp: [vec![0.0; MAX_COMP], vec![0.0; MAX_COMP]],
            comp_write: 0,
        }
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
    /// End of the last region (samples); playback stops when it passes it (0 = never).
    SetSongEnd(u64),
    SetMasterGain(f32),
    SetMetronome { enabled: bool, gain: f32 },
    /// Cycle playback between two sample positions (ignored while recording).
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
    /// Audition a note on an instrument track (velocity 0 = off).
    LiveNote { track: u64, pitch: u8, velocity: u8 },
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
}

/// State the audio thread publishes for the control side.
pub struct SharedState {
    pub position: AtomicU64,
    pub playing: AtomicBool,
    pub recording: AtomicBool,
    /// Timeline position (samples) where the current/last recording started.
    pub record_start: AtomicU64,
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
    counting: Option<CountIn>,
    song_end: u64,
    master_plugins: Vec<Box<PluginRt>>,
    /// Slowest track's plug-in delay, and the timeline read-ahead that hides all delays.
    max_track_latency: usize,
    lookahead: u64,
    sample_rate: f64,
    loop_range: Option<(u64, u64)>,
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
            counting: None,
            song_end: 0,
            master_plugins: Vec::with_capacity(PLUGIN_CAPACITY),
            max_track_latency: 0,
            lookahead: 0,
            sample_rate: sample_rate.max(1) as f64,
            loop_range: None,
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

    /// The loop applies while playing, but not while recording.
    fn active_loop(&self) -> Option<(u64, u64)> {
        self.loop_range.filter(|_| self.transport.playing && !self.transport.recording)
    }

    fn flush_all_notes(&mut self) {
        for t in &mut self.tracks {
            t.flush = true;
        }
    }

    /// Plug-in delay compensation: each track's latency, the slowest one, and the read-ahead
    /// (slowest track + master chain). Tracks read the timeline that far ahead and the faster
    /// ones are delayed to match, so the output lines up with the playhead.
    fn update_latency(&mut self) {
        let chain = |plugins: &[Box<PluginRt>]| plugins.iter().filter(|p| !p.bypass).map(|p| p.processor.latency()).sum::<usize>();
        let mut max = 0;
        for t in &mut self.tracks {
            let inst = t.instrument.as_ref().filter(|p| !p.bypass).map_or(0, |p| p.processor.latency());
            t.latency = (inst + chain(&t.plugins)).min(MAX_COMP - 1);
            max = max.max(t.latency);
        }
        self.max_track_latency = max;
        self.lookahead = (max + chain(&self.master_plugins)) as u64;
        self.shared.latency.store(self.lookahead as u32, Ordering::Relaxed);
    }

    /// Where the tracks read the timeline for output at playhead `pos`: `lookahead` ahead,
    /// wrapping at the cycle end. (`pos` is negative during a start's pre-roll.)
    fn read_position(&self, pos: i64) -> u64 {
        let ahead = (pos + self.lookahead as i64).max(0) as u64;
        match self.active_loop() {
            Some((start, end)) if pos >= 0 && (pos as u64) < end && ahead >= end => start + (ahead - end),
            _ => ahead,
        }
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
                    if on && !self.transport.recording {
                        self.shared.record_start.store(self.transport.position_samples as u64, Ordering::Relaxed);
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
                Command::SetSongEnd(end) => self.song_end = end,
                Command::AddTrack(t) => {
                    if self.tracks.len() < MAX_TRACKS {
                        self.tracks.push(t);
                    } else {
                        self.discard(Garbage::Track(t));
                    }
                }
                Command::RemoveTrack(id) => {
                    if let Some(i) = self.tracks.iter().position(|t| t.id == id) {
                        let t = self.tracks.remove(i);
                        self.discard(Garbage::Track(t));
                    }
                }
                Command::ResetTracks(mut new) => {
                    std::mem::swap(&mut self.tracks, &mut *new);
                    self.discard(Garbage::Tracks(new));
                }
                Command::SetTrack { track, params } => {
                    if let Some(t) = self.track_mut(track) {
                        t.params = params;
                    }
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
                }
                Command::ReorderTracks(order) => {
                    self.tracks.sort_unstable_by_key(|t| order.iter().position(|&id| id == t.id).unwrap_or(usize::MAX));
                    self.discard(Garbage::Ids(order));
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

    /// Process interleaved device buffers. `input` may be empty (no input device).
    pub fn process(&mut self, input: &[f32], in_channels: usize, output: &mut [f32], out_channels: usize) {
        self.handle_commands();
        self.update_latency();
        let frames = output.len() / out_channels.max(1);
        let in_frames = if in_channels > 0 { input.len() / in_channels } else { 0 };
        let mut done = 0;
        while done < frames {
            let mut n = (frames - done).min(MAX_BLOCK);
            // End the block exactly where a count-in finishes, so playback starts on time.
            if let Some(c) = &self.counting {
                n = n.min((c.total - c.done) as usize).max(1);
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

        if self.transport.recording {
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
        let any_solo = self.tracks.iter().any(|t| t.params.solo);
        // Tracks render the timeline `lookahead` ahead; plug-ins see that position.
        let pos = self.read_position(self.transport.position_samples);
        let block_transport = Transport { position_samples: pos as i64, ..self.transport };
        let max_latency = self.max_track_latency;

        for (ti, track) in self.tracks.iter_mut().enumerate() {
            let [l, r] = &mut track.buf;
            l[..n].fill(0.0);
            r[..n].fill(0.0);

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
                    for i in 0..count {
                        l[dst_start + i] += src_l[src_start + i] * clip.gain;
                        r[dst_start + i] += src_r[src_start + i] * clip.gain;
                    }
                }
            }

            let p = &track.params;
            if p.monitor && p.input_channel < in_ch {
                for i in 0..n {
                    let s = self.input[p.input_channel][i];
                    l[i] += s;
                    r[i] += s;
                }
            }

            if let Some(inst) = track.instrument.as_mut() {
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

            for plugin in track.plugins.iter_mut().filter(|p| !p.bypass) {
                let [sl, sr] = &mut self.scratch;
                {
                    let inputs: [&[f32]; 2] = [&l[..n], &r[..n]];
                    let mut outputs: [&mut [f32]; 2] = [&mut sl[..n], &mut sr[..n]];
                    plugin.processor.process(&inputs, &mut outputs, n, &block_transport, &[]);
                }
                l[..n].copy_from_slice(&sl[..n]);
                r[..n].copy_from_slice(&sr[..n]);
            }

            // Line the track up with the slowest one (plug-in delay compensation).
            let delay = max_latency - track.latency;
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
            let (gl, gr) = pan_gains(p.volume, p.pan);
            let mut peak = 0.0f32;
            let audible = !p.mute && (!any_solo || p.solo);
            for i in 0..n {
                let (a, b) = (l[i] * gl, r[i] * gr);
                peak = peak.max(a.abs()).max(b.abs());
                if audible {
                    self.master[0][i] += a;
                    self.master[1][i] += b;
                }
            }
            update_peak(&self.shared.track_peaks[ti], peak);
        }

        // Master inserts, then the master fader.
        for plugin in self.master_plugins.iter_mut().filter(|p| !p.bypass) {
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
        for (ch, m) in self.master.iter_mut().enumerate() {
            for s in &mut m[..n] {
                *s *= self.master_gain;
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
        TrackParams { volume: 1.0, pan: 0.0, mute: false, solo: false, armed: false, monitor: false, input_channel: 0 }
    }

    #[test]
    fn plays_clip_at_position() {
        let (mut m, mut tx, _g, shared) = mixer();
        let data = Arc::new(AudioData { channels: vec![vec![0.5; 100]], sample_rate: 48_000 });
        tx.push(Command::AddTrack(Box::new(TrackRt::new(1, params())))).ok();
        tx.push(Command::AddClip { track: 1, clip: ClipRt { start: 10, len: 100, offset: 0, gain: 1.0, mute: false, data } }).ok();
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
            tx.push(Command::AddClip { track: id, clip: ClipRt { start: 1000, len: 4, offset: 0, gain: 1.0, mute: false, data: data.clone() } }).ok();
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

    #[test]
    fn compensated_playback_wraps_exactly_at_the_cycle() {
        let (mut m, mut tx, _g, _s) = mixer();
        let mut impulse = vec![0.0f32; 4];
        impulse[0] = 0.5;
        let data = Arc::new(AudioData { channels: vec![impulse], sample_rate: 48_000 });
        tx.push(Command::AddTrack(Box::new(TrackRt::new(1, params())))).ok();
        tx.push(Command::AddClip { track: 1, clip: ClipRt { start: 100, len: 4, offset: 0, gain: 1.0, mute: false, data } }).ok();
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

    #[test]
    fn removed_tracks_go_to_garbage() {
        let (mut m, mut tx, mut g, _s) = mixer();
        tx.push(Command::AddTrack(Box::new(TrackRt::new(7, params())))).ok();
        tx.push(Command::RemoveTrack(7)).ok();
        m.process(&[], 0, &mut [0.0; 8], 2);
        assert!(matches!(g.pop(), Ok(Garbage::Track(t)) if t.id == 7));
    }
}
