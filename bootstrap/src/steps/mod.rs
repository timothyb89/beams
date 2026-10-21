mod apt;
mod claude;
mod claude_md;
mod default_shell;
mod k3s_bot;
mod kubeconfig;
mod kubectl;
mod link;
mod linkutil;
mod releaseutil;
mod shell_config;
mod starship;
mod tbot_service;
mod teleport_bin;
mod zellij;

use std::path::Path;

use crate::events::{Emitter, Event, Status};
use crate::graph::{self, Capability, Node};

/// Capability names provided/required by the steps below - see
/// `DESIGN.md`'s "Task model". Centralized so dependent steps reference
/// the same constant rather than retyping a string.
pub mod cap {
    use super::Capability;

    pub const DOTFILES_LINKED: Capability = "dotfiles-linked";
    pub const CLAUDE_MD_MERGED: Capability = "claude-md-merged";
    pub const APT_PACKAGES_INSTALLED: Capability = "apt-packages-installed";
    pub const KUBECTL_INSTALLED: Capability = "kubectl-installed";
    pub const ZELLIJ_INSTALLED: Capability = "zellij-installed";
    pub const SHELL_CONFIG_LINKED: Capability = "shell-config-linked";
    pub const STARSHIP_INSTALLED: Capability = "starship-installed";
    pub const DEFAULT_SHELL_SET: Capability = "default-shell-set";
    pub const TELEPORT_BIN_INSTALLED: Capability = "teleport-bin-installed";
    pub const K3S_BOT_JOINED: Capability = "k3s-bot-joined";
    pub const TBOT_SERVICE_RUNNING: Capability = "tbot-service-running";
    pub const KUBECONFIG_LINKED: Capability = "kubeconfig-linked";
    pub const CLAUDE_UPDATED: Capability = "claude-updated";
}

/// A single idempotent bootstrap action. Steps are safe to re-run on a beam
/// that's already partially (or fully) bootstrapped.
///
/// `provides`/`requires` make a step a `graph::Node`: dependencies are
/// declared by capability, not by naming another step directly (see
/// `DESIGN.md`). Steps with no dependents/dependencies just leave both at
/// their default empty slice.
pub trait Step {
    fn id(&self) -> &'static str;

    /// Returns `Ok(Some(message))` on success, `Ok(None)` when the step had
    /// nothing to do (emits `Skip`), and `Err` on failure.
    fn run(&self, repo: &Path) -> anyhow::Result<Option<String>>;

    fn provides(&self) -> &'static [Capability] {
        &[]
    }

    fn requires(&self) -> &'static [Capability] {
        &[]
    }
}

impl Node for Box<dyn Step> {
    fn node_id(&self) -> &str {
        self.id()
    }

    fn provides(&self) -> &'static [Capability] {
        (**self).provides()
    }

    fn requires(&self) -> &'static [Capability] {
        (**self).requires()
    }
}

/// The bootstrap steps, in declaration order. Declaration order only
/// matters as a tie-break among steps with no dependency relationship to
/// each other - actual run order comes from `resolve_order` below, driven
/// by each step's declared capabilities.
fn all() -> Vec<Box<dyn Step>> {
    vec![
        Box::new(link::LinkFiles),
        Box::new(claude_md::MergeClaudeMd),
        Box::new(apt::InstallPackages),
        Box::new(kubectl::InstallKubectl),
        Box::new(zellij::InstallZellij),
        Box::new(shell_config::LinkShellConfig),
        Box::new(starship::InstallStarship),
        Box::new(default_shell::SetDefaultShell),
        Box::new(teleport_bin::InstallTeleportBin),
        Box::new(k3s_bot::JoinK3sBot),
        Box::new(tbot_service::StartTbotService),
        Box::new(kubeconfig::LinkKubeconfig),
        Box::new(claude::UpdateClaude),
    ]
}

/// `all()`, reordered to satisfy every step's declared `requires`. Panics
/// (via `expect`) on an unresolvable graph - that's a bug in this binary's
/// own step declarations, not something a caller can recover from.
fn resolved() -> Vec<Box<dyn Step>> {
    let steps = all();
    let order = graph::resolve_order(&steps).expect("bootstrap step dependency graph is invalid");
    // `resolve_order` gives us indices into `steps`; consume `steps` via
    // `Option` slots so each boxed step moves out exactly once, in the
    // resolved order, without needing `Step: Clone`.
    let mut slots: Vec<Option<Box<dyn Step>>> = steps.into_iter().map(Some).collect();
    order
        .into_iter()
        .map(|i| slots[i].take().expect("resolve_order repeats an index"))
        .collect()
}

/// Runs every step in order against `repo`, emitting progress through
/// `emitter`. Stops at the first failing step. Returns `true` if every step
/// succeeded (including skips).
pub fn run_all(repo: &Path, emitter: &mut dyn Emitter) -> bool {
    run(repo, emitter, None)
}

/// Runs just the step with the given id (used by `bin/beam-shell` to run
/// `link-files` synchronously before anything that needs `~/.ssh`/`~/bin`
/// in place, ahead of backgrounding the full run). Returns `true` if no
/// step id matches, same as an empty run.
pub fn run_only(repo: &Path, emitter: &mut dyn Emitter, step_id: &str) -> bool {
    run(repo, emitter, Some(step_id))
}

fn run(repo: &Path, emitter: &mut dyn Emitter, only: Option<&str>) -> bool {
    for step in resolved() {
        if only.is_some_and(|id| id != step.id()) {
            continue;
        }
        emitter.emit(Event::new(step.id(), Status::Start, None));
        match step.run(repo) {
            Ok(Some(message)) => {
                emitter.emit(Event::new(step.id(), Status::Ok, Some(message)));
            }
            Ok(None) => {
                emitter.emit(Event::new(step.id(), Status::Skip, None));
            }
            Err(err) => {
                emitter.emit(Event::new(step.id(), Status::Error, Some(err.to_string())));
                return false;
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The capability graph must resolve to exactly the order the steps
    /// were already hand-arranged in - no behavior change from
    /// introducing `graph::resolve_order` on top of the existing list.
    #[test]
    fn resolved_order_matches_original_hand_ordered_list() {
        let expected: Vec<&str> = all().iter().map(|s| s.id()).collect();
        let actual: Vec<&str> = resolved().iter().map(|s| s.id()).collect();
        assert_eq!(actual, expected);
    }
}
