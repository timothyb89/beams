//! Launch configuration: offline tools (`validate`/`explain`/`pack`),
//! `config launch` onto an existing Beam, and the profile selection
//! `beams ssh --profile` uses (`[launch] profile` in `config.toml`).

pub(crate) mod pack;
mod plan;
mod profile;
#[cfg(test)]
mod tests;

use beams_protocol::launch::Compression;
use clap::{Subcommand, ValueEnum};
use plan::{Action, Compilation};
use std::path::PathBuf;

#[derive(Subcommand)]
pub enum Command {
    /// Freeze a declaration and pre archive without contacting a Beam.
    Pack {
        #[arg(long, default_value = "builtin:development")]
        profile: String,
        #[arg(long)]
        output: PathBuf,
        #[arg(long, value_enum, default_value = "gzip")]
        compression: Codec,
    },
    /// Snapshot post sources of a prepared launch; reuse a completed spool.
    PackPost {
        #[arg(long)]
        launch_dir: PathBuf,
    },
    /// Pack a profile and drive it onto an existing Beam through the local
    /// daemon: the declaration and pre bundle are uploaded over the Beam's
    /// bridge and submitted; post is packed and uploaded once the Beam's
    /// interface is ready. Follows the launch until it finishes (exit 1 if
    /// it fails). Without `--headless`, post work waits for the Beam's
    /// initial shell and TUI to report ready (see
    /// docs/development/launches.md).
    Launch {
        #[arg(long, default_value = "builtin:development")]
        profile: String,
        /// Beam id, uuid, or tracked purpose (default: the `default` purpose).
        target: Option<String>,
        #[arg(long, default_value = "beams")]
        login: String,
        /// Release post work as soon as pre succeeds, without waiting for
        /// a shell or TUI.
        #[arg(long)]
        headless: bool,
        /// Return once the daemon has the launch instead of following it.
        #[arg(long)]
        no_follow: bool,
        #[arg(long, value_enum, default_value = "gzip")]
        compression: Codec,
    },
    /// Validate a launch profile without connecting to or modifying a Beam.
    Validate {
        #[arg(long, default_value = "builtin:development")]
        profile: String,
    },
    /// Show effective values, origins, batches, artifacts and dependencies.
    Explain {
        #[arg(long, default_value = "builtin:development")]
        profile: String,
        /// Include workstation source metadata as structured JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Codec {
    None,
    Gzip,
}

impl From<Codec> for Compression {
    fn from(codec: Codec) -> Self {
        match codec {
            Codec::None => Compression::None,
            Codec::Gzip => Compression::Gzip,
        }
    }
}

/// `[launch]` in `config.toml`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LaunchConfig {
    /// Profile `beams ssh` launches new Beams with when `--profile` isn't
    /// given: `builtin:NAME`, or a path (relative to `config.toml`'s
    /// directory). Unset: `DEFAULT_PROFILE`.
    profile: Option<String>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct ConfigFile {
    #[serde(default)]
    launch: LaunchConfig,
    /// The removed `[startup]` section, only to refuse it with directions.
    startup: Option<toml::Value>,
}

/// The profile for `beams ssh` without `--profile` or `[launch] profile`.
pub const DEFAULT_PROFILE: &str = "builtin:development";

/// A profile argument as recorded and compiled: bundled names as given,
/// paths made absolute against `base`.
pub fn resolve_profile(profile: &str, base: &std::path::Path) -> String {
    if profile.starts_with("builtin:") {
        return profile.to_string();
    }
    let path = crate::expand_tilde(profile);
    if path.is_absolute() { path } else { base.join(path) }.display().to_string()
}

/// The workstation's `[launch] profile`, resolved, else `DEFAULT_PROFILE`.
/// A leftover `[startup]` section is an error: it would otherwise be
/// silently ignored.
pub fn default_profile() -> anyhow::Result<String> {
    use anyhow::Context as _;
    let path = crate::agent::Config::path()?;
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(DEFAULT_PROFILE.to_string()),
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };
    let file: ConfigFile = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    if file.startup.is_some() {
        anyhow::bail!(
            "{}: [startup] is no longer supported; move its tasks (and [startup.ssh_grant], as \
             [ssh_grant]) into a profile and set [launch] profile (see docs/development/launches.md)",
            path.display()
        );
    }
    let base = path.parent().unwrap_or(std::path::Path::new("."));
    Ok(file.launch.profile.map_or_else(|| DEFAULT_PROFILE.to_string(), |p| resolve_profile(&p, base)))
}

