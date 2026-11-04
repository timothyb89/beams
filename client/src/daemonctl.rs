//! Lifecycle control for the local workstation daemon: executable identity
//! (so a daemon can tell its binary was replaced), the version-independent
//! `status`/`stop` control ops, and the `beams daemon status|stop|restart`
//! commands built on them.
//!
//! The control ops are deliberately outside the versioned frontend protocol:
//! a daemon answers `{"op":"status"}` and `{"op":"shutdown"}` as the *first*
//! message on a connection, before any hello, and their shapes must never
//! change. That way any future `beams` can always inspect and stop any daemon
//! that has this code, regardless of protocol version. Daemons that predate
//! the control ops are stopped with SIGTERM (which they handle gracefully),
//! using the PID the kernel reports for the socket's peer - no pgrep, and no
//! risk of signalling some unrelated process.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};

const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);
const STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// Identifies the exact executable file a process was started from. A
/// rebuild or reinstall replaces the file (new inode, size, or mtime), so a
/// mismatch between the recorded identity and what's at the path now means
/// "this process is running an outdated binary".
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExeIdentity {
    pub path: PathBuf,
    pub dev: u64,
    pub ino: u64,
    pub len: u64,
    pub mtime: i64,
    pub mtime_nsec: i64,
}

impl ExeIdentity {
    pub fn of(path: &Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        Some(Self {
            path: path.to_path_buf(),
            dev: meta.dev(),
            ino: meta.ino(),
            len: meta.len(),
            mtime: meta.mtime(),
            mtime_nsec: meta.mtime_nsec(),
        })
    }

    /// The identity of whatever file is at this path *now*, if it differs.
    pub fn replacement(&self) -> Option<Self> {
        Self::of(&self.path).filter(|current| current != self)
    }
}

static STARTUP_EXE: OnceLock<Option<ExeIdentity>> = OnceLock::new();

/// Records this process's executable identity. Call first thing in `main`:
/// on Linux, `current_exe` of an already-replaced binary resolves to a
/// "(deleted)" path, so it must be captured before anything can replace it.
pub fn record_startup_exe() {
    STARTUP_EXE.get_or_init(|| {
        std::env::current_exe()
            .ok()
            .and_then(|path| ExeIdentity::of(&path))
    });
}

pub fn startup_exe() -> Option<&'static ExeIdentity> {
    STARTUP_EXE
        .get_or_init(|| {
            std::env::current_exe()
                .ok()
                .and_then(|path| ExeIdentity::of(&path))
        })
        .as_ref()
}

/// Whether this process's own executable was replaced since it started.
/// A stale process must never replace a daemon: it may well be older than
/// the daemon it's talking to.
pub fn this_process_is_stale() -> bool {
    startup_exe().is_some_and(|exe| exe.replacement().is_some())
}

/// What a daemon reports about itself in reply to the `status` op.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DaemonInfo {
    pub pid: u32,
    pub version: String,
    pub protocol_version: u32,
    pub uptime_secs: u64,
    #[serde(default)]
    pub exe: Option<ExeIdentity>,
    /// The daemon's executable has been replaced on disk.
    #[serde(default)]
    pub binary_replaced: bool,
    /// Why stopping now would lose something (empty when idle).
    #[serde(default)]
    pub busy: Vec<String>,
    pub frontends: usize,
    pub bridges: usize,
    pub services: usize,
}

/// The stable control ops. Never change these shapes (see module docs).
#[derive(Debug, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum ControlRequest {
    Status,
    Shutdown { force: bool },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ControlReply {
    DaemonInfo {
        #[serde(flatten)]
        info: DaemonInfo,
    },
    ShuttingDown,
    ShutdownRefused {
        busy: Vec<String>,
    },
    /// What a daemon without the control ops answers (a rejected
    /// handshake); only its presence matters.
    Error {},
}

/// The outcome of a control request.
enum Control {
    Reply(ControlReply),
    /// Something is listening but doesn't understand control ops.
    Unsupported,
}

