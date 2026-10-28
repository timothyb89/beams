//! Supervised one-shot file copies owned by the workstation daemon.

use std::collections::VecDeque;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context};
use beams_protocol::{FileCopyDirection, OverwritePolicy, ServiceRequestKind, ServiceStatus};
use uuid::Uuid;

const COMMAND_QUEUE_CAPACITY: usize = 4;
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const DIAGNOSTIC_LINES: usize = 20;

#[derive(Clone, Debug)]
pub struct Target {
    pub beam_uuid: String,
    pub login: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Stop,
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
    local_path: PathBuf,
    events: SyncSender<Event>,
) -> SyncSender<Action> {
    let (commands, command_rx) = mpsc::sync_channel(COMMAND_QUEUE_CAPACITY);
    thread::spawn(move || {
        supervise(
            &program,
            &target,
            &service_id,
            &kind,
            &local_path,
            &events,
            &command_rx,
        )
    });
    commands
}

fn supervise(
    program: &str,
    target: &Target,
    service_id: &str,
    kind: &ServiceRequestKind,
    local_path: &Path,
    events: &SyncSender<Event>,
    commands: &Receiver<Action>,
) {
    let _ = events.send(Event {
        service_id: service_id.to_string(),
        kind: kind.clone(),
        status: ServiceStatus::Starting,
        message: Some("preparing file copy".to_string()),
    });
    let result = run_copy(
        program, target, kind, local_path, events, service_id, commands,
    );
    let (status, message) = match result {
        Ok(CopyOutcome::Completed(message)) => (ServiceStatus::Stopped, message),
        Ok(CopyOutcome::Cancelled) => (ServiceStatus::Stopped, "cancelled locally".to_string()),
        Err(err) => (ServiceStatus::Error, format!("{err:#}")),
    };
    let _ = events.send(Event {
        service_id: service_id.to_string(),
        kind: kind.clone(),
        status,
        message: Some(message),
    });
}

enum CopyOutcome {
    Completed(String),
    Cancelled,
}

