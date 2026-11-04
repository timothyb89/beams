use std::borrow::Cow;
use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context;

use super::{Task, Template};
use crate::graph::Capability;
use crate::releaseutil::run_streaming;
use crate::sshagent::discover_ssh_auth_sock;

/// Clones `repo` to `dest` with a plain `git clone` - no assumptions
/// about what kind of repo it is (GitHub, a beam, anything `git` itself
/// can reach). Skips if `dest` is already a git checkout.
///
/// For an ssh-style repo URL (`git@...`/`ssh://...`), `requires` the
/// `wait_ssh_agent_unlock` task's capability - `bin/ssh-agent-init`'s
/// actual interactive unlock still runs entirely outside this binary
/// (see `AGENTS.md`), but that task can at least wait for it to have
/// happened before this one tries to use the key. An https:// or local
/// filesystem `repo` has no such requirement.
///
/// Waiting for the unlock isn't enough on its own, though: `run()` also
/// passes `git` an explicit `SSH_AUTH_SOCK` via `sshagent`, for the same
/// reason `wait_ssh_agent_unlock` needs it - this runs inside
/// `beam-bootstrapd`, whose own environment never has one (see
/// `sshagent`'s module doc comment). Found live, one task after fixing
/// `wait_ssh_agent_unlock` itself: it correctly reported the agent
/// unlocked, then the clone right after it failed anyway with `git@
/// github.com: Permission denied (publickey)` - `git`'s own `ssh`
/// subprocess had no socket to find the now-unlocked key through either.
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

    fn needs_ssh_agent(&self) -> bool {
        self.repo.starts_with("git@") || self.repo.starts_with("ssh://")
    }
}

impl Task for CloneGitRepo {
    fn id(&self) -> String {
        format!("clone-git-repo:dest={}", self.dest.display())
    }

    fn provides(&self) -> Vec<Capability> {
        vec![Self::capability_for(&self.dest)]
    }

    fn requires(&self) -> Vec<Capability> {
        if self.needs_ssh_agent() {
            vec![super::cap::SSH_AGENT_UNLOCKED.into()]
        } else {
            Vec::new()
        }
    }

    fn run(&self) -> anyhow::Result<Option<String>> {
        self.run_with_progress(&|_| {})
    }

    fn run_with_progress(&self, progress: &dyn Fn(&str)) -> anyhow::Result<Option<String>> {
        if self.dest.join(".git").exists() {
            return Ok(None);
        }

        if let Some(parent) = self.dest.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }

        // `--progress` makes git report percent-complete even when it
        // isn't talking to a terminal (which it otherwise suppresses).
        let mut cmd = Command::new("git");
        cmd.args(["clone", "--progress", &self.repo, &self.dest.display().to_string()]);
        if self.needs_ssh_agent() {
            if let Some(sock) = discover_ssh_auth_sock() {
                cmd.env("SSH_AUTH_SOCK", sock);
            }
        }
        // Lets the workstation match this clone's key use to the startup
        // manifest that asked for it (see `executor::current_task_run`).
        if let Some(run) = crate::executor::current_task_run() {
            cmd.env("BEAMS_TASK_RUN", run);
        }
        // Reduces each raw update (`run_streaming` already splits git's
        // own `\r`-delimited ticks into one call per tick - see that
        // function's doc comment) down to just the phase name and
        // percentage, dropping the object-count/transfer-rate noise
        // (e.g. "remote: Counting objects:  43% (430/1000)" becomes
        // "Counting objects: 43%") - a line with no percentage at all
        // (the initial "Cloning into '...'" or a final "done.") is
        // forwarded as-is rather than dropped, since those still mark a
        // real phase transition worth showing.
        //
        // `last_summary` dedupes consecutive identical summaries - a
        // real clone of a large repo (confirmed live against
        // gravitational/core) emits the *same* percentage several times
        // in a row (e.g. a dozen `Receiving objects: 21%` updates back
        // to back, since several individual objects can round to the
        // same percent), which would otherwise mean a dozen identical
        // broadcasts/TUI-row updates for one real change in progress.
        let last_summary = RefCell::new(None::<String>);
        run_streaming(&mut cmd, &|line| {
            if let Some(message) = dedup_progress(&last_summary, line) {
                progress(&message);
            }
        })?;

