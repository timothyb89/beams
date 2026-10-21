use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context};

use super::{cap, Step};
use crate::graph::Capability;

const SERVICE_NAME: &str = "beam-bootstrapd";

/// Starts `beam-bootstrap daemon` (see `crate::daemon`) as a
/// beamctl-managed background service, same pattern as `tbot` in
/// `tbot_service.rs`. No `requires` - the daemon itself doesn't depend
/// on anything else in the fixed sequence having run first, so it comes
/// up as early as possible (its own wave-0 slot), giving a TUI or
/// on-demand CLI something to connect to as soon as `bin/beam-shell`
/// drops into zellij, not just once the whole sequence finishes.
///
/// Idempotent: skips if a service named `beam-bootstrapd` already
/// exists (beamctl supervises restarts on its own from there) -
/// `crate::daemon::serve` also independently refuses to double-bind the
/// socket, so this is defense in depth, not the only thing preventing
/// two daemons.
pub struct StartBootstrapDaemon;

impl Step for StartBootstrapDaemon {
    fn id(&self) -> &'static str {
        "start-bootstrap-daemon"
    }

    fn provides(&self) -> Vec<Capability> {
        vec![cap::BOOTSTRAP_DAEMON_RUNNING.into()]
    }

    fn run(&self, _repo: &Path) -> anyhow::Result<Option<String>> {
        if service_exists()? {
            return Ok(None);
        }

        let exe = std::env::current_exe().context("resolving this binary's own path")?;
        let output = Command::new("beamctl")
            .args(["start", "--name", SERVICE_NAME, "--"])
            .arg(&exe)
            .arg("daemon")
            .output()
            .context("running beamctl start")?;
        if !output.status.success() {
            bail!(
                "beamctl start {SERVICE_NAME} failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        Ok(Some(format!("started beamctl service {SERVICE_NAME}")))
    }
}

fn service_exists() -> anyhow::Result<bool> {
    let output = Command::new("beamctl")
        .args(["show", SERVICE_NAME, "--json"])
        .output()
        .context("running beamctl show")?;
    Ok(output.status.success())
}
