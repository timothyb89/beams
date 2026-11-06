//! Offline launch configuration tools. Live `ssh` retains the existing startup
//! path until the execution/transport milestones are implemented.

mod pack;
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

pub fn run(command: Command) -> anyhow::Result<u8> {
    match command {
        Command::Pack {
            profile,
            output,
            compression,
        } => {
            let compression = match compression {
                Codec::None => Compression::None,
                Codec::Gzip => Compression::Gzip,
            };
            let declaration = pack::prepare(&profile, &output, compression)?;
            println!(
                "Prepared {} at {} (pre only; post sources deferred)",
                declaration.launch_id,
                output.display()
            );
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
