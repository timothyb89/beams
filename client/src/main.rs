mod stage;
mod state;
mod tsh;

use std::path::{Path, PathBuf};

use anyhow::bail;
use chrono::{DateTime, Duration, Utc};
use clap::{Parser, Subcommand};

const MIN_TTL_HOURS: i64 = 12;
const MAX_CONCURRENT_BEAMS: usize = 5;
const READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);
const DEFAULT_LOGIN: &str = "beams";
const DEFAULT_PURPOSE: &str = "default";
/// Where the repo lands on the beam, relative to its `$HOME` - matches
/// what `bin/beam-shell`, the zellij layout, etc. all assume (`~/beams`).
const REMOTE_REPO_PATH: &str = "beams";

/// Spawn or reuse a Teleport Beam and drop straight into it.
#[derive(Parser)]
#[command(name = "beams")]
struct Args {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Connect to a beam, spawning or reusing one as needed.
    Ssh {
        /// Path to the beams repo checkout to copy onto a freshly spawned
        /// beam. Ignored when reusing an existing beam.
        #[arg(long, default_value = "~/beams")]
        repo: String,
        /// Named purpose this beam serves. Lets more than one beam be
        /// tracked independently - e.g. a side task vs. the default one
        /// plain `beams ssh` reuses.
        #[arg(long, default_value = DEFAULT_PURPOSE)]
        purpose: String,
        /// Region to request if a new beam needs to be created.
        #[arg(long)]
        region: Option<String>,
        /// Login to connect as.
        #[arg(long, default_value = DEFAULT_LOGIN)]
        login: String,
    },
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    match args.command {
        Commands::Ssh {
            repo,
            purpose,
            region,
            login,
        } => ssh(&expand_tilde(&repo), &purpose, region.as_deref(), &login),
    }
}

fn ssh(repo: &Path, purpose: &str, region: Option<&str>, login: &str) -> anyhow::Result<()> {
    let mut state = state::State::load()?;
    let beams = tsh::list_beams()?;

    let existing = state
        .purposes
        .get(purpose)
        .and_then(|entry| beams.iter().find(|b| b.uuid == entry.uuid));

    let beam = match existing.filter(|b| has_ttl_remaining(b, MIN_TTL_HOURS)) {
        Some(beam) => {
            eprintln!(
                "beams: reusing beam {} ({}) for purpose {purpose:?}",
                beam.id, beam.uuid
            );
            beam.clone()
        }
        None => {
            if beams.len() >= MAX_CONCURRENT_BEAMS {
                bail!(
                    "already at the {MAX_CONCURRENT_BEAMS}-beam limit ({} active) - clean one up \
                     with `tsh beams rm` first",
                    beams.len()
                );
            }

            eprintln!("beams: no usable beam for purpose {purpose:?}, spawning a new one...");
            let fresh = tsh::add_beam(region)?;

            eprintln!(
                "beams: waiting for {} to accept SSH connections...",
                fresh.id
            );
            tsh::wait_until_ready(&fresh.id, login, READY_TIMEOUT)?;

            eprintln!("beams: pushing {} to {}...", repo.display(), fresh.id);
            let branch = tsh::current_branch(repo)?;
            let origin_url = tsh::origin_url(repo)?;
            tsh::prepare_git_target(login, &fresh.uuid, REMOTE_REPO_PATH, &branch, origin_url.as_deref())?;
            tsh::push_repo(repo, login, &fresh.uuid, REMOTE_REPO_PATH, &branch)?;

            eprintln!("beams: copying the ssh key and bootstrap binary to {}...", fresh.id);
            let staging = stage::build(repo)?;
            let result = tsh::scp_dir(&staging.join("beams"), &fresh.id);
            let _ = std::fs::remove_dir_all(&staging);
            result?;

            // Only now that the beam is actually usable - track it for
            // this purpose. Saving state before scp succeeds would leave
            // a purpose pointing at a beam that never got bootstrapped,
            // with nothing short of a `tsh beams rm` ever fixing it (the
            // TTL check would keep "reusing" it indefinitely).
            state.purposes.insert(
                purpose.to_string(),
                state::PurposeEntry {
                    beam_id: fresh.id.clone(),
                    uuid: fresh.uuid.clone(),
                    created_at: Utc::now().to_rfc3339(),
                },
            );
            state.save()?;

            fresh
        }
    };

    eprintln!("beams: connecting to {} ({})...", beam.id, beam.uuid);
    tsh::ssh_exec(login, &beam.uuid, &["~/beams/bin/beam-shell".to_string()])
}

/// Does `beam` have more than `hours` of TTL left? Beams can't have their
/// TTL extended (project.md), so this is the only "is it still worth
/// reusing" signal available.
fn has_ttl_remaining(beam: &tsh::BeamInfo, hours: i64) -> bool {
    match DateTime::parse_from_rfc3339(&beam.expires) {
        Ok(expires) => expires.signed_duration_since(Utc::now()) > Duration::hours(hours),
        Err(_) => false,
    }
}

fn expand_tilde(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => {
            let home = std::env::var("HOME").unwrap_or_default();
            PathBuf::from(home).join(rest)
        }
        None => PathBuf::from(path),
    }
}
