//! SSH key grants for launch tasks (a profile's `[ssh_grant]`; see
//! `docs/development/launches.md`, "SSH key grants").
//!
//! Once a launch's covered phase can start, its SSH tasks may use the key
//! without a prompt, within limits the workstation enforces:
//!
//! - only user-auth logins to a *verified* host named in `hosts`, as `user`;
//! - at most `max_uses` signatures, within `window_secs`;
//! - only until the launch finishes, the bridge drops, or the user
//!   revokes it;
//! - only for requests the Beam attributes to the covered tasks' exact
//!   `BEAMS_TASK_RUN` tags. That part is advisory: it keeps an agent's own
//!   `git clone` during the window from being silently approved, but it's
//!   the host/user/count/time limits that bound what a compromised Beam
//!   could do. Every use shows a passive toast.
//!
//! (The module name predates profiles: grants first came with the
//! workstation's `[startup]` manifest, since removed.)

use serde::{Deserialize, Serialize};

use crate::agent::{AgentOperation, AgentPrompt, HostCheck};

/// A launch's `[ssh_grant]` as compiled: `ssh-grant.json` in the
/// workstation's launch directory, never sent to the Beam (the Beam can't
/// widen it, and nothing regenerates it from Beam state).
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct LaunchGrant {
    pub hosts: Vec<String>,
    pub user: String,
    pub window_secs: u64,
    pub max_uses: u32,
    /// Plan node IDs it covers (`tasks:<id>`).
    pub nodes: Vec<String>,
    /// The earliest phase of a covered node: the window opens when that
    /// phase can start (acceptance for pre, `interface-ready` for post).
    pub phase: beams_protocol::launch::Phase,
}

impl LaunchGrant {
    pub const FILE: &'static str = "ssh-grant.json";

    /// The `BEAMS_TASK_RUN` tags of the covered tasks in launch `launch_id`
    /// (`bootstrap/src/launch/runner.rs`: `<launch>/launch:<launch>:<node>`).
    pub fn task_runs(&self, launch_id: &str) -> Vec<String> {
        self.nodes.iter().map(|node| format!("{launch_id}/launch:{launch_id}:{node}")).collect()
    }
}

/// A live (or just-ended) grant, as shown in the menu.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct GrantRecord {
    pub grant_id: String,
    pub beam_uuid: String,
    pub manifest_id: String,
    pub hosts: Vec<String>,
    pub user: String,
    pub uses: u32,
    pub max_uses: u32,
    pub expires_at_ms: u64,
    /// Why it ended; `None` while it can still be used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended: Option<String>,
}

impl GrantRecord {
    pub fn usable(&self, now_ms: u64) -> bool {
        self.ended.is_none() && self.uses < self.max_uses && now_ms < self.expires_at_ms
    }

    /// Whether this grant approves `prompt` without asking.
    pub fn covers(&self, prompt: &AgentPrompt) -> bool {
        let host_ok = matches!(
            &prompt.operation,
            AgentOperation::Login { user, host: HostCheck::Verified { name } }
                if *user == self.user && self.hosts.iter().any(|host| host == name)
        );
        let run_ok = prompt
            .claimed
            .task_run
            .as_deref()
            .is_some_and(|run| run.starts_with(&format!("{}/", self.manifest_id)));
        host_ok && run_ok
    }

    fn hosts_label(&self) -> String {
        self.hosts.join(", ")
    }

    /// A launch's grant (`manifest_id` is the launch ID). Grants named
    /// `manifest-N` came from the removed `[startup]` manifest; the
    /// wording for them goes once the session menu stops using it.
    pub fn is_launch(&self) -> bool {
        !self.manifest_id.starts_with("manifest-")
    }

    pub fn summary(&self) -> String {
        let kind = if self.is_launch() { "Launch" } else { "Startup" };
        format!("{kind}: {} as {} · {} of {} used", self.hosts_label(), self.user, self.uses, self.max_uses)
    }

    pub fn describe(&self, now_ms: u64) -> Vec<(String, bool)> {
        let left = self.expires_at_ms.saturating_sub(now_ms) / 1000;
        let (tasks, finishes, scope) = if self.is_launch() {
            ("launch's SSH tasks", "the launch finishes", "Launch")
        } else {
            ("startup tasks", "startup finishes", "Startup manifest")
        };
        vec![
            (
                format!(
                    "This Beam's {tasks} may sign in to {} as {} without asking.",
                    self.hosts_label(),
                    self.user
                ),
                false,
            ),
            (
                format!(
                    "Used {} of {} times; ends in {}m{:02}s or when {finishes}.",
                    self.uses,
                    self.max_uses,
                    left / 60,
                    left % 60
                ),
                false,
            ),
            (format!("{scope}: {}.", self.manifest_id), false),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Claimed;

    fn prompt(user: &str, host: HostCheck, task_run: Option<&str>) -> AgentPrompt {
        AgentPrompt {
            key_fingerprint: "SHA256:k".into(),
            key_comment: None,
            operation: AgentOperation::Login { user: user.into(), host },
            claimed: Claimed { task_run: task_run.map(str::to_string), ..Default::default() },
            expires_at_ms: 0,
            granted: None,
        }
    }

    fn grant() -> GrantRecord {
        GrantRecord {
            grant_id: "g".into(),
            beam_uuid: "b".into(),
            manifest_id: "manifest-3".into(),
            hosts: vec!["github.com".into()],
            user: "git".into(),
            uses: 0,
            max_uses: 3,
            expires_at_ms: 1_000,
            ended: None,
        }
    }

    #[test]
    fn grants_cover_only_verified_logins_from_their_own_manifest() {
        let g = grant();
        let verified = || HostCheck::Verified { name: "github.com".into() };
        assert!(g.covers(&prompt("git", verified(), Some("manifest-3/clone-git-repo:dest=/x"))));
        // Wrong manifest (prefix trap), no attribution, other user/host, unverified host.
        assert!(!g.covers(&prompt("git", verified(), Some("manifest-30/clone"))));
        assert!(!g.covers(&prompt("git", verified(), None)));
        assert!(!g.covers(&prompt("root", verified(), Some("manifest-3/x"))));
        assert!(!g.covers(&prompt("git", HostCheck::Verified { name: "gitlab.com".into() }, Some("manifest-3/x"))));
        assert!(!g.covers(&prompt("git", HostCheck::Unverified { why: "x".into() }, Some("manifest-3/x"))));
        assert!(!g.covers(&prompt("git", HostCheck::UnknownKey { fingerprint: "f".into() }, Some("manifest-3/x"))));
        let mut signature = prompt("git", verified(), Some("manifest-3/x"));
        signature.operation = AgentOperation::Signature { namespace: "git".into() };
        assert!(!g.covers(&signature));
    }

    #[test]
    fn grants_stop_at_their_limits() {
        let mut g = grant();
        assert!(g.usable(999));
        assert!(!g.usable(1_000), "expired");
        g.uses = 3;
        assert!(!g.usable(0), "used up");
        g.uses = 0;
        g.ended = Some("revoked".into());
        assert!(!g.usable(0));
    }
}
