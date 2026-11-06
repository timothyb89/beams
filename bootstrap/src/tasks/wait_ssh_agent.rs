use std::process::Command;
use std::time::{Duration, Instant};

use super::{Task, Template};
use crate::graph::Capability;
use crate::sshagent::relay_command;

/// How often to poll `ssh-add -l` while waiting, and how long to wait
/// before giving up - overridable via env vars (`BEAM_BOOTSTRAP_SSH_WAIT_*`)
/// purely so tests don't have to burn ten real minutes to exercise the
/// timeout path; production callers never set these.
const POLL_INTERVAL: Duration = Duration::from_secs(2);
const TIMEOUT: Duration = Duration::from_secs(600);

/// Waits until the SSH agent relay (`agentproxy`) lists at least one key,
/// i.e. until a `beams ssh` session is attached and the workstation has
/// shared an allowed key (`[ssh_agent] allowed_keys`, see `beams
/// agent-keys`), by polling `ssh-add -l` against it. Provides
/// `cap::SSH_AGENT_READY`, which `clone_git_repo` requires for an
/// ssh-style repo URL.
///
/// Errors out after `TIMEOUT` rather than waiting forever, so a manifest
/// that includes it fails visibly if no key is ever shared.
pub struct WaitSshAgent;

impl WaitSshAgent {
    fn ssh_add_bin() -> String {
        std::env::var("BEAM_BOOTSTRAP_SSH_ADD_BIN").unwrap_or_else(|_| "ssh-add".to_string())
    }

    fn poll_interval() -> Duration {
        env_duration_ms("BEAM_BOOTSTRAP_SSH_WAIT_POLL_MS").unwrap_or(POLL_INTERVAL)
    }

    fn timeout() -> Duration {
        env_duration_ms("BEAM_BOOTSTRAP_SSH_WAIT_TIMEOUT_MS").unwrap_or(TIMEOUT)
    }

    /// `ssh-add -l` exits 0 with at least one key listed, 1 ("The agent has
    /// no identities.") while no key is shared, and 2 if it can't reach the
    /// relay at all (daemon not up yet). Only exit 0 counts. Always uses the
    /// relay's socket (`sshagent`), never this process's inherited one.
    fn has_identity() -> bool {
        let mut cmd = Command::new(Self::ssh_add_bin());
        cmd.arg("-l");
        relay_command(&mut cmd);
        cmd.output().map(|o| o.status.success()).unwrap_or(false)
    }
}

fn env_duration_ms(var: &str) -> Option<Duration> {
    std::env::var(var).ok()?.parse::<u64>().ok().map(Duration::from_millis)
}

impl Task for WaitSshAgent {
    fn id(&self) -> String {
        Self::NAME.to_string()
    }

    fn provides(&self) -> Vec<Capability> {
        vec![super::cap::SSH_AGENT_READY.into()]
    }

    fn run(&self) -> anyhow::Result<Option<String>> {
        self.run_with_progress(&|_| {})
    }

    fn run_with_progress(&self, progress: &dyn Fn(&str)) -> anyhow::Result<Option<String>> {
        if Self::has_identity() {
            return Ok(Some("the workstation is sharing an SSH key".to_string()));
        }

        let start = Instant::now();
        let timeout = Self::timeout();
        loop {
            std::thread::sleep(Self::poll_interval());
            if Self::has_identity() {
                return Ok(Some(format!("the workstation shared an SSH key after {}s", start.elapsed().as_secs())));
            }
            let elapsed = start.elapsed();
            if elapsed >= timeout {
                anyhow::bail!(
                    "timed out after {}s waiting for the workstation to share an SSH key (attach with \
                     `beams ssh`, and allow a key there: `beams agent-keys`)",
                    timeout.as_secs()
                );
            }
            progress(&format!("waiting for the workstation to share an SSH key ({}s)...", elapsed.as_secs()));
        }
    }
}

