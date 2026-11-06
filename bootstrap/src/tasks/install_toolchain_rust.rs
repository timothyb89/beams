use std::path::Path;
use std::process::Command;

use super::{Task, Template};
use crate::graph::Capability;
use crate::pathutil::expand_tilde;
use crate::releaseutil::download;

/// Installs the Rust toolchain via `rustup` (the same command
/// `bootstrap/README.md`'s "Building" section already tells a human to
/// run) to the default `~/.cargo`/`~/.rustup`. Needs `gcc` as a linker
/// first (same note as that README section) - installed here via apt
/// rather than assumed present.
pub struct InstallToolchainRust;

impl Task for InstallToolchainRust {
    fn id(&self) -> String {
        Self::NAME.to_string()
    }

    fn provides(&self) -> Vec<Capability> {
        vec!["rust-toolchain-installed".into()]
    }

    fn exclusive_resources(&self) -> Vec<Capability> {
        // The shared package helper holds apt only for its transaction.
        // Rustup itself holds toolchain destinations, so it can overlap apt.
        vec![
            format!("path:{}", expand_tilde("~/.cargo").display()).into(),
            format!("path:{}", expand_tilde("~/.rustup").display()).into(),
        ]
    }

    fn run(&self) -> anyhow::Result<Option<String>> {
        self.run_with_progress(&|_| {})
    }

    fn run_with_progress(&self, progress: &dyn Fn(&str)) -> anyhow::Result<Option<String>> {
        let rustc = expand_tilde("~/.cargo/bin/rustc");
        if already_installed(&rustc) {
            return Ok(None);
        }

        crate::packages::PackageManager::default().install(&["gcc".into()], progress)?;

        let script =
            std::env::temp_dir().join(format!("beam-bootstrap-rustup-init-{}", std::process::id()));
        download("https://sh.rustup.rs", &script)?;
        // rustup's own installer fetches and verifies the actual
        // toolchain itself - no separate checksum step needed here, as
        // opposed to the pinned-binary-download tasks/steps elsewhere in
        // this binary that have to do that themselves.
        //
        // No `-s --` here, unlike the familiar `curl ... | sh -s --
        // <args>` idiom everywhere rustup is documented - those flags
        // tell *sh* to read the script from stdin and then separate
        // sh's own flags from the script's. We already have the script
        // in a file and pass it as sh's argument directly, so the
        // script's own args just follow normally; `-s --` would
        // otherwise become the *script's* `$1`/`$2`, which is exactly
        // what failed running this for real the first time (rustup-init
        // reported "unexpected argument '-s'").
        crate::commandlog::run(
            Command::new("sh").arg(&script).args([
                "-y",
                "--default-toolchain",
                "stable",
                "--profile",
                "minimal",
            ]),
            progress,
        )?;
        let _ = std::fs::remove_file(&script);

        Ok(Some(format!(
            "installed rust toolchain via rustup to {}",
            rustc.display()
        )))
    }
}

impl Template for InstallToolchainRust {
    const NAME: &'static str = "install-toolchain-rust";
}

fn already_installed(rustc: &Path) -> bool {
    Command::new(rustc)
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .is_some()
}
