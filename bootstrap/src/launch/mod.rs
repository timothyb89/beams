//! Launch tooling: explicit artifact/phase commands, the daemon ops' CLI,
//! and `launch start`/`launch shell` (`session`). Never restarts the daemon.
pub mod daemon;
mod files;
mod runner;
pub mod session;
pub mod upload;
#[cfg(test)]
mod tests;

use anyhow::Context as _;
use beams_protocol::launch::{BundleDescriptor, Declaration, Phase};
use serde_json::{json, Value};
use clap::{Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Subcommand)]
pub enum Command {
    /// Execute one verified launch phase through the existing task executor.
    RunPhase {
        #[arg(long)]
        launch: PathBuf,
        #[arg(long)]
        staging: PathBuf,
        #[arg(long, value_enum)]
        phase: RunPhase,
        /// Explicit non-Zellij gate policy; required for post.
        #[arg(long)]
        headless: bool,
        #[arg(long)]
        state_dir: Option<PathBuf>,
        #[arg(long)]
        retry_interrupted: bool,
    },
    /// Shared locked package helper for hooks and standalone installs.
    InstallPackages {
        #[arg(required = true)]
        packages: Vec<String>,
    },
    /// Submit a verified launch to the running daemon, which runs pre then
    /// post on its shared resources and waits for external conditions.
    /// Follows its progress until it finishes (exit 1 if it fails).
    Submit {
        #[arg(long)]
        launch: PathBuf,
        /// Staging root holding (or later receiving) `pre/` and `post/`.
        #[arg(long)]
        staging: PathBuf,
        /// Release interface-ready as soon as pre succeeds.
        #[arg(long)]
        headless: bool,
        /// Return once accepted instead of following progress.
        #[arg(long)]
        no_follow: bool,
        /// Daemon socket (default ~/.beam-bootstrap/daemon.sock).
        #[arg(long)]
        socket: Option<String>,
    },
    /// Release a daemon launch's external condition: `interface-ready`, or
    /// `artifact-ready:pre|post` (the daemon verifies that staging first).
    Signal {
        #[arg(long)]
        launch_id: String,
        #[arg(long)]
        condition: String,
        #[arg(long)]
        socket: Option<String>,
    },
    /// Show daemon launches and their nodes.
    Status {
        #[arg(long)]
        launch_id: Option<String>,
        /// Print the raw snapshot JSON.
        #[arg(long)]
        json: bool,
        #[arg(long)]
        socket: Option<String>,
    },
    /// Report that an interface component is up. An interactive launch
    /// releases interface-ready once both `shell` and `tui` have. Does
    /// nothing (exit 0) without a launch ID, so the initial shell can call
    /// it unconditionally.
    Ready {
        #[arg(long, value_parser = ["shell", "tui"])]
        component: String,
        /// Default: `$BEAMS_LAUNCH_ID`.
        #[arg(long)]
        launch_id: Option<String>,
        #[arg(long)]
        socket: Option<String>,
    },
    /// Restart a failed or interrupted daemon launch with its accepted plan.
    Retry {
        #[arg(long)]
        launch_id: String,
        /// Also rerun unchecked hooks that were interrupted (review their
        /// effects first).
        #[arg(long)]
        retry_interrupted: bool,
        #[arg(long)]
        no_follow: bool,
        #[arg(long)]
        socket: Option<String>,
    },
    /// Hand a bundle archive on this machine to the daemon, which verifies
    /// and stages it for an accepted launch and releases its
    /// artifact-ready condition (as for a bundle uploaded over the bridge).
    /// Exits once that condition is released (0) or fails (1).
    Receive {
        #[arg(long)]
        launch_id: String,
        #[arg(long, value_enum)]
        phase: RunPhase,
        #[arg(long)]
        archive: PathBuf,
        #[arg(long)]
        descriptor: PathBuf,
        #[arg(long)]
        socket: Option<String>,
    },
    /// Start (or reattach to) a launch copied onto this Beam by
    /// `beams ssh --profile`: start the daemon, submit the launch, stage
    /// and run pre (printing progress), then replace this process with the
    /// launch's Zellij session. See `launch/session.rs`.
    Start {
        /// Directory holding `launch.json` and `pre/`.
        #[arg(long)]
        launch_dir: PathBuf,
        #[arg(long)]
        socket: Option<String>,
    },
    /// Report shell readiness for `$BEAMS_LAUNCH_ID` (in the background),
    /// then exec `$BEAMS_SHELL` with these arguments. Zellij's
    /// `default_shell` in a launch session (via `beams-shell`).
    Shell {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Verify an archive and atomically publish staging, without installing it.
    Unpack {
        #[arg(long)]
        archive: PathBuf,
        #[arg(long)]
        descriptor: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
}
#[derive(Clone, Copy, ValueEnum)]
pub enum RunPhase {
    Pre,
    Post,
}

impl From<RunPhase> for Phase {
    fn from(phase: RunPhase) -> Self {
        match phase {
            RunPhase::Pre => Phase::Pre,
            RunPhase::Post => Phase::Post,
        }
    }
}

pub fn run(command: Command) -> anyhow::Result<()> {
    match command {
        Command::Submit { launch, staging, headless, no_follow, socket } => {
            let mut conn = connect(socket)?;
            let reply = request(
                &mut conn,
                json!({
                    "op": "launch_submit",
                    "launch": std::path::absolute(&launch)?,
                    "staging": std::path::absolute(&staging)?,
                    "gate": if headless { "headless" } else { "interactive" },
                }),
            )?;
            follow(&mut conn, &reply, no_follow)?;
        }
        Command::Signal { launch_id, condition, socket } => {
            let reply = request(
                &mut connect(socket)?,
                json!({"op": "launch_signal", "launch_id": launch_id, "condition": condition}),
            )?;
            println!("{}", reply["message"].as_str().unwrap_or_default());
            if reply["released"] != true {
                anyhow::bail!("{condition} was not released");
            }
        }
        Command::Status { launch_id, json, socket } => {
            let reply = request(
                &mut connect(socket)?,
                json!({"op": "launch_snapshot", "launch_id": launch_id}),
            )?;
            if json {
                println!("{}", serde_json::to_string_pretty(&reply["launches"])?);
            } else {
                let launches: Vec<daemon::LaunchSnapshot> = serde_json::from_value(reply["launches"].clone())?;
                if launches.is_empty() {
                    println!("no launches");
                }
                for launch in &launches {
                    print_snapshot(launch);
                }
            }
        }
        Command::Ready { component, launch_id, socket } => {
            let Some(launch_id) = launch_id
                .or_else(|| std::env::var("BEAMS_LAUNCH_ID").ok())
                .filter(|id| !id.is_empty())
            else {
                return Ok(());
            };
            request(
                &mut connect(socket)?,
                json!({"op": "launch_ready", "launch_id": launch_id, "component": component}),
            )?;
        }
        Command::Start { launch_dir, socket } => session::start(&launch_dir, socket)?,
        Command::Shell { args } => session::shell(&args)?,
        Command::Receive { launch_id, phase, archive, descriptor, socket } => {
            receive(socket, &launch_id, phase.into(), &archive, &descriptor)?;
        }
        Command::Retry { launch_id, retry_interrupted, no_follow, socket } => {
            let mut conn = connect(socket)?;
            let reply = request(
                &mut conn,
                json!({"op": "launch_retry", "launch_id": launch_id, "retry_interrupted": retry_interrupted}),
            )?;
            follow(&mut conn, &reply, no_follow)?;
        }
        Command::RunPhase {
            launch,
            staging,
            phase,
            headless,
            state_dir,
            retry_interrupted,
        } => {
            let declaration: Declaration = beams_artifacts::read_json(&launch)?;
            let home = PathBuf::from(std::env::var("HOME")?);
            let state = state_dir.unwrap_or_else(|| home.join(".beam-bootstrap/launch-state"));
            let phase = Phase::from(phase);
            let id = declaration.launch_id.clone();
            let options = runner::Options {
                home,
                state,
                phase,
                retry_interrupted,
            };
            let conditions = std::sync::Arc::new(runner::ForegroundConditions::new(staging, headless));
            let ok = runner::run(
                declaration,
                options,
                &crate::executor::Resources::default(),
                conditions,
                &|event| println!("{}", crate::daemon::task_event_line(event, Some(&id))),
            )?;
            if !ok {
                anyhow::bail!("launch phase failed; inspect failed/blocked task events");
            }
        }
        Command::InstallPackages { packages } => {
            let result = crate::packages::PackageManager::default()
                .install(&packages, &|line| eprintln!("{line}"))?;
            println!(
                "{}",
                result.unwrap_or_else(|| "packages already installed".into())
            );
        }
        Command::Unpack {
            archive,
            descriptor,
            output,
        } => {
            let descriptor: BundleDescriptor = beams_artifacts::read_json(&descriptor)?;
            let manifest = beams_artifacts::receive(&archive, &descriptor, &output)?;
            println!(
                "Verified {} {}: {} artifacts staged at {}",
                descriptor.launch_id,
                descriptor.phase.name(),
                manifest.artifacts.len(),
                output.display()
            );
        }
    }
    Ok(())
}

fn connect(socket: Option<String>) -> anyhow::Result<crate::taskcli::Connection> {
    let path = crate::pathutil::expand_tilde(&socket.unwrap_or_else(crate::daemon::default_socket_path));
    crate::taskcli::Connection::connect(&path)
}

/// Sends one op and returns its direct reply (failing on an error reply).
fn request(conn: &mut crate::taskcli::Connection, mut op: Value) -> anyhow::Result<Value> {
    op["request_id"] = json!("launch-cli");
    conn.send(&op)?;
    loop {
        let reply = conn.recv()?.context("the daemon closed the connection without replying")?;
        if reply["request_id"] != "launch-cli" {
            continue;
        }
        if reply["type"] == "error" {
            anyhow::bail!("{}", reply["message"].as_str().unwrap_or("unknown error"));
        }
        return Ok(reply);
    }
}

/// `launch receive`: waits for the condition's release or failure, which
/// may be reported before the reply; a snapshot covers an archive that
/// was already staged (and so releases nothing new).
fn receive(
    socket: Option<String>,
    launch_id: &str,
    phase: Phase,
    archive: &std::path::Path,
    descriptor: &std::path::Path,
) -> anyhow::Result<()> {
    let condition = format!("artifact-ready:{}", phase.name());
    let mut conn = connect(socket)?;
    conn.send(&json!({
        "request_id": "launch-cli",
        "op": "launch_receive",
        "launch_id": launch_id,
        "phase": phase,
        "archive": std::path::absolute(archive)?,
        "descriptor": std::path::absolute(descriptor)?,
    }))?;
    while let Some(event) = conn.recv()? {
        let ours = event["request_id"] == "launch-cli";
        match event["type"].as_str() {
            Some("error") if ours => anyhow::bail!("{}", event["message"].as_str().unwrap_or("unknown error")),
            Some("launch_receiving") if ours => {
                conn.send(&json!({"request_id": "launch-cli", "op": "launch_snapshot", "launch_id": launch_id}))?;
            }
            Some("launch_snapshot") if ours => {
                let launches: Vec<daemon::LaunchSnapshot> = serde_json::from_value(event["launches"].clone())?;
                if launches.iter().any(|l| l.released.contains(&condition)) {
                    println!("{condition} released");
                    return Ok(());
                }
            }
            Some("launch_condition") if event["launch_id"] == launch_id && event["condition"] == condition.as_str() => {
                let message = event["message"].as_str().map(|m| format!(": {m}")).unwrap_or_default();
                if event["released"] == true {
                    println!("{condition} released{message}");
                    return Ok(());
                }
                anyhow::bail!("{condition} failed{message}");
            }
            _ => {}
        }
    }
    anyhow::bail!("the daemon closed the connection before {condition} was released")
}

fn print_snapshot(launch: &daemon::LaunchSnapshot) {
    println!(
        "{} {:?} ({:?} gate){}",
        launch.launch_id,
        launch.status,
        launch.gate,
        launch.message.as_deref().map(|m| format!(": {m}")).unwrap_or_default()
    );
    if !launch.ready.is_empty() || !launch.released.is_empty() {
        println!("  ready: [{}]; released: [{}]", launch.ready.join(", "), launch.released.join(", "));
    }
    for node in &launch.nodes {
        println!(
            "  {:<11} {:<4} {}{}",
            format!("{:?}", node.status).to_lowercase(),
            node.phase.name(),
            node.label,
            node.message.as_deref().map(|m| format!(" - {m}")).unwrap_or_default()
        );
    }
}

/// Prints a launch's progress from an accepting reply (a snapshot
/// boundary) until it finishes.
fn follow(conn: &mut crate::taskcli::Connection, reply: &Value, no_follow: bool) -> anyhow::Result<()> {
    let launch: daemon::LaunchSnapshot = serde_json::from_value(reply["launch"].clone())?;
    let id = launch.launch_id.clone();
    if reply["existing"] == true {
        println!("already accepted:");
    }
    print_snapshot(&launch);
    let terminal = |status: daemon::LaunchStatus| match status {
        daemon::LaunchStatus::Ok => Some(Ok(())),
        daemon::LaunchStatus::Failed => Some(Err(anyhow::anyhow!("launch {id} failed"))),
        daemon::LaunchStatus::Interrupted => Some(Err(anyhow::anyhow!(
            "launch {id} was interrupted; `beam-bootstrap launch retry --launch-id {id}` resumes it"
        ))),
        _ => None,
    };
    if let Some(done) = terminal(launch.status) {
        return done;
    }
    if no_follow {
        return Ok(());
    }
    let boundary = reply["seq"].as_u64().unwrap_or_default();
    while let Some(event) = conn.recv()? {
        if event["launch_id"] != id.as_str() || event["seq"].as_u64().is_none_or(|seq| seq <= boundary) {
            continue;
        }
        match event["type"].as_str() {
            Some("task_status") => println!(
                "  {:<11} {}{}",
                if event["waiting"] == true { "waiting" } else { event["status"].as_str().unwrap_or("?") },
                event["node"].as_str().unwrap_or("?"),
                event["message"].as_str().map(|m| format!(" - {m}")).unwrap_or_default()
            ),
            Some("launch_condition") => println!(
                "  {} {}{}",
                if event["released"] == true { "released" } else { "failed" },
                event["condition"].as_str().unwrap_or("?"),
                event["message"].as_str().map(|m| format!(" - {m}")).unwrap_or_default()
            ),
            Some("launch_status") => {
                let status: daemon::LaunchStatus = serde_json::from_value(event["status"].clone())?;
                println!(
                    "launch {:?}{}",
                    status,
                    event["message"].as_str().map(|m| format!(": {m}")).unwrap_or_default()
                );
                if let Some(done) = terminal(status) {
                    return done;
                }
            }
            _ => {}
        }
    }
    anyhow::bail!("the daemon closed the connection before launch {id} finished")
}
