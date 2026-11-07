mod apt;
mod claude;
mod claude_md;
mod daemon_service;
mod default_shell;
mod k3s_bot;
mod kubeconfig;
mod kubectl;
mod link;
mod linkutil;
mod shell_config;
mod starship;
mod tbot_service;
mod teleport_bin;
mod tool_paths;
mod zellij;

pub(crate) use zellij::InstallZellij;

use std::path::{Path, PathBuf};

use crate::events::{Emitter, Event, Status};
use crate::executor;
use crate::graph::{self, Capability, Node};
use crate::tasks::Task;

/// Capability names provided/required by the steps below - see
/// `docs/designs/DESIGN.md`'s "Task model". Centralized so dependent steps reference
/// the same constant rather than retyping a string.
pub mod cap {
    // Plain `&'static str`, not `Capability` (`Cow<'static, str>`) - call
    // sites convert with `.into()`. Keeping these as bare strings means a
    // step can still compare/format them as ordinary string constants
    // without unwrapping a `Cow` first.
    pub const DOTFILES_LINKED: &str = "dotfiles-linked";
    pub const CLAUDE_MD_MERGED: &str = "claude-md-merged";
    pub const APT_PACKAGES_INSTALLED: &str = "apt-packages-installed";
    pub const KUBECTL_INSTALLED: &str = "kubectl-installed";
    pub const ZELLIJ_INSTALLED: &str = "zellij-installed";
    pub const SHELL_CONFIG_LINKED: &str = "shell-config-linked";
    pub const STARSHIP_INSTALLED: &str = "starship-installed";
    pub const DEFAULT_SHELL_SET: &str = "default-shell-set";
    pub const TELEPORT_BIN_INSTALLED: &str = "teleport-bin-installed";
    pub const K3S_BOT_JOINED: &str = "k3s-bot-joined";
    pub const TBOT_SERVICE_RUNNING: &str = "tbot-service-running";
    pub const KUBECONFIG_LINKED: &str = "kubeconfig-linked";
    pub const CLAUDE_UPDATED: &str = "claude-updated";
    pub const BOOTSTRAP_DAEMON_RUNNING: &str = "bootstrap-daemon-running";
    pub const TOOL_PATHS_LINKED: &str = "tool-paths-linked";
}

/// Named resources a step's `run()` holds exclusively while running - see
/// `Step::exclusive_resources`. Distinct from `cap`: these aren't facts
/// that become permanently true, just runtime mutual exclusion against
/// other steps that touch the same underlying system resource.
pub mod resource {
    /// `apt-install` and `install-kubectl` both shell out to
    /// `apt-get`/`dpkg`, which don't tolerate two concurrent invocations
    /// (lock contention) - neither declares a capability dependency on
    /// the other (there isn't one), so without this tag the concurrent
    /// executor would happily start both at once and one would likely
    /// fail outright. Only serializes within one `executor::run` call
    /// (one process), though - see `crate::aptlock` for the
    /// cross-process version of this same lock, which every actual
    /// `apt-get` invocation goes through regardless of this tag.
    pub const APT: &str = "apt";
}

/// A single idempotent bootstrap action. Steps are safe to re-run on a beam
/// that's already partially (or fully) bootstrapped.
///
/// `provides`/`requires` make a step a `graph::Node`: dependencies are
/// declared by capability, not by naming another step directly (see
/// `docs/designs/DESIGN.md`). Steps with no dependents/dependencies just leave both at
/// their default empty slice.
///
/// `Send` so a `Box<dyn Step>`, adapted into a `Task` via `StepAsTask`,
/// can be handed off to whichever thread `executor::run` runs it
/// on.
pub trait Step: Send {
    fn id(&self) -> &'static str;

    /// Returns `Ok(Some(message))` on success, `Ok(None)` when the step had
    /// nothing to do (emits `Skip`), and `Err` on failure.
    fn run(&self, repo: &Path) -> anyhow::Result<Option<String>>;

    /// See `tasks::Task::run_with_progress` - same idea, one layer down.
    /// Defaults to ignoring `progress` and delegating to `run`; no
    /// existing step overrides this yet, but `StepAsTask` already relays
    /// it so one could without another adapter change.
    fn run_with_progress(&self, repo: &Path, _progress: &dyn Fn(&str)) -> anyhow::Result<Option<String>> {
        self.run(repo)
    }

    fn provides(&self) -> Vec<Capability> {
        Vec::new()
    }

