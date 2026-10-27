//! Live MIDI: which input ports are open, which instrument tracks play what's played on
//! them, and recording it into MIDI regions.
//!
//! Like Logic, input plays every record-armed instrument track, or when none is armed the
//! selected one. Armed instrument tracks record: the audio thread sends each message back
//! at the timeline position the performer heard when playing it (see `Mixer::route_midi`:
//! the playhead when it was processed, less the buffer it waited for and the output
//! latency), so a note played in time with what's heard lands on the beat. On stop each
//! armed instrument track gets one region spanning the recording; a cycle recording merges
//! every pass into one region spanning the cycle (overdub), from the record point if that
//! was earlier. Pitch bend, controllers (the sustain pedal too, like Logic: notes keep the
//! length they were held) and channel pressure are recorded into the region's controller
//! lanes the same way. There's no input monitoring switch: input always plays through the
//! receiving track's instrument. The take is timed in samples; on stop it becomes quarter
//! notes through the tempo map (and, with "Quantize on record", snaps to its grid).

use std::sync::atomic::Ordering;

use anyhow::{Result, anyhow};
use opentake_project::{Controller, ControllerEvent, ControllerLane, MidiClip, Note};
use serde::Serialize;

use super::Engine;
use super::midi::{MIN_NOTE_SECS, sorted};
use super::recording::RecordPlacement;
use crate::midi_input::{MidiMsg, MidiPortInfo, MidiSelection, port_names};
use crate::mixer::{Command, MAX_MIDI_TARGETS, RecMidi};

/// MIDI being recorded: the armed instrument tracks (ids) and what came back so far.
#[derive(Debug, Default)]
pub(super) struct MidiTake {
    pub tracks: Vec<u64>,
    pub events: Vec<RecMidi>,
}

/// A note of a take (timeline samples).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct TakeNote {
    pub start: i64,
    pub end: i64,
    pub pitch: u8,
    pub velocity: u8,
}

/// The notes of a take: each lasts as long as its key was held (the sustain pedal is
/// recorded as a controller), and notes still held end at `end`. In a cycle recording a
/// note-off before its note-on (the key was held over the wrap) ends the note at the cycle
/// end. Notes are at least `min_len` long.
pub(super) fn take_notes(events: &[RecMidi], cycle: Option<(i64, i64)>, end: i64, min_len: i64) -> Vec<TakeNote> {
    let mut notes = Vec::new();
    let mut close = |pitch: usize, (start, velocity): (i64, u8), at: i64| {
        let at = match cycle {
            Some((_, cycle_end)) if at < start => cycle_end,
            _ => at,
        };
        notes.push(TakeNote { start, end: at.max(start + min_len), pitch: pitch as u8, velocity });
    };
    // Sounding notes (start, velocity) by pitch.
    let mut held: [Option<(i64, u8)>; 128] = [None; 128];
    for e in events {
        let RecMidi::Msg(t, msg) = *e else { continue };
        match msg {
            MidiMsg::NoteOn { pitch, velocity } => {
                if let Some(n) = held[pitch as usize].replace((t, velocity)) {
                    close(pitch as usize, n, t);
                }
            }
            MidiMsg::NoteOff { pitch } => held[pitch as usize].take().into_iter().for_each(|n| close(pitch as usize, n, t)),
            MidiMsg::AllNotesOff => (0..128).for_each(|p| held[p].take().into_iter().for_each(|n| close(p, n, t))),
            MidiMsg::Control(..) => {}
        }
    }
    (0..128).for_each(|p| held[p].take().into_iter().for_each(|n| close(p, n, end)));
    notes.sort_by_key(|n| (n.start, n.pitch));
    notes
}

