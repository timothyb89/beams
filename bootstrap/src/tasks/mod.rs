//! Runtime-instantiated, templated tasks - see `DESIGN.md`'s "Templates
//! vs. instances". Unlike `steps::Step` (a fixed, compile-time-known list
//! run once at startup), a `Task` here is produced by *instantiating* a
//! template with concrete parameters at runtime: its id and capabilities
//! are derived from those params rather than being a compile-time
//! constant.
//!
//! There's no daemon/manifest/socket yet to drive these (that's the next
//! slice of the migration plan) - for now they're reachable directly via
//! `beam-bootstrap run-task <template> ...` (see `main.rs`), which
//! instantiates and runs exactly one task, on demand, without resolving
//! it against any wider graph. A template that `requires` another task's
//! capability (e.g. `checkout-pr-worktree` requiring a prior
//! `clone-git-repo`'s `repo-cloned:<path>`) only has that requirement
//! checked by its own `run()` - e.g. by checking the filesystem state the
//! capability implies - not by the graph resolver, until there's an
//! actual multi-task graph (daemon-driven) to resolve it against.

pub mod checkout_pr_worktree;
pub mod clone_git_repo;

use std::process::Command;

use anyhow::{bail, Context};

use crate::graph::{Capability, Node};

/// A task instance, produced by instantiating a template with concrete
/// params (e.g. `clone_git_repo::CloneGitRepo { repo, dest }`).
///
/// Mirrors `steps::Step`'s shape (`id`/`run`/`provides`/`requires`) but
/// with an owned, params-derived `id` and `Capability` values instead of
/// compile-time `&'static str` ones - see the module doc comment.
pub trait Task {
    fn id(&self) -> String;
    fn run(&self) -> anyhow::Result<Option<String>>;

    fn provides(&self) -> Vec<Capability> {
        Vec::new()
    }

    fn requires(&self) -> Vec<Capability> {
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
}