fn run_copy(
    program: &str,
    target: &Target,
    kind: &ServiceRequestKind,
    local_path: &Path,
    events: &SyncSender<Event>,
    service_id: &str,
    commands: &Receiver<Action>,
) -> anyhow::Result<CopyOutcome> {
    let ServiceRequestKind::FileCopy {
        direction,
        remote_path,
        overwrite,
        ..
    } = kind
    else {
        bail!("file-copy worker received a non-copy service");
    };
    if remote_path.is_empty() {
        bail!("remote path must not be empty");
    }
    let destination = format!("{}@{}", target.login, target.beam_uuid);
    let diagnostics = Arc::new(Mutex::new(VecDeque::new()));

    match direction {
        FileCopyDirection::Upload => {
            if !local_path.is_file() {
                bail!(
                    "local upload source {} is not a regular file",
                    local_path.display()
                );
            }
            let remote_temp = format!("{remote_path}.beams-copy-{}.part", Uuid::new_v4());
            let remote = format!("{destination}:{remote_temp}");
            let mut child = spawn_scp(
                program,
                local_path.as_os_str(),
                remote.as_ref(),
                Arc::clone(&diagnostics),
            )?;
            announce_running(events, service_id, kind, local_path, remote_path);
            if monitor(&mut child, commands)? {
                let _ = remove_remote(program, &destination, &remote_temp);
                return Ok(CopyOutcome::Cancelled);
            }
            let status = child.wait().context("waiting for upload")?;
            if !status.success() {
                let _ = remove_remote(program, &destination, &remote_temp);
                bail!("{}", child_failure("upload", status, &diagnostics));
            }
            finalize_remote(program, &destination, &remote_temp, remote_path, *overwrite)?;
            Ok(CopyOutcome::Completed(format!(
                "uploaded {} to {remote_path}",
                local_path.display()
            )))
        }
        FileCopyDirection::Download => {
            let parent = local_path
                .parent()
                .filter(|path| !path.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            if !parent.is_dir() {
                bail!(
                    "local destination directory {} does not exist",
                    parent.display()
                );
            }
            if *overwrite == OverwritePolicy::Deny && local_path.exists() {
                bail!("local destination {} already exists", local_path.display());
            }
            let temp = parent.join(format!(".beams-copy-{}.part", Uuid::new_v4()));
            let remote = format!("{destination}:{remote_path}");
            let mut child = spawn_scp(
                program,
                remote.as_ref(),
                temp.as_os_str(),
                Arc::clone(&diagnostics),
            )?;
            announce_running(events, service_id, kind, local_path, remote_path);
            if monitor(&mut child, commands)? {
                let _ = fs::remove_file(&temp);
                return Ok(CopyOutcome::Cancelled);
            }
            let status = child.wait().context("waiting for download")?;
            if !status.success() {
                let _ = fs::remove_file(&temp);
                bail!("{}", child_failure("download", status, &diagnostics));
            }
            install_download(&temp, local_path, *overwrite)?;
            Ok(CopyOutcome::Completed(format!(
                "downloaded {remote_path} to {}",
                local_path.display()
            )))
        }
    }
}

fn announce_running(
    events: &SyncSender<Event>,
    service_id: &str,
    kind: &ServiceRequestKind,
    local_path: &Path,
    remote_path: &str,
) {
    let _ = events.send(Event {
        service_id: service_id.to_string(),
        kind: kind.clone(),
        status: ServiceStatus::Running,
        message: Some(format!("copying {} ↔ {remote_path}", local_path.display())),
    });
}

fn spawn_scp(
    program: &str,
    source: &std::ffi::OsStr,
    destination: &std::ffi::OsStr,
    diagnostics: Arc<Mutex<VecDeque<String>>>,
) -> anyhow::Result<Child> {
    let mut command = Command::new(program);
    command
        .arg("scp")
        .arg(source)
        .arg(destination)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    crate::forward::configure_parent_death_signal(&mut command);
    let mut child = command.spawn().context("starting tsh scp")?;
    let stderr = child.stderr.take().context("capturing copy diagnostics")?;
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

/// Returns true when the local user cancelled the copy.
fn monitor(child: &mut Child, commands: &Receiver<Action>) -> anyhow::Result<bool> {
    loop {
        if child.try_wait().context("polling copy process")?.is_some() {
            return Ok(false);
        }
        match commands.recv_timeout(POLL_INTERVAL) {
            Ok(Action::Stop) => {
                let _ = child.kill();
                let _ = child.wait();
                return Ok(true);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                let _ = child.kill();
                let _ = child.wait();
                return Ok(true);
            }
        }
    }
}

fn install_download(
    temp: &Path,
    destination: &Path,
    overwrite: OverwritePolicy,
) -> anyhow::Result<()> {
    match overwrite {
        OverwritePolicy::Replace => fs::rename(temp, destination)
            .with_context(|| format!("installing download at {}", destination.display())),
        OverwritePolicy::Deny => {
            fs::hard_link(temp, destination)
                .with_context(|| format!("creating new download at {}", destination.display()))?;
            fs::remove_file(temp).context("removing download temporary file")
        }
    }
}

fn finalize_remote(
    program: &str,
    destination: &str,
    temp: &str,
    final_path: &str,
    overwrite: OverwritePolicy,
) -> anyhow::Result<()> {
    let temp = crate::tsh::shell_quote(temp);
    let final_path = crate::tsh::shell_quote(final_path);
    let script = match overwrite {
        OverwritePolicy::Replace => format!("mv -f -- {temp} {final_path}"),
        OverwritePolicy::Deny => {
            format!("ln -- {temp} {final_path} && rm -f -- {temp}")
        }
    };
    run_remote(program, destination, &script, "finalizing remote upload")
}

fn remove_remote(program: &str, destination: &str, path: &str) -> anyhow::Result<()> {
    run_remote(
        program,
        destination,
        &format!("rm -f -- {}", crate::tsh::shell_quote(path)),
        "removing remote upload temporary file",
    )
}

fn run_remote(
    program: &str,
    destination: &str,
    script: &str,
    operation: &str,
) -> anyhow::Result<()> {
    let remote_command = format!("bash -lc {}", crate::tsh::shell_quote(script));
    let mut command = Command::new(program);
    command
        .args(["ssh", destination, "--", &remote_command])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    crate::forward::configure_parent_death_signal(&mut command);
    let status = command.status().with_context(|| operation.to_string())?;
    if !status.success() {
        bail!("{operation} failed ({status})");
    }
    Ok(())
}

fn child_failure(
    operation: &str,
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
        format!("{operation} process exited with {status}")
    } else {
        format!("{operation} process exited with {status}: {detail}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    fn fake_program(script: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("beams-copy-test-{}", Uuid::new_v4()));
        let mut file = fs::File::create(&path).unwrap();
        file.write_all(script.as_bytes()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    #[test]
    fn download_deny_does_not_replace_an_existing_file() {
        let dir = std::env::temp_dir().join(format!("beams-copy-dest-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let temp = dir.join("temp");
        let destination = dir.join("destination");
        fs::write(&temp, "new").unwrap();
        fs::write(&destination, "old").unwrap();
        assert!(install_download(&temp, &destination, OverwritePolicy::Deny).is_err());
        assert_eq!(fs::read_to_string(&destination).unwrap(), "old");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn failed_copy_includes_bounded_diagnostics() {
        let program = fake_program("#!/bin/sh\necho copy-broke >&2\nexit 23\n");
        let dir = std::env::temp_dir().join(format!("beams-copy-source-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let source = dir.join("source");
        fs::write(&source, "data").unwrap();
        let (events, event_rx) = mpsc::sync_channel(8);
        let _actions = spawn(
            program.display().to_string(),
            Target {
                beam_uuid: "beam".to_string(),
                login: "beams".to_string(),
            },
            "copy".to_string(),
            ServiceRequestKind::FileCopy {
                direction: FileCopyDirection::Upload,
                remote_path: "/tmp/dest".to_string(),
                local_path_hint: None,
                overwrite: OverwritePolicy::Deny,
            },
            source,
            events,
        );
        assert_eq!(
            event_rx
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .status,
            ServiceStatus::Starting
        );
        assert_eq!(
            event_rx
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .status,
            ServiceStatus::Running
        );
        let failed = event_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(failed.status, ServiceStatus::Error);
        assert!(failed.message.unwrap().contains("copy-broke"));
        let _ = fs::remove_dir_all(dir);
        let _ = fs::remove_file(program);
    }

    #[test]
    fn download_completes_through_a_temporary_file() {
        let program = fake_program(
            "#!/bin/sh\n\
             if [ \"$1\" = scp ]; then\n\
               for arg do destination=$arg; done\n\
               printf copied > \"$destination\"\n\
             fi\n",
        );
        let dir = std::env::temp_dir().join(format!("beams-copy-download-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let destination = dir.join("destination");
        let (events, event_rx) = mpsc::sync_channel(8);
        let _actions = spawn(
            program.display().to_string(),
            Target {
                beam_uuid: "beam".to_string(),
                login: "beams".to_string(),
            },
            "copy".to_string(),
            ServiceRequestKind::FileCopy {
                direction: FileCopyDirection::Download,
                remote_path: "/tmp/source".to_string(),
                local_path_hint: None,
                overwrite: OverwritePolicy::Deny,
            },
            destination.clone(),
            events,
        );
        assert_eq!(
            event_rx
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .status,
            ServiceStatus::Starting
        );
        assert_eq!(
            event_rx
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .status,
            ServiceStatus::Running
        );
        assert_eq!(
            event_rx
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .status,
            ServiceStatus::Stopped
        );
        assert_eq!(fs::read_to_string(&destination).unwrap(), "copied");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        let _ = fs::remove_dir_all(dir);
        let _ = fs::remove_file(program);
    }

    #[test]
    fn cancelling_download_removes_its_temporary_file() {
        let program = fake_program(
            "#!/bin/sh\n\
             if [ \"$1\" = scp ]; then\n\
               trap 'exit 0' TERM INT\n\
               while :; do sleep 1; done\n\
             fi\n",
        );
        let dir = std::env::temp_dir().join(format!("beams-copy-cancel-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let destination = dir.join("destination");
        let (events, event_rx) = mpsc::sync_channel(8);
        let actions = spawn(
            program.display().to_string(),
            Target {
                beam_uuid: "beam".to_string(),
                login: "beams".to_string(),
            },
            "copy".to_string(),
            ServiceRequestKind::FileCopy {
                direction: FileCopyDirection::Download,
                remote_path: "/tmp/source".to_string(),
                local_path_hint: None,
                overwrite: OverwritePolicy::Deny,
            },
            destination.clone(),
            events,
        );
        assert_eq!(
            event_rx
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .status,
            ServiceStatus::Starting
        );
        assert_eq!(
            event_rx
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .status,
            ServiceStatus::Running
        );
        actions.send(Action::Stop).unwrap();
        let stopped = event_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(stopped.status, ServiceStatus::Stopped);
        assert_eq!(stopped.message.as_deref(), Some("cancelled locally"));
        assert!(!destination.exists());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        let _ = fs::remove_dir_all(dir);
        let _ = fs::remove_file(program);
    }
}