/// The controller lanes of a take for a region spanning timeline samples `from..to`: every
/// controller message, at its time from the region start (`at` gives it for a sample time;
/// earlier ones at the start, so the region opens with the value they left). Cycle passes
/// merge; exact duplicates (an event at the same time, or repeating the value before it) are
/// dropped.
pub(super) fn take_controllers(events: &[RecMidi], from: i64, to: i64, at: impl Fn(i64) -> f64) -> Vec<ControllerLane> {
    let mut lanes: Vec<(Controller, Vec<ControllerEvent>)> = Vec::new();
    for e in events {
        let RecMidi::Msg(t, MidiMsg::Control(kind, raw)) = *e else { continue };
        if t >= to {
            continue;
        }
        let ev = ControllerEvent { time_beats: at(t.max(from)), value: kind.from_raw(raw) };
        match lanes.iter_mut().find(|l| l.0 == kind) {
            Some(l) => l.1.push(ev),
            None => lanes.push((kind, vec![ev])),
        }
    }
    let mut lanes: Vec<ControllerLane> = lanes.into_iter().map(|(kind, events)| ControllerLane::new(kind, events)).collect();
    lanes.sort_by_key(|l| l.kind);
    lanes
}

/// The MIDI inputs there are and which are chosen and open.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct MidiInputs {
    pub selection: MidiSelection,
    pub ports: Vec<MidiPortInfo>,
    /// Ports that couldn't be opened.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

/// A MIDI region a recording made.
#[derive(Debug, Clone, Serialize)]
pub struct RecordedMidi {
    pub track: usize,
    pub region: usize,
    pub name: String,
    pub position_secs: f64,
    pub length_secs: f64,
    /// The region in quarter notes (as it's stored).
    pub position_beats: f64,
    pub length_beats: f64,
    pub notes: usize,
    /// Controller events (pitch bend, CCs, pressure) recorded.
    pub controller_events: usize,
}

impl Engine {
    // ---------------------------------------------------------------- input ports

    pub fn midi_selection(&self) -> &MidiSelection {
        &self.midi_selection
    }

    /// The MIDI input ports, rescanned: while audio runs, ports that were plugged in since
    /// are opened (if chosen) and ones that went away are closed.
    pub fn list_midi_inputs(&mut self) -> Result<MidiInputs> {
        let scanned = match self.audio.as_mut().filter(|a| a.streams.is_some()) {
            Some(a) => {
                let commands = &mut a.commands;
                a.midi.sync(&self.midi_selection, |rx| commands.push(Command::AddMidiInput(rx)).map_err(|_| anyhow!("audio command queue is full")))
            }
            None => port_names().map(|names| (names, Vec::new())),
        };
        // No MIDI system (e.g. a Linux box without ALSA sequencer) is no ports, not a failure.
        let (names, errors) = scanned.unwrap_or_else(|e| (Vec::new(), vec![format!("{e:#}")]));
        let open = self.audio.as_ref().map(|a| a.midi.names()).unwrap_or_default();
        let mut ports: Vec<MidiPortInfo> = names.iter().map(|n| MidiPortInfo { name: n.clone(), enabled: self.midi_selection.includes(n), open: open.contains(n) }).collect();
        // Chosen ports that aren't plugged in are still listed (and open when they come back).
        if let MidiSelection::Ports(chosen) = &self.midi_selection {
            for n in chosen.iter().filter(|n| !names.contains(n)) {
                ports.push(MidiPortInfo { name: n.clone(), enabled: true, open: false });
            }
        }
        Ok(MidiInputs { selection: self.midi_selection.clone(), ports, errors })
    }

    /// Choose the MIDI inputs (a setting, not an undoable edit): all of them, or ports by
    /// name: exact, else the one connected port whose name contains it (ignoring case). A
    /// name that matches no connected port is kept, and opens when that device is plugged in.
    pub fn set_midi_inputs(&mut self, selection: MidiSelection) -> Result<MidiInputs> {
        self.midi_selection = match selection {
            MidiSelection::Ports(wanted) => {
                let names = port_names().unwrap_or_default();
                let resolve = |w: &String| {
                    let mut matches = names.iter().filter(|n| n.to_lowercase().contains(&w.to_lowercase()));
                    match (names.contains(w), matches.next(), matches.next()) {
                        (false, Some(one), None) => one.clone(),
                        _ => w.clone(),
                    }
                };
                let mut chosen: Vec<String> = Vec::new();
                for name in wanted.iter().map(resolve) {
                    if !chosen.contains(&name) {
                        chosen.push(name);
                    }
                }
                MidiSelection::Ports(chosen)
            }
            s => s,
        };
        self.list_midi_inputs()
    }


    /// Remember the choice without touching any port (e.g. from the settings, before audio
    /// starts).
    pub fn set_midi_selection(&mut self, selection: MidiSelection) {
        self.midi_selection = selection;
    }

