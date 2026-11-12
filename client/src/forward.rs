//! Supervision for workstation-owned local SSH port forwards.
//!
//! A worker owns exactly one forwarding child (`BeamConnection::local_forward`,
//! today `tsh ssh -N -L`) for its full lifetime.
//! The daemon owns the worker through a command channel; no child PID is
//! persisted or exposed across the local UDS.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};
#[cfg(target_os = "linux")]
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::connection::{BeamConnection, Connection, ForwardPlan};
use anyhow::{bail, Context};
use beams_protocol::{ServiceRequestKind, ServiceStatus};

const COMMAND_QUEUE_CAPACITY: usize = 8;
const STARTUP_GRACE: Duration = Duration::from_millis(500);
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const AUTO_PORT_ATTEMPTS: usize = 8;
const DIAGNOSTIC_LINES: usize = 20;
/// A forward that ran this long before losing its master counts as stable.
const STABLE_FORWARD: Duration = Duration::from_secs(30);
const MAX_QUICK_REOPENS: u32 = 3;
const REOPEN_DELAY: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Stop,
    Restart,
}

#[derive(Clone, Debug)]
pub struct Event {
    pub service_id: String,
    pub kind: ServiceRequestKind,
    pub status: ServiceStatus,
    pub message: Option<String>,
}

pub fn spawn(
    connection: Connection,
    service_id: String,
    kind: ServiceRequestKind,
    events: SyncSender<Event>,
) -> SyncSender<Action> {
    let (commands, command_rx) = mpsc::sync_channel(COMMAND_QUEUE_CAPACITY);
    thread::spawn(move || supervise(connection, service_id, kind, events, command_rx));
    commands
}

fn supervise(
    connection: Connection,
    service_id: String,
    mut requested_kind: ServiceRequestKind,
    events: SyncSender<Event>,
    commands: Receiver<Action>,
) {
    let mut start_requested = true;
    let mut reopen_failures = 0;
    loop {
        if start_requested {
            let _ = events.send(Event {
                service_id: service_id.clone(),
                kind: requested_kind.clone(),
                status: ServiceStatus::Starting,
                message: Some("starting SSH local forward".to_string()),
            });
            match start_forward(connection.as_ref(), &requested_kind) {
                Ok(StartedForward {
                    running,
                    kind: running_kind,
                    diagnostics,
                }) => {
                    let started = Instant::now();
                    requested_kind = running_kind.clone();
                    let _ = events.send(Event {
                        service_id: service_id.clone(),
                        kind: running_kind.clone(),
                        status: ServiceStatus::Running,
                        message: Some(forward_label(&running_kind)),
                    });
                    match monitor_running(running, &commands) {
                        RunningOutcome::Stopped => {
                            let _ = events.send(Event {
                                service_id: service_id.clone(),
                                kind: running_kind,
                                status: ServiceStatus::Stopped,
                                message: Some("stopped by local client".to_string()),
                            });
                            start_requested = false;
                        }
                        RunningOutcome::Restart => {
                            let _ = events.send(Event {
                                service_id: service_id.clone(),
                                kind: running_kind,
                                status: ServiceStatus::Stopped,
                                message: Some("restarting".to_string()),
                            });
                            start_requested = true;
                        }
                        RunningOutcome::Exited(status) => {
                            let message = child_failure(status, &diagnostics);
                            let _ = events.send(Event {
                                service_id: service_id.clone(),
                                kind: running_kind,
                                status: ServiceStatus::Error,
                                message: Some(message),
                            });
                            start_requested = false;
                        }
                        RunningOutcome::ConnectionLost => {
                            // Re-open on the replacement master (or a tsh
                            // process), keeping the same local port. Give up
                            // if connections keep dropping right away.
                            reopen_failures = if started.elapsed() < STABLE_FORWARD { reopen_failures + 1 } else { 0 };
                            let retry = reopen_failures <= MAX_QUICK_REOPENS;
                            let _ = events.send(Event {
                                service_id: service_id.clone(),
                                kind: running_kind,
                                status: if retry { ServiceStatus::Starting } else { ServiceStatus::Error },
                                message: Some(if retry {
                                    "SSH connection replaced; reopening".to_string()
                                } else {
                                    "SSH connection keeps dropping".to_string()
                                }),
                            });
                            if retry {
                                thread::sleep(REOPEN_DELAY);
                            }
                            start_requested = retry;
                        }
                        RunningOutcome::OwnerGone => return,
                    }
                }
                Err(err) => {
                    let _ = events.send(Event {
                        service_id: service_id.clone(),
                        kind: requested_kind.clone(),
                        status: ServiceStatus::Error,
                        message: Some(format!("{err:#}")),
                    });
                    start_requested = false;
                }
            }
        }

        if !start_requested {
            match commands.recv() {
                Ok(Action::Restart) => start_requested = true,
                Ok(Action::Stop) => {}
                Err(_) => return,
            }
        }
    }
}

