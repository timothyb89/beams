mod agentproxy;
mod agentspike;
mod clientbridge;
mod commandlog;
mod daemon;
mod executor;
mod fetch;
mod graph;
mod install;
mod knownhosts;
mod launch;
mod pathutil;
mod packages;
mod releaseutil;
mod sshagent;
mod taskcli;
mod tasks;
mod tui;

use clap::{Parser, Subcommand, ValueEnum};
use serde_json::{json, Value};
use uuid::Uuid;

use pathutil::expand_tilde;
use tasks::Template;

/// The Beam side of Teleport Beams: the launch runtime (`launch start`
/// and friends, run by `beams ssh`), the daemon behind the TUI, task
/// templates, the workstation bridge and the SSH agent relay. Invoked as
/// `beams`, it's the Beam-side `beams fetch` CLI instead.
#[derive(Parser)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Verified-artifact and launch operations, including `launch start`
    /// (what `beams ssh` runs).
    Launch {
        #[command(subcommand)]
        command: launch::Command,
    },
    /// List available templated tasks and their param schema (see
    /// `tasks::TemplateInfo`).
    ListTasks,
    /// Instantiate and run a single templated task on demand, outside
    /// any launch. A stand-in for the daemon-backed
    /// `beams task run` described in docs/designs/DESIGN.md - until that exists, this
    /// runs the task directly, in-process, and does not check or resolve
    /// its declared dependencies against anything else.
    RunTask {
        #[command(subcommand)]
        task: TaskCommand,
    },
    /// Run the long-lived daemon: listens on a UDS socket for
    /// `list_tasks`/`run_task` requests (see `daemon`), broadcasting
    /// task-status events to every connected client. Meant to be
    /// supervised by `beamctl start` (`launch start` does that), not run
    /// directly for day-to-day use.
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
    /// required entry point `docs/designs/DESIGN.md`'s architecture diagram calls
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
    /// Bridge the workstation daemon's typed stdin/stdout protocol to
    /// the in-VM daemon socket. Normally launched over SSH by `beamsd`.
    ClientBridge {
        /// UDS socket path to connect to. Defaults to
        /// `~/.beam-bootstrap/daemon.sock`.
        #[arg(long)]
        socket: Option<String>,
    },
    /// Inspect managed workstation clients or request a client-owned
    /// service from inside this Beam.
    Client {
        #[command(subcommand)]
        command: ClientCommand,
    },
    /// Phase 0 spike (docs/designs/SSH_AGENT_PROXY_DESIGN.md): a filtering, logging
    /// SSH agent listener in front of a local upstream agent.
    #[command(hide = true)]
    AgentProxySpike {
        /// Socket to listen on (point SSH_AUTH_SOCK here).
        #[arg(long)]
        listen: std::path::PathBuf,
        /// Real agent socket to forward approved requests to.
        #[arg(long)]
        upstream: std::path::PathBuf,
        /// Simulated approval latency before each signature.
        #[arg(long, default_value_t = 0)]
        delay_ms: u64,
        /// Delay before answering identity listings.
        #[arg(long, default_value_t = 0)]
        delay_identities_ms: u64,
        /// known_hosts file used to name verified host keys.
        #[arg(long)]
        known_hosts: Option<std::path::PathBuf>,
        /// Only expose/sign with these SHA256 fingerprints (repeatable).
        #[arg(long = "allow")]
        allow: Vec<String>,
    },
    /// `beams fetch` under this binary's own name (normally run through
    /// the runtime's `beams` link).
    #[command(hide = true)]
    Fetch(fetch::FetchArgs),
}

#[derive(Subcommand)]
enum ClientCommand {
    /// Print the connected workstation's known_hosts entries for HOST
    /// (`name` or `[name]:port`): ssh's `KnownHostsCommand` on this Beam
    /// (see `knownhosts.rs`). Always exits 0; failures go to stderr.
    KnownHosts {
        /// Taken as given (even with a leading `-`) and validated.
        #[arg(allow_hyphen_values = true)]
        host: String,
        #[arg(long)]
        socket: Option<String>,
    },
    /// List workstation clients with live bridges to this Beam.
    List {
        #[arg(long)]
        socket: Option<String>,
    },
    /// Request a workstation-owned service. The local user must approve
    /// every remotely initiated request.
    Request {
        #[arg(long)]
        socket: Option<String>,
        #[command(subcommand)]
        service: ClientServiceCommand,
    },
}

