use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context};

use super::Step;

pub struct SetDefaultShell;

impl Step for SetDefaultShell {
    fn id(&self) -> &'static str {
        "default-shell"
    }

    fn run(&self, _repo: &Path) -> anyhow::Result<Option<String>> {
        let fish = which("fish").context("fish not found on PATH - did apt-install run?")?;
        let user = std::env::var("USER").context("USER is not set")?;

        if current_shell(&user)? == fish {
            return Ok(None);
        }

        let output = Command::new("sudo")
            .args(["chsh", "-s", &fish, &user])
            .output()
            .context("running chsh")?;
        if !output.status.success() {
            bail!(
                "chsh failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        Ok(Some(format!("set default shell for {user} to {fish}")))
    }
}

fn which(bin: &str) -> anyhow::Result<String> {
    let output = Command::new("which")
        .arg(bin)
        .output()
        .with_context(|| format!("running which {bin}"))?;
    if !output.status.success() {
        bail!("{bin} not found on PATH");
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn current_shell(user: &str) -> anyhow::Result<String> {
    let output = Command::new("getent")
        .args(["passwd", user])
        .output()
        .context("running getent passwd")?;
    if !output.status.success() {
        bail!("getent passwd {user} failed");
    }
    let line = String::from_utf8_lossy(&output.stdout);
    line.trim()
        .rsplit(':')
        .next()
        .map(str::to_string)
        .context("unexpected getent passwd output")
}