#[derive(Debug)]
struct StartedForward {
    running: Running,
    kind: ServiceRequestKind,
    diagnostics: Arc<Mutex<VecDeque<String>>>,
}

/// A forward that's up: its own process (Tier C) or registered on a shared
/// master (Tier B).
#[derive(Debug)]
enum Running {
    Child(Child),
    Master { cancel: Command, lease: crate::master::Lease },
}

impl Running {
    fn stop(self) {
        match self {
            Running::Child(mut child) => {
                let _ = child.kill();
                let _ = child.wait();
            }
            Running::Master { mut cancel, lease } => {
                if lease.is_alive() {
                    let _ = cancel.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
                }
            }
        }
    }
}

fn start_forward(
    connection: &dyn BeamConnection,
    kind: &ServiceRequestKind,
) -> anyhow::Result<StartedForward> {
    let ServiceRequestKind::LocalPortForward {
        remote_host,
        remote_port,
        local_port,
    } = kind
    else {
        bail!("port-forward worker received a non-forward service");
    };
    if remote_host.is_empty() || *remote_port == 0 {
        bail!("remote host and port must be non-empty");
    }

    let attempts = if local_port.is_some() {
        1
    } else {
        AUTO_PORT_ATTEMPTS
    };
    let mut last_error = None;
    for _ in 0..attempts {
        let selected_port = reserve_port(*local_port)?;
        let running_kind = ServiceRequestKind::LocalPortForward {
            remote_host: remote_host.clone(),
            remote_port: *remote_port,
            local_port: Some(selected_port),
        };
        let diagnostics = Arc::new(Mutex::new(VecDeque::new()));
        let plan = connection.local_forward(selected_port, remote_host, *remote_port);
        let attempt = match plan {
            ForwardPlan::Process(command) => {
                start_process(connection, command, Arc::clone(&diagnostics)).map(Running::Child)
            }
            ForwardPlan::Master { open, cancel, lease } => {
                open_on_master(open).map(|()| Running::Master { cancel, lease })
            }
        };
        match attempt {
            Ok(running) => {
                return Ok(StartedForward {
                    running,
                    kind: running_kind,
                    diagnostics,
                })
            }
            Err(Attempt::Failed(message)) => last_error = Some(message),
            Err(Attempt::Fatal(err)) => return Err(err),
        }
        if local_port.is_some() {
            break;
        }
    }
    bail!(
        "{}",
        last_error.unwrap_or_else(|| "port-forward process exited during startup".to_string())
    )
}

fn reserve_port(requested: Option<u16>) -> anyhow::Result<u16> {
    let port = requested.unwrap_or(0);
    let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
        .with_context(|| format!("local port {port} on 127.0.0.1 is unavailable"))?;
    let selected = listener
        .local_addr()
        .context("reading selected local port")?
        .port();
    drop(listener);
    Ok(selected)
}

