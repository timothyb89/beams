use std::path::Path;

use super::linkutil::{ensure_symlink, home_dir};
use super::Step;

/// (repo-relative path under `shell/`, target path relative to `~/.config`)
const LINKS: &[(&str, &str)] = &[
    ("shell/fish/config.fish", "fish/config.fish"),
    ("shell/starship.toml", "starship.toml"),
];

pub struct LinkShellConfig;

impl Step for LinkShellConfig {
    fn id(&self) -> &'static str {
        "shell-config"
    }

    fn run(&self, repo: &Path) -> anyhow::Result<Option<String>> {
        let config_dir = home_dir()?.join(".config");
        let mut created = Vec::new();

        for (source, target) in LINKS {
            let source = repo.join(source);
            let target = config_dir.join(target);
            if ensure_symlink(&source, &target)? {
                created.push(target.display().to_string());
            }
        }

        if created.is_empty() {
            Ok(None)
        } else {
            Ok(Some(format!("linked {}", created.join(", "))))
        }
    }
}
