use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::Context;

use crate::releaseutil::{arch_target, download, run, verify_checksum};
use super::{cap, Step};
use crate::graph::Capability;

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

    fn provides(&self) -> Vec<Capability> {
        vec![cap::ZELLIJ_INSTALLED.into()]
    }

    fn run(&self, _repo: &Path) -> anyhow::Result<Option<String>> {
        if already_installed() {
            return Ok(None);
        }

        let target = arch_target("zellij")?;
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
        verify_checksum(&binary, &checksum_file, "zellij")?;

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
