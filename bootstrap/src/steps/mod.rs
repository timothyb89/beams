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

/// A single idempotent bootstrap action. Steps are safe to re-run on a beam
/// that's already partially (or fully) bootstrapped.
pub trait Step {
    fn id(&self) -> &'static str;

    /// Returns `Ok(Some(message))` on success, `Ok(None)` when the step had
    /// nothing to do (emits `Skip`), and `Err` on failure.
    fn run(&self, repo: &Path) -> anyhow::Result<Option<String>>;
}

/// The fixed, ordered list of bootstrap steps run against a repo checkout.
pub fn all() -> Vec<Box<dyn Step>> {
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
    for step in all() {
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
