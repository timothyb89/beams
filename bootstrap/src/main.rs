mod events;
mod graph;
mod steps;
mod tasks;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use tasks::checkout_pr_worktree::CheckoutPrWorktree;
use tasks::clone_git_repo::CloneGitRepo;
use tasks::{Task, Template};

/// Bootstraps a fresh (or partially set up) Teleport Beam: links this
/// repo's dotfiles/bin into $HOME, installs apt packages, and runs
/// `claude update`. Safe to re-run.
#[derive(Parser)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,

    /// Path to the beams repo checkout to bootstrap from. Only used when
    /// running the full startup sequence (no subcommand given).
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

#[derive(Subcommand)]
enum Command {
    /// List available templated tasks (name only - no param schema yet,
    /// see `tasks::Template`'s doc comment).
    ListTasks,
    /// Instantiate and run a single templated task on demand, outside
    /// the fixed startup sequence. A stand-in for the daemon-backed
    /// `beams task run` described in DESIGN.md - until that exists, this
    /// runs the task directly, in-process, and does not check or resolve
    /// its declared dependencies against anything else.
    RunTask {
        #[command(subcommand)]
        task: TaskCommand,
    },
}

#[derive(Subcommand)]
enum TaskCommand {
    /// Clone a git repo to a destination path (see
    /// `tasks::clone_git_repo`).
    CloneGitRepo {
        /// Repo to clone - anything `git clone` itself accepts.
        #[arg(long)]
        repo: String,
        /// Destination path for the checkout.
        #[arg(long)]
        dest: String,
    },
    /// Check out a PR's head ref from an already-cloned repo's `origin`
    /// into a new worktree (see `tasks::checkout_pr_worktree`).
    CheckoutPrWorktree {
        /// Path to the already-cloned base repo.
        #[arg(long)]
        base: String,
        /// PR number to check out.
        #[arg(long)]
        pr: u32,
        /// Destination path for the new worktree.
        #[arg(long)]
        dest: String,
    },
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
    match args.command {
        Some(Command::ListTasks) => list_tasks(),
        Some(Command::RunTask { task }) => run_task(task),
        None => run_bootstrap(&args),
    }
}

fn list_tasks() {
    for name in [CloneGitRepo::NAME, CheckoutPrWorktree::NAME] {
        println!("{name}");
    }
}

fn run_bootstrap(args: &Args) {
    let repo = expand_tilde(&args.repo);
    let mut emitter = events::default_emitter(args.json);

    let ok = match &args.only {
        Some(step_id) => steps::run_only(&repo, &mut *emitter, step_id),
        None => steps::run_all(&repo, &mut *emitter),
    };
    std::process::exit(if ok { 0 } else { 1 });
}

fn run_task(command: TaskCommand) {
    let task: Box<dyn Task> = match command {
        TaskCommand::CloneGitRepo { repo, dest } => {
            Box::new(CloneGitRepo { repo, dest: expand_tilde(&dest) })
        }
        TaskCommand::CheckoutPrWorktree { base, pr, dest } => Box::new(CheckoutPrWorktree {
            base: expand_tilde(&base),
            pr,
            dest: expand_tilde(&dest),
        }),
    };

    match task.run() {
        Ok(Some(message)) => println!("ok: {message}"),
        Ok(None) => println!("skip: nothing to do"),
        Err(err) => {
            eprintln!("error: {err}");
            std::process::exit(1);
        }
    }
}