    fn requires(&self) -> Vec<Capability> {
        Vec::new()
    }

    /// See `tasks::Task::exclusive_resources` - same idea, one layer
    /// down. Checked only by the concurrent executor (`run_concurrent`
    /// below, via `StepAsTask`), not by anything in this trait itself.
    fn exclusive_resources(&self) -> Vec<Capability> {
        Vec::new()
    }
}

/// Adapts a `Step` (repo-path-parameterized, fixed at compile time) into
/// a `tasks::Task` (no-param `run()`, built at runtime) so the fixed
/// startup sequence can share one graph/executor with templated task
/// instances - see `executor::run`'s module doc comment. This is what
/// resolves the "`Step` and `Task` can't be ordered/run together" gap
/// noted when the daemon skeleton first landed: it turns out they didn't
/// need unifying into one trait, just a thin adapter from one to the
/// other.
struct StepAsTask {
    step: Box<dyn Step>,
    repo: PathBuf,
}

impl Task for StepAsTask {
    fn id(&self) -> String {
        self.step.id().to_string()
    }

    fn run(&self) -> anyhow::Result<Option<String>> {
        self.step.run(&self.repo)
    }

    fn run_with_progress(&self, progress: &dyn Fn(&str)) -> anyhow::Result<Option<String>> {
        self.step.run_with_progress(&self.repo, progress)
    }

    fn provides(&self) -> Vec<Capability> {
        self.step.provides()
    }

    fn requires(&self) -> Vec<Capability> {
        self.step.requires()
    }

    fn exclusive_resources(&self) -> Vec<Capability> {
        self.step.exclusive_resources()
    }
}

impl Node for Box<dyn Step> {
    fn node_id(&self) -> String {
        self.id().to_string()
    }

    fn provides(&self) -> Vec<Capability> {
        (**self).provides()
    }

