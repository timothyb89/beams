use std::fs;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context};

use super::{Task, Template};
use crate::graph::Capability;
use crate::pathutil::expand_tilde;
use crate::releaseutil::{download, run, verify_checksum_str};

/// code-server doesn't publish a `SHA256SUMS` asset (or any per-file
/// checksum sidecar) alongside its GitHub releases - same situation
/// `install_toolchain_go` already handles for go.dev, by pinning a hash
/// computed from a verified download rather than trusting an unsigned
/// one. Bump `VERSION` (and recompute these) deliberately, not often.
const VERSION: &str = "4.141.0";
const AMD64_SHA256: &str = "65bcd20b524d03eb1b5adb7f360244599ef0e5b74089a46a982d32c074b97a29";
const ARM64_SHA256: &str = "e5cc78da1d631c6cf6162f9b18eeaa8ee9d3239e1a88bb99d0602cdc5fa1852d";

/// Extensions installed from the default Open VSX gallery (code-server
/// doesn't use Microsoft's marketplace, for licensing reasons) - `go` for
/// the required Go LSP (gopls, fetched by the extension itself against
/// the Go toolchain already on disk), `rust-analyzer` for Rust.
const EXTENSIONS: &[&str] = &["golang.go", "rust-lang.rust-analyzer"];

const SERVICE_NAME: &str = "code-server";

/// Installs code-server (browser-based VS Code - see `TODO.md` for the
/// alternatives considered) and backgrounds it as a beamctl service,
/// bound to loopback port 8080 - the one port `tsh beams publish`
/// actually forwards (see `~/AGENTS.md`'s "Publishing apps from beam").
/// Publishing itself stays a separate, manual `tsh beams publish
/// $BEAM_ALIAS` step; this task only gets code-server installed and
/// running.
///
/// Requires the Go and Rust toolchains already on disk (`go-toolchain`/
/// `rust-toolchain` tasks) - their LSPs (gopls, rust-analyzer) need an
/// actual toolchain to drive, not just the editor extension. Declared via
/// `requires` for the daemon's manifest graph, but - same caveat as
/// `checkout_pr_worktree::CheckoutPrWorktree` - only enforced there, so
/// `run()` also checks the filesystem directly for a bare `task run`.
pub struct InstallCodeServer;

impl InstallCodeServer {
    fn install_dir() -> PathBuf {
        expand_tilde("~/.local/toolchains/code-server")
    }
}

impl Task for InstallCodeServer {
    fn id(&self) -> String {
        Self::NAME.to_string()
    }

    fn provides(&self) -> Vec<Capability> {
        vec!["code-server-running".into()]
    }

    fn requires(&self) -> Vec<Capability> {
        vec!["go-toolchain-installed".into(), "rust-toolchain-installed".into()]
    }

    fn run(&self) -> anyhow::Result<Option<String>> {
        if service_exists()? {
            return Ok(None);
        }

        let go_bin = expand_tilde("~/.local/toolchains/go/bin/go");
        if !go_bin.exists() {
            bail!("{} not found - run install-toolchain-go first", go_bin.display());
        }
        let rustc = expand_tilde("~/.cargo/bin/rustc");
        if !rustc.exists() {
            bail!("{} not found - run install-toolchain-rust first", rustc.display());
        }

        let dest = Self::install_dir();
        let code_server_bin = dest.join("bin/code-server");
        if !code_server_bin.exists() {
            install_binary(&dest)?;
        }

        // One invocation, `--install-extension` repeated - code-server's
        // own CLI accepts it multiple times per call.
        let mut install_extensions = Command::new(&code_server_bin);
        for ext in EXTENSIONS {
            install_extensions.arg("--install-extension").arg(ext);
        }
        run(&mut install_extensions)?;

        let output = Command::new("beamctl")
            .args([
                "start",
                "--name",
                SERVICE_NAME,
                "--",
                code_server_bin.to_str().unwrap(),
                "--bind-addr",
                "127.0.0.1:8080",
                "--auth",
                "none",
                "--disable-telemetry",
                "--disable-update-check",
            ])
            .output()
            .context("running beamctl start")?;
        if !output.status.success() {
            bail!(
                "beamctl start {SERVICE_NAME} failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        Ok(Some(format!(
            "installed code-server {VERSION} to {} and started it on 127.0.0.1:8080 - \
             run `tsh beams publish $BEAM_ALIAS` to reach it",
            dest.display()
        )))
    }
}

impl Template for InstallCodeServer {
    const NAME: &'static str = "install-code-server";
}

fn service_exists() -> anyhow::Result<bool> {
    let output = Command::new("beamctl")
        .args(["show", SERVICE_NAME, "--json"])
        .output()
        .context("running beamctl show")?;
    Ok(output.status.success())
}

/// Downloads+verifies the pinned release tarball for the running
/// architecture, extracts it, and moves its top-level
/// `code-server-<VERSION>-linux-<arch>/` entry to `dest` - replacing
/// whatever (if anything, from a previous version) was there, same
/// "stable path wins" approach `install_toolchain_go` takes with `go/`.
fn install_binary(dest: &std::path::Path) -> anyhow::Result<()> {
    let (arch, sha256) = match std::env::consts::ARCH {
        "x86_64" => ("amd64", AMD64_SHA256),
        "aarch64" => ("arm64", ARM64_SHA256),
        other => bail!("no known code-server release asset for architecture {other}"),
    };
    let url = format!(
        "https://github.com/coder/code-server/releases/download/v{VERSION}/code-server-{VERSION}-linux-{arch}.tar.gz"
    );

    let tmp = std::env::temp_dir().join(format!("beam-bootstrap-code-server-{}", std::process::id()));
    fs::create_dir_all(&tmp)?;
    let archive = tmp.join("code-server.tar.gz");
    download(&url, &archive)?;
    verify_checksum_str(&archive, sha256, "code-server")?;

    run(Command::new("tar").args(["-xzf", archive.to_str().unwrap(), "-C", tmp.to_str().unwrap()]))?;
    let extracted = tmp.join(format!("code-server-{VERSION}-linux-{arch}"));

    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let _ = fs::remove_dir_all(dest);
    fs::rename(&extracted, dest)
        .with_context(|| format!("moving {} to {}", extracted.display(), dest.display()))?;

    let _ = fs::remove_dir_all(&tmp);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Not a test of the download/extension-install/beamctl calls
    /// themselves (same reason `install_toolchain_go`/`_rust` have no
    /// tests - all three need network or a real beam) - just confirms
    /// the stable install path and the two prerequisite-toolchain paths
    /// this task checks resolve under `~` as expected.
    #[test]
    fn install_dir_is_under_the_toolchains_dir() {
        let dir = InstallCodeServer::install_dir();
        assert!(dir.ends_with(".local/toolchains/code-server"));
    }

    #[test]
    fn prerequisite_toolchain_paths_match_what_those_tasks_install() {
        let go_bin = expand_tilde("~/.local/toolchains/go/bin/go");
        assert!(go_bin.ends_with(".local/toolchains/go/bin/go"));

        let rustc = expand_tilde("~/.cargo/bin/rustc");
        assert!(rustc.ends_with(".cargo/bin/rustc"));
    }
}
