//! Supervised one-shot file copies owned by the workstation daemon.

use std::collections::VecDeque;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use beams_protocol::{
    FetchSource, FileCopyDirection, OverwritePolicy, ServiceRequestKind, ServiceStatus,
    TransferProgress,
};
use uuid::Uuid;

const COMMAND_QUEUE_CAPACITY: usize = 4;
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const DIAGNOSTIC_LINES: usize = 20;
/// How often a running fetch reports byte progress.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

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
    pub progress: Option<TransferProgress>,
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
        progress: None,
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
        progress: None,
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
        progress: None,
    });
}

fn spawn_scp(
    program: &str,
    source: &std::ffi::OsStr,
    destination: &std::ffi::OsStr,
    diagnostics: Arc<Mutex<VecDeque<String>>>,
) -> anyhow::Result<Child> {
    spawn_scp_with(program, false, source, destination, diagnostics)
}

fn spawn_scp_with(
    program: &str,
    recursive: bool,
    source: &std::ffi::OsStr,
    destination: &std::ffi::OsStr,
    diagnostics: Arc<Mutex<VecDeque<String>>>,
) -> anyhow::Result<Child> {
    let mut command = Command::new(program);
    command.arg("scp");
    if recursive {
        command.arg("-r");
    }
    command
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

/// Starts a supervised `beams fetch`: every source copied from the Beam into
/// the approved workstation `destination`.
pub fn spawn_fetch(
    program: String,
    target: Target,
    service_id: String,
    kind: ServiceRequestKind,
    destination: PathBuf,
    events: SyncSender<Event>,
) -> SyncSender<Action> {
    let (commands, command_rx) = mpsc::sync_channel(COMMAND_QUEUE_CAPACITY);
    thread::spawn(move || {
        let reporter = Reporter {
            events: &events,
            service_id: &service_id,
            kind: &kind,
        };
        reporter.send(ServiceStatus::Starting, "preparing fetch".to_string(), None);
        let mut progress = TransferProgress::default();
        let result = run_fetch(
            &program,
            &target,
            &kind,
            &destination,
            &reporter,
            &command_rx,
            &mut progress,
        );
        let (status, message) = match result {
            Ok(CopyOutcome::Completed(message)) => (ServiceStatus::Stopped, message),
            Ok(CopyOutcome::Cancelled) => (ServiceStatus::Stopped, "cancelled locally".to_string()),
            Err(err) => (ServiceStatus::Error, format!("{err:#}")),
        };
        reporter.send(status, message, Some(progress));
    });
    commands
}

struct Reporter<'a> {
    events: &'a SyncSender<Event>,
    service_id: &'a str,
    kind: &'a ServiceRequestKind,
}

impl Reporter<'_> {
    fn send(&self, status: ServiceStatus, message: String, progress: Option<TransferProgress>) {
        let _ = self.events.send(Event {
            service_id: self.service_id.to_string(),
            kind: self.kind.clone(),
            status,
            message: Some(message),
            progress,
        });
    }
}

/// Where each fetch source lands.
#[derive(Debug, PartialEq, Eq)]
pub struct FetchLayout {
    /// A directory to create before copying, if it doesn't exist yet.
    pub create_dir: Option<PathBuf>,
    pub targets: Vec<PathBuf>,
}

/// One plain file goes *to* `destination` (or into it, if that's an
/// existing directory); anything else goes into `destination` as a
/// directory, created if needed. Fails early when a target exists and
/// overwriting isn't allowed.
pub fn fetch_layout(
    sources: &[FetchSource],
    destination: &Path,
    overwrite: OverwritePolicy,
) -> anyhow::Result<FetchLayout> {
    let name = |source: &FetchSource| -> anyhow::Result<PathBuf> {
        let name = Path::new(&source.path)
            .file_name()
            .with_context(|| format!("Beam path {} has no file name", source.path))?;
        Ok(PathBuf::from(name))
    };
    let into_dir = !matches!(sources, [source] if !source.is_dir) || destination.is_dir();
    let layout = if into_dir {
        if destination.exists() && !destination.is_dir() {
            bail!(
                "{} is a file; several items (or a directory) need a destination directory",
                destination.display()
            );
        }
        let parent = destination.parent().unwrap_or_else(|| Path::new("/"));
        if !destination.exists() && !parent.is_dir() {
            bail!(
                "local destination directory {} does not exist",
                parent.display()
            );
        }
        FetchLayout {
            create_dir: (!destination.exists()).then(|| destination.to_path_buf()),
            targets: sources
                .iter()
                .map(|source| name(source).map(|name| destination.join(name)))
                .collect::<anyhow::Result<_>>()?,
        }
    } else {
        let parent = destination
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        if !parent.is_dir() {
            bail!(
                "local destination directory {} does not exist",
                parent.display()
            );
        }
        FetchLayout {
            create_dir: None,
            targets: vec![destination.to_path_buf()],
        }
    };
    if overwrite == OverwritePolicy::Deny {
        if let Some(existing) = layout
            .targets
            .iter()
            .find(|target| target.symlink_metadata().is_ok())
        {
            bail!(
                "{} already exists (allow overwriting to replace it)",
                existing.display()
            );
        }
    }
    Ok(layout)
}

