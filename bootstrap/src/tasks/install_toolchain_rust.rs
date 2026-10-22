use std::path::Path;
use std::process::Command;

use super::{Task, Template};
use crate::aptlock;
use crate::graph::Capability;
use crate::pathutil::expand_tilde;
use crate::releaseutil::{download, run};

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
        // apt-get install gcc below shares the real apt/dpkg lock
        // steps::apt/steps::kubectl already tag themselves against (see
        // steps::resource::APT's doc comment) - held for this task's
        // entire run(), not just the apt-get call specifically, since
        // there's no API yet for a task to release/reacquire a resource
        // mid-run. Coarser than ideal (blocks other apt-touching tasks
        // for the whole rustup install, not just the gcc install), but
        // simple and correct; narrow it if that ever actually matters.
        //
        // This only serializes other tasks in the *same*
        // `executor::run` call, though - it doesn't protect against the
        // fixed startup sequence's `apt-install`, a wholly separate OS
        // process, running `apt-get` at the same moment. `aptlock::apt_get`
        // below is what actually closes that gap (see its own doc
        // comment); this tag is still worth keeping too, since it avoids
        // even starting a second apt-touching task concurrently within
        // one process, rather than starting it and then blocking inside
        // `apt_get`'s `flock`.
        vec![crate::steps::resource::APT.into()]
    }

    fn run(&self) -> anyhow::Result<Option<String>> {
        let rustc = expand_tilde("~/.cargo/bin/rustc");
        if already_installed(&rustc) {
            return Ok(None);
        }

        aptlock::apt_get(&["update"])?;
        aptlock::apt_get(&["install", "-y", "gcc"])?;

        let script = std::env::temp_dir().join(format!("beam-bootstrap-rustup-init-{}", std::process::id()));
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
        run(Command::new("sh").arg(&script).args([
            "-y",
            "--default-toolchain",
            "stable",
            "--profile",
            "minimal",
        ]))?;
        let _ = std::fs::remove_file(&script);

        Ok(Some(format!("installed rust toolchain via rustup to {}", rustc.display())))
    }
}

impl Template for InstallToolchainRust {
    const NAME: &'static str = "install-toolchain-rust";
}

fn already_installed(rustc: &Path) -> bool {
    Command::new(rustc).arg("--version").output().ok().filter(|o| o.status.success()).is_some()
}
