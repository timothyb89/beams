//! Where processes started by `beam-bootstrapd` find the SSH agent.
//!
//! The agent is the daemon's own relay (`agentproxy`): it holds no keys
//! itself and asks the workstation for each signature (see
//! `docs/designs/SSH_AGENT_PROXY_DESIGN.md`). Interactive shells get the same socket as
//! `SSH_AUTH_SOCK` from `bin/beam-shell` and `shell/fish/config.fish`; the
//! daemon's own environment predates those, so tasks that run `git`/`ssh`
//! (`clone_git_repo`, `checkout_pr_worktree`, `wait_ssh_agent`) pass this
//! path explicitly.

use std::path::PathBuf;
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
}
