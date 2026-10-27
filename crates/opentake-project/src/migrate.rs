//! Bringing projects saved by older versions up to date (see [`PROJECT_VERSION`]).

use crate::model::{MidiClip, PROJECT_VERSION, Project};
use crate::tempo::{TempoChange, TempoMap};

impl Project {
    /// Convert a project read from an older file. Version 1 kept MIDI in seconds: every MIDI
    /// region goes to quarter notes through the tempo map it was saved with, so it plays
    /// exactly where it did. A region carrying a song's tempo marks (an imported score) is
    /// converted through the song's own tempo instead, so its notes stay on the song's beats
    /// and bars (its tab view lines up, and "Use Song Tempo" plays it as written); where the
    /// project followed the song's tempo, as it does by default, that's the same thing.
    /// Returns whether anything changed.
    pub fn upgrade(&mut self) -> bool {
        if self.version >= PROJECT_VERSION {
            return false;
        }
        let map = self.tempo_map();
        for c in self.tracks.iter_mut().flat_map(|t| &mut t.midi_clips) {
            // Fields read from a version 1 file hold seconds.
            let (pos_secs, pos_q) = (c.position_beats, map.quarters_at(c.position_beats.max(0.0)));
            match c.tempos.first() {
                Some(first) => {
                    // The marks are still in seconds here.
                    let changes: Vec<TempoChange> = c.tempos.iter().skip(1).map(|m| TempoChange { at_secs: m.start_beats, bpm: m.bpm, time_signature: None }).collect();
                    let song = TempoMap::new(first.bpm, (4, 4), &changes);
                    to_beats(c, |s| song.quarters_at(s));
                }
                None => to_beats(c, |s| map.quarters_at(pos_secs + s) - pos_q),
            }
            c.position_beats = pos_q;
        }
        let changed = self.version < PROJECT_VERSION && self.tracks.iter().any(|t| !t.midi_clips.is_empty());
        self.version = PROJECT_VERSION;
        changed
    }
}

/// Convert a region's seconds (relative to its start) with `f`.
fn to_beats(c: &mut MidiClip, f: impl Fn(f64) -> f64) {
    for n in &mut c.notes {
        let start = f(n.start_beats);
        n.length_beats = f(n.start_beats + n.length_beats) - start;
        n.start_beats = start;
    }
    for e in c.controllers.iter_mut().flat_map(|l| &mut l.events) {
        e.time_beats = f(e.time_beats);
    }
    for m in &mut c.tempos {
        m.start_beats = f(m.start_beats);
    }
    for b in c.tab.iter_mut().flat_map(|t| &mut t.bars) {
        b.start_beats = f(b.start_beats);
    }
    if let Some(l) = &mut c.looped {
        (l.len, l.phase) = (f(l.len), f(l.phase));
    }
    c.length_beats = f(c.length_beats);
}

#[cfg(test)]
mod tests {
    use crate::controller::Controller;
    use crate::model::{PROJECT_VERSION, Project};

