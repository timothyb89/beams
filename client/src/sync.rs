//! Mutagen-backed folder synchronization owned by the workstation daemon.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context};
use beams_protocol::{FolderSyncMode, ServiceRequestKind, ServiceStatus};
use serde_json::Value;

const COMMAND_QUEUE_CAPACITY: usize = 8;
const POLL_INTERVAL: Duration = Duration::from_secs(1);
const LIST_TEMPLATE: &str = "{{ json . }}";

const SSH_WRAPPER: &str = r#"#!/bin/sh
set -eu
while [ "$#" -gt 0 ]; do
    case "$1" in
        --) shift; break ;;
        -o|-p|-l|-i|-F|-J|-S) shift; [ "$#" -gt 0 ] && shift ;;
        -o*|-p*|-l*|-i*|-F*|-J*|-S*) shift ;;
        -*) shift ;;
        *) break ;;
    esac
done
[ "$#" -ge 1 ] || { echo "beams Mutagen ssh wrapper: missing destination" >&2; exit 2; }
destination=$1
shift
exec "${BEAMS_TSH:-tsh}" ssh "$destination" -- "$@"
"#;

const SCP_WRAPPER: &str = r#"#!/bin/sh
set -eu
while [ "$#" -gt 0 ]; do
    case "$1" in
        --) shift; break ;;
        -o|-P|-i|-F|-S|-c) shift; [ "$#" -gt 0 ] && shift ;;
        -o*|-P*|-i*|-F*|-S*|-c*) shift ;;
        -q|-B|-p|-v) shift ;;
        -*) shift ;;
        *) break ;;
    esac
done
[ "$#" -ge 2 ] || { echo "beams Mutagen scp wrapper: missing endpoints" >&2; exit 2; }
exec "${BEAMS_TSH:-tsh}" scp "$@"
"#;

#[derive(Clone, Debug)]
pub struct Target {
    pub beam_uuid: String,
    pub login: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Pause,
    Resume,
    Terminate,
    Detach,
}

#[derive(Clone, Debug)]
pub struct Event {
    pub service_id: String,
    pub kind: ServiceRequestKind,
    pub status: ServiceStatus,
    pub message: Option<String>,
}

pub fn session_name(beam_uuid: &str, service_id: &str) -> String {
    format!("beams-{beam_uuid}-{service_id}")
}

#[allow(clippy::too_many_arguments)]
pub fn spawn(
    mutagen_program: String,
    tsh_program: String,
    root: PathBuf,
    target: Target,
    service_id: String,
    session_name: String,
    kind: ServiceRequestKind,
    local_path: PathBuf,
    events: SyncSender<Event>,
) -> SyncSender<Action> {
    let (commands, command_rx) = mpsc::sync_channel(COMMAND_QUEUE_CAPACITY);
    thread::spawn(move || {
        supervise(
            &mutagen_program,
            &tsh_program,
            &root,
            &target,
            &service_id,
            &session_name,
            &kind,
            &local_path,
            &events,
            &command_rx,
        )
    });
    commands
}

