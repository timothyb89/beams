//! Runtime-instantiated, templated tasks - see `DESIGN.md`'s "Templates
//! vs. instances". Unlike `steps::Step` (a fixed, compile-time-known list
//! run once at startup), a `Task` here is produced by *instantiating* a
//! template with concrete parameters at runtime: its id and capabilities
//! are derived from those params rather than being a compile-time
//! constant.
//!
//! Two ways these get run - see `main.rs`'s `run-task` and `daemon.rs`'s
//! `run_task` op for the first: instantiate and run exactly *one* task
//! on demand, without resolving it against any wider graph. A template
//! that `requires` another task's capability (e.g. `checkout-pr-worktree`
//! requiring a prior `clone-git-repo`'s `repo-cloned:<path>`) only has
//! that requirement checked by its own `run()` there - e.g. by checking
//! the filesystem state the capability implies - not by the graph
//! resolver. That's deliberate: a bare on-demand task is meant for "I
//! already know the prerequisite is satisfied" use.
//!
//! The other way - `daemon.rs`'s `run_manifest` op - *does* resolve
//! several task instances against each other (`graph::resolve_levels`)
//! and runs them concurrently where the graph allows via
//! `executor::run`, which is also what the fixed startup sequence itself
//! runs on (see `steps::StepAsTask`, the adapter that lets a `Step` join
//! the same `Task` graph).

pub mod checkout_pr_worktree;
pub mod clone_git_repo;

use std::process::Command;

use anyhow::{bail, Context};
use serde_json::Value;

use crate::graph::{Capability, Node};
use checkout_pr_worktree::CheckoutPrWorktree;
use clone_git_repo::CloneGitRepo;

/// A task instance, produced by instantiating a template with concrete
/// params (e.g. `clone_git_repo::CloneGitRepo { repo, dest }`).
///
/// Mirrors `steps::Step`'s shape (`id`/`run`/`provides`/`requires`) but
/// with an owned, params-derived `id` and `Capability` values instead of
/// compile-time `&'static str` ones - see the module doc comment.
///
/// `Send` so `executor::run` can hand a task off to whichever thread
/// ends up running its wave.
pub trait Task: Send {
    fn id(&self) -> String;
    fn run(&self) -> anyhow::Result<Option<String>>;

    fn provides(&self) -> Vec<Capability> {
        Vec::new()
    }

    fn requires(&self) -> Vec<Capability> {
        Vec::new()
    }

    /// Named resources this task's `run()` holds exclusively while it's
    /// executing - not a capability (a fact that becomes permanently
    /// true), but mutual exclusion against other tasks that declare the
    /// same resource, enforced only at runtime by `executor::run`, which
    /// is the only thing that needs to care: two tasks with no
    /// capability relationship can still be unsafe to run at the same
    /// time (the canonical example here: `apt-install` and
    /// `install-kubectl` both shell out to `apt-get`/`dpkg`, which don't
    /// tolerate two concurrent invocations - see `steps::resource`).
    fn exclusive_resources(&self) -> Vec<Capability> {
        Vec::new()
    }
}

impl Node for Box<dyn Task> {
    fn node_id(&self) -> String {
        self.id()
    }

    fn provides(&self) -> Vec<Capability> {
        (**self).provides()
    }

    fn requires(&self) -> Vec<Capability> {
        (**self).requires()
    }
}

/// Marks a type as instantiable into a `Task`. The type *is* its own
/// params (e.g. `CloneGitRepo { repo, dest }` directly implements this) -
/// there's no separate params/schema type yet. `NAME` identifies the
/// template itself (stable across instances, e.g. `"clone-git-repo"`),
/// distinct from a specific instance's `Task::id()`.
///
/// This stays a plain Rust-level contract for now, not a dynamic
/// registry with introspectable param schemas - that only becomes
/// necessary once something needs to enumerate templates and build forms
/// or a manifest schema from them (the TUI/daemon work, still ahead - see
/// DESIGN.md's "Seeding manifest"). Designing that now, with exactly two
/// example templates and no real consumer yet, would be guessing at a
/// shape instead of discovering it from use.
pub trait Template: Task {
    const NAME: &'static str;
}

