use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context};

/// Shared helpers for steps/tasks that install a pinned release binary -
/// `steps::zellij`/`steps::starship`/`steps::teleport_bin`, and
/// `tasks::install_toolchain_go`. Lives at the crate root (not nested
/// under `steps`) since both `steps` and `tasks` need it.
pub fn arch_target(project: &str) -> anyhow::Result<&'static str> {
    let output = Command::new("uname")
        .arg("-m")
        .output()
        .context("running uname -m")?;
    match String::from_utf8_lossy(&output.stdout).trim() {
        "x86_64" => Ok("x86_64-unknown-linux-musl"),
        "aarch64" => Ok("aarch64-unknown-linux-musl"),
        other => bail!("no known {project} release asset for architecture {other}"),
    }
}

pub fn download(url: &str, dest: &Path) -> anyhow::Result<()> {
    run(Command::new("curl").args(["-fsSL", "-o", dest.to_str().unwrap(), url]))
}

/// Compares `sha256sum(file)` against the hash in `checksum_file` (its
/// first whitespace-separated field - some projects publish bare hashes,
/// others `<hash>  <path>` à la `sha256sum -c`).
pub fn verify_checksum(file: &Path, checksum_file: &Path, label: &str) -> anyhow::Result<()> {
    let expected = fs::read_to_string(checksum_file)
        .context("reading downloaded checksum file")?
        .split_whitespace()
        .next()
        .context("checksum file was empty")?
        .to_string();
    verify_checksum_str(file, &expected, label)
}

/// Same as `verify_checksum`, but against a checksum already known as a
/// string (e.g. pinned as a source constant, or pulled from a release
/// manifest JSON) rather than a downloaded sidecar file - not every
/// project publishes one of those (go.dev doesn't; it only lists
/// checksums in its own JSON release manifest).
pub fn verify_checksum_str(file: &Path, expected: &str, label: &str) -> anyhow::Result<()> {
    let output = Command::new("sha256sum")
        .arg(file)
        .output()
        .context("running sha256sum")?;
    let actual = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string();

    if actual != expected {
        bail!("{label} checksum mismatch: expected {expected}, got {actual}");
    }
    Ok(())
}

pub fn run(cmd: &mut Command) -> anyhow::Result<()> {
    let output = cmd.output().with_context(|| format!("running {cmd:?}"))?;
    if !output.status.success() {
        bail!(
            "{:?} failed ({}): {}",
            cmd,
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}