enum Attempt {
    /// This port didn't work (bind conflict, startup failure); another may.
    Failed(String),
    Fatal(anyhow::Error),
}

/// Starts a forwarding process and waits out `STARTUP_GRACE` for it to fail.
fn start_process(
    connection: &dyn BeamConnection,
    mut command: Command,
    diagnostics: Arc<Mutex<VecDeque<String>>>,
) -> Result<Child, Attempt> {
    let destination = connection.target().destination();
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    configure_parent_death_signal(&mut command);
    let mut child = command
        .spawn()
        .with_context(|| format!("starting local forward to {destination}"))
        .map_err(Attempt::Fatal)?;
    let stderr = child
        .stderr
        .take()
        .context("capturing port-forward diagnostics")
        .map_err(Attempt::Fatal)?;
    let reader_diagnostics = Arc::clone(&diagnostics);
    thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let mut lines = reader_diagnostics.lock().unwrap();
            if lines.len() == DIAGNOSTIC_LINES {
                lines.pop_front();
            }
            lines.push_back(line);
        }
    });
    let deadline = Instant::now() + STARTUP_GRACE;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Err(Attempt::Failed(child_failure(status, &diagnostics))),
            Ok(None) if Instant::now() >= deadline => return Ok(child),
            Ok(None) => thread::sleep(POLL_INTERVAL),
            Err(err) => return Err(Attempt::Fatal(anyhow::Error::new(err).context("checking port-forward startup"))),
        }
    }
}

/// Registers a forward on a master (`ssh -O forward`), which binds the port
/// before answering.
fn open_on_master(mut open: Command) -> Result<(), Attempt> {
    let output = open
        .stdin(Stdio::null())
        .output()
        .context("asking the SSH master to forward")
        .map_err(Attempt::Fatal)?;
    if output.status.success() {
        Ok(())
    } else {
        let detail = String::from_utf8_lossy(&output.stderr).trim().replace('\n', " | ");
        Err(Attempt::Failed(format!("SSH master refused the forward: {detail}")))
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn configure_parent_death_signal(command: &mut Command) {
    // SAFETY: this closure only invokes async-signal-safe libc calls before
    // exec. Linux's parent-death signal additionally covers an abrupt daemon
    // crash that cannot run the portable graceful-shutdown path.
    unsafe {
        command.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() == 1 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "beams daemon exited before forward exec",
                ));
            }
            Ok(())
        });
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn configure_parent_death_signal(_command: &mut Command) {
    // macOS and other Unix targets do not expose Linux PR_SET_PDEATHSIG.
    // `clientd::serve` catches normal termination signals and explicitly
    // stops every worker before exiting instead.
}

enum RunningOutcome {
    Stopped,
    Restart,
    Exited(std::process::ExitStatus),
    /// The shared master went away (replaced before certificate expiry, or
    /// disconnected); the forward went with it and can be re-opened.
    ConnectionLost,
    OwnerGone,
}

fn monitor_running(running: Running, commands: &Receiver<Action>) -> RunningOutcome {
    let mut running = running;
    loop {
        match &mut running {
            Running::Child(child) => match child.try_wait() {
                Ok(Some(status)) => return RunningOutcome::Exited(status),
                Ok(None) => {}
                Err(_) => {
                    let _ = child.kill();
                    let status = child.wait().unwrap_or_else(|_| synthetic_failure());
                    return RunningOutcome::Exited(status);
                }
            },
            Running::Master { lease, .. } => {
                if !lease.is_alive() {
                    return RunningOutcome::ConnectionLost;
                }
            }
        }
        match commands.recv_timeout(POLL_INTERVAL) {
            Ok(Action::Stop) => {
                running.stop();
                return RunningOutcome::Stopped;
            }
            Ok(Action::Restart) => {
                running.stop();
                return RunningOutcome::Restart;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                running.stop();
                return RunningOutcome::OwnerGone;
            }
        }
    }
}

