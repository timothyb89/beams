use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use serde::Deserialize;

use crate::session;

#[derive(Debug, Clone, Deserialize)]
pub struct BeamInfo {
    pub id: String,
    pub uuid: String,
    #[allow(dead_code)]
    pub owner: String,
    pub expires: String,
    #[serde(default)]
    pub region: Option<String>,
}

/// `tsh beams ls --format=json`. Only lists beams owned by the current
/// user (tsh's default; we never pass --all).
pub fn list_beams() -> anyhow::Result<Vec<BeamInfo>> {
    list_beams_with(&[])
}

/// `tsh beams ls --all --format=json` - every beam visible to the
/// current user, not just their own.
pub fn list_all_beams() -> anyhow::Result<Vec<BeamInfo>> {
    list_beams_with(&["--all"])
}

fn list_beams_with(extra: &[&str]) -> anyhow::Result<Vec<BeamInfo>> {
    let output = Command::new("tsh")
        .args(["beams", "ls", "--format=json"])
        .args(extra)
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

/// Runs `tsh ssh -t <login>@<uuid> -- <command>` under the client's local
/// PTY supervisor. This still bypasses the `tsh beams` layer per project.md
/// (it cannot take a remote command), but unlike the old `exec(2)` handoff it
/// leaves `beams` alive to intercept the local menu prefix and, later, service
/// approval requests.
pub fn ssh_interactive(
    login: &str,
    beam_id: &str,
    uuid: &str,
    command: &[String],
    escape_key: session::EscapeKey,
    local_daemon: session::LocalDaemon,
    display: crate::session::DisplayOptions,
) -> anyhow::Result<u8> {
    let target = format!("{login}@{uuid}");
    let mut args = vec![
        "ssh".to_string(),
        "-t".to_string(),
        target,
        "--".to_string(),
    ];
    args.extend(command.iter().cloned());
    session::run(
        "tsh",
        &args,
        session::ConnectionInfo {
            beam_id,
            login,
        },
        escape_key,
        local_daemon,
        display,
    )
    .context("running managed tsh ssh session")
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

/// The local repo's own `origin` remote URL (e.g.
/// `git@github.com:timothyb89/beams.git`), if it has one - `None` rather
/// than an error when it doesn't, since a repo with no `origin`
/// configured is a plausible (if unusual) state and shouldn't block the
/// rest of `ssh`'s spawn path over something `prepare_git_target` treats
/// as optional anyway.
pub fn origin_url(repo: &Path) -> anyhow::Result<Option<String>> {
    let output = Command::new("git")
        .current_dir(repo)
        .args(["remote", "get-url", "origin"])
        .output()
        .context("running git remote get-url origin")?;
    if !output.status.success() {
        return Ok(None);
    }
    let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if url.is_empty() {
        Ok(None)
    } else {
        Ok(Some(url))
    }
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
/// Also points the beam's own `origin` remote at `origin_url` (the
/// *real* GitHub remote, from `origin_url` above) when given one - the
/// `ext::`/`tsh ssh` URL `push_repo` actually pushes over only works
/// from the workstation that ran it (it shells out to `tsh` against a
/// login/uuid pair that's meaningless once this process exits), so it's
/// not something the beam could ever reuse itself. Without this, the
/// beam ends up with no `origin` at all - `git init` doesn't create one,
/// and nothing else ever has - leaving the one thing AGENTS.md's session
/// workflow assumes always works (commit and push to `origin` from
/// inside the beam) silently broken. `None` (no local `origin` to copy)
/// just skips this - same as before this existed.
///
/// Idempotent - skips `git init` if the directory's already a repo, and
/// `remote add`/`set-url` tolerates `origin` already existing from a
/// prior call - so safe to call again (e.g. for a future "push again to
/// sync" command, not just the initial copy).
pub fn prepare_git_target(
    login: &str,
    uuid: &str,
    remote_path: &str,
    branch: &str,
    origin_url: Option<&str>,
) -> anyhow::Result<()> {
    let script = git_target_script(remote_path, branch, origin_url);
    // `ssh` (and `tsh ssh`, mimicking it) doesn't exec the trailing argv
    // elements directly - it joins them with spaces into one line and
    // hands *that* to the remote's shell for its own parsing. Passing
    // `["bash", "-lc", script]` as three separate args therefore loses
    // the boundary around `script` entirely: the remote shell sees
    // `bash -lc mkdir -p 'beams' && ...` and parses `-lc`'s *single*
    // argument as just the word `mkdir`, with everything else
    // (`-p`, `'beams'`, ...) becoming unrelated positional params - this
    // is exactly what produced the "mkdir: missing operand" failure
    // found running this against a real beam the first time. Fixed by
    // passing exactly one already-fully-quoted argv element, so there's
    // nothing left for ssh to join.
    let remote_command = format!("bash -lc {}", shell_quote(&script));
    let target = format!("{login}@{uuid}");
    let status = Command::new("tsh")
        .args(["ssh", &target, "--", &remote_command])
        .status()
        .context("preparing git push target on beam")?;
    if !status.success() {
        bail!("preparing git push target on beam failed ({status})");
    }
    Ok(())
}

/// Builds the shell script `prepare_git_target` runs on the beam -
/// pulled out as its own pure function so the origin-remote logic (the
/// actual thing worth testing here) can be checked against a real local
/// git repo without going anywhere near `tsh`.
fn git_target_script(remote_path: &str, branch: &str, origin_url: Option<&str>) -> String {
    let origin_setup = match origin_url {
        // `remote add` is what runs on a fresh `git init` (no `origin`
        // yet); `remote set-url` is the idempotent path on a second call
        // against an already-prepared target, where `add` would just
        // fail with "remote origin already exists".
        Some(url) => format!(
            "git remote add origin {0} 2>/dev/null || git remote set-url origin {0}",
            shell_quote(url)
        ),
        None => "true".to_string(),
    };
    format!(
        "mkdir -p '{remote_path}' && cd '{remote_path}' && \
         if [ ! -d .git ]; then git init -q; fi && \
         git symbolic-ref HEAD 'refs/heads/{branch}' && \
         git config receive.denyCurrentBranch updateInstead && \
         {origin_setup}"
    )
}

/// Wraps `s` in single quotes for a POSIX shell, escaping any embedded
/// single quotes the standard way (`'\''`: close the quote, an escaped
/// literal `'`, reopen the quote).
pub(crate) fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `sh -c <shell_quote(s)>` should echo back exactly `s`, for any
    /// `s` - the actual property `prepare_git_target` relies on to
    /// survive `tsh`/`ssh`'s single-line rejoin of trailing argv
    /// elements (see that function's doc comment on the bug this caught
    /// the first time).
    fn round_trips_through_a_real_shell(s: &str) {
        let output = Command::new("sh")
            .arg("-c")
            .arg(format!("printf '%s' {}", shell_quote(s)))
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&output.stdout), s, "shell_quote({s:?}) didn't round-trip");
    }

    #[test]
    fn shell_quote_round_trips_plain_text() {
        round_trips_through_a_real_shell("mkdir -p 'beams' && cd beams");
    }

    #[test]
    fn shell_quote_round_trips_text_with_embedded_single_quotes() {
        round_trips_through_a_real_shell("it's a 'test' with lots of ''' quotes");
    }

    #[test]
    fn shell_quote_round_trips_an_empty_string() {
        round_trips_through_a_real_shell("");
    }

    /// Runs `script` via a real `sh -c` with `cwd` set to `repo` - same
    /// shape `tsh ssh ... -- bash -lc <script>` runs it in, just without
    /// `tsh`/`bash`/the remote hop.
    fn run_script(script: &str, repo: &Path) -> bool {
        Command::new("sh").arg("-c").arg(script).current_dir(repo).status().unwrap().success()
    }

    fn fresh_test_repo(name: &str) -> std::path::PathBuf {
        let repo = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&repo);
        std::fs::create_dir_all(&repo).unwrap();
        repo
    }

    /// The actual bug this is fixing: `git init` + a push over the
    /// `ext::` transport never leaves the beam with an `origin` remote
    /// at all - this is what makes `git_target_script` set one, and
    /// confirms it survives being run twice (the `remote add` /
    /// `remote set-url` fallback `prepare_git_target`'s doc comment
    /// describes), not just once on a pristine `git init`.
    #[test]
    fn git_target_script_sets_origin_remote_idempotently() {
        let repo = fresh_test_repo("tsh-test-origin-remote");
        let script = git_target_script(".", "main", Some("git@github.com:example/example.git"));

        assert!(run_script(&script, &repo), "first run (git init + remote add) should succeed");
        assert!(run_script(&script, &repo), "second run (remote set-url fallback) should succeed too");

        assert_eq!(
            origin_url(&repo).unwrap(),
            Some("git@github.com:example/example.git".to_string())
        );

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn git_target_script_without_origin_url_skips_remote_setup() {
        let script = git_target_script("beams", "main", None);
        assert!(!script.contains("git remote"), "script shouldn't touch origin when none was given: {script}");
    }

    #[test]
    fn origin_url_is_none_without_a_configured_remote() {
        let repo = fresh_test_repo("tsh-test-no-origin");
        assert!(Command::new("git").current_dir(&repo).args(["init", "-q"]).status().unwrap().success());

        assert_eq!(origin_url(&repo).unwrap(), None);

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn origin_url_reads_a_configured_remote() {
        let repo = fresh_test_repo("tsh-test-with-origin");
        assert!(Command::new("git").current_dir(&repo).args(["init", "-q"]).status().unwrap().success());
        assert!(Command::new("git")
            .current_dir(&repo)
            .args(["remote", "add", "origin", "git@github.com:example/example.git"])
            .status()
            .unwrap()
            .success());

        assert_eq!(origin_url(&repo).unwrap(), Some("git@github.com:example/example.git".to_string()));

        let _ = std::fs::remove_dir_all(&repo);
    }
}
