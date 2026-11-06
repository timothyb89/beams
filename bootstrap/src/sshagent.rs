//! Where processes started by `beam-bootstrapd` find the SSH agent.
//!
//! The agent is the daemon's own relay (`agentproxy`): it holds no keys
//! itself and asks the workstation for each signature (see
//! `docs/designs/SSH_AGENT_PROXY_DESIGN.md`). Interactive shells get the same socket as
//! `SSH_AUTH_SOCK` from `bin/beam-shell` and `shell/fish/config.fish`; the
//! daemon's own environment predates those, so tasks that run `git`/`ssh`
//! (`clone_git_repo`, `checkout_pr_worktree`, `wait_ssh_agent`) prepare
//! their commands with `relay_command`.

use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

use crate::pathutil::expand_tilde;

static RELAY_SOCKET: OnceLock<PathBuf> = OnceLock::new();

/// Records where this daemon's relay listens (next to its own socket).
pub fn set_relay_socket(path: PathBuf) {
    let _ = RELAY_SOCKET.set(path);
}

/// The relay socket path tasks should use: this daemon's, else the default
/// `~/.beam-bootstrap/ssh-agent.sock` (a task run in-process by the CLI
/// talks to the running daemon's relay). `BEAM_BOOTSTRAP_SSH_AUTH_SOCK`
/// overrides it, for tests only.
pub fn discover_ssh_auth_sock() -> Option<String> {
    if let Ok(sock) = std::env::var("BEAM_BOOTSTRAP_SSH_AUTH_SOCK") {
        if !sock.is_empty() {
            return Some(sock);
        }
    }
    let path = RELAY_SOCKET
        .get()
        .cloned()
        .unwrap_or_else(|| expand_tilde("~/.beam-bootstrap/ssh-agent.sock"));
    Some(path.display().to_string())
}

/// Points `cmd` at the relay (`discover_ssh_auth_sock`) and, inside a
/// tagged run, sets `BEAMS_TASK_RUN` (`executor::current_task_run`) so the
/// workstation can match the key use to the manifest that asked for it
/// (a startup grant covers only its own tasks). Every task that runs
/// `git`/`ssh` should go through this rather than setting either by hand:
/// a command without the tag falls outside the grant and prompts.
pub fn relay_command(cmd: &mut Command) -> &mut Command {
    if let Some(sock) = discover_ssh_auth_sock() {
        cmd.env("SSH_AUTH_SOCK", sock);
    }
    if let Some(run) = crate::executor::current_task_run() {
        cmd.env("BEAMS_TASK_RUN", run);
    }
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn always_names_a_relay_socket() {
        // Read-only: `wait_ssh_agent`'s tests set the override concurrently,
        // and it names an `ssh-agent.sock` too.
        let found = discover_ssh_auth_sock().unwrap();
        assert!(found.ends_with("ssh-agent.sock"), "{found}");
    }

    fn env_of(cmd: &Command, key: &str) -> Option<String> {
        cmd.get_envs()
            .find(|(k, _)| *k == key)
            .and_then(|(_, v)| v.map(|v| v.to_string_lossy().into_owned()))
    }

    #[test]
    fn relay_command_sets_socket_and_run_tag_only_inside_a_run() {
        let mut outside = Command::new("true");
        relay_command(&mut outside);
        assert!(env_of(&outside, "SSH_AUTH_SOCK").is_some_and(|s| s.ends_with("ssh-agent.sock")));
        assert_eq!(env_of(&outside, "BEAMS_TASK_RUN"), None);

        let inside = crate::executor::with_task_run(Some("manifest-3/clone".into()), || {
            let mut cmd = Command::new("true");
            relay_command(&mut cmd);
            cmd
        });
        assert_eq!(env_of(&inside, "BEAMS_TASK_RUN").as_deref(), Some("manifest-3/clone"));
    }
}
