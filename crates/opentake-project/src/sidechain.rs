//! Side-chain inputs (the side-chain menu in Logic's plug-in headers): a dynamics plug-in's
//! detector listens to another track or bus instead of its own input, e.g. a kick ducking a
//! pad or bass so it pumps. The audio the plug-in processes is still its own track's.
//!
//! Like `Track::output`, a key names its track by id in a session and by index in files. A
//! key is an edge of the processing order too (the key track is mixed first), so routing
//! stays acyclic counting outputs, sends and keys together (see [`Project::precedes`]). Only
//! effect slots (track inserts and the master's) take a key.

use serde::{Deserialize, Serialize};

use crate::model::Project;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct SideChain {
    /// The key track: its id in a session, its index in files.
    pub track: u64,
    /// Tapped after the source's inserts but before its fader, pan and mute, so a muted
    /// (ghost) kick still keys. Post-fader (the default) follows all three; solo never matters.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pre_fader: bool,
}

impl SideChain {
    /// A post-fader key from track `track` (id in a session).
    pub fn new(track: u64) -> Self {
        Self { track, pre_fader: false }
    }
}

impl Project {
    /// The track keying effect slot `slot` of track `index` (`None` = the master's): `None` if
    /// it isn't keyed, or by a track that's gone (or itself).
    pub fn key_source(&self, index: Option<usize>, slot: usize) -> Option<usize> {
        let slots = match index {
            Some(i) => &self.tracks[i].plugins,
            None => &self.master_plugins,
        };
        let id = slots.get(slot)?.side_chain?.track;
        self.track_by_id(id).filter(|&s| Some(s) != index)
    }

    /// Tracks with an effect slot keyed by track `index`.
    pub fn keyed_by(&self, index: usize) -> Vec<usize> {
        (0..self.tracks.len()).filter(|&i| (0..self.tracks[i].plugins.len()).any(|k| self.key_source(Some(i), k) == Some(index))).collect()
    }

    /// What has to be mixed after track `index`: the buses it feeds and the tracks it keys.
    pub fn downstream(&self, index: usize) -> Vec<usize> {
        let mut next = self.targets(index);
        for k in self.keyed_by(index) {
            if !next.contains(&k) {
                next.push(k);
            }
        }
        next
    }

    /// Whether `from` has to be mixed before `to` (through outputs, sends and keys).
    pub fn precedes(&self, from: usize, to: usize) -> bool {
        let mut seen = vec![false; self.tracks.len()];
        let mut stack = self.downstream(from);
        while let Some(b) = stack.pop() {
            if b == to {
                return true;
            }
            if !std::mem::replace(&mut seen[b], true) {
                stack.extend(self.downstream(b));
            }
        }
        false
    }

    /// Whether an effect slot of track `index` (`None` = the master) may be keyed by track
    /// `source`: not itself, and not something mixed after it (the master comes after all).
    pub fn can_key(&self, index: Option<usize>, source: usize) -> bool {
        source < self.tracks.len() && index.is_none_or(|i| i != source && !self.precedes(i, source))
    }

    /// Clear every key from track `index` (before deleting it; undo brings them back).
    /// Returns the tracks whose slots changed and whether the master's did.
    pub fn drop_keys_from(&mut self, index: usize) -> (Vec<usize>, bool) {
        let id = self.tracks[index].id;
        let clear = |slots: &mut Vec<crate::PluginSlot>| {
            let keyed = slots.iter().any(|s| s.side_chain.is_some_and(|k| k.track == id));
            slots.iter_mut().filter(|s| s.side_chain.is_some_and(|k| k.track == id)).for_each(|s| s.side_chain = None);
            keyed
        };
        let tracks = (0..self.tracks.len()).filter(|&i| clear(&mut self.tracks[i].plugins)).collect();
        (tracks, clear(&mut self.master_plugins))
    }

    /// Take every key out (still as file indices), so routing checks don't misread them while
    /// outputs and sends are converted; [`Project::keys_from_indices`] puts them back.
    pub(crate) fn take_keys(&mut self) -> Vec<(Option<usize>, usize, SideChain)> {
        let mut keys = Vec::new();
        for (i, t) in self.tracks.iter_mut().enumerate() {
            if let Some(inst) = &mut t.instrument {
                inst.side_chain = None; // instruments take no key
            }
            keys.extend(t.plugins.iter_mut().enumerate().filter_map(|(k, s)| s.side_chain.take().map(|c| (Some(i), k, c))));
        }
        keys.extend(self.master_plugins.iter_mut().enumerate().filter_map(|(k, s)| s.side_chain.take().map(|c| (None, k, c))));
        keys
    }

    /// Keys from track indices (as files store them) to session ids. Keys from a missing
    /// track, or that would close a loop, are dropped (of two making a loop the first stays).
    pub(crate) fn keys_from_indices(&mut self, keys: Vec<(Option<usize>, usize, SideChain)>) {
        for (owner, k, mut key) in keys {
            let source = key.track as usize;
            if !self.can_key(owner, source) {
                continue;
            }
            key.track = self.tracks[source].id;
            let slots = match owner {
                Some(i) => &mut self.tracks[i].plugins,
                None => &mut self.master_plugins,
            };
            slots[k].side_chain = Some(key);
        }
    }

