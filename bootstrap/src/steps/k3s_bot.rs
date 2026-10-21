use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context};
use serde::Deserialize;

use super::linkutil::home_dir;
use super::teleport_bin::BOT_DIR;
use super::{cap, Step};
use crate::graph::Capability;

/// Bot and target cluster/node names are static for now - see
/// bot-onboarding/ for the role/bot resources (applied once, cluster-wide,
/// not per-beam).
const BOT_NAME: &str = "k3s-bot";
const KUBE_CLUSTER: &str = "k3s";

#[derive(Deserialize)]
struct BotInstanceAdd {
    token_id: String,
}

/// Joins this beam as a new instance of the (already-provisioned) k3s-bot,
/// and writes a static tbot.yaml configured with a Kubernetes output (for
/// the k3s cluster) and an identity output (for SSH to its host node).
/// Idempotent: once `storage/key` exists, this beam has already joined and
/// has a renewable identity, so no new token/instance is requested on
/// re-runs - `start-tbot` (see tbot_service.rs) handles renewal from here.
pub struct JoinK3sBot;

impl Step for JoinK3sBot {
    fn id(&self) -> &'static str {
        "k3s-bot-join"
    }

    fn provides(&self) -> &'static [Capability] {
        &[cap::K3S_BOT_JOINED]
    }

    fn requires(&self) -> &'static [Capability] {
        // Runs `tctl` out of `bot_dir()/bin` - see `request_join_token`.
        &[cap::TELEPORT_BIN_INSTALLED]
    }

    fn run(&self, _repo: &Path) -> anyhow::Result<Option<String>> {
        let dir = bot_dir()?;
        for sub in ["storage", "kube", "identity"] {
            fs::create_dir_all(dir.join(sub))
                .with_context(|| format!("creating {}", dir.join(sub).display()))?;
        }

        write_config(&dir)?;

        if dir.join("storage/key").exists() {
            return Ok(None); // already joined; nothing new to do
        }

        let token = request_join_token()?;
        let token_path = dir.join("token");
        fs::write(&token_path, token).context("writing join token file")?;
        fs::set_permissions(&token_path, fs::Permissions::from_mode(0o600))
            .context("setting permissions on join token file")?;

        Ok(Some(format!("requested join token, wrote config to {}", dir.display())))
    }
}

pub fn bot_dir() -> anyhow::Result<PathBuf> {
    Ok(home_dir()?.join(BOT_DIR))
}

fn write_config(dir: &Path) -> anyhow::Result<()> {
    let proxy = std::env::var("TELEPORT_PROXY").context("TELEPORT_PROXY is not set")?;
    let config = format!(
        "version: v2\n\
         onboarding:\n\
         \x20 join_method: token\n\
         proxy_server: {proxy}\n\
         storage: {{type: directory, path: {storage}}}\n\
         outputs:\n\
         \x20 - type: kubernetes\n\
         \x20   kubernetes_cluster: {KUBE_CLUSTER}\n\
         \x20   destination: {{type: directory, path: {kube}}}\n\
         \x20 - type: identity\n\
         \x20   destination: {{type: directory, path: {identity}}}\n",
        storage = dir.join("storage").display(),
        kube = dir.join("kube").display(),
        identity = dir.join("identity").display(),
    );
    fs::write(dir.join("tbot.yaml"), config).context("writing tbot.yaml")
}

/// Runs `tctl bots instances add` and returns the join token. The token is
/// never written to stdout/logs - it's returned in memory and the caller
/// writes it straight to a 0600 file.
fn request_join_token() -> anyhow::Result<String> {
    let proxy = std::env::var("TELEPORT_PROXY").context("TELEPORT_PROXY is not set")?;
    let tctl = bot_dir()?.join("bin/tctl");

    let output = Command::new(&tctl)
        .args(["bots", "instances", "add", BOT_NAME, "--format=json"])
        .arg(format!("--auth-server={proxy}"))
        .env("TELEPORT_HOME", home_dir()?.join(".tsh"))
        .output()
        .with_context(|| format!("running {}", tctl.display()))?;

    if !output.status.success() {
        bail!(
            "tctl bots instances add {BOT_NAME} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let parsed: BotInstanceAdd = serde_json::from_slice(&output.stdout)
        .context("parsing tctl bots instances add output")?;
    Ok(parsed.token_id)
}
