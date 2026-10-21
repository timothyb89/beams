mod events;
mod graph;
mod steps;

use std::path::PathBuf;

use clap::Parser;

/// Bootstraps a fresh (or partially set up) Teleport Beam: links this
/// repo's dotfiles/bin into $HOME, installs apt packages, and runs
/// `claude update`. Safe to re-run.
#[derive(Parser)]
struct Args {
    /// Path to the beams repo checkout to bootstrap from.
    #[arg(long, default_value = "~/beams")]
    repo: String,

    /// Force JSON-lines event output instead of human-readable, even on a
    /// TTY. Useful for piping to a future client wrapper.
    #[arg(long)]
    json: bool,

    /// Run only the step with this id (e.g. `link-files`), instead of the
    /// full sequence.
    #[arg(long)]
    only: Option<String>,
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

fn main() {
    let args = Args::parse();
    let repo = expand_tilde(&args.repo);
    let mut emitter = events::default_emitter(args.json);

    let ok = match &args.only {
        Some(step_id) => steps::run_only(&repo, &mut *emitter, step_id),
        None => steps::run_all(&repo, &mut *emitter),
    };
    std::process::exit(if ok { 0 } else { 1 });
}