fn control(socket: &Path, request: &ControlRequest) -> anyhow::Result<Option<(Control, Option<u32>)>> {
    let mut stream = match UnixStream::connect(socket) {
        Ok(stream) => stream,
        Err(_) => return Ok(None),
    };
    let pid = peer_pid(&stream);
    stream.set_read_timeout(Some(CONTROL_TIMEOUT)).ok();
    let mut line = serde_json::to_vec(request)?;
    line.push(b'\n');
    stream.write_all(&line).context("sending daemon control request")?;
    let mut reader = BufReader::new(stream);
    let mut reply = String::new();
    reader
        .read_line(&mut reply)
        .context("reading daemon control reply")?;
    let control = match serde_json::from_str::<ControlReply>(reply.trim()) {
        // An old daemon rejects the unknown op as a bad handshake.
        Ok(ControlReply::Error { .. }) | Err(_) => Control::Unsupported,
        Ok(reply) => Control::Reply(reply),
    };
    Ok(Some((control, pid)))
}

/// The PID of the process on the other end of a connected Unix socket.
#[cfg(target_os = "linux")]
pub fn peer_pid(stream: &UnixStream) -> Option<u32> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    (result == 0 && cred.pid > 0).then_some(cred.pid as u32)
}

#[cfg(target_os = "macos")]
pub fn peer_pid(stream: &UnixStream) -> Option<u32> {
    let mut pid: libc::pid_t = 0;
    let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            (&mut pid as *mut libc::pid_t).cast(),
            &mut len,
        )
    };
    (result == 0 && pid > 0).then_some(pid as u32)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn peer_pid(_stream: &UnixStream) -> Option<u32> {
    None
}

fn wait_until_gone(socket: &Path, pid: Option<u32>) -> bool {
    let deadline = Instant::now() + STOP_TIMEOUT;
    while Instant::now() < deadline {
        let listening = UnixStream::connect(socket).is_ok();
        let alive = pid.is_some_and(|pid| unsafe { libc::kill(pid as libc::pid_t, 0) } == 0);
        if !listening && !alive {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    false
}

pub enum StopOutcome {
    NotRunning,
    Stopped,
    Refused(Vec<String>),
}

/// Asks the daemon on `socket` to shut down gracefully and waits for it to
/// exit. Unless `force`, a busy daemon refuses. A daemon too old for the
/// control ops is sent SIGTERM via its socket peer PID.
pub fn stop(socket: &Path, force: bool) -> anyhow::Result<StopOutcome> {
    let Some((control, pid)) = control(socket, &ControlRequest::Shutdown { force })? else {
        return Ok(StopOutcome::NotRunning);
    };
    match control {
        Control::Reply(ControlReply::ShuttingDown) => {}
        Control::Reply(ControlReply::ShutdownRefused { busy }) => {
            return Ok(StopOutcome::Refused(busy))
        }
        Control::Reply(other) => bail!("unexpected daemon reply: {other:?}"),
        Control::Unsupported => {
            let pid = pid.context(
                "the local daemon predates `beams daemon stop` and its PID couldn't be read from the socket",
            )?;
            if unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) } != 0 {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("sending SIGTERM to daemon pid {pid}"));
            }
        }
    }
    if !wait_until_gone(socket, pid) {
        bail!(
            "the local daemon{} did not exit within {STOP_TIMEOUT:?}",
            pid.map(|pid| format!(" (pid {pid})")).unwrap_or_default()
        );
    }
    Ok(StopOutcome::Stopped)
}

pub enum Status {
    NotRunning,
    Info(DaemonInfo),
    /// Listening, but too old to report status.
    Legacy { pid: Option<u32> },
}

pub fn status(socket: &Path) -> anyhow::Result<Status> {
    Ok(match control(socket, &ControlRequest::Status)? {
        None => Status::NotRunning,
        Some((Control::Reply(ControlReply::DaemonInfo { info }), _)) => Status::Info(info),
        Some((Control::Reply(other), _)) => bail!("unexpected daemon reply: {other:?}"),
        Some((Control::Unsupported, pid)) => Status::Legacy { pid },
    })
}

fn format_duration(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m{:02}s", secs / 60, secs % 60),
        _ => format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60),
    }
}

fn socket_or_default(socket: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    match socket {
        Some(socket) => Ok(socket),
        None => crate::clientd::default_socket_path(),
    }
}

