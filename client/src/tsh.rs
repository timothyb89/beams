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

/// The `ext::` git transport URL that pushes to `remote_path` (relative
/// to the beam's `$HOME`) over the same Teleport-authenticated `tsh ssh`
/// channel `ssh_exec` uses - not a new credential, and not the repo's
/// own dedicated keypair under `.ssh/` (that one's for the *beam* to
/// reach GitHub later, e.g. to clone `teleport`; see DESIGN.md's "Repo
/// delivery onto a beam"). `%S` is git's own placeholder - it expands to
/// `git-upload-pack`/`git-receive-pack` as appropriate; this function
/// doesn't need to know which.
///
/// `ext::` is disabled by default in modern git (`protocol.ext.allow`) -
/// callers must set `GIT_ALLOW_PROTOCOL=ext` on the invoking `git`
/// process, not something settable via the URL itself.
fn git_ext_url(login: &str, uuid: &str, remote_path: &str) -> String {
    format!("ext::tsh ssh {login}@{uuid} -- %S '{remote_path}'")
}

/// The local repo's current branch (e.g. `"main"`) - what `push_repo`
/// pushes, and what `prepare_git_target` points the beam's checked-out
/// branch at first (see that function's doc comment for why they have
/// to match).
pub fn current_branch(repo: &Path) -> anyhow::Result<String> {
    let output = Command::new("git")
        .current_dir(repo)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .context("running git rev-parse --abbrev-ref HEAD")?;
    if !output.status.success() {
        bail!(
            "git rev-parse --abbrev-ref HEAD failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Prepares `remote_path` (relative to `$HOME`) on the beam as a target
/// for `push_repo`: a non-bare git repo, checked out on `branch`, with
/// `receive.denyCurrentBranch=updateInstead` set so a push there updates
/// the working tree directly rather than just moving a ref. That setting
/// only takes effect for the branch HEAD already points at - which is
/// why this takes `branch` as a parameter and points HEAD there
/// explicitly, rather than trusting whatever a bare `git init` happens
/// to default to (observed to default to `master` on a stock beam,
/// silently *not* updating the working tree on a push of `main` - the
/// push still "succeeds", it just creates an unchecked-out ref, which is
/// a confusing failure mode to debug after the fact).
///
/// Idempotent - skips `git init` if the directory's already a repo, so
/// safe to call again (e.g. for a future "push again to sync" command,
/// not just the initial copy).
pub fn prepare_git_target(login: &str, uuid: &str, remote_path: &str, branch: &str) -> anyhow::Result<()> {
    let script = format!(
        "mkdir -p '{remote_path}' && cd '{remote_path}' && \
         if [ ! -d .git ]; then git init -q; fi && \
         git symbolic-ref HEAD 'refs/heads/{branch}' && \
         git config receive.denyCurrentBranch updateInstead"
    );
    let target = format!("{login}@{uuid}");
    let status = Command::new("tsh")
        .args(["ssh", &target, "--", "bash", "-lc", &script])
        .status()
        .context("preparing git push target on beam")?;
    if !status.success() {
        bail!("preparing git push target on beam failed ({status})");
    }
    Ok(())
}

/// Pushes `branch` from `repo` to `remote_path` on the beam (see
/// `prepare_git_target`), over the `ext::` transport `git_ext_url`
/// builds. Only transfers committed history - uncommitted local changes
/// don't make the trip, same caveat any git push has.
pub fn push_repo(repo: &Path, login: &str, uuid: &str, remote_path: &str, branch: &str) -> anyhow::Result<()> {
    let url = git_ext_url(login, uuid, remote_path);
    let status = Command::new("git")
        .current_dir(repo)
        .env("GIT_ALLOW_PROTOCOL", "ext")
        .args(["push", &url, branch])
        .status()
        .context("running git push")?;
    if !status.success() {
        bail!("git push to beam failed ({status})");
    }
    Ok(())
}
