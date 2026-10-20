use std::path::Path;

use super::linkutil::{ensure_symlink, home_dir};
use super::Step;

/// Top-level repo entries to symlink into `$HOME`. Grow this list as more
/// of the repo becomes relevant to a fresh beam.
///
/// `.claude` is deliberately excluded: `~/.claude` already holds real
/// beam-managed state (session history, credentials, plugins), so it can't
/// be replaced wholesale. See `claude_md` for the one piece of it (the repo's
/// `CLAUDE.md`) that does get merged in. Shell config lives under `shell/`
/// and is linked separately by `shell_config.rs`, since it targets
/// `~/.config/...` rather than `~` directly.
const LINKS: &[&str] = &["bin", ".ssh"];

pub struct LinkFiles;

impl Step for LinkFiles {
    fn id(&self) -> &'static str {
        "link-files"
    }

    fn run(&self, repo: &Path) -> anyhow::Result<Option<String>> {
        let home = home_dir()?;
        let mut created = Vec::new();

        for name in LINKS {
            let source = repo.join(name);
            let target = home.join(name);
            if ensure_symlink(&source, &target)? {
                created.push(*name);
            }
        }

        if created.is_empty() {
            Ok(None)
        } else {
            Ok(Some(format!("linked {}", created.join(", "))))
        }
    }
}
