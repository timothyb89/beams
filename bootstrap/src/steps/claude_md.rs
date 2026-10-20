use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;

use super::Step;

/// Marks the appended block so re-runs don't duplicate it, and so it's
/// obvious in `~/.claude/CLAUDE.md` where beam-provisioned content ends and
/// repo-provided content begins.
const MARKER: &str = "<!-- beam-bootstrap: appended from beams repo .claude/CLAUDE.md -->";

/// `~/.claude/CLAUDE.md` is beam-provisioned (onboarding/default guidance);
/// the repo's `.claude/CLAUDE.md` carries project-specific instructions
/// (e.g. the Teleport AI-use policy). Rather than symlinking over the
/// beam's copy, append the repo's content once, idempotently.
pub struct MergeClaudeMd;

impl Step for MergeClaudeMd {
    fn id(&self) -> &'static str {
        "claude-md-merge"
    }

    fn run(&self, repo: &Path) -> anyhow::Result<Option<String>> {
        let source = repo.join(".claude/CLAUDE.md");
        if !source.exists() {
            return Ok(None);
        }
        let addition = fs::read_to_string(&source)
            .with_context(|| format!("reading {}", source.display()))?;

        let target = home_dir()?.join(".claude/CLAUDE.md");
        let existing = fs::read_to_string(&target).unwrap_or_default();
        if existing.contains(MARKER) {
            return Ok(None); // already merged
        }

        let mut updated = existing;
        if !updated.is_empty() && !updated.ends_with('\n') {
            updated.push('\n');
        }
        updated.push('\n');
        updated.push_str(MARKER);
        updated.push('\n');
        updated.push_str(&addition);

        fs::write(&target, updated).with_context(|| format!("writing {}", target.display()))?;
        Ok(Some(format!("appended {} into {}", source.display(), target.display())))
    }
}

fn home_dir() -> anyhow::Result<PathBuf> {
    env::var("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}