pub fn print_status(socket: Option<PathBuf>) -> anyhow::Result<u8> {
    let socket = socket_or_default(socket)?;
    match status(&socket)? {
        Status::NotRunning => {
            println!("no local daemon is running ({})", socket.display());
            Ok(1)
        }
        Status::Legacy { pid } => {
            println!(
                "a local daemon{} is running, but it predates `beams daemon status`; \
                 `beams daemon restart` replaces it",
                pid.map(|pid| format!(" (pid {pid})")).unwrap_or_default()
            );
            Ok(0)
        }
        Status::Info(info) => {
            println!(
                "local daemon pid {}, up {} (beams {}, protocol {})",
                info.pid,
                format_duration(info.uptime_secs),
                info.version,
                info.protocol_version
            );
            if let Some(exe) = &info.exe {
                let note = if info.binary_replaced {
                    " - replaced on disk; restarts automatically once idle"
                } else {
                    ""
                };
                println!("executable: {}{note}", exe.path.display());
                if let Some(mine) = startup_exe() {
                    if mine.path != exe.path {
                        println!(
                            "note: this beams is {}; `beams daemon restart` would switch the daemon to it",
                            mine.path.display()
                        );
                    }
                }
            }
            println!(
                "{} frontend(s), {} bridge(s), {} service(s)",
                info.frontends, info.bridges, info.services
            );
            if info.busy.is_empty() {
                println!("idle: safe to restart");
            } else {
                println!("busy: {}", info.busy.join("; "));
            }
            Ok(0)
        }
    }
}

fn refused(busy: &[String]) -> u8 {
    eprintln!(
        "beams: the local daemon is busy ({}); retry once it's idle, or pass --force",
        busy.join("; ")
    );
    1
}

pub fn run_stop(socket: Option<PathBuf>, force: bool) -> anyhow::Result<u8> {
    let socket = socket_or_default(socket)?;
    Ok(match stop(&socket, force)? {
        StopOutcome::NotRunning => {
            eprintln!("beams: no local daemon is running");
            0
        }
        StopOutcome::Stopped => {
            eprintln!("beams: local daemon stopped");
            0
        }
        StopOutcome::Refused(busy) => refused(&busy),
    })
}

/// Stops the running daemon (if any) and starts a fresh one from *this*
/// `beams` binary. Running `beams ssh` sessions reconnect to it on their own.
pub fn run_restart(socket: Option<PathBuf>, force: bool) -> anyhow::Result<u8> {
    let explicit = socket.is_some();
    let socket = socket_or_default(socket)?;
    if let StopOutcome::Refused(busy) = stop(&socket, force)? {
        return Ok(refused(&busy));
    }
    if explicit {
        crate::clientd::spawn_daemon(&socket)?;
    } else {
        // `connect_or_start` waits for the new daemon to accept connections.
        drop(crate::clientd::connect_or_start()?);
    }
    let deadline = Instant::now() + CONTROL_TIMEOUT;
    loop {
        if let Status::Info(info) = status(&socket)? {
            eprintln!("beams: local daemon restarted (pid {})", info.pid);
            return Ok(0);
        }
        if Instant::now() >= deadline {
            bail!("the restarted local daemon did not come up on {}", socket.display());
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exe_identity_notices_a_replaced_file() {
        let dir = std::env::temp_dir().join(format!("beams-exe-identity-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("beams");
        std::fs::write(&path, "v1").unwrap();
        let original = ExeIdentity::of(&path).unwrap();
        assert_eq!(original.replacement(), None);

        // Replace it the way installers do: write elsewhere, then rename.
        let staged = dir.join("beams.new");
        std::fs::write(&staged, "version two").unwrap();
        std::fs::rename(&staged, &path).unwrap();
        let replacement = original.replacement().expect("replacement detected");
        assert_eq!(replacement.len, 11);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn peer_pid_reports_the_other_end_of_a_socket() {
        let (a, _b) = UnixStream::pair().unwrap();
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            assert_eq!(peer_pid(&a), Some(std::process::id()));
        }
    }

    #[test]
    fn durations_are_compact() {
        assert_eq!(format_duration(42), "42s");
        assert_eq!(format_duration(125), "2m05s");
        assert_eq!(format_duration(7380), "2h03m");
    }
}