    /// Keys from session ids to track indices, for saving (keys from missing tracks go).
    pub(crate) fn keys_to_indices(&mut self) {
        let index = |p: &Project, id: u64| p.track_by_id(id).map(|i| i as u64);
        for i in 0..self.tracks.len() {
            for k in 0..self.tracks[i].plugins.len() {
                let key = self.tracks[i].plugins[k].side_chain;
                self.tracks[i].plugins[k].side_chain = key.and_then(|c| Some(SideChain { track: index(self, c.track).filter(|&s| s != i as u64)?, ..c }));
            }
        }
        for k in 0..self.master_plugins.len() {
            let key = self.master_plugins[k].side_chain;
            self.master_plugins[k].side_chain = key.and_then(|c| Some(SideChain { track: index(self, c.track)?, ..c }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{PluginFormat, PluginSlot, Track};
    use crate::sends::AuxSend;

    fn comp(key: Option<(usize, bool)>) -> PluginSlot {
        let side_chain = key.map(|(t, pre_fader)| SideChain { track: t as u64, pre_fader });
        PluginSlot { name: "comp".into(), format: PluginFormat::Builtin, path: "comp".into(), uid: "comp".into(), bypass: false, state: Vec::new(), params: Vec::new(), id: 0, side_chain, instance: 0 }
    }

    /// Tracks (bus or not) with outputs, sends and keyed slots as file indices, converted.
    fn project(buses: &[bool], outputs: &[Option<usize>], sends: &[(usize, usize)], keys: &[(usize, usize)]) -> Project {
        let mut p = Project::default();
        for (i, (&bus, &out)) in buses.iter().zip(outputs).enumerate() {
            p.tracks.push(Track { id: 10 + i as u64, bus, output: out.map(|o| o as u64), ..Track::new(format!("T{i}")) });
        }
        for &(from, to) in sends {
            p.tracks[from].sends.push(AuxSend::new(to as u64));
        }
        for &(keyed, source) in keys {
            p.tracks[keyed].plugins.push(comp(Some((source, false))));
        }
        p.outputs_from_indices();
        p
    }

    #[test]
    fn keys_order_tracks_and_refuse_loops() {
        // Pad (0) keyed by the kick (1), both to the master; bus 2 keyed by the pad.
        let mut p = project(&[false, false, true], &[None, None, None], &[], &[(0, 1), (2, 0)]);
        assert_eq!(p.tracks[0].plugins[0].side_chain, Some(SideChain::new(11)), "indices become ids");
        assert_eq!((p.key_source(Some(0), 0), p.keyed_by(1)), (Some(1), vec![0]));
        assert_eq!(p.processing_order(), vec![1, 0, 2], "the key before the keyed track");
        assert!(p.precedes(1, 2) && !p.precedes(2, 1));
        assert!(!p.can_key(Some(1), 0) && !p.can_key(Some(1), 2), "kick keyed by what it keys");
        assert!(!p.can_key(Some(0), 0), "never itself");
        assert!(p.can_key(None, 0) && p.can_key(Some(2), 1));
        // A key edge counts for outputs and sends too.
        let mut q = project(&[false, true], &[None, None], &[], &[(0, 1)]);
        assert!(!q.can_route(0, Some(1)) && !q.can_send(0, 1), "the bus keys the track: it can't feed it");
        assert_eq!(q.processing_order(), vec![1, 0], "a plain track keyed by a bus waits for it");
        // Back to indices; a removed key track clears its keys.
        q.outputs_to_indices();
        assert_eq!(q.tracks[0].plugins[0].side_chain, Some(SideChain::new(1)));
        p.master_plugins.push(PluginSlot { side_chain: Some(SideChain { track: 11, pre_fader: true }), ..comp(None) });
        assert_eq!(p.drop_keys_from(1), (vec![0], true));
        assert!(p.tracks[0].plugins[0].side_chain.is_none() && p.master_plugins[0].side_chain.is_none());
    }

    #[test]
    fn loops_and_bad_keys_in_files_are_dropped() {
        // 0 keyed by 1 and 1 keyed by 0: the second closes a loop. Bus 2 fed by 3 and keyed by 3
        // is fine; 3 keyed by bus 2 (which it feeds) isn't; 4 keyed by a missing track.
        let p = project(&[false, false, true, false, false], &[None, None, None, Some(2), None], &[], &[(0, 1), (1, 0), (2, 3), (3, 2), (4, 9)]);
        let keyed: Vec<bool> = p.tracks.iter().map(|t| t.plugins.iter().any(|s| s.side_chain.is_some())).collect();
        assert_eq!(keyed, vec![true, false, true, false, false]);
        assert_eq!(p.processing_order(), vec![1, 3, 4, 0, 2]);
    }

    #[test]
    fn compact_serialization_with_defaults() {
        let slot = PluginSlot { side_chain: Some(SideChain::new(3)), ..comp(None) };
        let json = serde_json::to_string(&slot).unwrap();
        assert!(json.contains(r#""side_chain":{"track":3}"#), "{json}");
        assert!(!serde_json::to_string(&comp(None)).unwrap().contains("side_chain"));
        let pre: SideChain = serde_json::from_str(r#"{"track":1,"pre_fader":true}"#).unwrap();
        assert!(pre.pre_fader);
    }
}
