//! `launch start` and `launch shell`: the Beam side of `beams ssh --profile`.
//!
//! The workstation copies a small runtime and one launch onto the Beam
//! (no repo checkout):
//!
//! ```text
//! ~/.beams/runtime/bin/beam-bootstrap    this binary
//! ~/.beams/runtime/bin/beams-shell       Zellij's default_shell (`launch shell`)
//! ~/.beams/runtime/zellij/beams-launch.kdl
//! ~/.beams/launches/<id>/launch.json, pre/{descriptor.json,bundle.tar*}
//! ```
//!
//! and runs `launch start --launch-dir ~/.beams/launches/<id>` as the
//! interactive SSH command. That starts the daemon, submits the launch
//! (idempotent: a reconnect finds it accepted), stages pre, installs
//! Zellij alongside pre, prints pre progress, and then replaces itself
//! with Zellij in a session named for the launch. Every pane's shell is
//! `launch shell`, which reports `shell` readiness and execs the plan's
//! shell; the TUI pane reports `tui`. Post work starts once both have.

use anyhow::{bail, Context};
use beams_protocol::launch::{BundleDescriptor, Declaration, Phase};
use serde_json::json;
use std::collections::BTreeMap;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::daemon::{LaunchSnapshot, LaunchStatus, NodeStatus};

/// How long to wait for a just-started daemon's socket.
const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(15);
/// Env var carrying the plan's shell to `launch shell`.
pub const SHELL_ENV: &str = "BEAMS_SHELL";

/// The Zellij session for a launch. One per launch, so attaching never
/// lands in a session whose panes carry another launch's environment.
pub fn session_name(launch_id: &str) -> String {
    format!("beams-{}", launch_id.split('-').next().unwrap_or(launch_id))
}

pub fn start(launch_dir: &Path, socket: Option<String>) -> anyhow::Result<()> {
    let launch_dir = std::path::absolute(launch_dir)?;
    let launch_file = launch_dir.join("launch.json");
    let declaration: Declaration = beams_artifacts::read_json(&launch_file)?;
    let id = declaration.launch_id.clone();
    let exe = std::env::current_exe().context("resolving this binary's path")?;
    let runtime = exe
        .parent()
        .and_then(Path::parent)
        .context("this binary is not in a runtime bin/ directory")?
        .to_path_buf();
    let home = PathBuf::from(std::env::var("HOME").context("HOME is not set")?);
    eprintln!("beams: launch {id}");

    // Zellij (a pinned, verified download) doesn't depend on pre; fetch it
    // while pre runs.
    let zellij = std::thread::spawn(|| {
        use crate::steps::Step;
        crate::steps::InstallZellij.run(Path::new("")).map_err(|err| format!("{err:#}"))
    });

    // Before pre: profile hooks and post clones may use ssh.
    if let Err(err) = crate::knownhosts::install_ssh_config(&exe) {
        eprintln!("beams: known_hosts lookups through the workstation are unavailable: {err:#}");
    }
    ensure_daemon(&exe, socket.clone())?;
    let mut conn = super::connect(socket.clone())?;
    let reply = super::request(
        &mut conn,
        json!({
            "op": "launch_submit",
            "launch": launch_file,
            "staging": launch_dir.join("staging"),
            "gate": "interactive",
        }),
    )?;
    let launch: LaunchSnapshot = serde_json::from_value(reply["launch"].clone())?;
    if launch.plan_digest != declaration.plan_digest {
        bail!("the daemon holds launch {id} with a different plan digest");
    }
    if !launch.released.iter().any(|c| c == "artifact-ready:pre") {
        let pre = launch_dir.join("pre");
        let descriptor_path = pre.join("descriptor.json");
        let descriptor: BundleDescriptor = beams_artifacts::read_json(&descriptor_path)?;
        let archive = pre.join(match descriptor.compression {
            beams_protocol::launch::Compression::None => "bundle.tar",
            beams_protocol::launch::Compression::Gzip => "bundle.tar.gz",
        });
        super::receive(socket.clone(), &id, Phase::Pre, &archive, &descriptor_path)?;
    }
    let pre = wait_for_pre(&mut conn, &id, reply["seq"].as_u64().unwrap_or_default());

    let zellij = zellij.join().map_err(|_| anyhow::anyhow!("the Zellij install panicked"))?;
    if let Err(err) = pre.and(zellij.map(|_| ()).map_err(|err| anyhow::anyhow!("installing Zellij: {err}"))) {
        eprintln!("beams: {err:#}");
        eprintln!(
            "beams: opening a plain shell instead; `beam-bootstrap launch status --launch-id {id}` shows \
             details and `beam-bootstrap launch retry --launch-id {id}` retries"
        );
        let err = Command::new("/bin/bash").arg("-l").env_remove("BEAMS_LAUNCH_ID").exec();
        bail!("starting bash: {err}");
    }

    let env = super::runner::plan_environment(&declaration, &home, &std::env::var("PATH").unwrap_or_default());
    let shell = declaration.plan.shell.program.clone().unwrap_or_else(|| "/bin/bash".into());
    let config = write_zellij_config(&home, &runtime, &id)?;
    let mut zellij = Command::new("zellij");
    zellij
        .arg("--config")
        .arg(&config)
        .args(["attach", &session_name(&id), "--create"])
        .envs(&env)
        .env("BEAMS_LAUNCH_ID", &id)
        .env(SHELL_ENV, &shell)
        .env("SHELL", &shell)
        .env("BEAM_BOOTSTRAP_BIN", &exe)
        .env("TELEPORT_ADD_KEYS_TO_AGENT", "no");
    if let Some(sock) = crate::sshagent::discover_ssh_auth_sock() {
        zellij.env("SSH_AUTH_SOCK", sock);
    }
    let err = zellij.exec();
    bail!("starting zellij: {err}")
}