        Ok(Some(format!("cloned {} into {}", self.repo, self.dest.display())))
    }
}

impl Template for CloneGitRepo {
    const NAME: &'static str = "clone-git-repo";
}

/// Reduces one raw `git clone --progress` update to `"<phase>: <N>%"`,
/// dropping everything else on the line (object counts, transfer rate,
/// the `remote: ` prefix a line forwarded from the remote side carries).
/// `None` if `line` doesn't contain a percentage at all (e.g. `"Cloning
/// into '...'"`. `"Resolving deltas: 100% (5/5), done."` or `"done."`).
///
/// Finds the first run of ASCII digits immediately followed by `%`,
/// rather than assuming a fixed line shape - git's own phase names
/// ("Counting objects", "Compressing objects", "Receiving objects",
/// "Resolving deltas") aren't otherwise enumerated here, so a future
/// phase this hasn't seen yet still summarizes correctly.
fn summarize_progress(line: &str) -> Option<String> {
    let line = line.trim();
    let percent_end = line.find('%')?;
    let digits_start = line[..percent_end].rfind(|c: char| !c.is_ascii_digit()).map(|i| i + 1).unwrap_or(0);
    let percent = &line[digits_start..percent_end];
    if percent.is_empty() {
        return None;
    }

    let phase = line[..digits_start].trim().trim_end_matches(':').trim();
    let phase = phase.strip_prefix("remote:").map(str::trim).unwrap_or(phase);

    Some(format!("{phase}: {percent}%"))
}

