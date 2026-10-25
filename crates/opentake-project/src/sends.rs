//! Sends (Logic's aux sends): besides its output, a track — or a bus — can feed buses
//! through sends, each at its own level, tapped before or after the fader. The usual use is
//! one reverb or delay bus shared by many tracks.
//!
//! Like `Track::output`, a send names its bus by track id in a session and by track index in
//! files. Routing stays acyclic counting outputs and sends together (see [`Project::feeds`]).

use serde::{Deserialize, Serialize};

use crate::model::Project;

/// Highest send level (linear, +6 dB like the fader).
pub const MAX_SEND_GAIN: f32 = 2.0;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AuxSend {
    /// The bus it feeds: its track id in a session, its index in files.
    pub bus: u64,
    /// Linear gain (1.0 = 0 dB); controls show it on the fader's scale.
    #[serde(default = "unity")]
    pub level: f32,
    /// Tapped before the fader and pan, so the send ignores them (mute still silences it).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pre_fader: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub mute: bool,
    /// Id within its track that automation lanes refer to, so they follow the send when
    /// others are removed (0 = not assigned yet; see [`Project::assign_send_ids`]).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub id: u32,
}

fn unity() -> f32 {
    1.0
}

fn is_zero(v: &u32) -> bool {
    *v == 0
}

impl AuxSend {
    /// A post-fader send at 0 dB to bus `bus` (id in a session).
    pub fn new(bus: u64) -> Self {
        Self { bus, level: 1.0, pre_fader: false, mute: false, id: 0 }
    }
}

impl Project {
    /// Index of the bus send `k` of track `index` feeds (`None` if it's gone or not a bus).
    pub fn send_dest(&self, index: usize, k: usize) -> Option<usize> {
        let id = self.tracks[index].sends.get(k)?.bus;
        self.track_by_id(id).filter(|&b| b != index && self.tracks[b].bus)
    }

    /// Every bus track `index` feeds directly: its output, then its sends.
    pub fn targets(&self, index: usize) -> Vec<usize> {
        let sends = (0..self.tracks[index].sends.len()).filter_map(|k| self.send_dest(index, k));
        self.output_of(index).into_iter().chain(sends).collect()
    }

    /// Whether `from` reaches `to` through outputs and sends (directly or via buses).
    pub fn feeds(&self, from: usize, to: usize) -> bool {
        let mut seen = vec![false; self.tracks.len()];
        let mut stack = self.targets(from);
        while let Some(b) = stack.pop() {
            if b == to {
                return true;
            }
            if !std::mem::replace(&mut seen[b], true) {
                stack.extend(self.targets(b));
            }
        }
        false
    }

    /// Whether track `index` may send to `bus`: a bus that isn't the track itself or mixed
    /// before it (fed by it, or keying it).
    pub fn can_send(&self, index: usize, bus: usize) -> bool {
        bus != index && self.tracks.get(bus).is_some_and(|t| t.bus) && !self.precedes(bus, index)
    }

    /// Tracks with a send to bus `index`.
    pub fn send_sources_of(&self, index: usize) -> Vec<usize> {
        (0..self.tracks.len()).filter(|&i| (0..self.tracks[i].sends.len()).any(|k| self.send_dest(i, k) == Some(index))).collect()
    }

    /// Give every send without an id a fresh one (unique within its track).
    pub fn assign_send_ids(&mut self) {
        for t in &mut self.tracks {
            let mut next = t.sends.iter().map(|s| s.id).max().unwrap_or(0) + 1;
            let mut seen = Vec::new();
            for s in &mut t.sends {
                if s.id == 0 || seen.contains(&s.id) {
                    s.id = next;
                    next += 1;
                }
                seen.push(s.id);
            }
        }
    }

    /// Sends from track indices (as files store them) to session ids. Sends to what isn't a
    /// bus, or that would close a loop, are dropped. Call after the outputs are converted.
    pub(crate) fn sends_from_indices(&mut self) {
        let ids: Vec<(u64, bool)> = self.tracks.iter().map(|t| (t.id, t.bus)).collect();
        let pending: Vec<_> = self.tracks.iter_mut().map(|t| std::mem::take(&mut t.sends)).collect();
        // One at a time, so of two sends making a loop the first stays.
        for (i, sends) in pending.into_iter().enumerate() {
            for mut s in sends {
                let Some(&(id, true)) = ids.get(s.bus as usize).filter(|_| s.bus as usize != i) else { continue };
                if !self.feeds(s.bus as usize, i) {
                    s.bus = id;
                    self.tracks[i].sends.push(s);
                }
            }
        }
    }