/// Packs `profile` into a fresh launch directory under the local daemon's
/// launches dir, for `beams ssh`.
pub fn pack_for_ssh(profile: &str) -> anyhow::Result<(PathBuf, beams_protocol::launch::Declaration)> {
    let output = crate::clientd::launches_dir()?.join(uuid::Uuid::new_v4().to_string());
    std::fs::create_dir_all(output.parent().expect("has a parent"))?;
    let declaration = pack::prepare(profile, &output, Compression::Gzip)?;
    Ok((output, declaration))
}

fn launch(
    profile: &str,
    target: Option<&str>,
    login: &str,
    headless: bool,
    no_follow: bool,
    compression: Compression,
) -> anyhow::Result<u8> {
    use crate::clientd::{LaunchStage, ServerEvent};
    use beams_protocol::launch::{Gate, LaunchStatus};

    let mut state = crate::state::State::load()?;
    let located = crate::beamcmd::locate(target.unwrap_or(crate::DEFAULT_PURPOSE), &mut state)?;
    let target = crate::bridge::Target {
        beam_id: located.beam.id.clone(),
        beam_uuid: located.beam.uuid.clone(),
        login: login.to_string(),
        endpoint: Some(located.endpoint),
    };
    let output = crate::clientd::launches_dir()?.join(uuid::Uuid::new_v4().to_string());
    std::fs::create_dir_all(output.parent().expect("has a parent"))?;
    let declaration = pack::prepare(profile, &output, compression)?;
    let id = declaration.launch_id;
    eprintln!("beams: launch {id} ({profile}) onto {}, packed at {}", target.beam_id, output.display());

    let mut frontend = crate::clientd::connect_or_start()?;
    frontend.ensure_bridge(&target, &uuid::Uuid::new_v4().to_string())?;
    let gate = if headless { Gate::Headless } else { Gate::Interactive };
    frontend.start_launch(&target, &output, gate)?;
    let mut last = String::new();
    while let Ok(event) = frontend.events.recv() {
        let launch = match event {
            ServerEvent::LaunchChanged { launch } if launch.launch_id == id => launch,
            ServerEvent::Error { message } => anyhow::bail!("{message}"),
            _ => continue,
        };
        if no_follow {
            return Ok(0);
        }
        let mut line = format!(
            "{}{}",
            launch.status.map(|s| format!("{s:?}").to_lowercase()).unwrap_or_else(|| "submitting".into()),
            launch.message.as_deref().map(|m| format!(": {m}")).unwrap_or_default()
        );
        if let Some(upload) = &launch.upload {
            line.push_str(&format!(" ({} {}/{} bytes)", upload.blob.name(), upload.sent, upload.total));
        }
        if line != last {
            eprintln!("beams: {line}");
            last = line;
        }
        if launch.stage == LaunchStage::Finished {
            return Ok(if launch.status == Some(LaunchStatus::Ok) { 0 } else { 1 });
        }
    }
    anyhow::bail!("the local daemon disconnected before launch {id} finished")
}

pub fn run(command: Command) -> anyhow::Result<u8> {
    match command {
        Command::Pack {
            profile,
            output,
            compression,
        } => {
            let declaration = pack::prepare(&profile, &output, compression.into())?;
            println!(
                "Prepared {} at {} (pre only; post sources deferred)",
                declaration.launch_id,
                output.display()
            );
        }
        Command::Launch { profile, target, login, headless, no_follow, compression } => {
            return launch(&profile, target.as_deref(), &login, headless, no_follow, compression.into());
        }
        Command::PackPost { launch_dir } => {
            let descriptor = pack::prepare_post(&launch_dir)?;
            println!(
                "Post bundle: {} bytes, sha256 {}",
                descriptor.bytes, descriptor.sha256
            );
        }
        Command::Validate { profile } => {
            let compilation = plan::compile(&profile)?;
            println!(
                "Valid: {profile} ({} nodes, {} artifacts)",
                compilation.plan.nodes.len(),
                compilation.plan.artifacts.len()
            );
        }
        Command::Explain { profile, json } => {
            let compilation = plan::compile(&profile)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&compilation)?);
            } else {
                print!("{}", explain(&compilation));
            }
        }
    }
    Ok(0)
}

