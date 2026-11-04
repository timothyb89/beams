//! Recently used workstation directories, offered by the path dropdown
//! while a path field is still empty. Stored as a small JSON file next to
//! the local daemon's state so every `beams ssh` session shares them.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const MAX_RECENTS: usize = 8;

#[derive(Default, Deserialize, Serialize)]
struct Stored {
    #[serde(default)]
    directories: Vec<String>,
}

#[derive(Debug, Default)]
pub struct RecentPaths {
    /// Where to persist; `None` keeps them in memory only (tests).
    file: Option<PathBuf>,
    directories: Vec<String>,
}

impl RecentPaths {
    pub fn load(file: PathBuf) -> Self {
        let directories = std::fs::read(&file)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Stored>(&bytes).ok())
            .map(|stored| stored.directories)
            .unwrap_or_default();
        Self {
            file: Some(file),
            directories,
        }
    }

    /// `~/.config/beams-client/recent-paths.json` (or under
    /// `$XDG_CONFIG_HOME`); in memory only if neither can be found.
    pub fn load_default() -> Self {
        match crate::clientd::config_dir() {
            Ok(dir) => Self::load(dir.join("beams-client/recent-paths.json")),
            Err(_) => Self::default(),
        }
    }

    /// Most recent first, as typed into a field (`~/` abbreviated, ending
    /// in `/`).
    pub fn directories(&self) -> Vec<String> {
        self.directories.clone()
    }

    /// Records a used workstation path: the directory itself, or a file's
    /// parent directory.
    pub fn record(&mut self, path: &Path, is_dir: bool) {
        let dir = if is_dir { Some(path) } else { path.parent() };
        let Some(dir) = dir.filter(|dir| dir.is_absolute()) else {
            return;
        };
        let shown = display(dir);
        self.directories.retain(|existing| *existing != shown);
        self.directories.insert(0, shown);
        self.directories.truncate(MAX_RECENTS);
        if let Some(file) = &self.file {
            let stored = Stored {
                directories: self.directories.clone(),
            };
            if let Some(parent) = file.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Ok(bytes) = serde_json::to_vec_pretty(&stored) {
                // Write-then-rename so concurrent sessions never read a
                // half-written file.
                let temp = file.with_extension(format!("json.{}.tmp", std::process::id()));
                if std::fs::write(&temp, bytes).is_ok() {
                    let _ = std::fs::rename(&temp, file);
                }
            }
        }
    }
}

/// `path` as a user would type it: under `$HOME` as `~/...`, always with a
/// trailing `/`.
fn display(path: &Path) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut shown = match home
        .as_deref()
        .and_then(|home| path.strip_prefix(home).ok())
    {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    };
    if !shown.ends_with('/') {
        shown.push('/');
    }
    shown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_parent_directories_most_recent_first_and_persists() {
        let dir = std::env::temp_dir().join(format!("beams-recent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("recent-paths.json");
        let home = PathBuf::from(std::env::var("HOME").unwrap());

        let mut recents = RecentPaths::load(file.clone());
        assert!(recents.directories().is_empty());
        recents.record(&home.join("Downloads/report.csv"), false);
        recents.record(Path::new("/srv/data"), true);
        recents.record(&home.join("Downloads/other.csv"), false);
        recents.record(Path::new("relative/ignored"), true);
        assert_eq!(recents.directories(), vec!["~/Downloads/", "/srv/data/"]);

        let reloaded = RecentPaths::load(file);
        assert_eq!(reloaded.directories(), vec!["~/Downloads/", "/srv/data/"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn keeps_a_bounded_list() {
        let mut recents = RecentPaths::default();
        for index in 0..20 {
            recents.record(Path::new(&format!("/d{index}")), true);
        }
        assert_eq!(recents.directories().len(), MAX_RECENTS);
        assert_eq!(recents.directories()[0], "/d19/");
    }
}
