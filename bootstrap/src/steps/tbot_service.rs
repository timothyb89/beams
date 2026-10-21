use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context};

use super::k3s_bot::bot_dir;
use super::{cap, Step};
use crate::graph::Capability;

const SERVICE_NAME: &str = "tbot";

/// Starts tbot as a beamctl-managed background service so it keeps
/// renewing both outputs for the lifetime of the beam. Idempotent: skips
/// if a service named `tbot` already exists (beamctl supervises restarts
/// on its own from there).
pub struct StartTbotService;

impl Step for StartTbotService {
    fn id(&self) -> &'static str {
        "start-tbot"
    }

    fn provides(&self) -> &'static [Capability] {
        &[cap::TBOT_SERVICE_RUNNING]
    }

    fn requires(&self) -> &'static [Capability] {
        // Needs `tbot` on disk and `tbot.yaml`/the join token written by
        // the join step - see `bot_dir()` usage below.
        &[cap::TELEPORT_BIN_INSTALLED, cap::K3S_BOT_JOINED]
    }

    fn run(&self, _repo: &Path) -> anyhow::Result<Option<String>> {
        if service_exists()? {
            return Ok(None);
        }

        let dir = bot_dir()?;
        let tbot = dir.join("bin/tbot");
        let config = dir.join("tbot.yaml");
        let token = dir.join("token");

        let mut args: Vec<String> = vec![
            "start".into(),
            "--name".into(),
            SERVICE_NAME.into(),
            "--".into(),
            tbot.to_string_lossy().into_owned(),
            "start".into(),
            "-c".into(),
            config.to_string_lossy().into_owned(),
        ];
        // Only needed on first join; once storage/key exists tbot loads its
        // existing identity and ignores these (see k3s_bot.rs).
        if token.exists() {
            args.push(format!("--token={}", token.display()));
            args.push("--join-method=token".to_string());
        }

        let output = Command::new("beamctl")
            .args(&args)
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
