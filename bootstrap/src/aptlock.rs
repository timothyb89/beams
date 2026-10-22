//! Cross-process serialization for `apt-get`/`dpkg`.
//!
//! `steps::Step::exclusive_resources`/`tasks::Task::exclusive_resources`
//! (see `resource::APT`'s doc comment) only serialize tasks sharing a
//! resource tag *within one `executor::run` call* - a `Mutex` local to
//! that one process. The fixed startup sequence (`apt-install`,
//! `install-kubectl`) and a later daemon-driven manifest
//! (`install-toolchain-rust`'s `apt-get install gcc`) are two genuinely
//! separate OS processes with no shared lock at all otherwise - a
//! manifest submitted while the startup sequence's own `apt-install`
//! wave is still running could race both on the real apt/dpkg lock
//! (found as a gap while adding the toolchain tasks, see TODO.md).
//!
//! Fixed here with a well-known lock file and the system `flock(1)`
//! utility wrapping every actual `apt-get` invocation, rather than a
//! hand-rolled `fcntl`/`flock(2)` binding - no new dependency (this
//! crate otherwise has none for raw syscalls), and the lock is released
//! by the kernel the instant the wrapped process exits, crash or not,
//! same guarantee a Drop-based Rust-level lock would give but without
//! needing one.

use std::path::PathBuf;
use std::process::Command;

use anyhow::Context;

use crate::pathutil::expand_tilde;
use crate::releaseutil::run;

fn lock_path() -> PathBuf {
    expand_tilde("~/.beam-bootstrap/apt.lock")
}

/// Runs `sudo apt-get <args>`, serialized via `flock(1)` against every
/// other `beam-bootstrap` process (this one included, across separate
/// invocations) doing the same on this beam. `args` is e.g. `["update"]`
/// or `["install", "-y", "fish"]`.
pub fn apt_get(args: &[&str]) -> anyhow::Result<()> {
    let lock_path = lock_path();
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    // No `--` separator here, unlike most GNU tools - `flock`'s own
    // arg parsing doesn't support one at all (confirmed the hard way:
    // `flock <file> -- sudo ...` makes flock try to execute a program
    // literally named `--`, failing with "No such file or directory").
    // `flock [options] <file> <command> [<argument>...]` just takes
    // everything after `<file>` as the command directly.
    run(Command::new("flock").arg(&lock_path).arg("sudo").arg("apt-get").args(args))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Not a test of `flock`/`apt-get` themselves - just confirms the
    /// lock file ends up where `apt_get` says it does, parent
    /// directories and all, since every actual apt-get invocation in
    /// this test suite would otherwise need root (not available in
    /// CI/sandboxed test runs) to exercise for real.
    #[test]
    fn lock_path_is_under_the_daemon_state_dir_and_creatable() {
        let path = lock_path();
        assert!(path.ends_with(".beam-bootstrap/apt.lock"));

        let parent = path.parent().unwrap();
        std::fs::create_dir_all(parent).unwrap();
        assert!(parent.is_dir());
    }

    /// `flock(1)` itself, used as `apt_get` uses it, actually serializes
    /// two concurrent processes - the property this whole module exists
    /// for. Runs against a disposable lock file + a disposable critical
    /// section (incrementing a counter file without a lock would race),
    /// not real `apt-get`, so it doesn't need root or network.
    #[test]
    fn flock_actually_serializes_concurrent_processes() {
        let lock = std::env::temp_dir().join(format!("aptlock-test-{}.lock", std::process::id()));
        let counter = std::env::temp_dir().join(format!("aptlock-test-{}.counter", std::process::id()));
        let _ = std::fs::remove_file(&lock);
        std::fs::write(&counter, "0").unwrap();

        // Each child: read the counter, sleep (widening the window a
        // missing lock would let the other process land in), write
        // read+1 back - classic lost-update race if unserialized.
        let counter_path = shell_escape(&counter);
        let script = format!(
            "flock {} sh -c 'n=$(cat {counter_path}); sleep 0.2; echo $((n+1)) > {counter_path}'",
            shell_escape(&lock),
        );

        let children: Vec<_> = (0..5)
            .map(|_| Command::new("sh").arg("-c").arg(&script).spawn().unwrap())
            .collect();
        for mut child in children {
            assert!(child.wait().unwrap().success());
        }

        let final_count: i32 = std::fs::read_to_string(&counter).unwrap().trim().parse().unwrap();
        assert_eq!(final_count, 5, "flock should have serialized all 5 increments");

        let _ = std::fs::remove_file(&lock);
        let _ = std::fs::remove_file(&counter);
    }

    fn shell_escape(path: &std::path::Path) -> String {
        format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
    }
}
