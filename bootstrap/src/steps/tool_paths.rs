use std::fs;
use std::path::Path;

use anyhow::Context;

use super::linkutil::{ensure_symlink, home_dir};
use super::{cap, Step};
use crate::graph::Capability;

/// Directories a tool might land a binary in later, created here -
/// empty, usually - purely so they're already on PATH before anything's
/// actually installed into them. `~/.cargo/bin` isn't listed: rustup's
/// own installer already wires that into both bash and fish on its own.
/// See `shell/fish/conf.d/beams-path.fish`'s doc comment for why this
/// ordering (directory exists before the shell config first runs) is
/// what makes a tool installed *after* a shell started still show up in
/// it with no reconfiguration.
const TOOLCHAIN_BIN_DIRS: &[&str] = &[".local/bin", ".local/toolchains/go/bin", ".local/toolchains/code-server/bin"];

/// Makes `beam-bootstrap` itself, and the Go/code-server toolchains'
/// `bin/` dirs (installed later by their own on-demand tasks), reachable
/// by bare name from an interactive shell - not just via the full paths
/// `bin/beam-shell`/`client/src/stage.rs` already resolve internally for
/// their own use.
pub struct LinkToolPaths;

impl Step for LinkToolPaths {
    fn id(&self) -> &'static str {
        "link-tool-paths"
    }

    fn provides(&self) -> Vec<Capability> {
        vec![cap::TOOL_PATHS_LINKED.into()]
    }

    fn run(&self, _repo: &Path) -> anyhow::Result<Option<String>> {
        let home = home_dir()?;
        let mut changed = Vec::new();

        for dir in TOOLCHAIN_BIN_DIRS {
            let path = home.join(dir);
            if !path.exists() {
                fs::create_dir_all(&path).with_context(|| format!("creating {}", path.display()))?;
                changed.push(path.display().to_string());
            }
        }

        // Symlinks whichever binary is running *this step right now* -
        // sidesteps re-deriving `bin/beam-shell`'s musl-release/
        // native-release/native-debug fallback chain, since that's
        // exactly the binary actually in use. `force=true`: this
        // symlink is ours alone to manage, always safe to repoint (e.g.
        // after a rebuild at a different path), same reasoning
        // `link.rs` already uses for `~/.ssh`.
        let current_exe = std::env::current_exe().context("resolving this binary's own path")?;
        let target = home.join(".local/bin/beam-bootstrap");
        if ensure_symlink(&current_exe, &target, true)? {
            changed.push(target.display().to_string());
        }

        if changed.is_empty() {
            Ok(None)
        } else {
            Ok(Some(format!("prepared {}", changed.join(", "))))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pure listing check - the directories/symlink themselves get
    /// exercised for real by every other test in this suite that
    /// bootstraps a beam end to end (this step has no apt/network/
    /// beamctl side effects to mock around, unlike most of its
    /// siblings).
    #[test]
    fn toolchain_bin_dirs_cover_go_and_code_server_but_not_cargo() {
        assert!(TOOLCHAIN_BIN_DIRS.contains(&".local/toolchains/go/bin"));
        assert!(TOOLCHAIN_BIN_DIRS.contains(&".local/toolchains/code-server/bin"));
        assert!(!TOOLCHAIN_BIN_DIRS.iter().any(|d| d.contains("cargo")));
    }
}
