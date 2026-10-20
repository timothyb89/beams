use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context};

use super::Step;

pub struct UpdateClaude;

impl Step for UpdateClaude {
    fn id(&self) -> &'static str {
        "claude-update"
    }

    fn run(&self, _repo: &Path) -> anyhow::Result<Option<String>> {
        let output = Command::new("claude")
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