#[allow(clippy::too_many_arguments)]
fn supervise(
    mutagen_program: &str,
    tsh_program: &str,
    root: &Path,
    target: &Target,
    service_id: &str,
    session_name: &str,
    kind: &ServiceRequestKind,
    local_path: &Path,
    events: &SyncSender<Event>,
    commands: &Receiver<Action>,
) {
    emit(
        events,
        service_id,
        kind,
        ServiceStatus::Starting,
        "checking Mutagen and endpoints",
    );
    let result = run(
        mutagen_program,
        tsh_program,
        root,
        target,
        service_id,
        session_name,
        kind,
        local_path,
        events,
        commands,
    );
    if let Err(err) = result {
        emit(
            events,
            service_id,
            kind,
            ServiceStatus::Error,
            &format!("{err:#}"),
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn run(
    mutagen_program: &str,
    tsh_program: &str,
    root: &Path,
    target: &Target,
    service_id: &str,
    session_name: &str,
    kind: &ServiceRequestKind,
    local_path: &Path,
    events: &SyncSender<Event>,
    commands: &Receiver<Action>,
) -> anyhow::Result<()> {
    let ServiceRequestKind::FolderSync {
        remote_path,
        mode,
        ignore_vcs,
        ignores,
        ..
    } = kind
    else {
        bail!("folder-sync worker received a non-sync service");
    };
    check_version(mutagen_program)?;
    if !local_path.is_dir() {
        bail!(
            "local synchronization endpoint {} is not a directory",
            local_path.display()
        );
    }
    validate_remote_directory(tsh_program, target, remote_path)?;
    let transport = prepare_transport(root)?;

    match inspect_session(mutagen_program, tsh_program, &transport, session_name)? {
        Some(session) => validate_session(
            &session,
            session_name,
            target,
            local_path,
            remote_path,
            *mode,
            *ignore_vcs,
            ignores,
        )?,
        None => create_session(
            mutagen_program,
            tsh_program,
            &transport,
            session_name,
            target,
            local_path,
            remote_path,
            *mode,
            *ignore_vcs,
            ignores,
        )?,
    }

    let mut previous = None;
    loop {
        match commands.recv_timeout(POLL_INTERVAL) {
            Ok(Action::Pause) => {
                run_sync_command(
                    mutagen_program,
                    tsh_program,
                    &transport,
                    "pause",
                    session_name,
                )?;
            }
            Ok(Action::Resume) => {
                run_sync_command(
                    mutagen_program,
                    tsh_program,
                    &transport,
                    "resume",
                    session_name,
                )?;
            }
            Ok(Action::Terminate) => {
                run_sync_command(
                    mutagen_program,
                    tsh_program,
                    &transport,
                    "terminate",
                    session_name,
                )?;
                emit(
                    events,
                    service_id,
                    kind,
                    ServiceStatus::Stopped,
                    "Mutagen session terminated",
                );
                return Ok(());
            }
            Ok(Action::Detach) | Err(RecvTimeoutError::Disconnected) => return Ok(()),
            Err(RecvTimeoutError::Timeout) => {}
        }

        let Some(session) =
            inspect_session(mutagen_program, tsh_program, &transport, session_name)?
        else {
            emit(
                events,
                service_id,
                kind,
                ServiceStatus::Stopped,
                "Mutagen session no longer exists",
            );
            return Ok(());
        };
        validate_session(
            &session,
            session_name,
            target,
            local_path,
            remote_path,
            *mode,
            *ignore_vcs,
            ignores,
        )?;
        let current = summarize_session(&session);
        if previous.as_ref() != Some(&current) {
            emit(events, service_id, kind, current.0, &current.1);
            previous = Some(current);
        }
    }
}

struct Transport {
    data: PathBuf,
    bin: PathBuf,
}

fn prepare_transport(root: &Path) -> anyhow::Result<Transport> {
    let data = root.join("data");
    let bin = root.join("bin");
    fs::create_dir_all(&data).with_context(|| format!("creating {}", data.display()))?;
    fs::create_dir_all(&bin).with_context(|| format!("creating {}", bin.display()))?;
    fs::set_permissions(root, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("protecting {}", root.display()))?;
    fs::set_permissions(&data, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("protecting {}", data.display()))?;
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("protecting {}", bin.display()))?;
    write_wrapper(&bin.join("ssh"), SSH_WRAPPER)?;
    write_wrapper(&bin.join("scp"), SCP_WRAPPER)?;
    Ok(Transport { data, bin })
}

fn write_wrapper(path: &Path, contents: &str) -> anyhow::Result<()> {
    if fs::read_to_string(path).ok().as_deref() != Some(contents) {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o700)
            .open(path)
            .with_context(|| format!("writing {}", path.display()))?;
        file.write_all(contents.as_bytes())
            .with_context(|| format!("writing {}", path.display()))?;
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("protecting {}", path.display()))
}

fn check_version(program: &str) -> anyhow::Result<()> {
    let output = Command::new(program)
        .arg("version")
        .stdin(Stdio::null())
        .output()
        .with_context(|| {
            "Mutagen is required for folder sync; install Mutagen 0.18 or newer and ensure `mutagen` is on PATH"
        })?;
    if !output.status.success() {
        bail!("checking Mutagen version failed: {}", diagnostics(&output));
    }
    let version = String::from_utf8_lossy(&output.stdout);
    let version = version.trim().trim_start_matches('v');
    let mut parts = version.split('.');
    let major = parts.next().and_then(|part| part.parse::<u64>().ok());
    let minor = parts.next().and_then(|part| part.parse::<u64>().ok());
    if !matches!((major, minor), (Some(major), Some(minor)) if major > 0 || minor >= 18) {
        bail!("Mutagen 0.18 or newer is required; found {version:?}");
    }
    Ok(())
}

fn validate_remote_directory(
    tsh_program: &str,
    target: &Target,
    remote_path: &str,
) -> anyhow::Result<()> {
    if remote_path.is_empty() {
        bail!("remote synchronization path must not be empty");
    }
    let destination = format!("{}@{}", target.login, target.beam_uuid);
    let test = format!("test -d -- {}", crate::tsh::shell_quote(remote_path));
    let remote_command = format!("bash -lc {}", crate::tsh::shell_quote(&test));
    let status = Command::new(tsh_program)
        .args(["ssh", &destination, "--", &remote_command])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("checking remote synchronization directory")?;
    if !status.success() {
        bail!("remote synchronization endpoint {remote_path:?} is not an accessible directory");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn create_session(
    program: &str,
    tsh_program: &str,
    transport: &Transport,
    name: &str,
    target: &Target,
    local_path: &Path,
    remote_path: &str,
    mode: FolderSyncMode,
    ignore_vcs: bool,
    ignores: &[String],
) -> anyhow::Result<()> {
    let remote = format!("{}@{}:{remote_path}", target.login, target.beam_uuid);
    let mut command = mutagen_command(program, tsh_program, transport);
    command
        .args([
            "sync",
            "create",
            "--name",
            name,
            "--no-global-configuration",
            "--mode",
            mode.as_mutagen(),
        ])
        .arg(if ignore_vcs {
            "--ignore-vcs"
        } else {
            "--no-ignore-vcs"
        });
    for ignore in ignores {
        command.args(["--ignore", ignore]);
    }
    let output = command
        .arg(local_path)
        .arg(remote)
        .output()
        .context("creating Mutagen synchronization session")?;
    if !output.status.success() {
        bail!(
            "creating Mutagen synchronization session failed: {}",
            diagnostics(&output)
        );
    }
    Ok(())
}

fn inspect_session(
    program: &str,
    tsh_program: &str,
    transport: &Transport,
    name: &str,
) -> anyhow::Result<Option<Value>> {
    let output = mutagen_command(program, tsh_program, transport)
        .args(["sync", "list", name, "--template", LIST_TEMPLATE])
        .output()
        .context("listing Mutagen synchronization session")?;
    if !output.status.success() {
        let detail = diagnostics(&output);
        if detail.contains("did not match any sessions") {
            return Ok(None);
        }
        bail!("listing Mutagen synchronization session failed: {detail}");
    }
    let sessions: Vec<Value> =
        serde_json::from_slice(&output.stdout).context("parsing Mutagen session JSON")?;
    match sessions.len() {
        0 => Ok(None),
        1 => Ok(sessions.into_iter().next()),
        count => bail!("Mutagen name {name:?} unexpectedly matched {count} sessions"),
    }
}

#[allow(clippy::too_many_arguments)]
fn validate_session(
    session: &Value,
    name: &str,
    target: &Target,
    local_path: &Path,
    remote_path: &str,
    mode: FolderSyncMode,
    ignore_vcs: bool,
    ignores: &[String],
) -> anyhow::Result<()> {
    if session.get("name").and_then(Value::as_str) != Some(name) {
        bail!("Mutagen session name no longer matches persisted service");
    }
    if session.get("mode").and_then(Value::as_str) != Some(mode.as_mutagen()) {
        bail!("Mutagen session mode no longer matches persisted service");
    }
    let alpha = session
        .get("alpha")
        .context("Mutagen session has no alpha endpoint")?;
    if alpha.get("protocol").and_then(Value::as_str) != Some("local")
        || alpha.get("path").and_then(Value::as_str) != local_path.to_str()
    {
        bail!("Mutagen alpha endpoint no longer matches the approved workstation directory");
    }
    let beta = session
        .get("beta")
        .context("Mutagen session has no beta endpoint")?;
    if beta.get("protocol").and_then(Value::as_str) != Some("ssh")
        || beta.get("user").and_then(Value::as_str) != Some(target.login.as_str())
        || beta.get("host").and_then(Value::as_str) != Some(target.beam_uuid.as_str())
        || beta.get("path").and_then(Value::as_str) != Some(remote_path)
    {
        bail!("Mutagen beta endpoint no longer matches the approved Beam directory");
    }
    let ignore = session.get("ignore").unwrap_or(&Value::Null);
    if ignore.get("vcs").and_then(Value::as_bool).unwrap_or(false) != ignore_vcs {
        bail!("Mutagen VCS-ignore setting no longer matches persisted service");
    }
    let actual_ignores = ignore
        .get("paths")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    if actual_ignores != ignores.iter().map(String::as_str).collect::<Vec<_>>() {
        bail!("Mutagen ignore rules no longer match persisted service");
    }
    Ok(())
}

fn summarize_session(session: &Value) -> (ServiceStatus, String) {
    if session.get("paused").and_then(Value::as_bool) == Some(true) {
        return (ServiceStatus::Paused, "paused".to_string());
    }
    let status = session
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let conflicts = session
        .get("conflicts")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    if !conflicts.is_empty() {
        let roots = conflicts
            .iter()
            .filter_map(|conflict| conflict.get("root").and_then(Value::as_str))
            .take(3)
            .collect::<Vec<_>>()
            .join(", ");
        let suffix = if roots.is_empty() {
            String::new()
        } else {
            format!(": {roots}")
        };
        return (
            ServiceStatus::Degraded,
            format!("{} conflict(s){suffix}", conflicts.len()),
        );
    }
    match status {
        "watching" => {
            let cycles = session
                .get("successfulCycles")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            (
                ServiceStatus::Running,
                format!("watching; {cycles} successful cycle(s)"),
            )
        }
        "disconnected" => (
            ServiceStatus::Degraded,
            "Mutagen endpoints disconnected".to_string(),
        ),
        "halted-on-root-empty" | "halted-on-root-deletion" | "halted-on-root-type-change" => {
            (ServiceStatus::Error, status.replace('-', " "))
        }
        "unknown" => (ServiceStatus::Unknown, "Mutagen status unknown".to_string()),
        other => (ServiceStatus::Running, other.replace('-', " ")),
    }
}

fn run_sync_command(
    program: &str,
    tsh_program: &str,
    transport: &Transport,
    operation: &str,
    name: &str,
) -> anyhow::Result<()> {
    let output = mutagen_command(program, tsh_program, transport)
        .args(["sync", operation, name])
        .output()
        .with_context(|| format!("running Mutagen sync {operation}"))?;
    if !output.status.success() {
        bail!("Mutagen sync {operation} failed: {}", diagnostics(&output));
    }
    Ok(())
}

fn mutagen_command(program: &str, tsh_program: &str, transport: &Transport) -> Command {
    let mut command = Command::new(program);
    command
        .env("MUTAGEN_DATA_DIRECTORY", &transport.data)
        .env("MUTAGEN_SSH_PATH", &transport.bin)
        .env("BEAMS_TSH", tsh_program)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn diagnostics(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !stderr.is_empty() {
        stderr
    } else if !stdout.is_empty() {
        stdout
    } else {
        format!("process exited with {}", output.status)
    }
}

fn emit(
    events: &SyncSender<Event>,
    service_id: &str,
    kind: &ServiceRequestKind,
    status: ServiceStatus,
    message: &str,
) {
    let _ = events.send(Event {
        service_id: service_id.to_string(),
        kind: kind.clone(),
        status,
        message: Some(message.to_string()),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use uuid::Uuid;

    #[test]
    fn deterministic_session_name_contains_both_owners() {
        assert_eq!(
            session_name("beam-uuid", "service-id"),
            "beams-beam-uuid-service-id"
        );
    }

    #[test]
    fn transport_wrappers_are_private_and_idempotent() {
        let root = std::env::temp_dir().join(format!("beams-sync-transport-{}", Uuid::new_v4()));
        let first = prepare_transport(&root).unwrap();
        let second = prepare_transport(&root).unwrap();
        assert_eq!(first.data, second.data);
        assert_eq!(
            fs::metadata(first.bin.join("ssh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert!(fs::read_to_string(first.bin.join("ssh"))
            .unwrap()
            .contains("BEAMS_TSH"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn session_summary_reports_conflicts_pause_and_watching() {
        let conflicted = json!({
            "status": "watching",
            "conflicts": [{"root": "a"}, {"root": "b"}]
        });
        assert_eq!(
            summarize_session(&conflicted),
            (ServiceStatus::Degraded, "2 conflict(s): a, b".to_string())
        );
        assert_eq!(
            summarize_session(&json!({"paused": true, "status": "disconnected"})),
            (ServiceStatus::Paused, "paused".to_string())
        );
        assert_eq!(
            summarize_session(&json!({"status": "watching", "successfulCycles": 3})),
            (
                ServiceStatus::Running,
                "watching; 3 successful cycle(s)".to_string()
            )
        );
    }

    #[test]
    fn adoption_validation_rejects_endpoint_drift() {
        let local = Path::new("/tmp/project");
        let target = Target {
            beam_uuid: "beam-uuid".to_string(),
            login: "beams".to_string(),
        };
        let mut session = json!({
            "name": "name",
            "mode": "two-way-safe",
            "alpha": {"protocol": "local", "path": "/tmp/project"},
            "beta": {
                "protocol": "ssh",
                "user": "beams",
                "host": "beam-uuid",
                "path": "/home/beams/project"
            },
            "ignore": {"vcs": true, "paths": ["target"]}
        });
        validate_session(
            &session,
            "name",
            &target,
            local,
            "/home/beams/project",
            FolderSyncMode::TwoWaySafe,
            true,
            &["target".to_string()],
        )
        .unwrap();
        session["beta"]["host"] = Value::String("other-beam".to_string());
        assert!(validate_session(
            &session,
            "name",
            &target,
            local,
            "/home/beams/project",
            FolderSyncMode::TwoWaySafe,
            true,
            &["target".to_string()],
        )
        .is_err());
    }
}