    fn requires(&self) -> Vec<Capability> {
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
        Box::new(tool_paths::LinkToolPaths),
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
        Box::new(daemon_service::StartBootstrapDaemon),
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

/// Runs every step against `repo`, emitting progress through `emitter`,
/// exploiting concurrency wherever the capability graph allows it (see
/// `executor::run`) - e.g. `apt-install` and `install-teleport-bin` no
/// longer wait on each other just because they're both in one list.
/// A failed step blocks only the steps that depend on it; unrelated steps
/// keep going. Returns `true` if every step succeeded (including skips).
pub fn run_all(repo: &Path, emitter: &dyn Emitter) -> bool {
    run(repo, emitter, None)
}

/// Runs just the step with the given id, directly and synchronously - no
/// concurrency, no waiting on other steps (used by `bin/beam-shell` to get
/// `~/.ssh`/`~/bin` in place before anything else, which needs to happen
/// immediately, not whenever the scheduler gets to it). Still
/// validates the *whole* step graph first, via `resolved()`, same as
/// always - a broken capability declaration is a bug worth catching even
/// when only running one step in isolation. Returns `true` if no step id
/// matches, same as an empty run.
pub fn run_only(repo: &Path, emitter: &dyn Emitter, step_id: &str) -> bool {
    run(repo, emitter, Some(step_id))
}

fn run(repo: &Path, emitter: &dyn Emitter, only: Option<&str>) -> bool {
    match only {
        Some(step_id) => run_single(repo, emitter, step_id),
        None => run_concurrent(repo, emitter),
    }
}

fn run_single(repo: &Path, emitter: &dyn Emitter, step_id: &str) -> bool {
    for step in resolved() {
        if step.id() != step_id {
            continue;
        }
        emitter.emit(Event::new(step.id(), Status::Start, None));
        return match step.run(repo) {
            Ok(Some(message)) => {
                emitter.emit(Event::new(step.id(), Status::Ok, Some(message)));
                true
            }
            Ok(None) => {
                emitter.emit(Event::new(step.id(), Status::Skip, None));
                true
            }
            Err(err) => {
                emitter.emit(Event::new(step.id(), Status::Error, Some(err.to_string())));
                false
            }
        };
    }
    true // no step id matches - same as an empty run
}

fn run_concurrent(repo: &Path, emitter: &dyn Emitter) -> bool {
    let tasks: Vec<Box<dyn Task>> = all()
        .into_iter()
        .map(|step| Box::new(StepAsTask { step, repo: repo.to_path_buf() }) as Box<dyn Task>)
        .collect();

    let on_event = |event: executor::Event| {
        let translated = match event {
            executor::Event::Start { task_id } => Event::new(&task_id, Status::Start, None),
            // `events::Status` has no in-progress state - this fixed
            // sequence's output has always just been start/terminal, and
            // no fixed `Step` overrides `run_with_progress` yet (see that
            // method's doc comment) to actually produce one of these.
            // Dropped rather than forcing a new `Status` variant through
            // `events.rs`'s human/JSON emitters for a case nothing
            // triggers today.
            executor::Event::Progress { .. } => return,
            executor::Event::Ok { task_id, message } => {
                Event::new(&task_id, Status::Ok, Some(message))
            }
            executor::Event::Skip { task_id } => Event::new(&task_id, Status::Skip, None),
            // A step that never started because one it depends on failed
            // - reported as an error (the run as a whole did fail), with
            // a message naming the failed dependency.
            executor::Event::Error { task_id, message } | executor::Event::Blocked { task_id, message } => {
                Event::new(&task_id, Status::Error, Some(message))
            }
        };
        emitter.emit(translated);
    };

    // `tasks` is `all()` adapted 1:1, so this validates the same graph
    // `resolved()` already proves resolvable (see its own doc comment) -
    // `expect` rather than propagating is consistent with that.
    executor::run(tasks, &on_event).expect("bootstrap step dependency graph is invalid")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Once the executor runs independent steps concurrently, exact
    /// positional equality with the original hand-written list is no
    /// longer the right invariant (an unconstrained step like
    /// `claude-update` legitimately moves earlier now, into the same
    /// wave as other no-dependency steps - see the commit that added
    /// this test for the full story). What still has to hold: every real
    /// dependency edge is respected, and resolving is a reordering of
    /// `all()`, not a filter - same 13 ids, no duplicates or omissions.
    #[test]
    fn resolved_respects_every_known_dependency() {
        let order: Vec<&str> = resolved().iter().map(|s| s.id()).collect();
        let pos = |id: &str| order.iter().position(|&x| x == id).unwrap_or_else(|| panic!("{id} missing from resolved order"));

        assert!(pos("apt-install") < pos("default-shell"), "default-shell needs fish from apt-install");
        assert!(pos("install-teleport-bin") < pos("k3s-bot-join"));
        assert!(pos("install-teleport-bin") < pos("start-tbot"));
        assert!(pos("k3s-bot-join") < pos("start-tbot"));
        assert!(pos("start-tbot") < pos("kubeconfig-symlink"));

        let mut actual_ids: Vec<&str> = order.clone();
        actual_ids.sort();
        let mut expected_ids: Vec<&str> = all().iter().map(|s| s.id()).collect();
        expected_ids.sort();
        assert_eq!(actual_ids, expected_ids);
    }

    /// `apt-install` and `install-kubectl` have no capability dependency
    /// on each other (there isn't one to declare), but both shell out to
    /// `apt-get`/`dpkg` - without this tag, the concurrent executor would
    /// happily run them at once and hit real lock contention. See
    /// `resource::APT`'s doc comment.
    #[test]
    fn apt_touching_steps_declare_the_apt_exclusive_resource() {
        let apt_resource: Vec<Capability> = vec![resource::APT.into()];
        assert_eq!(apt::InstallPackages.exclusive_resources(), apt_resource);
        assert_eq!(kubectl::InstallKubectl.exclusive_resources(), apt_resource);
    }

    /// `StepAsTask` just delegates `provides`/`requires`/
    /// `exclusive_resources` to the wrapped `Step`, so adapting the whole
    /// fixed list into `Task`s and resolving *that* graph should succeed
    /// exactly like `resolved()` resolving the `Step` graph does -
    /// doesn't call `run()` on anything (which would mean real
    /// `apt-get`/network/filesystem side effects in a unit test).
    #[test]
    fn steps_adapted_to_tasks_still_resolve() {
        let repo = PathBuf::from("/nonexistent-but-unused-by-resolve");
        let tasks: Vec<Box<dyn Task>> = all()
            .into_iter()
            .map(|step| Box::new(StepAsTask { step, repo: repo.clone() }) as Box<dyn Task>)
            .collect();
        let levels = graph::resolve_levels(&tasks).expect("adapted graph should resolve");
        assert_eq!(levels.iter().map(Vec::len).sum::<usize>(), tasks.len());
    }
}
