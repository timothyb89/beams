//! The workstation-owned startup manifest and its SSH key grant (see
//! `docs/designs/SSH_AGENT_PROXY_DESIGN.md`, "Client-owned startup manifest").
//!
//! `beamsd` submits `[startup] tasks` to each Beam once, the first time
//! its bridge connects. If `[startup.ssh_grant]` is set, the clones that
//! manifest runs may use the SSH key without a prompt, within limits the
//! workstation enforces:
//!
//! - only user-auth logins to a *verified* host named in `hosts`, as `user`;
//! - at most `max_uses` signatures, within `window_secs`;
//! - only until the manifest finishes, the bridge drops, or the user
//!   revokes it;
//! - only for requests the Beam attributes to that manifest's tasks
//!   (`BEAMS_TASK_RUN`). That part is advisory: it keeps an agent's own
//!   `git clone` during the window from being silently approved, but it's
//!   the host/user/count/time limits that bound what a compromised Beam
//!   could do. Every use shows a passive toast.

use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::time::Duration;

use anyhow::Context;
use beams_protocol::{ManifestParam, ManifestTask};
use serde::{Deserialize, Serialize};

use crate::agent::{AgentOperation, AgentPrompt, HostCheck};

const DEFAULT_GRANT_WINDOW: Duration = Duration::from_secs(10 * 60);
const MAX_GRANT_WINDOW: Duration = Duration::from_secs(60 * 60);
/// Headroom over one signature per clone (a retry, a submodule).
const GRANT_SPARE_USES: u32 = 2;

/// `[startup]` in `config.toml`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StartupConfig {
    /// Tasks for every new Beam, as `{ template = "...", params = {...} }`
    /// (see `beam-bootstrap list-tasks`). Default: the Go and Rust
    /// toolchains.
    #[serde(default = "default_tasks")]
    pub tasks: Vec<ManifestTask>,
    #[serde(default)]
    pub ssh_grant: Option<GrantConfig>,
}

impl Default for StartupConfig {
    fn default() -> Self {
        Self { tasks: default_tasks(), ssh_grant: None }
    }
}

fn default_tasks() -> Vec<ManifestTask> {
    ["install-toolchain-go", "install-toolchain-rust"]
        .into_iter()
        .map(|template| ManifestTask { template: template.to_string(), params: Default::default() })
        .collect()
}

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

/// `[startup.ssh_grant]`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GrantConfig {
    /// Host names as your `known_hosts` spells them (`github.com`,
    /// `[host]:2222`).
    pub hosts: Vec<String>,
    #[serde(default = "default_user")]
    pub user: String,
    #[serde(default)]
    pub window_secs: Option<u64>,
    /// Default: one per ssh-style `clone-git-repo` task, plus two.
    #[serde(default)]
    pub max_uses: Option<u32>,
}

fn default_user() -> String {
    "git".to_string()
}

impl GrantConfig {
    pub fn window(&self) -> Duration {
        self.window_secs.map(Duration::from_secs).unwrap_or(DEFAULT_GRANT_WINDOW).min(MAX_GRANT_WINDOW)
    }

    pub fn max_uses_for(&self, tasks: &[ManifestTask]) -> u32 {
        self.max_uses.unwrap_or_else(|| {
            let clones = tasks
                .iter()
                .filter(|task| task.template == "clone-git-repo")
                .filter(|task| {
                    matches!(task.params.get("repo"), Some(ManifestParam::Str(repo))
                        if repo.starts_with("git@") || repo.starts_with("ssh://"))
                })
                .count() as u32;
            clones + GRANT_SPARE_USES
        })
    }
}

impl StartupConfig {
    /// The tasks to submit: `tasks`, plus a leading `wait-ssh-agent` when an
    /// ssh-style clone needs the key and none was listed (`clone-git-repo`
    /// requires what it provides).
    pub fn manifest_tasks(&self) -> Vec<ManifestTask> {
        let needs_key = self.tasks.iter().any(|task| {
            task.template == "clone-git-repo"
                && matches!(task.params.get("repo"), Some(ManifestParam::Str(repo))
                    if repo.starts_with("git@") || repo.starts_with("ssh://"))
        });
        let has_wait = self
            .tasks
            .iter()
            .any(|task| task.template == "wait-ssh-agent" || task.template == "wait-ssh-agent-unlock");
        let mut tasks = Vec::with_capacity(self.tasks.len() + 1);
        if needs_key && !has_wait {
            tasks.push(ManifestTask { template: "wait-ssh-agent".to_string(), params: Default::default() });
        }
        tasks.extend(self.tasks.iter().cloned());
        tasks
    }
}

#[derive(Debug, Default, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    startup: Option<StartupConfig>,
}

impl StartupConfig {
    pub fn load() -> anyhow::Result<Self> {
        Self::load_from(&crate::agent::Config::path()?)
    }

    pub fn load_from(path: &Path) -> anyhow::Result<Self> {
        match fs::read_to_string(path) {
            Ok(text) => Self::parse(&text).with_context(|| format!("parsing {}", path.display())),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let file: ConfigFile = toml::from_str(text)?;
        Ok(file.startup.unwrap_or_default())
    }
}

/// A live (or just-ended) startup grant, as shown in the menu.
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

    /// A launch's grant (`manifest_id` is then the launch ID; startup
    /// manifests are `manifest-N`).
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

    #[test]
    fn config_defaults_and_use_budget() {
        let config = StartupConfig::parse("").unwrap();
        assert_eq!(config, StartupConfig::default());
        assert_eq!(config.tasks.len(), 2);

        let config = StartupConfig::parse(
            r#"
            [startup]
            tasks = [
              { template = "clone-git-repo", params = { repo = "git@github.com:o/a.git", dest = "~/a" } },
              { template = "clone-git-repo", params = { repo = "https://example.com/b.git", dest = "~/b" } },
              { template = "checkout-pr-worktree", params = { base = "~/a", pr = 12, dest = "~/w" } },
            ]
            [startup.ssh_grant]
            hosts = ["github.com"]
            window_secs = 99999
            "#,
        )
        .unwrap();
        assert_eq!(config.tasks[2].params.get("pr"), Some(&ManifestParam::Int(12)));
        let grant = config.ssh_grant.clone().unwrap();
        assert_eq!(grant.user, "git");
        assert_eq!(grant.window(), MAX_GRANT_WINDOW);
        assert_eq!(grant.max_uses_for(&config.tasks), 1 + GRANT_SPARE_USES);
        let submitted = config.manifest_tasks();
        assert_eq!(submitted[0].template, "wait-ssh-agent", "added for the ssh clone");
        assert_eq!(submitted.len(), 4);
        assert_eq!(StartupConfig::default().manifest_tasks().len(), 2, "nothing added without ssh clones");
        assert!(StartupConfig::parse("[startup]\ntask = []\n").is_err(), "typos are errors");
        assert!(StartupConfig::parse("[startup]\ntasks = []\n").unwrap().tasks.is_empty());
    }
}
