//! Saving, in two parts: [`Engine::prepare_save`] collects what goes in the file (each
//! plug-in's current state, which must be asked for on the main thread), and
//! [`SaveJob::write`] encodes and writes it, which can run on any thread so a large project
//! or a slow disk doesn't hold up the GUI.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use opentake_project::Project;

use super::{Engine, PROJECT_EXTENSION, is_ext, is_rpp};

/// A plug-in that took at least this long to report its state is named in the log.
const SLOW_STATE: Duration = Duration::from_millis(5);

/// A project ready to be written to `path` (see [`Engine::prepare_save`]).
pub struct SaveJob {
    project: Project,
    path: PathBuf,
    opened: u64,
}

/// A written [`SaveJob`], for [`Engine::saved_as`].
#[derive(Debug, Clone)]
pub struct Saved {
    pub path: PathBuf,
    opened: u64,
}

impl SaveJob {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Encode and write the file. It's written next to its destination and renamed into
    /// place, so an interrupted save leaves the previous file intact.
    pub fn write(self) -> Result<Saved> {
        let started = Instant::now();
        let name = self.path.file_name().ok_or_else(|| anyhow!("no file name in {}", self.path.display()))?.to_string_lossy();
        let tmp = self.path.with_file_name(format!(".{name}.saving"));
        let written = if is_rpp(&self.path) {
            opentake_project::rpp::save(&self.project, &tmp).map_err(anyhow::Error::from)
        } else if is_ext(&self.path, "dawproject") {
            // Writes through its own temporary file.
            opentake_project::dawproject::save(&self.project, &self.path).map(drop).map_err(anyhow::Error::from)
        } else {
            serde_json::to_string_pretty(&self.project).map_err(anyhow::Error::from).and_then(|json| Ok(std::fs::write(&tmp, json)?))
        };
        if let Err(e) = written.and_then(|()| if tmp.exists() { Ok(std::fs::rename(&tmp, &self.path)?) } else { Ok(()) }) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        let bytes = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
        tracing::info!("save: wrote {} ({} KB) in {:?}", self.path.display(), bytes / 1024, started.elapsed());
        Ok(Saved { path: self.path, opened: self.opened })
    }
}

impl Engine {
    /// Save as Reaper `.rpp`, DAWproject or native `.otp`/`.json`, chosen by extension.
    pub fn save_project(&mut self, path: Option<&Path>) -> Result<PathBuf> {
        let saved = self.prepare_save(path)?.write()?;
        self.saved_as(&saved);
        Ok(saved.path)
    }

    /// What saving to `path` (the project's own path if `None`) writes: the project with
    /// every loaded plug-in's current state. Write it with [`SaveJob::write`], then call
    /// [`Engine::saved_as`].
    pub fn prepare_save(&mut self, path: Option<&Path>) -> Result<SaveJob> {
        let started = Instant::now();
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
        let mut project = self.project.clone();
        let mut slow = Vec::new();
        let (mut count, mut total) = (0, Duration::ZERO);
        let tracks = project.tracks.iter_mut().flat_map(|t| {
            let name = t.name.clone();
            t.instrument.iter_mut().chain(t.plugins.iter_mut()).map(move |s| (name.clone(), s))
        });
        for (owner, slot) in tracks.chain(project.master_plugins.iter_mut().map(|s| ("master".to_string(), s))) {
            let asked = Instant::now();
            let Some(state) = self.capture_state(slot) else { continue };
            let took = asked.elapsed();
            (count, total) = (count + 1, total + took);
            if took >= SLOW_STATE {
                slow.push(format!("'{}' on {owner}: {took:?}", slot.name));
            }
            match state {
                Ok(state) => slot.state = state,
                Err(e) => tracing::warn!("'{}': state not saved: {e:#}", slot.name),
            }
        }
        // Files refer to buses by track index (ids are per session).
        project.outputs_to_indices();
        let slow = if slow.is_empty() { String::new() } else { format!(" — slowest: {}", slow.join(", ")) };
        tracing::info!("save: {count} plug-in state(s) in {total:?}, ready to write in {:?}{slow}", started.elapsed());
        Ok(SaveJob { project, path, opened: self.opened })
    }

    /// A [`SaveJob`] was written: the project now lives there (unless another project was
    /// opened meanwhile).
    pub fn saved_as(&mut self, saved: &Saved) {
        if saved.opened == self.opened {
            self.project_path = Some(saved.path.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_save_finishing_after_another_project_opened_leaves_it_alone() {
        let dir = std::env::temp_dir().join(format!("opentake-save-{}", std::process::id()));
        let mut e = Engine::new();
        e.add_track("A").unwrap();
        let job = e.prepare_save(Some(&dir.join("a.otp"))).unwrap();
        e.new_project("B").unwrap();
        let saved = job.write().unwrap();
        e.saved_as(&saved);
        assert_eq!(e.project_path(), None, "B was never saved");
        let back: Project = serde_json::from_str(&std::fs::read_to_string(&saved.path).unwrap()).unwrap();
        assert_eq!(back.tracks[0].name, "A");
        let leftovers: Vec<_> = std::fs::read_dir(&dir).unwrap().filter_map(|f| f.ok()).map(|f| f.file_name()).collect();
        assert_eq!(leftovers.len(), 1, "no temporary file left: {leftovers:?}");

        let job = e.prepare_save(Some(&dir.join("b.rpp"))).unwrap();
        let saved = job.write().unwrap();
        e.saved_as(&saved);
        assert_eq!(e.project_path(), Some(dir.join("b.rpp").as_path()));
        let _ = std::fs::remove_dir_all(dir);
    }
}
