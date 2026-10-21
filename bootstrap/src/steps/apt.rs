use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context};

use super::Step;

/// Packages to install on every beam, that are available from Debian's
/// default repos as-is. apt-get already no-ops packages that are already
/// installed, so this list doesn't need its own is-it-installed check -
/// just grow it as needed. zellij and kubectl aren't in Debian's repos and
/// are installed by their own dedicated steps instead (see `zellij.rs` and
/// `kubectl.rs`).
const PACKAGES: &[&str] = &["micro", "less", "fish"];

pub struct InstallPackages;

impl Step for InstallPackages {
    fn id(&self) -> &'static str {
        "apt-install"
    }

    fn run(&self, _repo: &Path) -> anyhow::Result<Option<String>> {
        // A fresh beam's apt index can be stale/empty - `apt-get install`
        // then fails with "unable to locate package" for everything,
        // indistinguishable from a real typo in PACKAGES. Always update
        // first; it's a no-op cost-wise if the index is already current.
        let update = Command::new("sudo")
            .arg("apt-get")
            .arg("update")
            .output()
            .context("running apt-get update")?;
        if !update.status.success() {
            bail!(
                "apt-get update failed ({}): {}",
                update.status,
                String::from_utf8_lossy(&update.stderr).trim()
            );
        }

        let output = Command::new("sudo")
            .arg("apt-get")
            .arg("install")
            .arg("-y")
            .args(PACKAGES)
            .output()
            .context("running apt-get install")?;

        if !output.status.success() {
            bail!(
                "apt-get install failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        Ok(Some(format!("installed {}", PACKAGES.join(", "))))
    }
}
