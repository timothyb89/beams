use std::env;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};

/// Shared by any step that needs to symlink a repo path into place
/// (`link.rs`, `shell_config.rs`). Ensures `target` is a symlink to
/// `source`, creating parent directories as needed. Returns `true` if a
/// new symlink was created, `false` if one already existed and pointed at
/// `source`.
///
/// If `target` exists as something else (a real file/dir, or a symlink
/// pointing elsewhere) and `force` is `false`, this refuses to touch it
/// rather than clobbering something unexpected - the right call for e.g.
/// `~/.claude`, which already holds real beam-managed state. If `force` is
/// `true`, the existing entry is moved aside to `<target>.orig` (once;
/// fails rather than overwriting a prior `.orig`) before linking. Only
/// use this for explicitly managed targets, such as the bootstrap binary
/// link under `~/.local/bin`; preserve Beam-managed SSH state.
pub fn ensure_symlink(source: &Path, target: &Path, force: bool) -> anyhow::Result<bool> {
    if !source.exists() {
        bail!("expected {} in repo checkout, not found", source.display());
    }

    match fs::symlink_metadata(target) {
        Ok(meta) if meta.file_type().is_symlink() => {
            let existing = fs::read_link(target)
                .with_context(|| format!("reading existing symlink {}", target.display()))?;
            if existing == source {
                return Ok(false); // already linked correctly
            }
            if !force {
                bail!(
                    "{} is already a symlink to {} (expected {})",
                    target.display(),
                    existing.display(),
                    source.display()
                );
            }
            fs::remove_file(target)
                .with_context(|| format!("removing stale symlink {}", target.display()))?;
            link(source, target)?;
            Ok(true)
        }
        Ok(_) => {
            if !force {
                bail!("{} already exists and is not a symlink", target.display());
            }
            let backup = PathBuf::from(format!("{}.orig", target.display()));
            if backup.exists() {
                bail!(
                    "{} already exists - won't overwrite; move it aside manually first",
                    backup.display()
                );
            }
            fs::rename(target, &backup).with_context(|| {
                format!("moving {} aside to {}", target.display(), backup.display())
            })?;
            link(source, target)?;
            Ok(true)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            link(source, target)?;
            Ok(true)
        }
        Err(err) => Err(err).context(format!("checking {}", target.display())),
    }
}

fn link(source: &Path, target: &Path) -> anyhow::Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    symlink(source, target)
        .with_context(|| format!("linking {} -> {}", target.display(), source.display()))
}

pub fn home_dir() -> anyhow::Result<PathBuf> {
    env::var("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}