    /// Sends from session ids to track indices, for saving (sends to missing buses go).
    pub(crate) fn sends_to_indices(&mut self) {
        let dests: Vec<Vec<Option<usize>>> = (0..self.tracks.len()).map(|i| (0..self.tracks[i].sends.len()).map(|k| self.send_dest(i, k)).collect()).collect();
        for (t, dests) in self.tracks.iter_mut().zip(dests) {
            let mut it = dests.into_iter();
            t.sends.retain_mut(|s| match it.next().flatten() {
                Some(b) => {
                    s.bus = b as u64;
                    true
                }
                None => false,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Track;

    /// Tracks (bus or not) with outputs and sends as indices, converted to ids.
    fn project(buses: &[bool], outputs: &[Option<usize>], sends: &[(usize, usize)]) -> Project {
        let mut p = Project::default();
        for (i, (&bus, &out)) in buses.iter().zip(outputs).enumerate() {
            p.tracks.push(Track { id: 10 + i as u64, bus, output: out.map(|o| o as u64), ..Track::new(format!("T{i}")) });
        }
        for &(from, to) in sends {
            p.tracks[from].sends.push(AuxSend::new(to as u64));
        }
        p.outputs_from_indices();
        p
    }

    #[test]
    fn sends_order_buses_and_refuse_loops() {
        // 0 -> master, sends to bus 3 (reverb); bus 2 -> master, sends to 3; bus 1 (first in
        // the list) is fed by the reverb.
        let mut p = project(&[false, true, true, true], &[None, None, None, Some(1)], &[(0, 3), (2, 3)]);
        assert_eq!(p.tracks[0].sends[0].bus, 13, "indices become ids");
        assert_eq!(p.targets(2), vec![3]);
        assert!(p.feeds(0, 1) && !p.feeds(1, 0));
        assert_eq!(p.processing_order(), vec![0, 2, 3, 1], "senders before receivers");
        assert_eq!(p.send_sources_of(3), vec![0, 2]);
        assert!(!p.can_send(3, 2) && !p.can_route(1, Some(2)), "would loop through the sends");
        assert!(!p.can_send(0, 0) && !p.can_send(1, 0), "only to buses, never itself");
        assert!(p.can_send(0, 2) && p.can_send(2, 1));
        p.outputs_to_indices();
        assert_eq!((p.tracks[0].sends[0].bus, p.tracks[3].output), (3, Some(1)));

        // A loop or a non-bus target in a file: the send that would close it goes.
        let bad = project(&[true, true, false], &[Some(1), None, None], &[(1, 0), (2, 2), (2, 1)]);
        assert!(bad.tracks[1].sends.is_empty() && bad.tracks[2].sends.len() == 1);
    }

    #[test]
    fn solo_follows_sends_both_ways() {
        // 0 -> master + send to reverb bus 2; 1 -> master + send to 2.
        let mut p = project(&[false, false, true], &[None, None, None], &[(0, 2), (1, 2)]);
        p.tracks[0].solo = true;
        assert_eq!(p.audible(), vec![true, false, true], "the soloed track keeps its reverb");
        p.tracks[0].solo = false;
        p.tracks[2].solo = true;
        assert_eq!(p.audible(), vec![true, true, true], "a soloed reverb keeps what feeds it");
    }

    #[test]
    fn send_ids_and_compact_serialization() {
        let mut p = project(&[false, true], &[None, None], &[(0, 1), (0, 1)]);
        p.tracks[0].sends[1].id = 0;
        p.tracks[0].sends[0].id = 4;
        p.assign_send_ids();
        assert_eq!(p.tracks[0].sends.iter().map(|s| s.id).collect::<Vec<_>>(), vec![4, 5]);
        let json = serde_json::to_string(&AuxSend::new(1)).unwrap();
        assert_eq!(json, r#"{"bus":1,"level":1.0}"#);
        let old: Track = serde_json::from_str(r#"{"name":"T","volume":1,"pan":0,"mute":false,"solo":false,"armed":false,"input_channel":0,"monitor_input":false,"clips":[],"plugins":[]}"#).unwrap();
        assert!(old.sends.is_empty() && !serde_json::to_string(&old).unwrap().contains("sends"));
    }
}