    /// A version 1 file: 120 bpm, slowing to 60 at 4 s (bar 3). A region at 3 s (beat 6)
    /// running 3 s (to beat 10) with notes, a controller and a loop in seconds.
    const OLD: &str = r#"{
        "name": "Old", "tempo_bpm": 120.0, "time_signature": [4, 4], "sample_rate": 48000,
        "tempo_changes": [{ "at_secs": 4.0, "bpm": 60.0 }],
        "tracks": [{
            "name": "Keys", "volume": 1.0, "pan": 0.0, "mute": false, "solo": false, "armed": false,
            "input_channel": 0, "monitor_input": false, "clips": [], "plugins": [],
            "midi_clips": [{
                "name": "Riff", "position_secs": 3.0, "length_secs": 3.0,
                "notes": [
                    { "start_secs": 0.0, "length_secs": 0.5, "pitch": 60, "velocity": 100 },
                    { "start_secs": 0.75, "length_secs": 0.5, "pitch": 62, "velocity": 90 },
                    { "start_secs": 2.0, "length_secs": 1.0, "pitch": 64, "velocity": 80 }
                ],
                "controllers": [{ "kind": { "cc": 1 }, "events": [{ "time_secs": 1.5, "value": 1.0 }] }],
                "looped": { "len_secs": 3.0 }
            }]
        }]
    }"#;

    #[test]
    fn version_1_midi_converts_to_beats_and_plays_where_it_did() {
        let mut p: Project = serde_json::from_str(OLD).unwrap();
        assert_eq!(p.version, 1, "no version field: the old format");
        assert!(p.upgrade());
        assert_eq!(p.version, PROJECT_VERSION);
        let c = &p.tracks[0].midi_clips[0];
        // 3 s at 120 = beat 6; 4 s = beat 8; then a beat a second.
        assert_eq!((c.position_beats, c.length_beats), (6.0, 4.0));
        let notes: Vec<_> = c.notes.iter().map(|n| (n.start_beats, n.length_beats)).collect();
        assert_eq!(notes, [(0.0, 1.0), (1.5, 0.75), (3.0, 1.0)], "the second note crosses the tempo change");
        assert_eq!(c.lane(Controller::MOD_WHEEL).unwrap().events[0].time_beats, 2.5);
        assert_eq!(c.looped.unwrap().len, 4.0);
        // Back to seconds through the same map: exactly the old times.
        let map = p.tempo_map();
        let old = [(0.0, 0.5), (0.75, 0.5), (2.0, 1.0)];
        for (n, (s, l)) in c.notes.iter().zip(old) {
            let start = c.secs_of(&map, n.start_beats) - 3.0;
            let len = c.secs_of(&map, n.start_beats + n.length_beats) - c.secs_of(&map, n.start_beats);
            assert!((start - s).abs() < 1e-12 && (len - l).abs() < 1e-12, "{n:?}");
        }
        assert_eq!(c.end_secs(&map), 6.0);
        // Converted once: upgrading again changes nothing, and saving writes the new version.
        let before = p.clone();
        assert!(!p.upgrade());
        assert_eq!(p, before);
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains(r#""version":2"#) && json.contains(r#""position_beats":6.0"#) && !json.contains("_secs\":3.0"), "{json}");
        let again: Project = serde_json::from_str(&json).unwrap();
        assert_eq!(again, p);
    }

    #[test]
    fn song_regions_convert_through_the_songs_own_tempo() {
        // An imported song (marks: 120, then 60 from 2 s) at 0 s in a project left at 100 bpm.
        let old = r#"{
            "name": "Song", "tempo_bpm": 100.0, "time_signature": [4, 4], "sample_rate": 48000,
            "tracks": [{
                "name": "Gtr", "volume": 1.0, "pan": 0.0, "mute": false, "solo": false, "armed": false,
                "input_channel": 0, "monitor_input": false, "clips": [], "plugins": [],
                "midi_clips": [{
                    "name": "Song", "position_secs": 0.0, "length_secs": 4.0,
                    "notes": [{ "start_secs": 3.0, "length_secs": 1.0, "pitch": 45, "velocity": 100 }],
                    "tab": { "tuning": [40], "bars": [{ "start_secs": 0.0, "number": 1, "numerator": 4, "denominator": 4 }, { "start_secs": 2.0, "number": 2, "numerator": 4, "denominator": 4 }] },
                    "tempos": [{ "start_secs": 0.0, "bpm": 120.0 }, { "start_secs": 2.0, "bpm": 60.0 }]
                }]
            }]
        }"#;
        let mut p: Project = serde_json::from_str(old).unwrap();
        p.upgrade();
        let c = &p.tracks[0].midi_clips[0];
        // Song beats: bar 2 at beat 4, the note a second into the 60 bpm part = beat 5.
        assert_eq!(c.tab.as_ref().unwrap().bars.iter().map(|b| b.start_beats).collect::<Vec<_>>(), [0.0, 4.0]);
        assert_eq!(c.tempos.iter().map(|m| (m.start_beats, m.bpm)).collect::<Vec<_>>(), [(0.0, 120.0), (4.0, 60.0)]);
        assert_eq!((c.notes[0].start_beats, c.notes[0].length_beats, c.length_beats), (5.0, 1.0, 6.0));
    }
}
