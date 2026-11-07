use std::fs;
use std::process::Command;

use anyhow::Context;

use crate::releaseutil::{arch_target, download, run, verify_checksum};

/// starship isn't packaged for Debian - install a pinned binary release
/// from GitHub instead. Bump this (and re-check asset names at
/// https://github.com/starship/starship/releases) to upgrade.
const VERSION: &str = "v1.27.0";
const INSTALL_PATH: &str = "/usr/local/bin/starship";

/// Installs the pinned release (`Ok(None)` if it's already there), for
/// the `install-starship` template (`tasks::install_starship`).
pub fn install() -> anyhow::Result<Option<String>> {
    if already_installed() {
        return Ok(None);
    }

    let target = arch_target("starship")?;
    let base =
        format!("https://github.com/starship/starship/releases/download/{VERSION}/starship-{target}");
    let tmp =
        std::env::temp_dir().join(format!("beam-bootstrap-starship-{}", std::process::id()));
    fs::create_dir_all(&tmp).context("creating temp dir")?;
    let archive = tmp.join("starship.tar.gz");
    let checksum_file = tmp.join("starship.sha256");

    download(&format!("{base}.tar.gz"), &archive)?;
    download(&format!("{base}.tar.gz.sha256"), &checksum_file)?;
    // Unlike zellij's release, starship's published checksum covers
    // the tar.gz asset itself, not the extracted binary.
    verify_checksum(&archive, &checksum_file, "starship")?;

    run(Command::new("tar").args([
        "-xzf",
        archive.to_str().unwrap(),
        "-C",
        tmp.to_str().unwrap(),
    ]))?;

    let binary = tmp.join("starship");
    run(Command::new("sudo").args([
        "install",
        "-m",
        "0755",
        binary.to_str().unwrap(),
        INSTALL_PATH,
    ]))?;

    let _ = fs::remove_dir_all(&tmp);
    Ok(Some(format!("installed starship {VERSION} to {INSTALL_PATH}")))
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