#[derive(Subcommand)]
enum ClientServiceCommand {
    /// Request a loopback-only local port forward to an endpoint in the
    /// Beam. The workstation user must approve it before a listener is
    /// created.
    PortForward {
        #[arg(long, default_value = "127.0.0.1")]
        remote_host: String,
        #[arg(long)]
        remote_port: u16,
        #[arg(long)]
        local_port: Option<u16>,
    },
    /// Request a one-shot file upload from a path selected and confirmed
    /// by the workstation user.
    Upload {
        #[arg(long)]
        remote_path: String,
        #[arg(long)]
        local_path_hint: Option<String>,
        #[arg(long)]
        overwrite: bool,
    },
    /// Request a one-shot download to a path selected and confirmed by
    /// the workstation user.
    Download {
        #[arg(long)]
        remote_path: String,
        #[arg(long)]
        local_path_hint: Option<String>,
        #[arg(long)]
        overwrite: bool,
    },
    /// Request a long-running Mutagen folder sync. The workstation user
    /// selects and confirms the local directory before creation.
    Sync {
        #[arg(long)]
        remote_path: String,
        #[arg(long)]
        local_path_hint: Option<String>,
        #[arg(long, value_enum, default_value_t = ClientSyncMode::TwoWaySafe)]
        mode: ClientSyncMode,
        #[arg(long)]
        include_vcs: bool,
        #[arg(long = "ignore")]
        ignores: Vec<String>,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum ClientSyncMode {
    TwoWaySafe,
    TwoWayResolved,
    OneWaySafe,
    OneWayReplica,
}

impl From<ClientSyncMode> for beams_protocol::FolderSyncMode {
    fn from(mode: ClientSyncMode) -> Self {
        match mode {
            ClientSyncMode::TwoWaySafe => Self::TwoWaySafe,
            ClientSyncMode::TwoWayResolved => Self::TwoWayResolved,
            ClientSyncMode::OneWaySafe => Self::OneWaySafe,
            ClientSyncMode::OneWayReplica => Self::OneWayReplica,
        }
    }
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
    /// Install the pinned starship release to /usr/local/bin (see
    /// `tasks::install_starship`). No params.
    InstallStarship,
    /// Set `git config --global user.name`/`user.email` on the Beam
    /// (see `tasks::configure_git_identity`). Either may be omitted.
    ConfigureGitIdentity {
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        email: Option<String>,
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
    /// Wait until the workstation shares an SSH key through the agent relay
    /// (see `tasks::wait_ssh_agent`). No params.
    WaitSshAgent,
}

/// Whether this process was started as `beams` (`~/.beams/runtime/bin/beams`).
fn invoked_as_beams() -> bool {
    std::env::args_os()
        .next()
        .map(std::path::PathBuf::from)
        .and_then(|path| path.file_name().map(|name| name == "beams"))
        .unwrap_or(false)
}

fn main() {
    if invoked_as_beams() {
        std::process::exit(fetch::run_beams_cli());
    }
    let args = Args::parse();
    let exit_code = match args.command {
        Command::Launch { command } => match launch::run(command) {
            Ok(()) => 0,
            Err(err) => { eprintln!("error: {err:#}"); 1 }
        },
        Command::ListTasks => {
            list_tasks();
            0
        }
        Command::RunTask { task } => run_task(task),
        Command::Daemon { socket } => {
            run_daemon(socket);
            0 // run_daemon exits the process itself on error; reaching here means a clean shutdown
        }
        Command::Tui { socket } => {
            run_tui(socket);
            0 // ditto
        }
        Command::Task { task } => run_daemon_task(task),
        Command::ClientBridge { socket } => {
            run_client_bridge(socket);
            0
        }
        Command::Client { command } => run_client_command(command),
        Command::Fetch(args) => fetch::run_fetch(args),
        Command::AgentProxySpike { listen, upstream, delay_ms, delay_identities_ms, known_hosts, allow } => {
            let config = agentspike::SpikeConfig {
                listen,
                upstream,
                delay: std::time::Duration::from_millis(delay_ms),
                delay_identities: std::time::Duration::from_millis(delay_identities_ms),
                known_hosts,
                allow: allow.into_iter().collect(),
            };
            match agentspike::run(config) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("agent-proxy-spike: {e:#}");
                    1
                }
            }
        }
    };
    std::process::exit(exit_code);
}

fn run_client_bridge(socket: Option<String>) {
    let socket_path = expand_tilde(&socket.unwrap_or_else(daemon::default_socket_path));
    if let Err(err) = clientbridge::run(&socket_path) {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

fn run_client_command(command: ClientCommand) -> i32 {
    match command {
        ClientCommand::KnownHosts { host, socket } => match knownhosts::run_cli(&host, socket) {
            Ok(()) => 0,
            Err(err) => {
                eprintln!("beams known-hosts: {err:#}");
                0
            }
        },
        ClientCommand::List { socket } => client_list(socket),
        ClientCommand::Request { socket, service } => client_request(socket, service),
    }
}

fn client_list(socket: Option<String>) -> i32 {
    let socket_path = expand_tilde(&socket.unwrap_or_else(daemon::default_socket_path));
    let mut conn = match connect_or_exit_code(&socket_path) {
        Ok(conn) => conn,
        Err(code) => return code,
    };
    if let Err(err) = conn.send(&json!({"request_id": "1", "op": "list_clients"})) {
        eprintln!("error: {err}");
        return 1;
    }
    match conn.recv() {
        Ok(Some(value)) if value.get("type").and_then(Value::as_str) == Some("clients") => {
            let clients = value.get("clients").and_then(Value::as_array).cloned().unwrap_or_default();
            if clients.is_empty() {
                println!("no managed Beams clients connected");
            } else {
                for client in clients {
                    println!(
                        "{} bridge={} sessions={} status={}",
                        client.get("client_id").and_then(Value::as_str).unwrap_or("?"),
                        client.get("bridge_id").and_then(Value::as_str).unwrap_or("?"),
                        client.get("session_count").and_then(Value::as_u64).unwrap_or(0),
                        client.get("status").and_then(Value::as_str).unwrap_or("?"),
                    );
                }
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

fn client_request(socket: Option<String>, service: ClientServiceCommand) -> i32 {
    let session_token = match std::env::var("BEAMS_SESSION_TOKEN") {
        Ok(token) if !token.is_empty() => token,
        _ => {
            eprintln!(
                "error: no managed Beams client is associated with this shell\n\
                 reconnect with `beams ssh`, then retry the request"
            );
            return 1;
        }
    };
    let client_id = std::env::var("BEAMS_CLIENT_ID").ok();
    let kind = match service {
        ClientServiceCommand::PortForward { remote_host, remote_port, local_port } => {
            beams_protocol::ServiceRequestKind::LocalPortForward { remote_host, remote_port, local_port }
        }
        ClientServiceCommand::Upload { remote_path, local_path_hint, overwrite } => {
            beams_protocol::ServiceRequestKind::FileCopy {
                direction: beams_protocol::FileCopyDirection::Upload,
                remote_path,
                local_path_hint,
                overwrite: if overwrite {
                    beams_protocol::OverwritePolicy::Replace
                } else {
                    beams_protocol::OverwritePolicy::Deny
                },
            }
        }
        ClientServiceCommand::Download { remote_path, local_path_hint, overwrite } => {
            beams_protocol::ServiceRequestKind::FileCopy {
                direction: beams_protocol::FileCopyDirection::Download,
                remote_path,
                local_path_hint,
                overwrite: if overwrite {
                    beams_protocol::OverwritePolicy::Replace
                } else {
                    beams_protocol::OverwritePolicy::Deny
                },
            }
        }
        ClientServiceCommand::Sync {
            remote_path,
            local_path_hint,
            mode,
            include_vcs,
            ignores,
        } => beams_protocol::ServiceRequestKind::FolderSync {
            remote_path,
            local_path_hint,
            mode: mode.into(),
            ignore_vcs: !include_vcs,
            ignores,
        },
    };
    let wait_for_service_completion =
        matches!(kind, beams_protocol::ServiceRequestKind::FileCopy { .. });
    let service_request = beams_protocol::ServiceRequest { request_id: Uuid::new_v4().to_string(), kind };
    let socket_path = expand_tilde(&socket.unwrap_or_else(daemon::default_socket_path));
    let mut conn = match connect_or_exit_code(&socket_path) {
        Ok(conn) => conn,
        Err(code) => return code,
    };
    let request_id = "1";
    if let Err(err) = conn.send(&json!({
        "request_id": request_id,
        "op": "request_service",
        "session_token": session_token,
        "client_id": client_id,
        "request": service_request,
    })) {
        eprintln!("error: {err}");
        return 1;
    }

    let mut accepted = false;
    loop {
        let value = match conn.recv() {
            Ok(Some(value)) => value,
            Ok(None) => {
                eprintln!("error: daemon closed the connection before the request was decided");
                return 1;
            }
            Err(err) => {
                eprintln!("error: {err}");
                return 1;
            }
        };
        match value.get("type").and_then(Value::as_str) {
            Some("error") if value.get("request_id").and_then(Value::as_str) == Some(request_id) => {
                eprintln!("error: {}", value.get("message").and_then(Value::as_str).unwrap_or("unknown daemon error"));
                return 1;
            }
            Some("service_request_accepted") if value.get("request_id").and_then(Value::as_str) == Some(request_id) => {
                accepted = true;
                println!("submitted {}; waiting for local approval", service_request.request_id);
            }
            Some("service_request_status")
                if value
                    .get("request")
                    .and_then(|request| request.get("request"))
                    .and_then(|request| request.get("request_id"))
                    .and_then(Value::as_str)
                    == Some(&service_request.request_id) =>
            {
                let status = value
                    .get("request")
                    .and_then(|request| request.get("status"))
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                let message = value.get("request").and_then(|request| request.get("message")).and_then(Value::as_str);
                if accepted || status != "pending_approval" {
                    match message {
                        Some(message) => println!("{status}: {message}"),
                        None => println!("{status}"),
                    }
                }
                match status {
                    "running" if !wait_for_service_completion => return 0,
                    "rejected" | "expired" | "failed" => return 1,
                    _ => {}
                }
            }
            Some("client_service_status" | "client_services_snapshot")
                if wait_for_service_completion =>
            {
                let Some(service) =
                    projected_service_for_request(&value, &service_request.request_id)
                else {
                    continue;
                };
                let status = service
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                let message = service.get("message").and_then(Value::as_str);
                match message {
                    Some(message) => println!("{status}: {message}"),
                    None => println!("{status}"),
                }
                match status {
                    "stopped" if message == Some("file copy completed") => return 0,
                    "stopped" => return 1,
                    "error" => return 1,
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

fn projected_service_for_request<'a>(
    value: &'a Value,
    request_id: &str,
) -> Option<&'a Value> {
    match value.get("type").and_then(Value::as_str) {
        Some("client_service_status") => value
            .get("service")
            .and_then(|owned| owned.get("service"))
            .filter(|service| {
                service.get("request_id").and_then(Value::as_str) == Some(request_id)
            }),
        Some("client_services_snapshot") => value
            .get("services")
            .and_then(Value::as_array)?
            .iter()
            .filter_map(|owned| owned.get("service"))
            .find(|service| {
                service.get("request_id").and_then(Value::as_str) == Some(request_id)
            }),
        _ => None,
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

/// Builds the `(template name, JSON params)` pair for a `TaskCommand` -
/// shared by `run_task` (in-process) and `run_daemon_task`'s `Run`
/// variant (daemon-backed), so the two don't drift on how a `TaskCommand`
/// maps onto `tasks::instantiate`'s actual inputs.
fn task_command_to_instance(command: TaskCommand) -> (&'static str, Value) {
    match command {
        TaskCommand::CloneGitRepo { repo, dest } => {
            (tasks::clone_git_repo::CloneGitRepo::NAME, json!({"repo": repo, "dest": dest}))
        }
        TaskCommand::CheckoutPrWorktree { base, pr, dest } => {
            (tasks::checkout_pr_worktree::CheckoutPrWorktree::NAME, json!({"base": base, "pr": pr, "dest": dest}))
        }
        TaskCommand::InstallStarship => ("install-starship", json!({})),
        TaskCommand::ConfigureGitIdentity { name, email } => {
            (tasks::configure_git_identity::ConfigureGitIdentity::NAME, json!({"name": name, "email": email}))
        }
        TaskCommand::InstallToolchainGo => (tasks::install_toolchain_go::InstallToolchainGo::NAME, json!({})),
        TaskCommand::InstallToolchainRust => (tasks::install_toolchain_rust::InstallToolchainRust::NAME, json!({})),
        TaskCommand::InstallCodeServer => (tasks::install_code_server::InstallCodeServer::NAME, json!({})),
        TaskCommand::WaitSshAgent => (tasks::wait_ssh_agent::WaitSshAgent::NAME, json!({})),
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
/// `docs/designs/DESIGN.md`'s architecture diagram calls `beams task run ...`.
fn daemon_run_task(socket: Option<String>, task: TaskCommand) -> i32 {
    let socket_path = expand_tilde(&socket.unwrap_or_else(daemon::default_socket_path));
    let (name, params) = task_command_to_instance(task);
    let mut conn = match connect_or_exit_code(&socket_path) {
        Ok(conn) => conn,
        Err(code) => return code,
    };

    let request_id = "1";
    if let Err(err) = conn.send(&json!({"request_id": request_id, "op": "run_task", "task": name, "params": params})) {
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
                let Some(id) = value.get("task_id").and_then(Value::as_str) else {
                    continue;
                };
                // Broadcasts go to every connection - only print/act on
                // the one this call actually submitted.
                // A manifest elsewhere can be running the same task id
                // concurrently with (well, serialized against) this
                // standalone one - its events carry a `manifest_id`.
                if task_id.as_deref() != Some(id) || !value.get("manifest_id").map_or(true, Value::is_null) {
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
                let planned = value.get("tasks").and_then(Value::as_array).cloned().unwrap_or_default();
                println!("submitted manifest {id} ({} tasks)", planned.len());
                for entry in &planned {
                    let task_id = entry.get("task_id").and_then(Value::as_str).unwrap_or("?");
                    let deps: Vec<&str> = entry
                        .get("depends_on")
                        .and_then(Value::as_array)
                        .map(|deps| deps.iter().filter_map(Value::as_str).collect())
                        .unwrap_or_default();
                    if deps.is_empty() {
                        println!("  {task_id}");
                    } else {
                        println!("  {task_id} (after {})", deps.join(", "));
                    }
                }
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
    use serde_json::json;

    use super::{projected_service_for_request, TaskCommand};

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
        let mut covered: Vec<String> = cmd.get_subcommands().map(|c| c.get_name().to_string()).collect();
        covered.sort();

        let mut expected: Vec<String> = crate::tasks::names().iter().map(|s| s.to_string()).collect();
        expected.sort();

        assert_eq!(
            covered, expected,
            "TaskCommand is missing a variant for some template(s) - \
             `task run`/`run-task` can't reach it without one"
        );
    }

    #[test]
    fn file_copy_waiter_finds_incremental_and_snapshot_service_updates() {
        let update = json!({
            "type": "client_service_status",
            "service": {
                "owner_client_id": "client",
                "service": {
                    "service_id": "service",
                    "request_id": "request",
                    "status": "stopped"
                }
            }
        });
        assert_eq!(
            projected_service_for_request(&update, "request")
                .and_then(|service| service.get("status"))
                .and_then(serde_json::Value::as_str),
            Some("stopped")
        );

        let snapshot = json!({
            "type": "client_services_snapshot",
            "services": [
                {
                    "owner_client_id": "client",
                    "service": {
                        "service_id": "other-service",
                        "request_id": "other-request",
                        "status": "running"
                    }
                },
                {
                    "owner_client_id": "client",
                    "service": {
                        "service_id": "service",
                        "request_id": "request",
                        "status": "error"
                    }
                }
            ]
        });
        assert_eq!(
            projected_service_for_request(&snapshot, "request")
                .and_then(|service| service.get("status"))
                .and_then(serde_json::Value::as_str),
            Some("error")
        );
        assert!(projected_service_for_request(&snapshot, "missing").is_none());
    }
}
