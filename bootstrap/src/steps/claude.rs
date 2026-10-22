use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context};

use super::{cap, Step};
use crate::graph::Capability;

pub struct UpdateClaude;

impl Step for UpdateClaude {
    fn id(&self) -> &'static str {
        "claude-update"
    }

    fn provides(&self) -> Vec<Capability> {
        vec![cap::CLAUDE_UPDATED.into()]
    }

    fn run(&self, _repo: &Path) -> anyhow::Result<Option<String>> {
        // Confirmed by digging into why this intermittently failed with
        // "sudo: claude: command not found": on at least some beam
        // images, `claude`'s npm-created bin symlink doesn't actually
        // sit at a plain `claude` anywhere on PATH (for any user, not
        // just under sudo's secure_path) - it's been moved aside to a
        // hidden, randomly-suffixed name instead (e.g.
        // `/usr/bin/.claude-<random>`), apparently by whatever harness
        // infrastructure manages the sandbox's own actively-running
        // Claude Code session (a *separate*, pinned install elsewhere
        // entirely - see `/proc/<pid>/exe` for any `claude` process to
        // confirm). That's almost certainly deliberate on the platform's
        // part (isolating/protecting the live session from an in-place
        // update it's currently running out of, and/or not leaving an
        // agent session a trivially-discoverable path to its own
        // interpreter) and not something `beam-bootstrap` should go
        // digging for the hidden name to route around - this step's job
        // is to opportunistically keep a *separate*, human-facing
        // `claude` CLI current when one happens to exist by its normal
        // name, not to manage the harness's own copy. Hence the
        // `which`-first check below rather than just trying to run it:
        // treat "no plain `claude` right now" as nothing to do, same as
        // any other step finding its target capability already (or, in
        // this case, not yet) in a state it has nothing to act on - not
        // as this step's own failure. A `claude` that *is* findable but
        // genuinely fails to update still reports that failure as
        // before.
        if !which_claude_exists() {
            return Ok(None);
        }

        // The install this beam image ships is root-owned (npm global
        // install under /usr/lib/node_modules), so a plain `claude update`
        // fails with a permissions error telling you to use sudo - just do
        // that. sudo resolves `claude` fine on its own since it's at a
        // normal system path (confirmed present by the check above), no
        // need to pass through $PATH explicitly.
        let output = Command::new("sudo")
            .arg("claude")
            .arg("update")
            .output()
            .context("running claude update")?;

        if !output.status.success() {
            bail!(
                "claude update failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        Ok(Some(String::from_utf8_lossy(&output.stdout).trim().to_string()))
    }
}

/// Whether a plain `claude` resolves on PATH right now, for this user -
/// checked with the ordinary `which`, not `sudo which`, since the two
/// have the same secure_path concern here and a mismatch between them
/// would itself be worth knowing about rather than silently papering
/// over (not expected in practice: sudoers' `secure_path` on this image
/// is a subset of the normal system dirs any of this would resolve
/// through anyway).
fn which_claude_exists() -> bool {
    Command::new("which").arg("claude").output().map(|o| o.status.success()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Not a meaningful assertion about *this* machine's `claude`
    /// specifically (whether it's findable depends on the beam image
    /// and whatever the harness is doing at the moment, see `run`'s doc
    /// comment) - just confirms `which_claude_exists` doesn't panic and
    /// returns a plain bool either way, which is all `run` needs from
    /// it.
    #[test]
    fn which_claude_exists_does_not_panic() {
        let _ = which_claude_exists();
    }
}
