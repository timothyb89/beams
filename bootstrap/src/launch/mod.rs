//! Explicit launch tooling; never restarts the daemon or runs fixed startup.

use beams_protocol::launch::BundleDescriptor;
use clap::Subcommand;
use std::path::PathBuf;

#[derive(Subcommand)]
pub enum Command {
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

pub fn run(command: Command) -> anyhow::Result<()> {
    match command {
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
