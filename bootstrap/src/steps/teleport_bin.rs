use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context};
use serde::Deserialize;

use super::linkutil::home_dir;
use super::releaseutil::{download, run, verify_checksum};
use super::{cap, Step};
use crate::graph::Capability;

/// Where tctl/tbot land, and everything the k3s bot steps need. Not under
/// `~/.config` or `/usr/local` - project.md only requires it be accessible
/// to the beams user, so it's self-contained under one directory.
pub const BOT_DIR: &str = ".teleport-bot";

#[derive(Deserialize)]
struct TshVersion {
    #[serde(rename = "proxyVersion")]
    proxy_version: String,
}

/// Downloads the `tctl`/`tbot` binaries matching the cluster's proxy
/// version (from `tsh version -f json`) and installs them to
/// `~/.teleport-bot/bin/`. Only these two binaries are extracted from the
/// release tarball - `teleport`/`tsh`/etc aren't needed here.
pub struct InstallTeleportBin;

impl Step for InstallTeleportBin {
    fn id(&self) -> &'static str {
        "install-teleport-bin"
    }

    fn provides(&self) -> &'static [Capability] {
        &[cap::TELEPORT_BIN_INSTALLED]
    }

    fn run(&self, _repo: &Path) -> anyhow::Result<Option<String>> {
        let version = proxy_version()?;
        let bin_dir = home_dir()?.join(BOT_DIR).join("bin");

        if already_installed(&bin_dir, &version) {
            return Ok(None);
        }

        let arch = match std::env::consts::ARCH {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            other => bail!("no known teleport release asset for architecture {other}"),
        };
        let base = format!("https://cdn.teleport.dev/teleport-v{version}-linux-{arch}-bin.tar.gz");

        let tmp = std::env::temp_dir()
            .join(format!("beam-bootstrap-teleport-{}", std::process::id()));
        fs::create_dir_all(&tmp).context("creating temp dir")?;
        let archive = tmp.join("teleport.tar.gz");
        let checksum_file = tmp.join("teleport.sha256");

        download(&base, &archive)?;
        download(&format!("{base}.sha256"), &checksum_file)?;
        verify_checksum(&archive, &checksum_file, "teleport release")?;

        run(Command::new("tar").args([
            "-xzf",
            archive.to_str().unwrap(),
            "-C",
            tmp.to_str().unwrap(),
            "teleport/tbot",
            "teleport/tctl",
        ]))?;

        fs::create_dir_all(&bin_dir).with_context(|| format!("creating {}", bin_dir.display()))?;
        for bin in ["tbot", "tctl"] {
            let src = tmp.join("teleport").join(bin);
            let dest = bin_dir.join(bin);
            fs::copy(&src, &dest)
                .with_context(|| format!("installing {} to {}", bin, dest.display()))?;
            fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))
                .with_context(|| format!("setting permissions on {}", dest.display()))?;
        }

        let _ = fs::remove_dir_all(&tmp);
        Ok(Some(format!("installed tbot/tctl {version} to {}", bin_dir.display())))
    }
}

/// `tsh version -f json`'s `proxyVersion` - matching the live proxy/auth
/// server, rather than the client (`tsh`) version, is the safer bet for
/// tbot/tctl compatibility.
fn proxy_version() -> anyhow::Result<String> {
    let output = Command::new("tsh")
        .args(["version", "-f", "json"])
        .output()
        .context("running tsh version -f json")?;
    if !output.status.success() {
        bail!(
            "tsh version failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let parsed: TshVersion =
        serde_json::from_slice(&output.stdout).context("parsing tsh version -f json output")?;
    Ok(parsed.proxy_version)
}

fn already_installed(bin_dir: &Path, version: &str) -> bool {
    Command::new(bin_dir.join("tbot"))
        .arg("version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(version))
        .unwrap_or(false)
}
