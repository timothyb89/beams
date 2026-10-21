use std::path::Path;

use super::linkutil::{ensure_symlink, home_dir};
use super::{cap, Step};
use crate::graph::Capability;

/// Top-level repo entries to symlink into `$HOME`, and whether an existing
/// non-symlink entry should be force-replaced (moved aside to `.orig`
/// first) rather than left alone. Grow this list as more of the repo
/// becomes relevant to a fresh beam.
///
/// `.ssh` is forced: beams provision their own default `~/.ssh` (a
/// delegated signing agent, not a plain key), but the repo's `.ssh` -
/// carrying one stable key across beams - is meant to replace it, not
/// coexist with it.
///
/// `.claude` is deliberately excluded entirely: `~/.claude` already holds
/// real beam-managed state (session history, credentials, plugins), so it
/// can't be replaced wholesale. See `claude_md` for the one piece of it
/// (the repo's `CLAUDE.md`) that does get merged in. Shell config lives
/// under `shell/` and is linked separately by `shell_config.rs`, since it
/// targets `~/.config/...` rather than `~` directly.
const LINKS: &[(&str, bool)] = &[("bin", false), (".ssh", true)];

pub struct LinkFiles;

impl Step for LinkFiles {
    fn id(&self) -> &'static str {
        "link-files"
    }

    fn provides(&self) -> &'static [Capability] {
        &[cap::DOTFILES_LINKED]
    }

    fn run(&self, repo: &Path) -> anyhow::Result<Option<String>> {
        let home = home_dir()?;
        let mut created = Vec::new();

        for (name, force) in LINKS {
            let source = repo.join(name);
            let target = home.join(name);
            if ensure_symlink(&source, &target, *force)? {
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
