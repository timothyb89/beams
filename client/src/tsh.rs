use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct BeamInfo {
    pub id: String,
    pub uuid: String,
    #[allow(dead_code)]
    pub owner: String,
    pub expires: String,
}

/// `tsh beams ls --format=json`. Only lists beams owned by the current
/// user (tsh's default; we never pass --all).
pub fn list_beams() -> anyhow::Result<Vec<BeamInfo>> {
    let output = Command::new("tsh")
        .args(["beams", "ls", "--format=json"])
        .output()
        .context("running tsh beams ls")?;
    if !output.status.success() {
        bail!(
            "tsh beams ls failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    serde_json::from_slice(&output.stdout).context("parsing tsh beams ls output")
}

/// `tsh beams add --no-console --format=json`, optionally pinned to a
/// region.
pub fn add_beam(region: Option<&str>) -> anyhow::Result<BeamInfo> {
    let mut args = vec!["beams", "add", "--no-console", "--format=json"];
    let region_flag;
    if let Some(region) = region {
        region_flag = format!("--region={region}");
        args.push(&region_flag);
    }
    let output = Command::new("tsh")
        .args(&args)
        .output()
        .context("running tsh beams add")?;
    if !output.status.success() {
        bail!(
            "tsh beams add failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    serde_json::from_slice(&output.stdout).context("parsing tsh beams add output")
}

/// Polls `tsh beams exec <beam_id> -- true` until it succeeds, or bails
/// after `timeout`. A just-created beam's `id`/`uuid` show up in `tsh
/// beams ls` immediately, but sshd isn't necessarily reachable yet -
/// `scp`/`ssh` fail outright ("not ready to accept SSH connections")
/// rather than retrying themselves, so the caller has to.
pub fn wait_until_ready(beam_id: &str, login: &str, timeout: Duration) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    let mut delay = Duration::from_secs(1);
    loop {
        let status = Command::new("tsh")
            .args(["beams", "exec", "-l", login, beam_id, "--", "true"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .context("running tsh beams exec")?;
        if status.success() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "beam {beam_id} did not become ready to accept SSH connections within {:?}",
                timeout
            );
        }
        std::thread::sleep(delay);
        delay = (delay * 2).min(Duration::from_secs(5));
    }
}

/// `tsh beams scp -r <local_dir> <beam_id>:` - lands at
/// `~/<local_dir-basename>` on the beam (scp's usual recursive-copy
/// semantics: the destination directory is created/merged under the given
/// remote path, here the beam's home directory).
pub fn scp_dir(local_dir: &Path, beam_id: &str) -> anyhow::Result<()> {
    let dest = format!("{beam_id}:");
    let status = Command::new("tsh")
        .args(["beams", "scp", "-r", "-q"])
        .arg(local_dir)
        .arg(&dest)
        .status()
        .context("running tsh beams scp")?;
    if !status.success() {
        bail!("tsh beams scp failed ({status})");
    }
    Ok(())
}

/// Execs (replacing this process) into `tsh ssh -t <login>@<uuid> --
/// <command>`, bypassing the `tsh beams` layer per project.md - it takes a
/// trailing command (and a real PTY via -t) where `tsh beams ssh` doesn't.
/// Never returns on success; the whole point is to hand off the terminal.
pub fn ssh_exec(login: &str, uuid: &str, command: &[String]) -> anyhow::Result<()> {
    let target = format!("{login}@{uuid}");
    let mut cmd = Command::new("tsh");
    cmd.args(["ssh", "-t", &target, "--"]).args(command);
    let err = cmd.exec(); // only returns on failure to exec
    Err(err).context("running tsh ssh")
}