/// Every template's name, in a stable (if arbitrary) order - what
/// `beam-bootstrap list-tasks` and the daemon's `list_tasks` op both
/// enumerate.
pub fn names() -> &'static [&'static str] {
    &[CloneGitRepo::NAME, CheckoutPrWorktree::NAME]
}

/// Builds a task instance from a template name plus its params as JSON -
/// the one instantiation path shared by the CLI (`run-task`, see
/// `main.rs`) and the daemon's `run_task` op, so both go through the same
/// params-parsing/validation rather than drifting into two copies of it.
///
/// Errors on an unknown template name or params that don't match what
/// the named template expects (missing/mistyped fields).
pub fn instantiate(name: &str, params: Value) -> anyhow::Result<Box<dyn Task>> {
    match name {
        CloneGitRepo::NAME => {
            #[derive(serde::Deserialize)]
            struct Params {
                repo: String,
                dest: String,
            }
            let p: Params = serde_json::from_value(params)
                .with_context(|| format!("parsing params for {name:?}"))?;
            Ok(Box::new(CloneGitRepo { repo: p.repo, dest: crate::pathutil::expand_tilde(&p.dest) }))
        }
        CheckoutPrWorktree::NAME => {
            #[derive(serde::Deserialize)]
            struct Params {
                base: String,
                pr: u32,
                dest: String,
            }
            let p: Params = serde_json::from_value(params)
                .with_context(|| format!("parsing params for {name:?}"))?;
            Ok(Box::new(CheckoutPrWorktree {
                base: crate::pathutil::expand_tilde(&p.base),
                pr: p.pr,
                dest: crate::pathutil::expand_tilde(&p.dest),
            }))
        }
        other => bail!("unknown template {other:?} (known: {})", names().join(", ")),
    }
}

/// Runs a `Command`, bailing with its stderr on a non-zero exit. Shared
/// by the task implementations below - same pattern as
/// `steps::releaseutil::run`, just not reachable from here (this module
/// isn't nested under `steps`).
pub(crate) fn run(cmd: &mut Command) -> anyhow::Result<()> {
    let output = cmd.output().with_context(|| format!("running {cmd:?}"))?;
    if !output.status.success() {
        bail!(
            "{:?} failed ({}): {}",
            cmd,
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::checkout_pr_worktree::CheckoutPrWorktree;
    use super::clone_git_repo::CloneGitRepo;
    use super::{Task, Template};

    /// Every template's generated instance id should be namespaced under
    /// its own `NAME` - keeps the two from drifting if one changes
    /// without the other.
    #[test]
    fn instance_ids_are_namespaced_under_their_template_name() {
        let clone = CloneGitRepo { repo: "x".into(), dest: PathBuf::from("/tmp/x") };
        assert!(clone.id().starts_with(&format!("{}:", CloneGitRepo::NAME)));

        let checkout = CheckoutPrWorktree {
            base: PathBuf::from("/tmp/base"),
            pr: 1,
            dest: PathBuf::from("/tmp/dest"),
        };
        assert!(checkout.id().starts_with(&format!("{}:", CheckoutPrWorktree::NAME)));
    }

    #[test]
    fn instantiate_builds_the_right_task_from_json_params() {
        let task = super::instantiate(
            CloneGitRepo::NAME,
            serde_json::json!({"repo": "https://example.invalid/x.git", "dest": "~/x"}),
        )
        .unwrap();
        assert_eq!(task.id(), format!("{}:dest={}/x", CloneGitRepo::NAME, std::env::var("HOME").unwrap()));
    }

    #[test]
    fn instantiate_rejects_an_unknown_template() {
        let err = super::instantiate("not-a-real-template", serde_json::json!({}))
            .err()
            .expect("unknown template should be an error");
        assert!(err.to_string().contains("not-a-real-template"));
    }

    #[test]
    fn instantiate_rejects_params_missing_a_required_field() {
        let err = super::instantiate(CloneGitRepo::NAME, serde_json::json!({"repo": "x"}))
            .err()
            .expect("missing dest should be an error");
        assert!(err.to_string().contains(CloneGitRepo::NAME));
    }
}