/// Starts the daemon under beamctl (from this runtime) unless its socket
/// already answers, then waits for it.
fn ensure_daemon(exe: &Path, socket: Option<String>) -> anyhow::Result<()> {
    if super::connect(socket.clone()).is_ok() {
        return Ok(());
    }
    let output = Command::new("beamctl")
        .args(["start", "--name", "beam-bootstrapd", "--"])
        .arg(exe)
        .arg("daemon")
        .output()
        .context("running beamctl start")?;
    if !output.status.success() {
        // An existing (stopped or still starting) service is fine; the
        // socket check below decides.
        eprintln!(
            "beams: beamctl start beam-bootstrapd: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let deadline = Instant::now() + DAEMON_START_TIMEOUT;
    loop {
        match super::connect(socket.clone()) {
            Ok(_) => return Ok(()),
            Err(err) if Instant::now() >= deadline => {
                return Err(err).context("the bootstrap daemon did not start (see `beamctl logs beam-bootstrapd`)")
            }
            Err(_) => std::thread::sleep(Duration::from_millis(200)),
        }
    }
}

/// Pre's outcome from a snapshot: `Some(Ok)` once every pre node succeeded
/// or was skipped, `Some(Err)` once one failed or the launch stopped.
fn pre_outcome(launch: &LaunchSnapshot) -> Option<anyhow::Result<()>> {
    let pre = || launch.nodes.iter().filter(|n| n.phase == Phase::Pre);
    if let Some(node) = pre().find(|n| matches!(n.status, NodeStatus::Error | NodeStatus::Blocked | NodeStatus::Interrupted)) {
        let why = node.message.as_deref().map(|m| format!(": {m}")).unwrap_or_default();
        return Some(Err(anyhow::anyhow!("pre task {} {:?}{why}", node.label, node.status)));
    }
    if matches!(launch.status, LaunchStatus::Failed | LaunchStatus::Interrupted) {
        let why = launch.message.as_deref().map(|m| format!(": {m}")).unwrap_or_default();
        return Some(Err(anyhow::anyhow!("launch {:?}{why}", launch.status)));
    }
    let staged = launch.released.iter().any(|c| c == "artifact-ready:pre");
    (staged && pre().all(|n| matches!(n.status, NodeStatus::Ok | NodeStatus::Skip))).then_some(Ok(()))
}

/// Prints pre progress until pre settles. Each event past the submission
/// boundary prompts a fresh snapshot, which decides.
fn wait_for_pre(conn: &mut crate::taskcli::Connection, id: &str, boundary: u64) -> anyhow::Result<()> {
    let snapshot = |conn: &mut crate::taskcli::Connection| -> anyhow::Result<LaunchSnapshot> {
        let reply = super::request(conn, json!({"op": "launch_snapshot", "launch_id": id}))?;
        let launches: Vec<LaunchSnapshot> = serde_json::from_value(reply["launches"].clone())?;
        launches.into_iter().next().with_context(|| format!("the daemon lost launch {id}"))
    };
    let launch = snapshot(conn)?;
    if let Some(outcome) = pre_outcome(&launch) {
        return outcome;
    }
    let mut shown = BTreeMap::new();
    for node in launch.nodes.iter().filter(|n| n.phase == Phase::Pre) {
        eprintln!("beams:   {:<8} {}", format!("{:?}", node.status).to_lowercase(), node.label);
        shown.insert(node.id.clone(), node.status);
    }
    while let Some(event) = conn.recv()? {
        if event["launch_id"] != id || event["seq"].as_u64().is_some_and(|seq| seq <= boundary) {
            continue;
        }
        let launch = snapshot(conn)?;
        for node in launch.nodes.iter().filter(|n| n.phase == Phase::Pre) {
            if shown.insert(node.id.clone(), node.status) != Some(node.status) {
                let why = node.message.as_deref().map(|m| format!(" - {m}")).unwrap_or_default();
                eprintln!("beams:   {:<8} {}{why}", format!("{:?}", node.status).to_lowercase(), node.label);
            }
        }
        if let Some(outcome) = pre_outcome(&launch) {
            return outcome;
        }
    }
    bail!("the daemon closed the connection before pre finished")
}

/// A per-launch Zellij config: the runtime's layout, and `beams-shell`
/// for every pane Zellij opens itself. Leaves the user's own Zellij
/// config alone.
fn write_zellij_config(home: &Path, runtime: &Path, id: &str) -> anyhow::Result<PathBuf> {
    let path = home.join(".cache/beams").join(format!("zellij-{}.kdl", session_name(id)));
    std::fs::create_dir_all(path.parent().expect("has a parent"))?;
    let kdl = |p: &Path| format!("{:?}", p.display().to_string());
    std::fs::write(
        &path,
        format!(
            "layout_dir {}\ndefault_layout \"beams-launch\"\ndefault_shell {}\n",
            kdl(&runtime.join("zellij")),
            kdl(&runtime.join("bin/beams-shell"))
        ),
    )
    .with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// `launch shell`: report `shell` readiness in the background, then become
/// the plan's shell. Outside a launch it just execs the shell.
pub fn shell(args: &[String]) -> anyhow::Result<()> {
    let shell = std::env::var(SHELL_ENV)
        .ok()
        .filter(|s| !s.is_empty() && Path::new(s).is_absolute())
        .unwrap_or_else(|| "/bin/bash".into());
    if std::env::var("BEAMS_LAUNCH_ID").is_ok_and(|id| !id.is_empty()) {
        if let Ok(exe) = std::env::current_exe() {
            // Not waited on: the report must not delay the prompt, and a
            // missing daemon must not keep the shell from starting.
            let _ = Command::new(exe)
                .args(["launch", "ready", "--component", "shell"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
        }
    }
    let err = Command::new(&shell).args(args).exec();
    bail!("starting {shell}: {err}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::daemon::NodeState;
    use beams_protocol::launch::Gate;

    fn node(phase: Phase, status: NodeStatus) -> NodeState {
        NodeState {
            id: format!("{phase:?}-{status:?}"),
            label: "n".into(),
            phase,
            status,
            message: None,
            log: Default::default(),
        }
    }

    fn launch(nodes: Vec<NodeState>, released: &[&str], status: LaunchStatus) -> LaunchSnapshot {
        LaunchSnapshot {
            launch_id: "id".into(),
            plan_digest: "d".into(),
            gate: Gate::Interactive,
            status,
            message: None,
            nodes,
            ready: Vec::new(),
            released: released.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn pre_settles_only_after_staging_and_ignores_post() {
        let waiting = launch(vec![node(Phase::Pre, NodeStatus::Ok)], &[], LaunchStatus::Running);
        assert!(pre_outcome(&waiting).is_none());
        let empty = launch(vec![node(Phase::Post, NodeStatus::Waiting)], &["artifact-ready:pre"], LaunchStatus::Running);
        assert!(matches!(pre_outcome(&empty), Some(Ok(()))));
        let running = launch(
            vec![node(Phase::Pre, NodeStatus::Ok), node(Phase::Pre, NodeStatus::Running)],
            &["artifact-ready:pre"],
            LaunchStatus::Running,
        );
        assert!(pre_outcome(&running).is_none());
        let failed = launch(
            vec![node(Phase::Pre, NodeStatus::Error), node(Phase::Post, NodeStatus::Waiting)],
            &["artifact-ready:pre"],
            LaunchStatus::Running,
        );
        assert!(matches!(pre_outcome(&failed), Some(Err(_))));
        let stopped = launch(vec![], &[], LaunchStatus::Interrupted);
        assert!(matches!(pre_outcome(&stopped), Some(Err(_))));
    }

    #[test]
    fn session_names_are_per_launch() {
        assert_eq!(session_name("0a1b2c3d-1111-2222-3333-444455556666"), "beams-0a1b2c3d");
    }
}