fn explain(compilation: &Compilation) -> String {
    use std::fmt::Write;
    let mut out = String::from("Offline launch plan v1 (execution wiring pending)\n");
    writeln!(out, "Profiles: {}", compilation.profiles.join(" -> ")).unwrap();
    writeln!(
        out,
        "Shell: {}",
        compilation.plan.shell.program.as_deref().unwrap()
    )
    .unwrap();
    writeln!(
        out,
        "PATH entries: {}",
        compilation.plan.shell.path.as_ref().unwrap().join(", ")
    )
    .unwrap();
    writeln!(
        out,
        "Change login shell: {}",
        compilation.plan.shell.change_login_shell.unwrap()
    )
    .unwrap();
    for (key, value) in &compilation.plan.env {
        writeln!(out, "Environment: {key}={value}").unwrap();
    }
    if let Some(grant) = &compilation.ssh_grant {
        writeln!(
            out,
            "SSH grant: {} as {}, {} uses within {}s from {}; covers {}",
            grant.hosts.join(", "),
            grant.user,
            grant.max_uses,
            grant.window_secs,
            if grant.phase == beams_protocol::launch::Phase::Pre { "acceptance" } else { "interface-ready" },
            grant.nodes.join(", ")
        )
        .unwrap();
    }
    for (key, origin) in &compilation.origins {
        if key.starts_with("shell.") || key.starts_with("env.") || key.starts_with("package:") {
            writeln!(out, "  {key} <- {}", origin.context()).unwrap();
        }
    }
    for artifact in &compilation.plan.artifacts {
        writeln!(
            out,
            "Artifact {} [{}]: {:?}; used by {}",
            artifact.id,
            artifact.phase.name(),
            compilation.sources[&artifact.id],
            artifact.used_by.join(", ")
        )
        .unwrap();
    }
    for note in &compilation.notes {
        writeln!(out, "Note: {note}").unwrap();
    }
    writeln!(
        out,
        "Graph (topological display; independent work may overlap):"
    )
    .unwrap();
    for node in &compilation.plan.nodes {
        let description = match &node.action {
            Action::Packages { packages } => format!(
                "packages: {}",
                if packages.is_empty() {
                    "(empty; skip)".into()
                } else {
                    packages.join(", ")
                }
            ),
            Action::Script {
                artifact,
                entrypoint,
                ..
            } => format!("script: {artifact}/{entrypoint}"),
            Action::File { target, .. } => format!("file: {target}"),
            Action::Builtin { template, .. } => format!("template: {template}"),
            Action::Condition => "readiness condition".into(),
            Action::Runtime => "core runtime".into(),
        };
        writeln!(
            out,
            "  {} [{}] {} ({description})",
            node.id,
            node.phase.name(),
            node.label
        )
        .unwrap();
        writeln!(
            out,
            "    after: {}",
            if node.after.is_empty() {
                "(none)".into()
            } else {
                node.after.join(", ")
            }
        )
        .unwrap();
        if !node.resources.is_empty() {
            writeln!(out, "    resources: {}", node.resources.join(", ")).unwrap();
        }
        writeln!(
            out,
            "    origin: {}",
            compilation.origins[&node.id].context()
        )
        .unwrap();
        if let Some(instance) = compilation.origins.get(&format!("instance:{}", node.id)) {
            let definition = &compilation.origins[&node.id];
            if instance.profile != definition.profile || instance.key != definition.key {
                writeln!(out, "    instance origin: {}", instance.context()).unwrap();
            }
        }
    }
    out
}
