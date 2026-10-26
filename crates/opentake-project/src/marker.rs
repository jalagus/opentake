//! Markers: named points along the timeline (Logic's marker track) for finding your way
//! around a song. A marker's section runs to the next marker, which is what "set cycle to
//! marker" loops.

use serde::{Deserialize, Serialize};

use crate::model::Project;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Marker {
    /// Persistent id, unique within the project (0 = not assigned yet; see
    /// [`Project::tidy_markers`]). Edits address markers by it, since moving one can
    /// reorder them.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub id: u32,
    pub time_secs: f64,
    pub name: String,
    /// RGB; `None` is the default marker colour.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<[u8; 3]>,
}

fn is_zero(v: &u32) -> bool {
    *v == 0
}

impl Marker {
    pub fn new(time_secs: f64, name: impl Into<String>) -> Self {
        Self { id: 0, time_secs, name: name.into(), color: None }
    }
}

/// Marker colours offered by the GUI and accepted by name over MCP.
pub const MARKER_COLORS: [(&str, [u8; 3]); 8] = [
    ("red", [226, 84, 84]),
    ("orange", [232, 146, 60]),
    ("yellow", [226, 196, 72]),
    ("green", [96, 190, 100]),
    ("teal", [64, 186, 176]),
    ("blue", [86, 140, 230]),
    ("purple", [160, 110, 220]),
    ("pink", [222, 104, 170]),
];

/// Markers closer together than this count as the same place (imports skip duplicates,
/// next/previous skip the one under the playhead).
pub const MARKER_EPSILON: f64 = 1e-3;

impl Project {
    /// Give any marker without an id (new, or from a file or importer) a fresh one (a
    /// duplicated id is replaced on its later copies), and sort them by time.
    pub fn tidy_markers(&mut self) {
        let mut next = self.markers.iter().map(|m| m.id).max().unwrap_or(0) + 1;
        let mut seen = std::collections::HashSet::new();
        for m in &mut self.markers {
            m.time_secs = m.time_secs.max(0.0);
            if m.id == 0 || !seen.insert(m.id) {
                m.id = next;
                seen.insert(next);
                next += 1;
            }
        }
        // After the ids, so a duplicate is the copy added later, not the later one in time.
        self.markers.sort_by(|a, b| a.time_secs.total_cmp(&b.time_secs));
    }

    pub fn marker_index(&self, id: u32) -> Option<usize> {
        self.markers.iter().position(|m| m.id == id)
    }

    /// Add markers from an import, skipping any already there (same name and place), so
    /// importing several parts of one song doesn't repeat them.
    pub fn merge_markers(&mut self, markers: impl IntoIterator<Item = Marker>) {
        for m in markers {
            if !self.markers.iter().any(|o| o.name == m.name && (o.time_secs - m.time_secs).abs() < MARKER_EPSILON) {
                self.markers.push(m);
            }
        }
        self.tidy_markers();
    }

    /// The first marker after `t`.
    pub fn next_marker(&self, t: f64) -> Option<&Marker> {
        self.markers.iter().find(|m| m.time_secs > t + MARKER_EPSILON)
    }

    /// The last marker before `t`.
    pub fn previous_marker(&self, t: f64) -> Option<&Marker> {
        self.markers.iter().rev().find(|m| m.time_secs < t - MARKER_EPSILON)
    }

    /// The marker whose section `t` is in (the last one at or before it).
    pub fn marker_at(&self, t: f64) -> Option<&Marker> {
        self.markers.iter().rev().find(|m| m.time_secs <= t + MARKER_EPSILON)
    }

    /// Marker `index`'s section: from it to the next marker, or for the last one to the
    /// end of the song (at least a bar).
    pub fn marker_range(&self, index: usize) -> Option<(f64, f64)> {
        let start = self.markers.get(index)?.time_secs;
        let end = match self.markers[index + 1..].iter().find(|m| m.time_secs > start + MARKER_EPSILON) {
            Some(next) => next.time_secs,
            None => self.length_secs().max(start + self.tempo_map().bar_secs_at(start)),
        };
        Some((start, end))
    }

    /// "Marker N" for a new marker, N one past the highest such name.
    pub fn next_marker_name(&self) -> String {
        let n = self.markers.iter().filter_map(|m| m.name.strip_prefix("Marker ")?.trim().parse::<u32>().ok()).max().unwrap_or(0);
        format!("Marker {}", n + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(times: &[f64]) -> Project {
        let mut p = Project { markers: times.iter().map(|&t| Marker::new(t, format!("at {t}"))).collect(), ..Project::default() };
        p.tidy_markers();
        p
    }

    #[test]
    fn tidy_sorts_and_assigns_unique_ids() {
        let mut p = project(&[8.0, 2.0, 4.0]);
        assert_eq!(p.markers.iter().map(|m| m.time_secs).collect::<Vec<_>>(), vec![2.0, 4.0, 8.0]);
        let ids: Vec<u32> = p.markers.iter().map(|m| m.id).collect();
        assert_eq!(ids, vec![2, 3, 1], "ids follow insertion, not time");
        p.markers.push(Marker { id: 2, ..Marker::new(1.0, "dup") });
        p.tidy_markers();
        assert_eq!(p.markers[0].name, "dup");
        assert_eq!(p.markers[0].id, 4, "a duplicated id is replaced");
        assert_eq!(p.marker_index(3), Some(2));
    }

    #[test]
    fn navigation_and_ranges() {
        let p = project(&[2.0, 4.0, 8.0]);
        assert_eq!(p.next_marker(0.0).map(|m| m.time_secs), Some(2.0));
        assert_eq!(p.next_marker(2.0).map(|m| m.time_secs), Some(4.0), "skips the one at the playhead");
        assert_eq!(p.previous_marker(4.0).map(|m| m.time_secs), Some(2.0));
        assert!(p.previous_marker(2.0).is_none() && p.next_marker(8.0).is_none());
        assert_eq!(p.marker_at(5.0).map(|m| m.time_secs), Some(4.0));
        assert_eq!(p.marker_range(0), Some((2.0, 4.0)));
        // The last marker runs to the end of the song, here at least one bar (2 s at 120).
        assert_eq!(p.marker_range(2), Some((8.0, 10.0)));
        assert_eq!(p.marker_range(3), None);
        assert_eq!(p.next_marker_name(), "Marker 1");
    }

    #[test]
    fn merging_skips_duplicates_and_serde_defaults() {
        let mut p = project(&[2.0]);
        p.merge_markers([Marker::new(2.0, "at 2"), Marker::new(2.0, "Other"), Marker::new(1.0, "Intro")]);
        assert_eq!(p.markers.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(), vec!["Intro", "at 2", "Other"]);
        p.markers[0].color = Some([1, 2, 3]);
        let json = serde_json::to_string(&p).unwrap();
        let back: Project = serde_json::from_str(&json).unwrap();
        assert_eq!(back.markers, p.markers);
        assert!(!serde_json::to_string(&Project::default()).unwrap().contains("markers"), "no markers, no field");
        let m: Marker = serde_json::from_str(r#"{"time_secs":1,"name":"x"}"#).unwrap();
        assert_eq!((m.id, m.color), (0, None));
    }
}
