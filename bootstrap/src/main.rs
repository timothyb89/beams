mod aptlock;
mod daemon;
mod events;
mod executor;
mod graph;
mod pathutil;
mod releaseutil;
mod steps;
mod taskcli;
mod tasks;
mod tui;

use clap::{Parser, Subcommand};
use serde_json::{json, Value};

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
    /// Daemon-backed task operations - the agent-friendly, no-TUI-
    /// required entry point `DESIGN.md`'s architecture diagram calls
    /// `beams task run ...`, and the CLI sugar for `run_manifest` that
    /// TODO.md had as a separate, still-open item - both landed as this
    /// one subcommand group once it became clear they're the same piece
    /// of work. Unlike `list-tasks`/`run-task` above, these talk to an
    /// already-running daemon over its socket rather than running
    /// in-process, so progress is visible to - and broadcast from - any
    /// other connected client (a TUI, another `task run` call) too.
    Task {
        #[command(subcommand)]
        task: DaemonTaskCommand,
    },
}

#[derive(Subcommand)]
enum DaemonTaskCommand {
    /// List available templates via the daemon (confirms one's actually
    /// up and reachable, unlike the in-process `list-tasks` above).
    List {
        #[arg(long)]
        socket: Option<String>,
    },
    /// Submit a single templated task to the daemon and block until it
    /// reaches a terminal state, printing progress as it happens.
    Run {
        #[arg(long)]
        socket: Option<String>,
        #[command(subcommand)]
        task: TaskCommand,
    },
    /// Submit a manifest - several task instances resolved against each
    /// other, per `daemon.rs`'s module doc comment - from a JSON file
    /// (the same `[{"template": ..., "params": {...}}, ...]` shape
    /// `bootstrap/README.md`'s `nc -U` example hand-writes) and block
    /// until the whole manifest finishes.
    RunManifest {
        #[arg(long)]
        socket: Option<String>,
        /// Path to a JSON file containing a `[{"template", "params"},
        /// ...]` array.
        #[arg(long)]
        file: String,
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
    /// Install the Go toolchain via the official release (see
    /// `tasks::install_toolchain_go`). No params.
    InstallToolchainGo,
    /// Install the Rust toolchain via rustup (see
    /// `tasks::install_toolchain_rust`). No params.
    InstallToolchainRust,
    /// Install code-server and background it as a beamctl service (see
    /// `tasks::install_code_server`). No params.
    InstallCodeServer,
    /// Wait for `bin/ssh-agent-init`'s interactive unlock to have added
    /// the key to the running ssh-agent (see
    /// `tasks::wait_ssh_agent_unlock`). No params.
    WaitSshAgentUnlock,
}

fn main() {
    let args = Args::parse();
    let exit_code = match args.command {
        Some(Command::ListTasks) => {
            list_tasks();
            0
        }
        Some(Command::RunTask { task }) => run_task(task),
        Some(Command::Daemon { socket }) => {
            run_daemon(socket);
            0 // run_daemon exits the process itself on error; reaching here means a clean shutdown
        }
        Some(Command::Tui { socket }) => {
            run_tui(socket);
            0 // ditto
        }
        Some(Command::Task { task }) => run_daemon_task(task),
        None => {
            run_bootstrap(&args);
            0 // run_bootstrap exits the process itself; see its own doc comment
        }
    };
    std::process::exit(exit_code);
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
/// shared by `run_task` (in-process) and `run_daemon_task`'s `Run`
/// variant (daemon-backed), so the two don't drift on how a `TaskCommand`
/// maps onto `tasks::instantiate`'s actual inputs.
fn task_command_to_instance(command: TaskCommand) -> (&'static str, Value) {
    match command {
        TaskCommand::CloneGitRepo { repo, dest } => {
            (tasks::clone_git_repo::CloneGitRepo::NAME, json!({"repo": repo, "dest": dest}))
        }
        TaskCommand::CheckoutPrWorktree { base, pr, dest } => (
            tasks::checkout_pr_worktree::CheckoutPrWorktree::NAME,
            json!({"base": base, "pr": pr, "dest": dest}),
        ),
        TaskCommand::InstallToolchainGo => {
            (tasks::install_toolchain_go::InstallToolchainGo::NAME, json!({}))
        }
        TaskCommand::InstallToolchainRust => {
            (tasks::install_toolchain_rust::InstallToolchainRust::NAME, json!({}))
        }
        TaskCommand::InstallCodeServer => {
            (tasks::install_code_server::InstallCodeServer::NAME, json!({}))
        }
        TaskCommand::WaitSshAgentUnlock => {
            (tasks::wait_ssh_agent_unlock::WaitSshAgentUnlock::NAME, json!({}))
        }
    }
}

/// Runs a single templated task in-process - no daemon, no dependency
/// resolution against anything else. Goes through `tasks::instantiate`,
/// the same path the daemon's `run_task` op uses, rather than
/// constructing task structs directly.
fn run_task(command: TaskCommand) -> i32 {
    let (name, params) = task_command_to_instance(command);

    let task = match tasks::instantiate(name, params) {
        Ok(task) => task,
        Err(err) => {
            eprintln!("error: {err}");
            return 1;
        }
    };

    match task.run_with_progress(&|line| println!("  {line}")) {
        Ok(Some(message)) => {
            println!("ok: {message}");
            0
        }
        Ok(None) => {
            println!("skip: nothing to do");
            0
        }
        Err(err) => {
            eprintln!("error: {err}");
            1
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

fn run_daemon_task(command: DaemonTaskCommand) -> i32 {
    match command {
        DaemonTaskCommand::List { socket } => daemon_list_tasks(socket),
        DaemonTaskCommand::Run { socket, task } => daemon_run_task(socket, task),
        DaemonTaskCommand::RunManifest { socket, file } => daemon_run_manifest(socket, &file),
    }
}

/// Connects or prints a connection error and returns the exit code a
/// caller should propagate - the one bit of boilerplate every
/// `daemon_*` function below needs before it can do anything else.
fn connect_or_exit_code(socket_path: &std::path::Path) -> Result<taskcli::Connection, i32> {
    taskcli::Connection::connect(socket_path).map_err(|err| {
        eprintln!("error: {err}");
        1
    })
}

fn daemon_list_tasks(socket: Option<String>) -> i32 {
    let socket_path = expand_tilde(&socket.unwrap_or_else(daemon::default_socket_path));
    let mut conn = match connect_or_exit_code(&socket_path) {
        Ok(conn) => conn,
        Err(code) => return code,
    };

    if let Err(err) = conn.send(&json!({"request_id": "1", "op": "list_tasks"})) {
        eprintln!("error: {err}");
        return 1;
    }

    match conn.recv() {
        Ok(Some(value)) if value.get("type").and_then(Value::as_str) == Some("tasks") => {
            for template in value.get("tasks").and_then(Value::as_array).into_iter().flatten() {
                let name = template.get("template").and_then(Value::as_str).unwrap_or("?");
                let params: Vec<String> = template
                    .get("params")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|p| {
                        let name = p.get("name").and_then(Value::as_str).unwrap_or("?");
                        let kind = p.get("kind").and_then(Value::as_str).unwrap_or("?");
                        let required = p.get("required").and_then(Value::as_bool).unwrap_or(true);
                        format!("{name}{}: {kind}", if required { "" } else { "?" })
                    })
                    .collect();
                println!("{name} ({})", params.join(", "));
            }
            0
        }
        Ok(Some(value)) => {
            eprintln!("error: unexpected daemon reply: {value}");
            1
        }
        Ok(None) => {
            eprintln!("error: daemon closed the connection without replying");
            1
        }
        Err(err) => {
            eprintln!("error: {err}");
            1
        }
    }
}

/// Submits one templated task to the daemon (same `TaskCommand` ->
/// `tasks::instantiate` mapping `run_task` uses in-process) and blocks,
/// printing every `task_status` broadcast for it, until it reaches a
/// terminal state - the agent-friendly on-demand entry point
/// `DESIGN.md`'s architecture diagram calls `beams task run ...`.
fn daemon_run_task(socket: Option<String>, task: TaskCommand) -> i32 {
    let socket_path = expand_tilde(&socket.unwrap_or_else(daemon::default_socket_path));
    let (name, params) = task_command_to_instance(task);
    let mut conn = match connect_or_exit_code(&socket_path) {
        Ok(conn) => conn,
        Err(code) => return code,
    };

    let request_id = "1";
    if let Err(err) =
        conn.send(&json!({"request_id": request_id, "op": "run_task", "task": name, "params": params}))
    {
        eprintln!("error: {err}");
        return 1;
    }

    let mut task_id: Option<String> = None;
    loop {
        let value = match conn.recv() {
            Ok(Some(value)) => value,
            Ok(None) => {
                eprintln!("error: daemon closed the connection before the task finished");
                return 1;
            }
            Err(err) => {
                eprintln!("error: {err}");
                return 1;
            }
        };

        match value.get("type").and_then(Value::as_str) {
            // This connection's own direct reply to the `run_task`
            // above - carries the task_id every subsequent broadcast
            // (from *any* connection, not just this one) will use, per
            // `daemon.rs`'s module doc comment.
            Some("accepted") if value.get("request_id").and_then(Value::as_str) == Some(request_id) => {
                let id = value.get("task_id").and_then(Value::as_str).unwrap_or("?").to_string();
                println!("submitted {id}");
                task_id = Some(id);
            }
            Some("error") if value.get("request_id").and_then(Value::as_str) == Some(request_id) => {
                let message = value.get("message").and_then(Value::as_str).unwrap_or("unknown error");
                eprintln!("error: {message}");
                return 1;
            }
            Some("task_status") => {
                let Some(id) = value.get("task_id").and_then(Value::as_str) else { continue };
                // Broadcasts go to every connection - only print/act on
                // the one this call actually submitted.
                if task_id.as_deref() != Some(id) {
                    continue;
                }
                let status = value.get("status").and_then(Value::as_str).unwrap_or("?");
                let message = value.get("message").and_then(Value::as_str);
                match message {
                    Some(message) => println!("{status}: {id} ({message})"),
                    None => println!("{status}: {id}"),
                }
                match status {
                    "ok" | "skip" => return 0,
                    "error" => return 1,
                    _ => {} // "start" - keep waiting
                }
            }
            _ => {} // not something this command cares about
        }
    }
}

/// Submits a manifest read from `file` (the same `[{"template",
/// "params"}, ...]` shape `bootstrap/README.md`'s `nc -U` example
/// hand-writes) and blocks, printing every task's progress as it
/// happens, until the whole manifest reaches a terminal state. The CLI
/// sugar for `run_manifest` TODO.md had as a separate open item.
fn daemon_run_manifest(socket: Option<String>, file: &str) -> i32 {
    let socket_path = expand_tilde(&socket.unwrap_or_else(daemon::default_socket_path));
    let contents = match std::fs::read_to_string(expand_tilde(file)) {
        Ok(contents) => contents,
        Err(err) => {
            eprintln!("error: reading {file}: {err}");
            return 1;
        }
    };
    let tasks: Value = match serde_json::from_str(&contents) {
        Ok(value) => value,
        Err(err) => {
            eprintln!("error: parsing {file} as JSON: {err}");
            return 1;
        }
    };

    let mut conn = match connect_or_exit_code(&socket_path) {
        Ok(conn) => conn,
        Err(code) => return code,
    };

    let request_id = "1";
    if let Err(err) = conn.send(&json!({"request_id": request_id, "op": "run_manifest", "tasks": tasks})) {
        eprintln!("error: {err}");
        return 1;
    }

    let mut manifest_id: Option<String> = None;
    loop {
        let value = match conn.recv() {
            Ok(Some(value)) => value,
            Ok(None) => {
                eprintln!("error: daemon closed the connection before the manifest finished");
                return 1;
            }
            Err(err) => {
                eprintln!("error: {err}");
                return 1;
            }
        };

        match value.get("type").and_then(Value::as_str) {
            Some("accepted") if value.get("request_id").and_then(Value::as_str) == Some(request_id) => {
                let id = value.get("manifest_id").and_then(Value::as_str).unwrap_or("?").to_string();
                println!("submitted manifest {id}, waves: {}", value.get("waves").cloned().unwrap_or(Value::Null));
                manifest_id = Some(id);
            }
            Some("error") if value.get("request_id").and_then(Value::as_str) == Some(request_id) => {
                let message = value.get("message").and_then(Value::as_str).unwrap_or("unknown error");
                eprintln!("error: {message}");
                return 1;
            }
            Some("task_status") => {
                if value.get("manifest_id").and_then(Value::as_str) != manifest_id.as_deref() {
                    continue; // some other connection's standalone task, or a different manifest
                }
                let task_id = value.get("task_id").and_then(Value::as_str).unwrap_or("?");
                let status = value.get("status").and_then(Value::as_str).unwrap_or("?");
                let message = value.get("message").and_then(Value::as_str);
                match message {
                    Some(message) => println!("{status}: {task_id} ({message})"),
                    None => println!("{status}: {task_id}"),
                }
            }
            Some("manifest_status") if value.get("manifest_id").and_then(Value::as_str) == manifest_id.as_deref() => {
                match value.get("status").and_then(Value::as_str) {
                    Some("start") => continue, // already said so via "submitted manifest ..." above
                    Some("ok") => return 0,
                    Some("error") => return 1,
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::Subcommand;

    use super::TaskCommand;

    /// `task run`/`run-task` only reach a template if `TaskCommand` has a
    /// variant for it - easy to forget when adding a new template (it's
    /// exactly what happened for `install-toolchain-go`/`-rust` and
    /// `install-code-server`, all three only reachable via
    /// `run-manifest` until this test existed to catch it). Builds the
    /// actual subcommand list clap's derive generates for `TaskCommand`
    /// rather than hand-maintaining a second list here, so it can't drift
    /// from what clap really exposes.
    #[test]
    fn task_command_covers_every_known_template() {
        let cmd = TaskCommand::augment_subcommands(clap::Command::new("task-run"));
        let mut covered: Vec<String> =
            cmd.get_subcommands().map(|c| c.get_name().to_string()).collect();
        covered.sort();

        let mut expected: Vec<String> = crate::tasks::names().iter().map(|s| s.to_string()).collect();
        expected.sort();

        assert_eq!(
            covered, expected,
            "TaskCommand is missing a variant for some template(s) - \
             `task run`/`run-task` can't reach it without one"
        );
    }
}