    /// Open the chosen ports for a newly started audio engine (problems are logged).
    pub(super) fn open_midi_inputs(&mut self) {
        match self.list_midi_inputs() {
            Ok(m) => m.errors.iter().for_each(|e| tracing::warn!("MIDI input {e}")),
            Err(e) => tracing::warn!("MIDI input unavailable: {e:#}"),
        }
    }

    // ---------------------------------------------------------------- routing

    /// The selected track (the GUI's), which plays MIDI input when no instrument track is
    /// armed.
    pub fn select_track(&mut self, track: Option<usize>) -> Result<()> {
        self.selected_track = track.and_then(|t| self.project.tracks.get(t)).map(|t| t.id);
        self.sync_midi_targets()
    }

    /// Track ids that play MIDI input: the armed instrument tracks, else the selected one.
    fn midi_targets(&self) -> Vec<u64> {
        let instruments = || self.project.tracks.iter().filter(|t| t.is_instrument() && !t.bus);
        let mut ids: Vec<u64> = instruments().filter(|t| t.armed).map(|t| t.id).collect();
        if ids.is_empty() {
            ids.extend(instruments().filter(|t| Some(t.id) == self.selected_track).map(|t| t.id));
        }
        ids.truncate(MAX_MIDI_TARGETS);
        ids
    }

    /// Track indices that play MIDI input.
    pub fn midi_input_tracks(&self) -> Vec<usize> {
        self.midi_targets().iter().filter_map(|&id| self.project.track_by_id(id)).collect()
    }

    /// Tell the audio thread which tracks play MIDI input, if that changed.
    pub(super) fn sync_midi_targets(&mut self) -> Result<()> {
        let targets = self.midi_targets();
        if self.audio.is_none() || targets == self.live_midi_targets {
            return Ok(());
        }
        let mut ids = [0; MAX_MIDI_TARGETS];
        ids[..targets.len()].copy_from_slice(&targets);
        self.send(Command::SetMidiTargets { ids, len: targets.len() })?;
        self.live_midi_targets = targets;
        Ok(())
    }

    // ---------------------------------------------------------------- recording

    /// Take what the audio thread recorded so far (it's drained every GUI frame and on stop,
    /// so the ring never fills). Without a take in progress it's dropped.
    pub(super) fn drain_midi_take(&mut self) {
        let Some(a) = self.audio.as_mut() else { return };
        while let Ok(m) = a.midi_record.pop() {
            if let Some(take) = &mut self.midi_take {
                take.events.push(m);
            }
        }
    }

    /// The notes recorded so far (in quarter notes from the song start), to draw while
    /// recording.
    pub fn recording_notes(&mut self) -> Vec<Note> {
        self.drain_midi_take();
        let Some(take) = &self.midi_take else { return Vec::new() };
        let sr = self.sample_rate() as f64;
        let now = self.shared.position.load(Ordering::Relaxed) as i64;
        let cycle = self.project.loop_enabled.then_some(((self.project.loop_start_secs * sr) as i64, (self.project.loop_end_secs * sr) as i64));
        let map = self.project.tempo_map();
        let q = |samples: i64| map.quarters_at(samples.max(0) as f64 / sr);
        take_notes(&take.events, cycle, now, 1)
            .iter()
            .map(|n| Note { start_beats: q(n.start), length_beats: q(n.end) - q(n.start), pitch: n.pitch, velocity: n.velocity, tab: None })
            .collect()
    }