fn run_fetch(
    program: &str,
    target: &Target,
    kind: &ServiceRequestKind,
    destination: &Path,
    reporter: &Reporter<'_>,
    commands: &Receiver<Action>,
    progress: &mut TransferProgress,
) -> anyhow::Result<CopyOutcome> {
    let ServiceRequestKind::FileFetch {
        sources, overwrite, ..
    } = kind
    else {
        bail!("fetch worker received a non-fetch service");
    };
    *progress = TransferProgress {
        bytes_done: 0,
        bytes_total: sources.iter().map(|source| source.size).sum(),
        files_done: 0,
        files_total: sources.len(),
    };
    let layout = fetch_layout(sources, destination, *overwrite)?;
    if let Some(dir) = &layout.create_dir {
        fs::create_dir(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let remote_host = format!("{}@{}", target.login, target.beam_uuid);
    reporter.send(
        ServiceStatus::Running,
        format!("fetching into {}", destination.display()),
        Some(*progress),
    );
    for (source, final_path) in sources.iter().zip(&layout.targets) {
        let parent = final_path.parent().unwrap_or_else(|| Path::new("."));
        let temp = parent.join(format!(".beams-fetch-{}.part", Uuid::new_v4()));
        // Directories are copied *into* a fresh temp directory: `tsh scp -r`
        // always nests the source inside the destination (even one that
        // doesn't exist yet), unlike OpenSSH, so pre-creating it gives one
        // layout either way: `temp/<name>`.
        let (copy_to, fetched) = if source.is_dir {
            fs::create_dir(&temp).with_context(|| format!("creating {}", temp.display()))?;
            let name = Path::new(&source.path)
                .file_name()
                .context("Beam directory has no name")?;
            (temp.clone(), temp.join(name))
        } else {
            (temp.clone(), temp.clone())
        };
        let diagnostics = Arc::new(Mutex::new(VecDeque::new()));
        let remote = format!("{remote_host}:{}", source.path);
        let mut child = match spawn_scp_with(
            program,
            source.is_dir,
            remote.as_ref(),
            copy_to.as_os_str(),
            Arc::clone(&diagnostics),
        ) {
            Ok(child) => child,
            Err(err) => {
                remove_local(&temp);
                return Err(err);
            }
        };
        let done_before = progress.bytes_done;
        let cancelled = monitor_with_progress(&mut child, commands, || {
            let current = done_before + local_size(&temp).min(source.size);
            if current != progress.bytes_done {
                progress.bytes_done = current;
                reporter.send(
                    ServiceStatus::Running,
                    format!("fetching {}", source.path),
                    Some(*progress),
                );
            }
        })?;
        if cancelled {
            remove_local(&temp);
            return Ok(CopyOutcome::Cancelled);
        }
        let status = child.wait().context("waiting for fetch")?;
        if !status.success() {
            remove_local(&temp);
            bail!("{}", child_failure("fetch", status, &diagnostics));
        }
        let installed = if source.is_dir {
            if fetched.is_dir() {
                install_directory(&fetched, final_path, *overwrite)
            } else {
                Err(anyhow::anyhow!(
                    "the copied directory is missing from {}",
                    temp.display()
                ))
            }
        } else {
            install_download(&fetched, final_path, *overwrite)
        };
        remove_local(&temp);
        installed?;
        progress.bytes_done = done_before + source.size;
        progress.files_done += 1;
        reporter.send(
            ServiceStatus::Running,
            format!("fetched {}", source.path),
            Some(*progress),
        );
    }
    Ok(CopyOutcome::Completed(match layout.targets.as_slice() {
        [single] => format!("fetched to {}", single.display()),
        targets => format!(
            "fetched {} items into {}",
            targets.len(),
            destination.display()
        ),
    }))
}

/// Like `monitor`, calling `tick` every `PROGRESS_INTERVAL` meanwhile.
fn monitor_with_progress(
    child: &mut Child,
    commands: &Receiver<Action>,
    mut tick: impl FnMut(),
) -> anyhow::Result<bool> {
    let mut last_tick = Instant::now();
    loop {
        if child.try_wait().context("polling copy process")?.is_some() {
            return Ok(false);
        }
        match commands.recv_timeout(POLL_INTERVAL) {
            Ok(Action::Stop) | Err(RecvTimeoutError::Disconnected) => {
                let _ = child.kill();
                let _ = child.wait();
                return Ok(true);
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
        if last_tick.elapsed() >= PROGRESS_INTERVAL {
            last_tick = Instant::now();
            tick();
        }
    }
}

/// Bytes at `path` so far: a file's length or a directory tree's total.
fn local_size(path: &Path) -> u64 {
    let Ok(meta) = path.symlink_metadata() else {
        return 0;
    };
    if !meta.is_dir() {
        return meta.len();
    }
    let mut total = 0;
    let mut pending = vec![path.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            match entry.path().symlink_metadata() {
                Ok(meta) if meta.is_dir() => pending.push(entry.path()),
                Ok(meta) if meta.is_file() => total += meta.len(),
                _ => {}
            }
        }
    }
    total
}

fn remove_local(path: &Path) {
    if path.is_dir() {
        let _ = fs::remove_dir_all(path);
    } else {
        let _ = fs::remove_file(path);
    }
}

/// Moves a fetched directory into place. Replacing swaps the old one aside
/// first, so a failure never leaves neither.
fn install_directory(
    temp: &Path,
    destination: &Path,
    overwrite: OverwritePolicy,
) -> anyhow::Result<()> {
    if destination.symlink_metadata().is_err() {
        return fs::rename(temp, destination)
            .with_context(|| format!("installing {}", destination.display()));
    }
    if overwrite == OverwritePolicy::Deny {
        bail!(
            "{} already exists (allow overwriting to replace it)",
            destination.display()
        );
    }
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    let old = parent.join(format!(".beams-fetch-{}.old", Uuid::new_v4()));
    fs::rename(destination, &old)
        .with_context(|| format!("moving aside {}", destination.display()))?;
    if let Err(err) = fs::rename(temp, destination) {
        let _ = fs::rename(&old, destination);
        return Err(err).with_context(|| format!("installing {}", destination.display()));
    }
    remove_local(&old);
    Ok(())
}

#[cfg(test)]
mod fetch_tests {
    use super::*;

    fn source(path: &str, is_dir: bool) -> FetchSource {
        FetchSource {
            path: path.to_string(),
            size: 1,
            is_dir,
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("beams-fetch-layout-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("existing-dir")).unwrap();
        fs::write(dir.join("existing.txt"), "x").unwrap();
        dir
    }

    #[test]
    fn one_file_goes_to_the_destination_or_into_a_directory() {
        let dir = scratch("single");
        let file = [source("/b/report.csv", false)];
        let layout = fetch_layout(&file, &dir.join("renamed.csv"), OverwritePolicy::Deny).unwrap();
        assert_eq!(
            layout,
            FetchLayout {
                create_dir: None,
                targets: vec![dir.join("renamed.csv")]
            }
        );
        let layout = fetch_layout(&file, &dir.join("existing-dir"), OverwritePolicy::Deny).unwrap();
        assert_eq!(layout.targets, vec![dir.join("existing-dir/report.csv")]);
        assert!(fetch_layout(&file, &dir.join("missing/x.csv"), OverwritePolicy::Deny).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn several_items_go_into_a_directory_created_on_demand() {
        let dir = scratch("many");
        let many = [source("/b/a.log", false), source("/b/logs", true)];
        let layout = fetch_layout(&many, &dir.join("new"), OverwritePolicy::Deny).unwrap();
        assert_eq!(layout.create_dir, Some(dir.join("new")));
        assert_eq!(
            layout.targets,
            vec![dir.join("new/a.log"), dir.join("new/logs")]
        );
        let err =
            fetch_layout(&many, &dir.join("existing.txt"), OverwritePolicy::Deny).unwrap_err();
        assert!(err.to_string().contains("is a file"), "{err}");
        // A lone directory also goes inside the destination.
        let layout = fetch_layout(&[source("/b/logs", true)], &dir, OverwritePolicy::Deny).unwrap();
        assert_eq!(layout.targets, vec![dir.join("logs")]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn existing_targets_fail_unless_overwriting() {
        let dir = scratch("exists");
        let file = [source("/b/existing.txt", false)];
        let err = fetch_layout(&file, &dir, OverwritePolicy::Deny).unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");
        assert!(fetch_layout(&file, &dir, OverwritePolicy::Replace).is_ok());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn replacing_a_directory_swaps_it_whole() {
        let dir = scratch("swap");
        let temp = dir.join(".incoming");
        fs::create_dir_all(&temp).unwrap();
        fs::write(temp.join("new.txt"), "new").unwrap();
        fs::write(dir.join("existing-dir/old.txt"), "old").unwrap();
        assert!(
            install_directory(&temp, &dir.join("existing-dir"), OverwritePolicy::Deny).is_err()
        );
        install_directory(&temp, &dir.join("existing-dir"), OverwritePolicy::Replace).unwrap();
        assert!(dir.join("existing-dir/new.txt").exists());
        assert!(!dir.join("existing-dir/old.txt").exists());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 2, "no leftovers");
        assert_eq!(local_size(&dir), 4);
        fs::remove_dir_all(&dir).unwrap();
    }
}
