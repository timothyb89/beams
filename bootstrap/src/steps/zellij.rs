use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context};

use super::Step;

/// zellij isn't packaged for Debian at all - install a pinned binary
/// release from GitHub instead. Bump this (and re-check the asset names at
/// https://github.com/zellij-org/zellij/releases) to upgrade.
const VERSION: &str = "v0.45.1";
const INSTALL_PATH: &str = "/usr/local/bin/zellij";

pub struct InstallZellij;

impl Step for InstallZellij {
    fn id(&self) -> &'static str {
        "install-zellij"
    }

    fn run(&self, _repo: &Path) -> anyhow::Result<Option<String>> {
        if already_installed() {
            return Ok(None);
        }

        let target = arch_target()?;
        let base = format!(
            "https://github.com/zellij-org/zellij/releases/download/{VERSION}/zellij-{target}"
        );
        let tmp = std::env::temp_dir().join(format!("beam-bootstrap-zellij-{}", std::process::id()));
        fs::create_dir_all(&tmp).context("creating temp dir")?;
        let archive = tmp.join("zellij.tar.gz");
        let checksum_file = tmp.join("zellij.sha256sum");

        download(&format!("{base}.tar.gz"), &archive)?;
        download(&format!("{base}.sha256sum"), &checksum_file)?;

        run(Command::new("tar").args([
            "-xzf",
            archive.to_str().unwrap(),
            "-C",
            tmp.to_str().unwrap(),
        ]))?;

        // The published checksum covers the extracted binary, not the
        // tar.gz asset itself - verify after extracting.
        let binary = tmp.join("zellij");
        verify_checksum(&binary, &checksum_file)?;

        run(Command::new("sudo").args([
            "install",
            "-m",
            "0755",
            binary.to_str().unwrap(),
            INSTALL_PATH,
        ]))?;

        let _ = fs::remove_dir_all(&tmp);
        Ok(Some(format!("installed zellij {VERSION} to {INSTALL_PATH}")))
    }
}

fn already_installed() -> bool {
    Command::new(INSTALL_PATH)
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(VERSION.trim_start_matches('v')))
        .unwrap_or(false)
}

fn arch_target() -> anyhow::Result<&'static str> {
    let output = Command::new("uname")
        .arg("-m")
        .output()
        .context("running uname -m")?;
    match String::from_utf8_lossy(&output.stdout).trim() {
        "x86_64" => Ok("x86_64-unknown-linux-musl"),
        "aarch64" => Ok("aarch64-unknown-linux-musl"),
        other => bail!("no known zellij release asset for architecture {other}"),
    }
}

fn download(url: &str, dest: &Path) -> anyhow::Result<()> {
    run(Command::new("curl").args(["-fsSL", "-o", dest.to_str().unwrap(), url]))
}

fn verify_checksum(archive: &Path, checksum_file: &Path) -> anyhow::Result<()> {
    let expected = fs::read_to_string(checksum_file)
        .context("reading downloaded checksum file")?
        .split_whitespace()
        .next()
        .context("checksum file was empty")?
        .to_string();

    let output = Command::new("sha256sum")
        .arg(archive)
        .output()
        .context("running sha256sum")?;
    let actual = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string();

    if actual != expected {
        bail!("zellij archive checksum mismatch: expected {expected}, got {actual}");
    }
    Ok(())
}

fn run(cmd: &mut Command) -> anyhow::Result<()> {
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