    /// Turn a finished take into a region on each of its tracks (none if nothing was played).
    pub(super) fn add_midi_take(&mut self, take: MidiTake, placement: &RecordPlacement) -> Result<Vec<RecordedMidi>> {
        let sr = self.sample_rate() as f64;
        let end = take.events.iter().rev().find_map(|e| if let RecMidi::End(t) = e { Some(*t) } else { None });
        let end = end.unwrap_or_else(|| self.shared.position.load(Ordering::Relaxed) as i64);
        let cycle = placement.cycle.map(|c| (c.start, c.end));
        let min_len = (MIN_NOTE_SECS * sr).ceil() as i64;
        let notes = take_notes(&take.events, cycle, end, min_len);
        let (from, to) = match cycle {
            Some((start, cycle_end)) => (placement.start.min(start), cycle_end),
            None => (placement.start, notes.iter().map(|n| n.end).fold(end, i64::max)),
        };
        let from = from.max(0);
        let secs = |samples: i64| samples as f64 / sr;
        // Samples to quarter notes from the region start, through the tempo map.
        let map = self.project.tempo_map();
        let start_q = map.quarters_at(secs(from));
        let q = |samples: i64| map.quarters_at(secs(samples)) - start_q;
        let mut notes: Vec<Note> = notes
            .iter()
            .map(|n| (n.start.max(from), n.end.min(to), n))
            .filter(|(start, end, _)| end - start >= min_len)
            .map(|(start, end, n)| Note { start_beats: q(start), length_beats: q(end) - q(start), pitch: n.pitch, velocity: n.velocity.max(1), tab: None })
            .collect();
        if let Some(quantize) = self.project.record_quantize {
            quantize.apply(&mut notes, start_q, &map, |_| true);
            notes = sorted(notes);
        }
        let controllers = take_controllers(&take.events, from, to, q);
        let controller_events = controllers.iter().map(|l| l.events.len()).sum();
        if notes.is_empty() && controller_events == 0 {
            return Ok(Vec::new());
        }
        let mut added = Vec::new();
        for id in take.tracks {
            let Some(index) = self.project.track_by_id(id) else { continue };
            let name = self.project.tracks[index].name.clone();
            let clip = MidiClip { notes: notes.clone(), controllers: controllers.clone(), ..MidiClip::new(name.clone(), start_q, q(to)) };
            self.project.tracks[index].midi_clips.push(clip);
            self.resync_midi(index)?;
            let region = self.project.tracks[index].midi_clips.len() - 1;
            let (position_secs, length_secs) = (secs(from), secs(to - from));
            added.push(RecordedMidi { track: index, region, name, position_secs, length_secs, position_beats: start_q, length_beats: q(to), notes: notes.len(), controller_events });
        }
        Ok(added)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::recording::CycleWrap;
    use crate::midi_input::RawMidi;
    use crate::mixer::Mixer;

    const fn on(t: i64, pitch: u8) -> RecMidi {
        RecMidi::Msg(t, MidiMsg::NoteOn { pitch, velocity: 100 })
    }
    const fn off(t: i64, pitch: u8) -> RecMidi {
        RecMidi::Msg(t, MidiMsg::NoteOff { pitch })
    }
    const fn note(start: i64, end: i64, pitch: u8) -> TakeNote {
        TakeNote { start, end, pitch, velocity: 100 }
    }

    #[test]
    fn take_notes_last_as_held_and_end_at_the_stop() {
        let pedal = |t, v| RecMidi::Msg(t, MidiMsg::Control(Controller::SUSTAIN, v));
        let events = [on(0, 60), off(100, 60), pedal(150, 127), on(200, 62), on(350, 62), off(400, 62), pedal(500, 0), on(600, 64)];
        // The pedal doesn't lengthen notes; 62 struck again ends the first; 64 is held at the stop.
        let notes = take_notes(&events, None, 1000, 10);
        assert_eq!(notes, vec![note(0, 100, 60), note(200, 350, 62), note(350, 400, 62), note(600, 1000, 64)]);
        // All Notes Off ends everything; very short notes get the minimum length.
        let events = [on(0, 60), on(5, 61), off(6, 61), RecMidi::Msg(50, MidiMsg::AllNotesOff)];
        assert_eq!(take_notes(&events, None, 1000, 10), vec![note(0, 50, 60), note(5, 15, 61)]);
        // Held over a cycle wrap: ends at the cycle end.
        assert_eq!(take_notes(&[on(1900, 60), off(1100, 60)], Some((1000, 2000)), 1500, 10), vec![note(1900, 2000, 60)]);
    }

    #[test]
    fn take_controllers_make_lanes_relative_to_the_region() {
        let cc = |t, kind, v| RecMidi::Msg(t, MidiMsg::Control(kind, v));
        let bend = Controller::PitchBend;
        let events = [cc(500, bend, 9000), cc(900, bend, 10000), on(1000, 60), cc(1000, bend, 12000), cc(1500, bend, 12000), cc(1500, Controller::SUSTAIN, 127), cc(1700, bend, 8192), cc(3000, bend, 0)];
        let lanes = take_controllers(&events, 1000, 2000, |t| (t - 1000) as f64 / 1000.0);
        let ev = |t: f64, v: f32| ControllerEvent { time_beats: t, value: v };
        assert_eq!(lanes.len(), 2);
        assert_eq!(lanes[0].kind, bend);
        // Before the region: the latest lands at its start, then is overwritten by the event
        // right there; the repeat at 1500 and anything after the end go.
        assert_eq!(lanes[0].events, vec![ev(0.0, bend.from_raw(12000)), ev(0.7, 0.0)]);
        assert_eq!(lanes[1].events, vec![ev(0.5, 1.0)]);
    }

    /// An engine with instrument tracks "Keys" and "Pads" and a test mixer reading one MIDI
    /// "port" the test writes to directly.
    fn rig() -> (Engine, Mixer, rtrb::Producer<RawMidi>) {
        let mut e = Engine::new();
        e.add_instrument_track("Keys", "synth").unwrap();
        e.add_instrument_track("Pads", "synth").unwrap();
        let mut mixer = e.attach_test_mixer(48_000);
        let (tx, rx) = rtrb::RingBuffer::new(64);
        e.send(Command::AddMidiInput(rx)).unwrap();
        run(&mut mixer, 1);
        (e, mixer, tx)
    }

    /// Callbacks of 480 frames (10 ms).
    fn run(m: &mut Mixer, callbacks: usize) {
        let mut out = vec![0.0f32; 2 * 480];
        for _ in 0..callbacks {
            m.process(&[], 0, &mut out, 2);
        }
    }

    /// Played now (no timing: it lands at the start of the next callback).
    fn play(tx: &mut rtrb::Producer<RawMidi>, bytes: &[u8]) {
        tx.push(RawMidi::new(0, bytes).unwrap()).unwrap();
    }

    fn arm(e: &mut Engine, track: usize, on: bool) {
        e.update_track(track, crate::TrackUpdate { armed: Some(on), ..Default::default() }).unwrap();
    }

    /// Which tracks made sound since the last call.
    fn sounding(e: &Engine) -> Vec<bool> {
        e.shared.track_peaks[..e.project().tracks.len()].iter().map(|p| crate::mixer::SharedState::take_peak(p) > 0.01).collect()
    }

    /// Stop while the "device" keeps running (stop waits for the audio thread).
    fn stop(e: &mut Engine, m: &mut Mixer) -> super::super::Recorded {
        let done = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|s| {
            s.spawn(|| {
                while !done.load(Ordering::Relaxed) {
                    run(m, 1);
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            });
            let r = e.stop().unwrap();
            done.store(true, Ordering::Relaxed);
            r
        })
    }

    #[test]
    fn input_plays_the_armed_instrument_tracks_else_the_selected_one() {
        let (mut e, mut mixer, mut tx) = rig();
        play(&mut tx, &[0x90, 60, 100]);
        run(&mut mixer, 10);
        assert_eq!(sounding(&e), [false, false], "nothing selected or armed");
        play(&mut tx, &[0x80, 60, 0]);

        e.select_track(Some(1)).unwrap();
        assert_eq!(e.midi_input_tracks(), [1]);
        play(&mut tx, &[0x90, 60, 100]);
        run(&mut mixer, 10);
        assert_eq!(sounding(&e), [false, true], "the selected track plays");
        play(&mut tx, &[0x80, 60, 0]);
        run(&mut mixer, 300); // the release dies away
        sounding(&e);

        arm(&mut e, 0, true);
        assert_eq!(e.midi_input_tracks(), [0], "armed tracks take over from the selection");
        play(&mut tx, &[0x90, 64, 100]);
        run(&mut mixer, 10);
        assert_eq!(sounding(&e), [true, false]);
        assert_eq!((e.status().midi_keys_down, e.status().midi_notes_received), (vec![64], 3));
    }

    #[test]
    fn the_sustain_pedal_holds_notes_until_it_comes_up() {
        let (mut e, mut mixer, mut tx) = rig();
        e.select_track(Some(0)).unwrap();
        let held = |e: &Engine, m: &mut Mixer| {
            run(m, 300); // any release dies away
            sounding(e);
            run(m, 5);
            sounding(e)[0]
        };
        play(&mut tx, &[0xB0, 64, 127]);
        play(&mut tx, &[0x90, 60, 100]);
        play(&mut tx, &[0x80, 60, 0]);
        assert!(held(&e, &mut mixer), "the pedal holds the released key");
        play(&mut tx, &[0xB0, 64, 0]);
        assert!(!held(&e, &mut mixer), "and lets go when it comes up");
    }

    #[test]
    fn live_notes_land_at_their_offset_in_the_callback() {
        let (mut e, mut mixer, mut tx) = rig();
        e.select_track(Some(0)).unwrap();
        run(&mut mixer, 1);
        // Callbacks 10 ms apart: a note played 5 ms into one period plays 240 frames into
        // the next callback.
        let base = crate::midi_input::now_ns();
        mixer.set_callback_time(base);
        run(&mut mixer, 1);
        tx.push(RawMidi::new(base + 5_000_000, &[0x90, 60, 100]).unwrap()).unwrap();
        mixer.set_callback_time(base + 10_000_000);
        let mut out = vec![0.0f32; 2 * 480];
        mixer.process(&[], 0, &mut out, 2);
        let first = out.chunks(2).position(|f| f[0].abs() > 1e-9).expect("the note sounds");
        assert!((240..=242).contains(&first), "{first}");
    }

    const MS: f64 = 0.0005;
    /// Half a millisecond in quarter notes at 120 bpm.
    const QMS: f64 = 2.0 * MS;

    #[test]
    fn recording_places_notes_where_they_were_heard() {
        let (mut e, mut mixer, mut tx) = rig();
        arm(&mut e, 0, true);
        arm(&mut e, 1, true);
        e.seek(1.0).unwrap();
        run(&mut mixer, 1);
        let started = e.start_recording().unwrap();
        assert_eq!((started.files.len(), started.midi_tracks.len()), (0, 2), "MIDI only, no audio input needed");
        run(&mut mixer, 100); // to 2.0 s
        // Without timing a note waited half a callback on average (240 frames, 5 ms); the
        // test device has no output latency.
        play(&mut tx, &[0x90, 60, 100]);
        run(&mut mixer, 50);
        play(&mut tx, &[0x80, 60, 0]);
        play(&mut tx, &[0x90, 67, 90]); // still held at the stop
        run(&mut mixer, 50); // to 3.0 s
        let r = stop(&mut e, &mut mixer);
        assert_eq!(r.midi_regions.len(), 2);
        let c = &e.project().tracks[0].midi_clips[0];
        // In quarter notes: two to the second at 120 bpm.
        assert_eq!((c.position_beats, c.notes.len()), (2.0, 2));
        let n = c.notes[0];
        assert!((n.start_beats - 1.99).abs() < QMS && (n.length_beats - 1.0).abs() < QMS, "{n:?}");
        let held = c.notes[1];
        // The stop lands wherever the "device" thread was when it arrived: a callback or two
        // after 3.0 s on a slow machine.
        assert!((held.start_beats - 2.99).abs() < QMS && (3.99 - QMS..3.99 + 0.06).contains(&c.length_beats), "{held:?} {c:?}");
        assert!((held.start_beats + held.length_beats - c.length_beats).abs() < 1e-9, "the held note ends at the stop");
        assert_eq!(e.project().tracks[1].midi_clips[0].notes, c.notes);

        // One undo step takes both regions away and the playhead back to the record point.
        assert_eq!(e.undo().unwrap().as_deref(), Some("Record"));
        assert!(e.project().tracks.iter().all(|t| t.midi_clips.is_empty()));
        run(&mut mixer, 1);
        assert_eq!(e.status().position_secs, 1.0);
    }

    #[test]
    fn recording_captures_pitch_bend_and_controllers_where_they_were_heard() {
        let (mut e, mut mixer, mut tx) = rig();
        arm(&mut e, 0, true);
        e.start_recording().unwrap();
        run(&mut mixer, 50); // 0.5 s
        play(&mut tx, &[0xB0, 64, 127]);
        play(&mut tx, &[0x90, 60, 100]);
        play(&mut tx, &[0xE0, 0, 0x60]); // bend up
        run(&mut mixer, 10);
        play(&mut tx, &[0xB0, 1, 90]); // mod wheel
        play(&mut tx, &[0xB0, 1, 90]); // repeated: dropped
        play(&mut tx, &[0x80, 60, 0]);
        run(&mut mixer, 10);
        play(&mut tx, &[0xE0, 0, 0x40]); // back to the centre
        play(&mut tx, &[0xB0, 64, 0]);
        run(&mut mixer, 10);
        let r = stop(&mut e, &mut mixer);
        assert_eq!((r.midi_regions[0].notes, r.midi_regions[0].controller_events), (1, 5));
        let c = &e.project().tracks[0].midi_clips[0];
        // In milliseconds (a beat is 500 ms).
        let lane = |k| c.lane(k).unwrap().events.iter().map(|e| ((e.time_beats * 500.0).round(), e.value)).collect::<Vec<_>>();
        // Placed like the notes: 0.5 s less half a callback.
        assert_eq!(lane(Controller::PitchBend), vec![(495.0, Controller::PitchBend.from_raw(0x60 << 7)), (695.0, 0.0)]);
        assert_eq!(lane(Controller::MOD_WHEEL), vec![(595.0, 90.0 / 127.0)]);
        assert_eq!(lane(Controller::SUSTAIN), vec![(495.0, 1.0), (695.0, 0.0)]);
        assert!((c.notes[0].length_beats - 0.2).abs() < QMS, "the pedal doesn't lengthen the note: {:?}", c.notes[0]);
    }

    #[test]
    fn output_latency_moves_recorded_notes_earlier() {
        let (mut e, mut mixer, mut tx) = rig();
        arm(&mut e, 0, true);
        mixer.report_output_latency(960.0); // 20 ms
        e.start_recording().unwrap();
        run(&mut mixer, 50); // to 0.5 s
        play(&mut tx, &[0x90, 60, 100]);
        run(&mut mixer, 10);
        play(&mut tx, &[0x80, 60, 0]);
        run(&mut mixer, 10);
        stop(&mut e, &mut mixer);
        let n = e.project().tracks[0].midi_clips[0].notes[0];
        // 0.5 s less half a callback (5 ms) and the output latency (20 ms).
        assert!((n.start_beats - 0.95).abs() < QMS, "{n:?}");
    }

    #[test]
    fn cycle_recording_merges_every_pass_into_one_region() {
        let (mut e, mut mixer, mut tx) = rig();
        arm(&mut e, 0, true);
        // Cycle 1.0..2.0 s, recorded from 0.5 s: the region spans 0.5..2.0 s.
        e.set_loop(Some(true), Some(1.0), Some(2.0)).unwrap();
        e.seek(0.5).unwrap();
        run(&mut mixer, 1);
        e.start_recording().unwrap();
        run(&mut mixer, 75); // 1.25 s
        play(&mut tx, &[0x90, 60, 100]);
        run(&mut mixer, 10);
        play(&mut tx, &[0x80, 60, 0]);
        run(&mut mixer, 80); // round the cycle: 1.15 s in the second pass
        play(&mut tx, &[0x90, 64, 100]);
        run(&mut mixer, 10);
        play(&mut tx, &[0x80, 64, 0]);
        run(&mut mixer, 50);
        let r = stop(&mut e, &mut mixer);
        assert_eq!(r.midi_regions.len(), 1);
        let c = &e.project().tracks[0].midi_clips[0];
        assert_eq!((c.position_beats, c.length_beats), (1.0, 3.0));
        let notes: Vec<(u8, f64)> = c.notes.iter().map(|n| (n.pitch, n.start_beats)).collect();
        assert_eq!(notes.len(), 2);
        assert!(notes[0].0 == 64 && (notes[0].1 - 1.29).abs() < QMS, "both passes, relative to the region start: {notes:?}");
        assert!(notes[1].0 == 60 && (notes[1].1 - 1.49).abs() < QMS, "{notes:?}");
    }

    #[test]
    fn audio_and_midi_record_together() {
        let dir = std::env::temp_dir().join(format!("opentake-midirec-{}", std::process::id()));
        let mut e = Engine::new();
        e.new_project("Song").unwrap();
        e.save_project(Some(&dir.join("song.otp"))).unwrap();
        e.add_instrument_track("Keys", "synth").unwrap();
        arm(&mut e, 0, true);
        arm(&mut e, 1, true);
        let mut mixer = e.attach_test_mixer_with_input(48_000, 1);
        let (mut tx, rx) = rtrb::RingBuffer::new(64);
        e.send(Command::AddMidiInput(rx)).unwrap();
        let started = e.start_recording().unwrap();
        assert_eq!((started.files.len(), started.midi_tracks.len()), (1, 1));
        let input = vec![0.25f32; 480];
        let mut out = vec![0.0f32; 2 * 480];
        for k in 0..60 {
            match k {
                20 => play(&mut tx, &[0x90, 60, 100]),
                40 => play(&mut tx, &[0x80, 60, 0]),
                _ => {}
            }
            mixer.process(&input, 1, &mut out, 2);
        }
        let r = stop(&mut e, &mut mixer);
        assert_eq!((r.clips.len(), r.midi_regions.len()), (1, 1));
        assert_eq!(e.history_info().undo.as_deref(), Some("Record"));
        e.undo().unwrap();
        let p = e.project();
        assert!(p.tracks[0].clips.is_empty() && p.tracks[1].midi_clips.is_empty(), "one undo step for both");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn recording_needs_an_armed_track() {
        let (mut e, _mixer, _tx) = rig();
        assert!(e.start_recording().unwrap_err().to_string().contains("armed"));
        // An armed audio track without an input device can't record.
        e.add_track("Voice").unwrap();
        arm(&mut e, 2, true);
        assert!(e.start_recording().unwrap_err().to_string().contains("input"));
    }

    #[test]
    fn a_take_that_wrapped_spans_the_cycle() {
        let mut e = Engine::new();
        e.add_instrument_track("Keys", "synth").unwrap();
        let id = e.project().tracks[0].id;
        let sr = 48_000;
        let p = RecordPlacement { start: sr, origin: sr, latency: 0, cycle: Some(CycleWrap { start: sr, end: 2 * sr, wrap: sr }) };
        let take = MidiTake { tracks: vec![id], events: vec![on(sr + 4800, 60), off(sr + 9600, 60), RecMidi::End(sr + 100)] };
        let added = e.add_midi_take(take, &p).unwrap();
        assert_eq!((added[0].position_secs, added[0].length_secs, added[0].notes), (1.0, 1.0, 1));
    }

    #[test]
    fn a_take_becomes_beats_through_the_tempo_map_and_can_snap_to_the_grid() {
        let mut e = Engine::new();
        e.add_instrument_track("Keys", "synth").unwrap();
        // 120 bpm, then 60 from 1.5 s (beat 3).
        e.set_tempo_changes(vec![opentake_project::TempoChange { at_secs: 1.5, bpm: 60.0, time_signature: None }], None).unwrap();
        let id = e.project().tracks[0].id;
        let sr = 48_000;
        let p = RecordPlacement { start: sr, origin: sr, latency: 0, cycle: None };
        // Played 1.3..2.0 s (beats 2.6..3.5), stopped at 2.5 s (beat 4).
        let take = || MidiTake { tracks: vec![id], events: vec![on(62_400, 60), off(96_000, 60), RecMidi::End(120_000)] };
        let added = e.add_midi_take(take(), &p).unwrap();
        assert_eq!((added[0].position_beats, added[0].length_beats), (2.0, 2.0));
        let n = e.project().tracks[0].midi_clips[0].notes[0];
        assert!((n.start_beats - 0.6).abs() < 1e-9 && (n.length_beats - 0.9).abs() < 1e-9, "{n:?}");
        // Back in seconds through the map: where it was played.
        let c = &e.project().tracks[0].midi_clips[0];
        assert!((c.secs_of(&e.project().tempo_map(), n.start_beats) - 1.3).abs() < 1e-9);

        // Quantize on record: the start snaps to the beat, the length stays.
        e.set_record_quantize(Some(opentake_project::Quantize::new(opentake_project::QuantizeGrid::Quarter)));
        e.add_midi_take(take(), &p).unwrap();
        let n = e.project().tracks[0].midi_clips[1].notes[0];
        assert!((n.start_beats - 1.0).abs() < 1e-9 && (n.length_beats - 0.9).abs() < 1e-9, "{n:?}");
    }
}