impl Template for WaitSshAgent {
    const NAME: &'static str = "wait-ssh-agent";
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    // These tests all mutate process-wide env vars (`BEAM_BOOTSTRAP_SSH_*`)
    // - serialized so they can't stomp on each other if `cargo test` ever
    // runs this module's tests in parallel (the default).
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Writes a fake `ssh-add` to a scratch dir and points
    /// `BEAM_BOOTSTRAP_SSH_ADD_BIN` at it - `$1` is `-l` always; the
    /// script's own exit code is whatever `exit_code_path`'s *current*
    /// contents say, so a test can flip the fake agent from "locked" to
    /// "unlocked" mid-run by rewriting that file.
    fn fake_ssh_add(dir: &std::path::Path, exit_code_path: &std::path::Path) {
        let script = dir.join("ssh-add");
        fs::write(&script, format!("#!/bin/sh\nexit \"$(cat {})\"\n", exit_code_path.display())).unwrap();
        let mut perms = fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script, perms).unwrap();
        std::env::set_var("BEAM_BOOTSTRAP_SSH_ADD_BIN", &script);
    }

    fn scratch_dir(label: &str) -> std::path::PathBuf {
        let tmp = std::env::temp_dir().join(format!("wait-ssh-agent-unlock-test-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        tmp
    }

    #[test]
    fn already_unlocked_returns_immediately_with_no_wait() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = scratch_dir("already-unlocked");
        let exit_code = tmp.join("exit_code");
        fs::write(&exit_code, "0").unwrap(); // ssh-add -l succeeds right away
        fake_ssh_add(&tmp, &exit_code);
        std::env::set_var("BEAM_BOOTSTRAP_SSH_WAIT_TIMEOUT_MS", "100");

        let start = Instant::now();
        let message = WaitSshAgent.run().unwrap().unwrap();
        assert!(message.contains("is sharing"));
        assert!(start.elapsed() < Duration::from_millis(500), "should not have polled/slept at all");

        std::env::remove_var("BEAM_BOOTSTRAP_SSH_ADD_BIN");
        std::env::remove_var("BEAM_BOOTSTRAP_SSH_WAIT_TIMEOUT_MS");
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn waits_through_a_locked_poll_then_succeeds_once_unlocked() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = scratch_dir("waits-then-succeeds");
        let exit_code = tmp.join("exit_code");
        fs::write(&exit_code, "1").unwrap(); // starts locked
        fake_ssh_add(&tmp, &exit_code);
        std::env::set_var("BEAM_BOOTSTRAP_SSH_WAIT_POLL_MS", "20");
        std::env::set_var("BEAM_BOOTSTRAP_SSH_WAIT_TIMEOUT_MS", "5000");

        // Flip to "unlocked" shortly after the first poll would have
        // happened, from a background thread - simulates the human
        // finishing the passphrase prompt mid-wait.
        let flip_path = exit_code.clone();
        let flip = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            fs::write(&flip_path, "0").unwrap();
        });

        let progress_calls = std::sync::atomic::AtomicUsize::new(0);
        let message = WaitSshAgent
            .run_with_progress(&|_| {
                progress_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            })
            .unwrap()
            .unwrap();
        flip.join().unwrap();

        assert!(message.contains("shared an SSH key after"), "unexpected message: {message}");
        assert!(
            progress_calls.load(std::sync::atomic::Ordering::SeqCst) >= 1,
            "should have reported progress at least once while waiting"
        );

        std::env::remove_var("BEAM_BOOTSTRAP_SSH_ADD_BIN");
        std::env::remove_var("BEAM_BOOTSTRAP_SSH_WAIT_POLL_MS");
        std::env::remove_var("BEAM_BOOTSTRAP_SSH_WAIT_TIMEOUT_MS");
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn gives_up_with_an_error_after_the_timeout() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = scratch_dir("times-out");
        let exit_code = tmp.join("exit_code");
        fs::write(&exit_code, "1").unwrap(); // never unlocks
        fake_ssh_add(&tmp, &exit_code);
        std::env::set_var("BEAM_BOOTSTRAP_SSH_WAIT_POLL_MS", "10");
        std::env::set_var("BEAM_BOOTSTRAP_SSH_WAIT_TIMEOUT_MS", "50");

        let err = WaitSshAgent.run().unwrap_err();
        assert!(err.to_string().contains("timed out"), "unexpected error: {err}");

        std::env::remove_var("BEAM_BOOTSTRAP_SSH_ADD_BIN");
        std::env::remove_var("BEAM_BOOTSTRAP_SSH_WAIT_POLL_MS");
        std::env::remove_var("BEAM_BOOTSTRAP_SSH_WAIT_TIMEOUT_MS");
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn provides_the_shared_capability_constant() {
        assert_eq!(WaitSshAgent.provides(), vec![Capability::from(super::super::cap::SSH_AGENT_READY)]);
    }

    /// `ssh-add` must be pointed at the relay socket from `sshagent`, not
    /// at whatever `SSH_AUTH_SOCK` this process inherited.
    #[test]
    fn asks_the_relay_socket_not_the_inherited_agent() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = scratch_dir("asks-the-relay");
        let saved_sock = std::env::var("SSH_AUTH_SOCK").ok();
        std::env::set_var("SSH_AUTH_SOCK", "/some/other/agent.sock");
        let relay = tmp.join("ssh-agent.sock").display().to_string();
        std::env::set_var("BEAM_BOOTSTRAP_SSH_AUTH_SOCK", &relay);

        let script = tmp.join("ssh-add");
        fs::write(&script, format!("#!/bin/sh\n[ \"$SSH_AUTH_SOCK\" = \"{relay}\" ] && exit 0 || exit 2\n")).unwrap();
        let mut perms = fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script, perms).unwrap();
        std::env::set_var("BEAM_BOOTSTRAP_SSH_ADD_BIN", &script);

        let message = WaitSshAgent.run().unwrap().unwrap();
        assert!(message.contains("is sharing"), "unexpected message: {message}");

        std::env::remove_var("BEAM_BOOTSTRAP_SSH_ADD_BIN");
        std::env::remove_var("BEAM_BOOTSTRAP_SSH_AUTH_SOCK");
        match saved_sock {
            Some(sock) => std::env::set_var("SSH_AUTH_SOCK", sock),
            None => std::env::remove_var("SSH_AUTH_SOCK"),
        }
        let _ = fs::remove_dir_all(&tmp);
    }
}
