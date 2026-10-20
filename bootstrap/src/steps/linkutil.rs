use std::env;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};

/// Shared by any step that needs to symlink a repo path into place
/// (`link.rs`, `shell_config.rs`). Ensures `target` is a symlink to
/// `source`, creating parent directories as needed. Returns `true` if a
/// new symlink was created, `false` if one already existed and pointed at
/// `source`. Refuses to touch `target` if it exists and is anything else
/// (a real file/dir, or a symlink pointing somewhere else) rather than
/// clobbering something unexpected.
pub fn ensure_symlink(source: &Path, target: &Path) -> anyhow::Result<bool> {
    if !source.exists() {
        bail!("expected {} in repo checkout, not found", source.display());
    }

    match fs::symlink_metadata(target) {
        Ok(meta) if meta.file_type().is_symlink() => {
            let existing = fs::read_link(target)
                .with_context(|| format!("reading existing symlink {}", target.display()))?;
            if existing == source {
                Ok(false) // already linked correctly
            } else {
                bail!(
                    "{} is already a symlink to {} (expected {})",
                    target.display(),
                    existing.display(),
                    source.display()
                );
            }
        }
        Ok(_) => {
            bail!("{} already exists and is not a symlink", target.display());
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            symlink(source, target)
                .with_context(|| format!("linking {} -> {}", target.display(), source.display()))?;
            Ok(true)
        }
        Err(err) => Err(err).context(format!("checking {}", target.display())),
    }
}

pub fn home_dir() -> anyhow::Result<PathBuf> {
    env::var("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}
