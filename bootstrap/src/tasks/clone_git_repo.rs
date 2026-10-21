use std::borrow::Cow;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context;

use super::{run, Task, Template};
use crate::graph::Capability;

/// Clones `repo` to `dest` with a plain `git clone` - no assumptions
/// about what kind of repo it is (GitHub, a beam, anything `git` itself
/// can reach). Skips if `dest` is already a git checkout.
///
/// Doesn't yet declare a `requires` on ssh-agent being unlocked for an
/// ssh:// repo URL - there's no step/task in this binary that *provides*
/// that capability yet (`bin/ssh-agent-init` runs outside this binary
/// entirely, see `AGENTS.md`). Revisit once something here can represent
/// that precondition.
pub struct CloneGitRepo {
    pub repo: String,
    pub dest: PathBuf,
}

impl CloneGitRepo {
    /// The capability a successful clone to `dest` provides - exposed so
    /// other tasks (e.g. `checkout_pr_worktree`) can `require` it without
    /// duplicating the naming scheme.
    pub fn capability_for(dest: &Path) -> Capability {
        Cow::Owned(format!("repo-cloned:{}", dest.display()))
    }
}

impl Task for CloneGitRepo {
    fn id(&self) -> String {
        format!("clone-git-repo:dest={}", self.dest.display())
    }

    fn provides(&self) -> Vec<Capability> {
        vec![Self::capability_for(&self.dest)]
    }

    fn run(&self) -> anyhow::Result<Option<String>> {
        if self.dest.join(".git").exists() {
            return Ok(None);
        }

        if let Some(parent) = self.dest.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }

        run(Command::new("git").args(["clone", &self.repo, &self.dest.display().to_string()]))?;

        Ok(Some(format!("cloned {} into {}", self.repo, self.dest.display())))
    }
}

impl Template for CloneGitRepo {
    const NAME: &'static str = "clone-git-repo";
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

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

    /// A minimal local repo with one commit, usable as a clone source
    /// without any network access.
    fn make_source_repo(dir: &Path) {
        fs::create_dir_all(dir).unwrap();
        git(dir, &["init", "-q", "-b", "main"]);
        git(dir, &["config", "user.email", "test@example.com"]);
        git(dir, &["config", "user.name", "test"]);
        fs::write(dir.join("README.md"), "hello\n").unwrap();
        git(dir, &["add", "README.md"]);
        git(dir, &["commit", "-q", "-m", "initial"]);
    }

    #[test]
    fn clones_then_skips_on_rerun() {
        let tmp = std::env::temp_dir().join(format!("clone-git-repo-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let source = tmp.join("source");
        let dest = tmp.join("dest");
        make_source_repo(&source);

        let task = CloneGitRepo { repo: source.display().to_string(), dest: dest.clone() };

        let first = task.run().expect("first clone should succeed");
        assert!(first.is_some(), "first run should report work done, not a skip");
        assert!(dest.join("README.md").exists());
        assert!(dest.join(".git").exists());

        let second = task.run().expect("second run should succeed");
        assert!(second.is_none(), "re-run against an existing checkout should skip");

        assert_eq!(
            task.provides(),
            vec![CloneGitRepo::capability_for(&dest)],
            "provided capability should match capability_for(dest)"
        );

        let _ = fs::remove_dir_all(&tmp);
    }
}
