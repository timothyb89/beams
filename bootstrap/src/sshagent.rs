//! Finding the real ssh-agent socket from inside `beam-bootstrapd`.
//!
//! Shared by `tasks::wait_ssh_agent_unlock` (polls `ssh-add -l`) and
//! `tasks::clone_git_repo` (runs `git clone` over an ssh:// URL, which
//! shells out to `ssh` itself as its transport) - both need to find the
//! agent from a process that was never the one that unlocked it.
//!
//! `beam-bootstrapd` is a long-lived daemon started once, early, by
//! `bin/beam-shell` - before `bin/ssh-agent-init` ever runs (see that
//! script's own step ordering). A process's environment is fixed at its
//! own startup; it never picks up a later interactive shell's `source
//! ssh-agent-init`. Confirmed live on two separate beams:
//! `/proc/<daemon-pid>/environ` had no `SSH_*` variables whatsoever, so
//! any task that just ran `ssh-add`/`git`/`ssh` with the daemon's
//! inherited environment would unconditionally fail - not "sometimes
//! races the unlock", genuinely never works, independent of whether a
//! human had actually unlocked the key in the dedicated pane.
//! `wait_ssh_agent_unlock` fixing only *itself* against this was the
//! first-found half of the bug: `clone_git_repo`'s `git clone` over
//! `ssh://`/`git@...` hit the exact same thing one task later (`git
//! clone` -> `Permission denied (publickey)`, since the `ssh` it shells
//! out to had no `SSH_AUTH_SOCK` either), which is what made this worth
//! sharing rather than fixing in just the one place it was first found.

use std::path::PathBuf;

use crate::pathutil::expand_tilde;

/// Where `bin/ssh-agent-init` persists the agent's env (`SSH_AUTH_SOCK`/
/// `SSH_AGENT_PID`) once it starts one - `${XDG_RUNTIME_DIR:-$HOME/.ssh}/agent.env`,
/// matching that script's own `env_file` exactly. Overridable via
/// `BEAM_BOOTSTRAP_SSH_AGENT_ENV_FILE` purely for tests.
fn agent_env_path() -> PathBuf {
    if let Ok(path) = std::env::var("BEAM_BOOTSTRAP_SSH_AGENT_ENV_FILE") {
        return PathBuf::from(path);
    }
    match std::env::var("XDG_RUNTIME_DIR") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir).join("agent.env"),
        _ => expand_tilde("~/.ssh/agent.env"),
    }
}

/// Finds the real ssh-agent socket to talk to, regardless of what (if
/// anything) this process's *own* environment already has - see the
/// module doc comment for why that's not redundant. Prefers an
/// already-set `SSH_AUTH_SOCK` (e.g. a bare `run-task`/`task run`
/// invoked directly from an interactive shell that already has one) over
/// `agent_env_path()`'s file, so this doesn't fight a caller that already
/// knows the right socket.
///
/// Callers pass the result to `Command::env("SSH_AUTH_SOCK", ...)` on
/// whatever subprocess actually needs to reach the agent - this function
/// only finds the value, it doesn't mutate this process's own env (which
/// would race every other concurrently-running task in the same daemon).
pub fn discover_ssh_auth_sock() -> Option<String> {
    if let Ok(sock) = std::env::var("SSH_AUTH_SOCK") {
        if !sock.is_empty() {
            return Some(sock);
        }
    }

    let contents = std::fs::read_to_string(agent_env_path()).ok()?;
    contents.lines().find_map(|line| {
        let value = line.trim().strip_prefix("SSH_AUTH_SOCK=")?;
        Some(value.split(';').next().unwrap_or(value).to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Mutex;

    // Both tests mutate process-wide env vars - serialized so they can't
    // stomp on each other if `cargo test` ever runs this module's tests
    // in parallel (the default).
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn scratch_dir(label: &str) -> std::path::PathBuf {
        let tmp = std::env::temp_dir().join(format!("sshagent-test-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        tmp
    }

    #[test]
    fn prefers_an_already_set_env_var_over_the_file() {
        let _guard = ENV_LOCK.lock().unwrap();
        let saved = std::env::var("SSH_AUTH_SOCK").ok();
        std::env::set_var("SSH_AUTH_SOCK", "/already/set/sock");
        // Even pointing at a file with a *different* value shouldn't
        // override an already-set env var.
        std::env::set_var("BEAM_BOOTSTRAP_SSH_AGENT_ENV_FILE", "/does/not/exist");

        assert_eq!(discover_ssh_auth_sock(), Some("/already/set/sock".to_string()));

        match saved {
            Some(v) => std::env::set_var("SSH_AUTH_SOCK", v),
            None => std::env::remove_var("SSH_AUTH_SOCK"),
        }
        std::env::remove_var("BEAM_BOOTSTRAP_SSH_AGENT_ENV_FILE");
    }

    #[test]
    fn falls_back_to_parsing_the_agent_env_file_when_unset() {
        let _guard = ENV_LOCK.lock().unwrap();
        let saved = std::env::var("SSH_AUTH_SOCK").ok();
        std::env::remove_var("SSH_AUTH_SOCK");

        let tmp = scratch_dir("falls-back");
        let agent_env_file = tmp.join("agent.env");
        fs::write(
            &agent_env_file,
            "SSH_AUTH_SOCK=/tmp/ssh-xyz/agent.123; export SSH_AUTH_SOCK;\nSSH_AGENT_PID=123; export SSH_AGENT_PID;\n",
        )
        .unwrap();
        std::env::set_var("BEAM_BOOTSTRAP_SSH_AGENT_ENV_FILE", &agent_env_file);

        assert_eq!(discover_ssh_auth_sock(), Some("/tmp/ssh-xyz/agent.123".to_string()));

        std::env::remove_var("BEAM_BOOTSTRAP_SSH_AGENT_ENV_FILE");
        match saved {
            Some(v) => std::env::set_var("SSH_AUTH_SOCK", v),
            None => std::env::remove_var("SSH_AUTH_SOCK"),
        }
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn returns_none_when_unset_and_no_file_exists() {
        let _guard = ENV_LOCK.lock().unwrap();
        let saved = std::env::var("SSH_AUTH_SOCK").ok();
        std::env::remove_var("SSH_AUTH_SOCK");
        std::env::set_var("BEAM_BOOTSTRAP_SSH_AGENT_ENV_FILE", "/definitely/does/not/exist");

        assert_eq!(discover_ssh_auth_sock(), None);

        std::env::remove_var("BEAM_BOOTSTRAP_SSH_AGENT_ENV_FILE");
        match saved {
            Some(v) => std::env::set_var("SSH_AUTH_SOCK", v),
            None => std::env::remove_var("SSH_AUTH_SOCK"),
        }
    }
}