#[cfg(unix)]
fn synthetic_failure() -> std::process::ExitStatus {
    use std::os::unix::process::ExitStatusExt;
    std::process::ExitStatus::from_raw(1 << 8)
}

fn child_failure(
    status: std::process::ExitStatus,
    diagnostics: &Arc<Mutex<VecDeque<String>>>,
) -> String {
    let detail = diagnostics
        .lock()
        .unwrap()
        .iter()
        .filter(|line| !line.trim().is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join(" | ");
    if detail.is_empty() {
        format!("SSH forward exited with {status}")
    } else {
        format!("SSH forward exited with {status}: {detail}")
    }
}

fn forward_label(kind: &ServiceRequestKind) -> String {
    match kind {
        ServiceRequestKind::LocalPortForward {
            remote_host,
            remote_port,
            local_port: Some(local_port),
        } => format!("127.0.0.1:{local_port} -> {remote_host}:{remote_port}"),
        ServiceRequestKind::LocalPortForward { .. } => "local forward running".to_string(),
        ServiceRequestKind::FileCopy { .. } | ServiceRequestKind::FileFetch { .. } => {
            "invalid file-copy forward".to_string()
        }
        ServiceRequestKind::FolderSync { .. } => "invalid folder-sync forward".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use uuid::Uuid;

    fn fake_program(script: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "beams-forward-test-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(script.as_bytes()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    fn test_connection(program: &std::path::Path) -> Connection {
        crate::connection::TshConnection::shared(
            program.display().to_string(),
            crate::connection::BeamTarget {
                beam_id: "beam".to_string(),
                beam_uuid: "beam-uuid".to_string(),
                login: "beams".to_string(),
                endpoint: None,
            },
        )
    }

    fn request(local_port: Option<u16>) -> ServiceRequestKind {
        ServiceRequestKind::LocalPortForward {
            remote_host: "127.0.0.1".to_string(),
            remote_port: 8080,
            local_port,
        }
    }

    #[test]
    fn explicit_bind_conflicts_fail_before_spawning_ssh() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let err = reserve_port(Some(port)).unwrap_err().to_string();
        assert!(err.contains("unavailable"));
    }

    #[test]
    fn worker_reports_running_stop_and_restart_with_selected_auto_port() {
        let program = fake_program("#!/bin/sh\ntrap 'exit 0' TERM INT\nwhile :; do sleep 1; done\n");
        let (events, event_rx) = mpsc::sync_channel(16);
        let commands = spawn(
            test_connection(&program),
            "service-1".to_string(),
            request(None),
            events,
        );

        let starting = event_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(starting.status, ServiceStatus::Starting);
        let running = event_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(running.status, ServiceStatus::Running);
        assert!(matches!(
            running.kind,
            ServiceRequestKind::LocalPortForward {
                local_port: Some(port),
                ..
            } if port > 0
        ));

        commands.send(Action::Stop).unwrap();
        assert_eq!(
            event_rx.recv_timeout(Duration::from_secs(2)).unwrap().status,
            ServiceStatus::Stopped
        );
        commands.send(Action::Restart).unwrap();
        assert_eq!(
            event_rx.recv_timeout(Duration::from_secs(2)).unwrap().status,
            ServiceStatus::Starting
        );
        assert_eq!(
            event_rx.recv_timeout(Duration::from_secs(2)).unwrap().status,
            ServiceStatus::Running
        );
        drop(commands);
        let _ = std::fs::remove_file(program);
    }

    #[test]
    fn startup_failure_includes_captured_diagnostics() {
        let program = fake_program("#!/bin/sh\necho 'bind failed for test' >&2\nexit 23\n");
        let err = start_forward(
            test_connection(&program).as_ref(),
            &request(Some(32123)),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("bind failed for test"), "{err}");
        let _ = std::fs::remove_file(program);
    }
}