/// Summarizes `line` via `summarize_progress`, then drops it (returning
/// `None`) if it's identical to the last *summarized* line this `last`
/// cell has seen - see `run_with_progress`'s call site for why that
/// matters. A line with no percentage at all is never deduped (and
/// doesn't update `last`): those are rare, one-off phase-transition
/// markers ("Cloning into '...'", "done."), not a repeating tick.
fn dedup_progress(last: &RefCell<Option<String>>, line: &str) -> Option<String> {
    let Some(summary) = summarize_progress(line) else {
        return Some(line.to_string());
    };
    let mut last = last.borrow_mut();
    if last.as_deref() == Some(summary.as_str()) {
        return None;
    }
    *last = Some(summary.clone());
    Some(summary)
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

    #[test]
    fn run_with_progress_reports_at_least_one_line_and_matches_runs_final_message() {
        let tmp = std::env::temp_dir().join(format!("clone-git-repo-progress-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let source = tmp.join("source");
        let dest = tmp.join("dest");
        make_source_repo(&source);

        let task = CloneGitRepo { repo: source.display().to_string(), dest: dest.clone() };
        let lines = std::sync::Mutex::new(Vec::<String>::new());
        let message = task
            .run_with_progress(&|line| lines.lock().unwrap().push(line.to_string()))
            .expect("clone should succeed")
            .expect("first run should report work done, not a skip");

        assert_eq!(message, format!("cloned {} into {}", source.display(), dest.display()));
        assert!(
            !lines.lock().unwrap().is_empty(),
            "git clone --progress should have reported at least one stderr line"
        );
        assert!(dest.join(".git").exists());

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn requires_ssh_agent_unlocked_only_for_an_ssh_style_repo_url() {
        let https = CloneGitRepo { repo: "https://example.invalid/x.git".into(), dest: PathBuf::from("/tmp/x") };
        assert!(https.requires().is_empty());

        let local = CloneGitRepo { repo: "/tmp/some/local/repo".into(), dest: PathBuf::from("/tmp/y") };
        assert!(local.requires().is_empty());

        let ssh_shorthand =
            CloneGitRepo { repo: "git@github.com:org/repo.git".into(), dest: PathBuf::from("/tmp/z") };
        assert_eq!(ssh_shorthand.requires(), vec![Capability::from(crate::tasks::cap::SSH_AGENT_UNLOCKED)]);

        let ssh_url =
            CloneGitRepo { repo: "ssh://git@github.com/org/repo.git".into(), dest: PathBuf::from("/tmp/w") };
        assert_eq!(ssh_url.requires(), vec![Capability::from(crate::tasks::cap::SSH_AGENT_UNLOCKED)]);
    }

    #[test]
    fn summarize_progress_extracts_phase_and_percent_dropping_the_rest() {
        assert_eq!(
            summarize_progress("remote: Counting objects:  43% (430/1000)"),
            Some("Counting objects: 43%".to_string())
        );
        assert_eq!(
            summarize_progress("Receiving objects:  43% (430/1000), 26.69 KiB | 455.00 KiB/s"),
            Some("Receiving objects: 43%".to_string())
        );
        assert_eq!(
            summarize_progress("Resolving deltas: 100% (5/5), done."),
            Some("Resolving deltas: 100%".to_string())
        );
    }

    #[test]
    fn summarize_progress_is_none_for_a_line_with_no_percentage() {
        assert_eq!(summarize_progress("Cloning into '/tmp/x'..."), None);
        assert_eq!(summarize_progress("done."), None);
    }

    #[test]
    fn dedup_progress_drops_a_repeated_identical_percentage() {
        let last = RefCell::new(None);
        assert_eq!(
            dedup_progress(&last, "Receiving objects:  21% (1/5)"),
            Some("Receiving objects: 21%".to_string())
        );
        // Same phase and percent again - e.g. a second object that
        // happens to round to the same tick - should be dropped.
        assert_eq!(dedup_progress(&last, "Receiving objects:  21% (2/5)"), None);
        // A real change (percent *or* phase) should always come through.
        assert_eq!(
            dedup_progress(&last, "Receiving objects:  22% (3/5)"),
            Some("Receiving objects: 22%".to_string())
        );
        assert_eq!(
            dedup_progress(&last, "Resolving deltas:  22% (1/9)"),
            Some("Resolving deltas: 22%".to_string())
        );
    }

    #[test]
    fn dedup_progress_never_drops_a_no_percentage_line() {
        let last = RefCell::new(Some("Receiving objects: 50%".to_string()));
        // Even though it doesn't match the cached summary (it can't -
        // it's not a percentage line at all), a phase-transition marker
        // should always be forwarded, not deduped against unrelated state.
        assert_eq!(dedup_progress(&last, "done."), Some("done.".to_string()));
    }

    /// Regression test for the actual bug reported live: before
    /// `run_streaming` was fixed to split on `\r` as well as `\n`, every
    /// `\r`-delimited tick within one phase got buffered into a single
    /// giant concatenated string instead of one call per tick - this
    /// clones a real multi-object local repo (one commit alone doesn't
    /// produce enough objects for git to emit more than a single
    /// progress tick) and confirms every reported line is short and, if
    /// it mentions a percentage at all, in the clean "<phase>: N%" shape
    /// `summarize_progress` produces - not hundreds of ticks glued
    /// together.
    #[test]
    fn progress_lines_are_individual_ticks_not_one_giant_concatenated_blob() {
        let tmp = std::env::temp_dir().join(format!("clone-git-repo-ticks-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let source = tmp.join("source");
        let dest = tmp.join("dest");
        fs::create_dir_all(&source).unwrap();
        git(&source, &["init", "-q", "-b", "main"]);
        git(&source, &["config", "user.email", "test@example.com"]);
        git(&source, &["config", "user.name", "test"]);
        for i in 0..30 {
            fs::write(source.join(format!("f{i}")), format!("{i}")).unwrap();
        }
        git(&source, &["add", "."]);
        git(&source, &["commit", "-q", "-m", "many files"]);

        let task = CloneGitRepo { repo: source.display().to_string(), dest: dest.clone() };
        let lines = std::sync::Mutex::new(Vec::<String>::new());
        task.run_with_progress(&|line| lines.lock().unwrap().push(line.to_string()))
            .expect("clone should succeed")
            .expect("first run should report work done, not a skip");

        let lines = lines.lock().unwrap();
        for (i, line) in lines.iter().enumerate() {
            assert!(
                line.len() < 80,
                "expected a short per-tick line, got a {}-byte blob: {line:?}",
                line.len()
            );
            if line.contains('%') {
                assert!(
                    line.matches('%').count() == 1,
                    "expected one summarized percentage per line, got multiple in {line:?}"
                );
            }
            if let Some(previous) = i.checked_sub(1).map(|j| &lines[j]) {
                assert_ne!(previous, line, "deduping should have dropped this repeat");
            }
        }

        let _ = fs::remove_dir_all(&tmp);
    }
}
