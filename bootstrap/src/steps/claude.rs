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
        // The install this beam image ships is root-owned (npm global
        // install under /usr/lib/node_modules), so a plain `claude update`
        // fails with a permissions error telling you to use sudo - just do
        // that. sudo resolves `claude` fine on its own since it's at a
        // normal system path, no need to pass through $PATH explicitly.
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
