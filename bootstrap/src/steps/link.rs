use std::env;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};

use super::Step;

/// Top-level repo entries to symlink into `$HOME`. Grow this list as more
/// of the repo becomes relevant to a fresh beam.
///
/// `.claude` is deliberately excluded: `~/.claude` already holds real
/// beam-managed state (session history, credentials, plugins), so it can't
/// be replaced wholesale. See `claude_md` for the one piece of it (the repo's
/// `CLAUDE.md`) that does get merged in.
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
            if !source.exists() {
                bail!("expected {} in repo checkout, not found", source.display());
            }
            let target = home.join(name);

            match fs::symlink_metadata(&target) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    let existing = fs::read_link(&target)
                        .with_context(|| format!("reading existing symlink {}", target.display()))?;
                    if existing == source {
                        continue; // already linked correctly
                    }
                    bail!(
                        "{} is already a symlink to {} (expected {})",
                        target.display(),
                        existing.display(),
                        source.display()
                    );
                }
                Ok(_) => {
                    // Exists and isn't a symlink at all - don't clobber a
                    // real file/directory someone put there.
                    bail!("{} already exists and is not a symlink", target.display());
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    symlink(&source, &target).with_context(|| {
                        format!("linking {} -> {}", target.display(), source.display())
                    })?;
                    created.push(*name);
                }
                Err(err) => return Err(err).context(format!("checking {}", target.display())),
            }
        }

        if created.is_empty() {
            Ok(None)
        } else {
            Ok(Some(format!("linked {}", created.join(", "))))
        }
    }
}

fn home_dir() -> anyhow::Result<PathBuf> {
    env::var("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}
