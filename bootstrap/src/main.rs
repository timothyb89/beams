mod daemon;
mod events;
mod executor;
mod graph;
mod pathutil;
mod steps;
mod tasks;
mod tui;

use clap::{Parser, Subcommand};
use serde_json::json;

use pathutil::expand_tilde;
use tasks::Template;

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
    /// List available templated tasks and their param schema (see
    /// `tasks::TemplateInfo`).
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
    /// Run the long-lived daemon: listens on a UDS socket for
    /// `list_tasks`/`run_task` requests (see `daemon`), broadcasting
    /// task-status events to every connected client. Meant to be
    /// supervised by `beamctl start`, not run directly for day-to-day
    /// use yet - nothing drives this from the fixed startup sequence or
    /// a TUI yet (see `DESIGN.md`'s migration plan).
    Daemon {
        /// UDS socket path to listen on. Defaults to
        /// `~/.beam-bootstrap/daemon.sock`.
        #[arg(long)]
        socket: Option<String>,
    },
    /// Ratatui client for the daemon (see `tui`): deferred/templated
    /// tasks with param input on top, concurrent per-task progress rows
    /// plus an aggregate gauge on the bottom. Requires a daemon already
    /// listening (`beam-bootstrap daemon`) - doesn't spawn one itself.
    Tui {
        /// UDS socket path to connect to. Defaults to
        /// `~/.beam-bootstrap/daemon.sock`.
        #[arg(long)]
        socket: Option<String>,
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

fn main() {
    let args = Args::parse();
    match args.command {
        Some(Command::ListTasks) => list_tasks(),
        Some(Command::RunTask { task }) => run_task(task),
        Some(Command::Daemon { socket }) => run_daemon(socket),
        Some(Command::Tui { socket }) => run_tui(socket),
        None => run_bootstrap(&args),
    }
}

fn list_tasks() {
    for info in tasks::templates() {
        let params: Vec<String> = info
            .params
            .iter()
            .map(|p| format!("{}{}: {}", p.name, if p.required { "" } else { "?" }, p.kind))
            .collect();
        println!("{} ({})", info.template, params.join(", "));
    }
}

fn run_bootstrap(args: &Args) {
    let repo = expand_tilde(&args.repo);
    let emitter = events::default_emitter(args.json);

    let ok = match &args.only {
        Some(step_id) => steps::run_only(&repo, &*emitter, step_id),
        None => steps::run_all(&repo, &*emitter),
    };
    std::process::exit(if ok { 0 } else { 1 });
}

/// Builds the `(template name, JSON params)` pair for a `TaskCommand` -
/// goes through `tasks::instantiate` from there, the same path the
/// daemon's `run_task` op uses, rather than constructing task structs
/// directly (see that function's doc comment).
fn run_task(command: TaskCommand) {
    let (name, params) = match command {
        TaskCommand::CloneGitRepo { repo, dest } => {
            (tasks::clone_git_repo::CloneGitRepo::NAME, json!({"repo": repo, "dest": dest}))
        }
        TaskCommand::CheckoutPrWorktree { base, pr, dest } => (
            tasks::checkout_pr_worktree::CheckoutPrWorktree::NAME,
            json!({"base": base, "pr": pr, "dest": dest}),
        ),
    };

    let task = match tasks::instantiate(name, params) {
        Ok(task) => task,
        Err(err) => {
            eprintln!("error: {err}");
            std::process::exit(1);
        }
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

fn run_daemon(socket: Option<String>) {
    let socket_path = expand_tilde(&socket.unwrap_or_else(daemon::default_socket_path));
    if let Err(err) = daemon::serve(&socket_path) {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}

fn run_tui(socket: Option<String>) {
    let socket_path = expand_tilde(&socket.unwrap_or_else(daemon::default_socket_path));
    if let Err(err) = tui::run(&socket_path) {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}
