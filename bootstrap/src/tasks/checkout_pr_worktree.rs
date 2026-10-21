use std::borrow::Cow;
use std::path::PathBuf;
use std::process::Command;

use anyhow::bail;

use super::clone_git_repo::CloneGitRepo;
use super::{Task, Template};
use crate::graph::Capability;
use crate::releaseutil::run;

/// Fetches PR `pr`'s head ref from `base`'s `origin` remote and checks it
/// out into a new worktree at `dest`, as a separate working directory
/// alongside `base`'s own checkout (`git worktree`, not a second clone).
///
/// Requires `base` to already be a git checkout with an `origin` remote
/// reachable for the `pull/<pr>/head` ref - i.e. a prior `CloneGitRepo`
/// task against `base`. That's declared via `requires`
/// (`CloneGitRepo::capability_for(base)`), but - per this module's doc
/// comment - only checked by `run()` itself for now, not by a graph
/// resolver, since there's no multi-task graph driving these yet.
///
/// Only fetches on first checkout (skips if `dest` already exists) - a
/// second PR update to an existing worktree isn't handled yet.
pub struct CheckoutPrWorktree {
    pub base: PathBuf,
    pub pr: u32,
    pub dest: PathBuf,
}

impl Task for CheckoutPrWorktree {
    fn id(&self) -> String {
        format!("checkout-pr-worktree:dest={}", self.dest.display())
    }

    fn provides(&self) -> Vec<Capability> {
        vec![Cow::Owned(format!("pr-worktree:{}", self.dest.display()))]
    }

    fn requires(&self) -> Vec<Capability> {
        vec![CloneGitRepo::capability_for(&self.base)]
    }

    fn run(&self) -> anyhow::Result<Option<String>> {
        if self.dest.join(".git").exists() {
            return Ok(None);
        }
        if !self.base.join(".git").exists() {
            bail!(
                "{} is not a git checkout - run clone-git-repo against it first",
                self.base.display()
            );
        }

        let branch = format!("pr-{}", self.pr);
        let refspec = format!("pull/{}/head:{}", self.pr, branch);
        run(Command::new("git").current_dir(&self.base).args(["fetch", "origin", &refspec]))?;
        run(Command::new("git").current_dir(&self.base).args([
            "worktree",
            "add",
            &self.dest.display().to_string(),
            &branch,
        ]))?;

        Ok(Some(format!(
            "checked out PR #{} into {} (branch {branch})",
            self.pr,
            self.dest.display()
        )))
    }
}

impl Template for CheckoutPrWorktree {
    const NAME: &'static str = "checkout-pr-worktree";
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn git(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("running git {args:?}: {e}"));
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// An "origin" repo with a main branch commit, plus a second commit
    /// stashed under `refs/pull/<pr>/head` - the same shape GitHub
    /// exposes a PR's head as, without needing any network access.
    fn make_origin_with_pr(dir: &Path, pr: u32) {
        fs::create_dir_all(dir).unwrap();
        git(dir, &["init", "-q", "-b", "main"]);
        git(dir, &["config", "user.email", "test@example.com"]);
        git(dir, &["config", "user.name", "test"]);
        fs::write(dir.join("README.md"), "main\n").unwrap();
        git(dir, &["add", "README.md"]);
        git(dir, &["commit", "-q", "-m", "initial"]);

        git(dir, &["checkout", "-q", "-b", "feature"]);
        fs::write(dir.join("README.md"), "pr change\n").unwrap();
        git(dir, &["commit", "-q", "-am", "pr change"]);
        git(dir, &["update-ref", &format!("refs/pull/{pr}/head"), "feature"]);
        git(dir, &["checkout", "-q", "main"]);
        git(dir, &["branch", "-D", "feature"]);
    }

    #[test]
    fn checks_out_pr_into_worktree_then_skips_on_rerun() {
        let tmp =
            std::env::temp_dir().join(format!("checkout-pr-worktree-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let origin = tmp.join("origin");
        let base = tmp.join("base");
        let dest = tmp.join("pr-worktree");
        make_origin_with_pr(&origin, 7);

        CloneGitRepo { repo: origin.display().to_string(), dest: base.clone() }
            .run()
            .expect("base clone should succeed");

        let task = CheckoutPrWorktree { base: base.clone(), pr: 7, dest: dest.clone() };

        let first = task.run().expect("checkout should succeed");
        assert!(first.is_some());
        assert_eq!(fs::read_to_string(dest.join("README.md")).unwrap(), "pr change\n");
        assert!(dest.join(".git").exists());

        let second = task.run().expect("re-run should succeed");
        assert!(second.is_none(), "re-run against an existing worktree should skip");

        assert_eq!(task.requires(), vec![CloneGitRepo::capability_for(&base)]);

        let _ = fs::remove_dir_all(&tmp);
    }
}
