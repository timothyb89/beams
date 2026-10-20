use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context};

/// Builds a temp directory named `beams/` containing exactly what a fresh
/// beam needs: every git-tracked file in `repo` (so it automatically
/// tracks the repo rather than needing a hand-maintained file list), plus
/// the two things deliberately *not* tracked in git but still required -
/// the ssh private key, and a prebuilt `beam-bootstrap` binary (no Rust
/// toolchain on a fresh beam; see bootstrap/README.md).
pub fn build(repo: &Path) -> anyhow::Result<PathBuf> {
    let tmp = std::env::temp_dir().join(format!("beams-client-stage-{}", std::process::id()));
    let dest_root = tmp.join("beams");
    fs::create_dir_all(&dest_root).with_context(|| format!("creating {}", dest_root.display()))?;

    for rel in tracked_files(repo)? {
        copy_preserving_mode(&repo.join(&rel), &dest_root.join(&rel))?;
    }

    let key = repo.join(".ssh/beams_id_ed25519");
    if key.exists() {
        copy_preserving_mode(&key, &dest_root.join(".ssh/beams_id_ed25519"))?;
    }

    let release_bin = repo.join("bootstrap/target/release/beam-bootstrap");
    let debug_bin = repo.join("bootstrap/target/debug/beam-bootstrap");
    let bin = if release_bin.exists() {
        release_bin
    } else if debug_bin.exists() {
        debug_bin
    } else {
        bail!(
            "no beam-bootstrap binary found under {}/bootstrap/target - run `cargo build --release` there first",
            repo.display()
        );
    };
    let rel = bin.strip_prefix(repo).unwrap_or(&bin);
    copy_preserving_mode(&bin, &dest_root.join(rel))?;

    Ok(tmp)
}

fn tracked_files(repo: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let output = Command::new("git")
        .args(["-C"])
        .arg(repo)
        .args(["ls-files"])
        .output()
        .context("running git ls-files")?;
    if !output.status.success() {
        bail!(
            "git ls-files failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(PathBuf::from)
        .collect())
}

fn copy_preserving_mode(src: &Path, dest: &Path) -> anyhow::Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    fs::copy(src, dest).with_context(|| format!("copying {} to {}", src.display(), dest.display()))?;
    let mode = fs::metadata(src)
        .with_context(|| format!("reading permissions of {}", src.display()))?
        .permissions()
        .mode();
    fs::set_permissions(dest, fs::Permissions::from_mode(mode))
        .with_context(|| format!("setting permissions on {}", dest.display()))?;
    Ok(())
}
