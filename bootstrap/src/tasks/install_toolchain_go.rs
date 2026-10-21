use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::bail;

use super::{Task, Template};
use crate::graph::Capability;
use crate::pathutil::expand_tilde;
use crate::releaseutil::{download, run, verify_checksum_str};

/// go.dev doesn't publish per-file `.sha256` sidecars the way GitHub
/// releases do (confirmed - `curl .../go1.27.3.linux-amd64.tar.gz.sha256`
/// just redirects to the download page) - checksums only live in its own
/// JSON release manifest (`https://go.dev/dl/?mode=json`). Pinning both
/// the version and its checksums as constants here, verified once
/// against that manifest directly, rather than fetching+parsing JSON on
/// every run for a value that only changes when `VERSION` does anyway.
const VERSION: &str = "1.27.3";
const AMD64_SHA256: &str = "65efbc80fdc1fa2cd2631cad5b481777a68e8725f34e89f50fc6ac714e379b40";
const ARM64_SHA256: &str = "a06e99309dbcbdca1a8ccd2282c3ee146e55930886e26b4f1193269587165f49";

/// Installs the official Go toolchain to `~/.local/toolchains/go` -
/// self-contained, no `PATH`/shell-config changes attempted (same
/// pattern as `steps::teleport_bin`'s `~/.teleport-bot/bin`): the
/// official tarball already extracts to a top-level `go/` directory, so
/// `go/bin/go` ends up at a predictable path either way.
pub struct InstallToolchainGo;

impl InstallToolchainGo {
    fn install_dir() -> std::path::PathBuf {
        expand_tilde("~/.local/toolchains")
    }
}

impl Task for InstallToolchainGo {
    fn id(&self) -> String {
        Self::NAME.to_string()
    }

    fn provides(&self) -> Vec<Capability> {
        vec!["go-toolchain-installed".into()]
    }

    fn run(&self) -> anyhow::Result<Option<String>> {
        let dest = Self::install_dir();
        let go_bin = dest.join("go/bin/go");
        if already_installed(&go_bin) {
            return Ok(None);
        }

        let (arch, sha256) = match std::env::consts::ARCH {
            "x86_64" => ("amd64", AMD64_SHA256),
            "aarch64" => ("arm64", ARM64_SHA256),
            other => bail!("no known go release asset for architecture {other}"),
        };
        let url = format!("https://go.dev/dl/go{VERSION}.linux-{arch}.tar.gz");

        let tmp = std::env::temp_dir().join(format!("beam-bootstrap-go-{}", std::process::id()));
        fs::create_dir_all(&tmp)?;
        let archive = tmp.join("go.tar.gz");
        download(&url, &archive)?;
        verify_checksum_str(&archive, sha256, "go toolchain")?;

        fs::create_dir_all(&dest)?;
        // `-xzf ... -C dest`: the tarball's own top-level `go/` entry
        // becomes `dest/go/`, replacing any previous install there.
        run(Command::new("tar").args(["-xzf", archive.to_str().unwrap(), "-C", dest.to_str().unwrap()]))?;

        let _ = fs::remove_dir_all(&tmp);
        Ok(Some(format!("installed go {VERSION} to {}", dest.join("go").display())))
    }
}

impl Template for InstallToolchainGo {
    const NAME: &'static str = "install-toolchain-go";
}

fn already_installed(go_bin: &Path) -> bool {
    Command::new(go_bin)
        .arg("version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(VERSION))
        .unwrap_or(false)
}
