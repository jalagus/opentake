//! Marker edits (undoable) and navigation: create, rename, move, recolour and delete
//! markers, jump between them, and cycle a marker's section.

use anyhow::{Result, anyhow, bail};
use opentake_project::Marker;

use super::Engine;

/// A change to a marker; `None` fields stay. `color: Some(None)` resets the colour.
#[derive(Debug, Clone, Default)]
pub struct MarkerUpdate {
    pub time_secs: Option<f64>,
    pub name: Option<String>,
    pub color: Option<Option<[u8; 3]>>,
}

impl Engine {
    /// Add a marker at `time_secs` ("Marker N" without a name). Returns it (with its id).
    pub fn add_marker(&mut self, time_secs: f64, name: Option<String>, color: Option<[u8; 3]>) -> Result<Marker> {
        if !time_secs.is_finite() || time_secs < 0.0 {
            bail!("marker time must be 0 or later (got {time_secs} s)");
        }
        self.record("Create Marker", None, |e| {
            let name = name.filter(|n| !n.trim().is_empty()).unwrap_or_else(|| e.project.next_marker_name());
            let id = e.project.markers.iter().map(|m| m.id).max().unwrap_or(0) + 1;
            let marker = Marker { id, color, ..Marker::new(time_secs, name.trim()) };
            e.project.markers.push(marker.clone());
            e.project.tidy_markers();
            Ok(marker)
        })
    }

    fn marker_index_or_err(&self, id: u32) -> Result<usize> {
        self.project.marker_index(id).ok_or_else(|| anyhow!("no marker with id {id}"))
    }

    /// Rename, move or recolour marker `id`. `merge` groups repeated edits (a drag) into one
    /// undo step.
    pub fn update_marker(&mut self, id: u32, u: MarkerUpdate, merge: Option<String>) -> Result<Marker> {
        let i = self.marker_index_or_err(id)?;
        if u.time_secs.is_some_and(|t| !t.is_finite() || t < 0.0) {
            bail!("marker time must be 0 or later");
        }
        let label = match (&u.time_secs, &u.name, &u.color) {
            (Some(_), None, None) => "Move Marker",
            (None, Some(_), None) => "Rename Marker",
            (None, None, Some(_)) => "Marker Color",
            _ => "Edit Marker",
        };
        self.record(label, merge, |e| {
            let m = &mut e.project.markers[i];
            if let Some(t) = u.time_secs {
                m.time_secs = t;
            }
            if let Some(n) = u.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
                m.name = n.to_string();
            }
            if let Some(c) = u.color {
                m.color = c;
            }
            e.project.tidy_markers();
            Ok(e.project.markers[e.project.marker_index(id).unwrap_or(0)].clone())
        })
    }

    pub fn remove_marker(&mut self, id: u32) -> Result<Marker> {
        let i = self.marker_index_or_err(id)?;
        self.record("Delete Marker", None, |e| Ok(e.project.markers.remove(i)))
    }

    /// Cycle marker `id`'s section (to the next marker, or the song end for the last one)
    /// and turn the cycle on. Returns the range.
    pub fn cycle_marker(&mut self, id: u32) -> Result<(f64, f64)> {
        let i = self.marker_index_or_err(id)?;
        let (start, end) = self.project.marker_range(i).ok_or_else(|| anyhow!("no marker with id {id}"))?;
        self.set_loop(Some(true), Some(start), Some(end))?;
        Ok((start, end))
    }
}

#[cfg(test)]
mod tests {
    use super::super::Engine;
    use super::MarkerUpdate;

    #[test]
    fn marker_edits_are_undoable_and_drags_merge() {
        let mut e = Engine::new();
        let a = e.add_marker(4.0, None, None).unwrap();
        let b = e.add_marker(2.0, Some("Intro".into()), Some([1, 2, 3])).unwrap();
        assert_eq!((a.name.as_str(), b.name.as_str()), ("Marker 1", "Intro"));
        assert_eq!(e.add_marker(8.0, Some("  ".into()), None).unwrap().name, "Marker 2", "a blank name gets the default");
        assert!(e.add_marker(-1.0, None, None).is_err());
        let names = |e: &Engine| e.project().markers.iter().map(|m| m.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(&e), vec!["Intro", "Marker 1", "Marker 2"], "sorted by time");

        // A drag past another marker: one undo step, and the id still finds it.
        for t in [5.0, 7.0, 9.0] {
            e.update_marker(a.id, MarkerUpdate { time_secs: Some(t), ..Default::default() }, Some(format!("marker-move:{}", a.id))).unwrap();
        }
        assert_eq!(names(&e), vec!["Intro", "Marker 2", "Marker 1"]);
        assert_eq!(e.history_info().undo.as_deref(), Some("Move Marker"));
        e.update_marker(a.id, MarkerUpdate { name: Some("Chorus".into()), color: Some(None), ..Default::default() }, None).unwrap();
        e.update_marker(b.id, MarkerUpdate { color: Some(None), ..Default::default() }, None).unwrap();
        assert!(e.project().markers.iter().all(|m| m.color.is_none()));
        e.remove_marker(b.id).unwrap();
        assert!(e.remove_marker(b.id).is_err());
        assert_eq!(names(&e), vec!["Marker 2", "Chorus"]);

        assert_eq!(e.undo().unwrap().as_deref(), Some("Delete Marker"));
        assert_eq!(e.undo().unwrap().as_deref(), Some("Marker Color"));
        assert_eq!(e.undo().unwrap().as_deref(), Some("Edit Marker"));
        assert_eq!(e.undo().unwrap().as_deref(), Some("Move Marker"));
        assert_eq!(names(&e), vec!["Intro", "Marker 1", "Marker 2"]);
        assert_eq!(e.project().markers[0].color, Some([1, 2, 3]));
        e.redo().unwrap();
        assert_eq!(e.project().markers[e.project().marker_index(a.id).unwrap()].time_secs, 9.0);
    }

    #[test]
    fn cycle_marker_loops_to_the_next_marker() {
        let mut e = Engine::new();
        let intro = e.add_marker(0.0, Some("Intro".into()), None).unwrap();
        let verse = e.add_marker(4.0, Some("Verse".into()), None).unwrap();
        assert_eq!(e.cycle_marker(intro.id).unwrap(), (0.0, 4.0));
        let p = e.project();
        assert!(p.loop_enabled && p.loop_start_secs == 0.0 && p.loop_end_secs == 4.0);
        // The last marker cycles to the song end (here one bar, nothing being in the song).
        assert_eq!(e.cycle_marker(verse.id).unwrap(), (4.0, 6.0));
        assert!(e.cycle_marker(99).is_err());
        assert_eq!(e.history_info().undo_steps.len(), 2, "the cycle is a setting, not an edit");
    }
}
