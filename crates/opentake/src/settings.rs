//! User settings persisted between sessions: plug-in scan folders and recent projects.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Settings {
    /// Folders scanned for VST3 plug-ins. `None` means the platform defaults.
    #[serde(default)]
    pub plugin_paths: Option<Vec<PathBuf>>,
    /// Most recently opened/saved projects, newest first.
    #[serde(default)]
    pub recent_projects: Vec<PathBuf>,
    /// Notation/tab view size (1.0 = normal).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_scale: Option<f32>,
    /// Notation/tab view on a white page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_paper: Option<bool>,
}

pub const MAX_RECENT: usize = 10;

/// `<config dir>/OpenTake/settings.json` (macOS: ~/Library/Application Support), or
/// `$OPENTAKE_CONFIG_DIR/settings.json` when that is set. Settings from the app's earlier
/// name (`mcp-daw`) are copied over the first time.
pub fn default_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("OPENTAKE_CONFIG_DIR") {
        return Some(PathBuf::from(dir).join("settings.json"));
    }
    let config = dirs::config_dir()?;
    let path = config.join("OpenTake").join("settings.json");
    migrate(&config.join("mcp-daw").join("settings.json"), &path);
    Some(path)
}

fn migrate(old: &Path, new: &Path) {
    if new.exists() || !old.exists() {
        return;
    }
    let copied = new.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|_| std::fs::copy(old, new));
    match copied {
        Ok(_) => tracing::info!("moved settings from {} to {}", old.display(), new.display()),
        Err(e) => tracing::warn!("could not migrate settings from {}: {e}", old.display()),
    }
}

impl Settings {
    /// Missing or unreadable files yield defaults (with a warning for the latter).
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                tracing::warn!("ignoring invalid settings file {}: {e}", path.display());
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    /// Move `project` to the top of the recent list.
    pub fn add_recent(&mut self, project: &Path) {
        self.recent_projects.retain(|p| p != project);
        self.recent_projects.insert(0, project.to_path_buf());
        self.recent_projects.truncate(MAX_RECENT);
    }

    /// Recent projects that still exist on disk.
    pub fn existing_recent(&self) -> Vec<PathBuf> {
        self.recent_projects.iter().filter(|p| p.exists()).cloned().collect()
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_missing_file() {
        let path = std::env::temp_dir().join(format!("opentake-settings-{}/settings.json", std::process::id()));
        assert_eq!(Settings::load(&path), Settings::default());
        let s = Settings { plugin_paths: Some(vec!["/tmp/plugs".into()]), recent_projects: vec!["/tmp/a.json".into()], tab_scale: Some(1.5), tab_paper: Some(true) };
        s.save(&path).unwrap();
        assert_eq!(Settings::load(&path), s);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn recent_list_is_newest_first_deduped_and_capped() {
        let mut s = Settings::default();
        for i in 0..12 {
            s.add_recent(Path::new(&format!("/p/{i}.json")));
        }
        s.add_recent(Path::new("/p/5.json"));
        assert_eq!(s.recent_projects.len(), MAX_RECENT);
        assert_eq!(s.recent_projects[0], PathBuf::from("/p/5.json"));
        assert_eq!(s.recent_projects.iter().filter(|p| p.ends_with("5.json")).count(), 1);
        assert_eq!(s.recent_projects[1], PathBuf::from("/p/11.json"));
    }

    #[test]
    fn settings_migrate_from_old_folder_once() {
        let base = std::env::temp_dir().join(format!("opentake-migrate-{}", std::process::id()));
        let (old, new) = (base.join("mcp-daw/settings.json"), base.join("OpenTake/settings.json"));
        Settings { recent_projects: vec!["/x.json".into()], ..Default::default() }.save(&old).unwrap();
        migrate(&old, &new);
        assert_eq!(Settings::load(&new).recent_projects, vec![PathBuf::from("/x.json")]);
        // An existing new file is never overwritten.
        Settings::default().save(&new).unwrap();
        migrate(&old, &new);
        assert!(Settings::load(&new).recent_projects.is_empty());
        std::fs::remove_dir_all(base).ok();
    }
}
