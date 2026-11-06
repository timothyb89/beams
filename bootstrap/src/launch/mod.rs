//! Explicit launch tooling; never restarts the daemon or runs fixed startup.
mod files;
mod runner;
#[cfg(test)]
mod tests;

use beams_protocol::launch::{BundleDescriptor, Declaration, Phase};
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

pub fn run(command: Command) -> anyhow::Result<()> {
    match command {
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
            let phase = match phase {
                RunPhase::Pre => Phase::Pre,
                RunPhase::Post => Phase::Post,
            };
            let id = declaration.launch_id.clone();
            let options = runner::Options {
                home,
                staging,
                state,
                phase,
                headless,
                retry_interrupted,
            };
            let ok = runner::run(declaration, options, &|event| {
                println!("{}", crate::daemon::task_event_line(event, Some(&id)))
            })?;
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
