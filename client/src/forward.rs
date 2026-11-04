//! Supervision for workstation-owned local SSH port forwards.
//!
//! A worker owns exactly one `tsh ssh -N -L` child for its full lifetime.
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

use anyhow::{bail, Context};
use beams_protocol::{ServiceRequestKind, ServiceStatus};

const COMMAND_QUEUE_CAPACITY: usize = 8;
const STARTUP_GRACE: Duration = Duration::from_millis(500);
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const AUTO_PORT_ATTEMPTS: usize = 8;
const DIAGNOSTIC_LINES: usize = 20;

#[derive(Clone, Debug)]
pub struct Target {
    pub beam_uuid: String,
    pub login: String,
}

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
    program: String,
    target: Target,
    service_id: String,
    kind: ServiceRequestKind,
    events: SyncSender<Event>,
) -> SyncSender<Action> {
    let (commands, command_rx) = mpsc::sync_channel(COMMAND_QUEUE_CAPACITY);
    thread::spawn(move || supervise(program, target, service_id, kind, events, command_rx));
    commands
}

fn supervise(
    program: String,
    target: Target,
    service_id: String,
    mut requested_kind: ServiceRequestKind,
    events: SyncSender<Event>,
    commands: Receiver<Action>,
) {
    let mut start_requested = true;
    loop {
        if start_requested {
            let _ = events.send(Event {
                service_id: service_id.clone(),
                kind: requested_kind.clone(),
                status: ServiceStatus::Starting,
                message: Some("starting SSH local forward".to_string()),
            });
            match start_forward(&program, &target, &requested_kind) {
                Ok(StartedForward {
                    mut child,
                    kind: running_kind,
                    diagnostics,
                }) => {
                    requested_kind = running_kind.clone();
                    let _ = events.send(Event {
                        service_id: service_id.clone(),
                        kind: running_kind.clone(),
                        status: ServiceStatus::Running,
                        message: Some(forward_label(&running_kind)),
                    });
                    match monitor_running(&mut child, &commands) {
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
    child: Child,
    kind: ServiceRequestKind,
    diagnostics: Arc<Mutex<VecDeque<String>>>,
}

fn start_forward(
    program: &str,
    target: &Target,
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
        let mut child = spawn_child(
            program,
            target,
            remote_host,
            *remote_port,
            selected_port,
            Arc::clone(&diagnostics),
        )?;
        let deadline = Instant::now() + STARTUP_GRACE;
        loop {
            match child.try_wait().context("checking port-forward startup")? {
                Some(status) => {
                    last_error = Some(child_failure(status, &diagnostics));
                    break;
                }
                None if Instant::now() >= deadline => {
                    return Ok(StartedForward {
                        child,
                        kind: running_kind,
                        diagnostics,
                    });
                }
                None => thread::sleep(POLL_INTERVAL),
            }
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

fn spawn_child(
    program: &str,
    target: &Target,
    remote_host: &str,
    remote_port: u16,
    local_port: u16,
    diagnostics: Arc<Mutex<VecDeque<String>>>,
) -> anyhow::Result<Child> {
    let forward = format!("127.0.0.1:{local_port}:{remote_host}:{remote_port}");
    let destination = format!("{}@{}", target.login, target.beam_uuid);
    let mut command = Command::new(program);
    command
        .args([
            "ssh",
            "-N",
            "-o",
            "ExitOnForwardFailure=yes",
            "-L",
            &forward,
            &destination,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    configure_parent_death_signal(&mut command);
    let mut child = command
        .spawn()
        .with_context(|| format!("starting local forward to {destination}"))?;
    let stderr = child.stderr.take().context("capturing port-forward diagnostics")?;
    thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let mut lines = diagnostics.lock().unwrap();
            if lines.len() == DIAGNOSTIC_LINES {
                lines.pop_front();
            }
            lines.push_back(line);
        }
    });
    Ok(child)
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
    OwnerGone,
}

fn monitor_running(child: &mut Child, commands: &Receiver<Action>) -> RunningOutcome {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return RunningOutcome::Exited(status),
            Ok(None) => {}
            Err(_) => {
                let _ = child.kill();
                let status = child.wait().unwrap_or_else(|_| synthetic_failure());
                return RunningOutcome::Exited(status);
            }
        }
        match commands.recv_timeout(POLL_INTERVAL) {
            Ok(Action::Stop) => {
                let _ = child.kill();
                let _ = child.wait();
                return RunningOutcome::Stopped;
            }
            Ok(Action::Restart) => {
                let _ = child.kill();
                let _ = child.wait();
                return RunningOutcome::Restart;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                let _ = child.kill();
                let _ = child.wait();
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
            program.display().to_string(),
            Target {
                beam_uuid: "beam-uuid".to_string(),
                login: "beams".to_string(),
            },
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
            &program.display().to_string(),
            &Target {
                beam_uuid: "beam-uuid".to_string(),
                login: "beams".to_string(),
            },
            &request(Some(32123)),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("bind failed for test"), "{err}");
        let _ = std::fs::remove_file(program);
    }
}
