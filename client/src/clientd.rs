//! Workstation-side daemon and its local frontend protocol.
//!
//! This is intentionally service-neutral for now. It establishes the pieces
//! every later port-forward/sync/copy implementation needs: one authoritative
//! per-user process, a permissioned UDS, a persistent client identity,
//! versioned typed messages, subscriptions, and durable request/service state.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const PROTOCOL: &str = "beams-client-services";
const PROTOCOL_VERSION: u32 = 1;
const MAX_FRAME_BYTES: usize = 64 * 1024;
const START_TIMEOUT: Duration = Duration::from_secs(3);
const START_RETRY: Duration = Duration::from_millis(50);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
const EVENT_QUEUE_CAPACITY: usize = 256;
const RATE_WINDOW: Duration = Duration::from_secs(1);
const MAX_MESSAGES_PER_WINDOW: usize = 120;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestStatus {
    PendingApproval,
    Approved,
    Rejected,
    Expired,
    Starting,
    Running,
    Failed,
}

impl RequestStatus {
    #[allow(dead_code)] // exercised now by tests; mutations arrive with Phase 3
    pub fn transition(self, next: Self) -> Result<Self, String> {
        let valid = matches!(
            (self, next),
            (
                Self::PendingApproval,
                Self::Approved | Self::Rejected | Self::Expired
            ) | (Self::Approved, Self::Starting | Self::Failed)
                | (Self::Starting, Self::Running | Self::Failed)
        );
        if valid {
            Ok(next)
        } else {
            Err(format!("invalid request transition {self:?} -> {next:?}"))
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceStatus {
    Starting,
    Running,
    Stopping,
    Stopped,
    Degraded,
    Unknown,
    Error,
}

impl ServiceStatus {
    #[allow(dead_code)] // exercised now by tests; mutations arrive with Phase 4
    pub fn transition(self, next: Self) -> Result<Self, String> {
        let valid = matches!(
            (self, next),
            (Self::Starting, Self::Running | Self::Stopped | Self::Error)
                | (
                    Self::Running,
                    Self::Stopping | Self::Degraded | Self::Unknown | Self::Error
                )
                | (
                    Self::Degraded,
                    Self::Running | Self::Stopping | Self::Unknown | Self::Error
                )
                | (
                    Self::Unknown,
                    Self::Running | Self::Stopping | Self::Stopped | Self::Error
                )
                | (Self::Stopping, Self::Stopped | Self::Error)
        );
        if valid {
            Ok(next)
        } else {
            Err(format!("invalid service transition {self:?} -> {next:?}"))
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RequestRecord {
    pub request_id: String,
    pub beam_uuid: String,
    pub kind: String,
    pub status: RequestStatus,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ServiceRecord {
    pub service_id: String,
    pub beam_uuid: String,
    pub kind: String,
    pub status: ServiceStatus,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum ClientMessage {
    Hello {
        protocol: String,
        version: u32,
        role: String,
    },
    Snapshot,
    Ping,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerEvent {
    Hello {
        protocol: String,
        version: u32,
        client_id: String,
    },
    Snapshot {
        client_id: String,
        frontends: usize,
        requests: Vec<RequestRecord>,
        services: Vec<ServiceRecord>,
    },
    DaemonStatus {
        frontends: usize,
    },
    Pong,
    Error {
        message: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PersistedState {
    version: u32,
    client_id: String,
    #[serde(default)]
    requests: BTreeMap<String, RequestRecord>,
    #[serde(default)]
    services: BTreeMap<String, ServiceRecord>,
}

impl Default for PersistedState {
    fn default() -> Self {
        Self {
            version: 1,
            client_id: Uuid::new_v4().to_string(),
            requests: BTreeMap::new(),
            services: BTreeMap::new(),
        }
    }
}

impl PersistedState {
    fn load_or_create(path: &Path) -> anyhow::Result<Self> {
        match fs::read_to_string(path) {
            Ok(contents) => {
                let state: Self = serde_json::from_str(&contents)
                    .with_context(|| format!("parsing {}", path.display()))?;
                if state.version != 1 {
                    bail!(
                        "unsupported daemon state version {} in {}",
                        state.version,
                        path.display()
                    );
                }
                if state.client_id.is_empty() {
                    bail!("empty client_id in {}", path.display());
                }
                Ok(state)
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let state = Self::default();
                state.save(path)?;
                Ok(state)
            }
            Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
        }
    }

    fn save(&self, path: &Path) -> anyhow::Result<()> {
        let parent = path
            .parent()
            .with_context(|| format!("state path {} has no parent", path.display()))?;
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;

        let temp = parent.join(format!(".daemon-state-{}.tmp", Uuid::new_v4()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)
            .with_context(|| format!("creating {}", temp.display()))?;
        serde_json::to_writer_pretty(&mut file, self).context("serializing daemon state")?;
        file.write_all(b"\n").context("finishing daemon state")?;
        file.sync_all().context("syncing daemon state")?;
        fs::rename(&temp, path).with_context(|| format!("replacing {}", path.display()))?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("setting permissions on {}", path.display()))
    }
}

struct Hub {
    state: Mutex<PersistedState>,
    subscribers: Mutex<Vec<SyncSender<ServerEvent>>>,
    frontends: AtomicUsize,
}

impl Hub {
    fn new(state: PersistedState) -> Self {
        Self {
            state: Mutex::new(state),
            subscribers: Mutex::new(Vec::new()),
            frontends: AtomicUsize::new(0),
        }
    }

    fn register(&self) -> (SyncSender<ServerEvent>, Receiver<ServerEvent>) {
        let (tx, rx) = mpsc::sync_channel(EVENT_QUEUE_CAPACITY);
        self.subscribers.lock().unwrap().push(tx.clone());
        self.frontends.fetch_add(1, Ordering::SeqCst);
        (tx, rx)
    }

    fn unregister(&self) {
        self.frontends.fetch_sub(1, Ordering::SeqCst);
        self.broadcast(ServerEvent::DaemonStatus {
            frontends: self.frontends.load(Ordering::SeqCst),
        });
    }

    fn broadcast(&self, event: ServerEvent) {
        self.subscribers.lock().unwrap().retain(|tx| {
            !matches!(
                tx.try_send(event.clone()),
                Err(TrySendError::Disconnected(_))
            )
        });
    }

    fn snapshot(&self) -> ServerEvent {
        let state = self.state.lock().unwrap();
        ServerEvent::Snapshot {
            client_id: state.client_id.clone(),
            frontends: self.frontends.load(Ordering::SeqCst),
            requests: state.requests.values().cloned().collect(),
            services: state.services.values().cloned().collect(),
        }
    }
}

/// A live frontend connection used by the local SSH menu.
pub struct Frontend {
    write_half: UnixStream,
    pub events: Receiver<ServerEvent>,
    pub client_id: String,
}

impl Frontend {
    pub fn connect(socket_path: &Path) -> anyhow::Result<Self> {
        let stream = UnixStream::connect(socket_path)
            .with_context(|| format!("connecting to {}", socket_path.display()))?;
        Self::from_stream(stream)
    }

    fn from_stream(mut stream: UnixStream) -> anyhow::Result<Self> {
        stream
            .set_read_timeout(Some(HANDSHAKE_TIMEOUT))
            .context("setting daemon handshake timeout")?;
        let hello = ClientMessage::Hello {
            protocol: PROTOCOL.to_string(),
            version: PROTOCOL_VERSION,
            role: "frontend".to_string(),
        };
        write_json(&mut stream, &hello).context("sending daemon handshake")?;

        let read_half = stream.try_clone().context("cloning daemon connection")?;
        let mut reader = BufReader::new(read_half);
        let first = read_json::<ServerEvent, _>(&mut reader)
            .context("reading daemon handshake")?
            .context("daemon closed during handshake")?;
        let client_id = match first {
            ServerEvent::Hello {
                protocol,
                version,
                client_id,
            } if protocol == PROTOCOL && version == PROTOCOL_VERSION => client_id,
            ServerEvent::Error { message } => bail!("daemon rejected handshake: {message}"),
            other => bail!("unexpected daemon handshake response: {other:?}"),
        };
        stream
            .set_read_timeout(None)
            .context("clearing daemon handshake timeout")?;

        let (tx, rx) = mpsc::sync_channel(EVENT_QUEUE_CAPACITY);
        thread::spawn(move || loop {
            match read_json::<ServerEvent, _>(&mut reader) {
                Ok(Some(event)) => {
                    if tx.try_send(event).is_err() {
                        return;
                    }
                }
                Ok(None) | Err(_) => return,
            }
        });

        let mut frontend = Self {
            write_half: stream,
            events: rx,
            client_id,
        };
        frontend.request_snapshot()?;
        Ok(frontend)
    }

    pub fn request_snapshot(&mut self) -> anyhow::Result<()> {
        write_json(&mut self.write_half, &ClientMessage::Snapshot)
            .context("requesting daemon snapshot")
    }
}

/// Connects to the local daemon, starting it in the background if needed.
pub fn connect_or_start() -> anyhow::Result<Frontend> {
    let socket_path = default_socket_path()?;
    if let Ok(frontend) = Frontend::connect(&socket_path) {
        return Ok(frontend);
    }

    spawn_daemon(&socket_path)?;
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        match Frontend::connect(&socket_path) {
            Ok(frontend) => return Ok(frontend),
            Err(err) if Instant::now() < deadline => {
                let _ = err;
                thread::sleep(START_RETRY);
            }
            Err(err) => {
                return Err(err).with_context(|| {
                    format!(
                        "local daemon did not become ready within {:?}; see {}",
                        START_TIMEOUT,
                        default_log_path()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|_| "daemon log".to_string())
                    )
                })
            }
        }
    }
}

fn spawn_daemon(socket_path: &Path) -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("resolving beams executable")?;
    let log_path = default_log_path()?;
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&log_path)
        .with_context(|| format!("opening {}", log_path.display()))?;
    let stderr = log.try_clone().context("cloning daemon log file")?;

    Command::new(exe)
        .arg("daemon")
        .arg("--socket")
        .arg(socket_path)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr))
        .process_group(0)
        .spawn()
        .context("starting local beams daemon")?;
    Ok(())
}

pub fn serve(socket_path: Option<PathBuf>) -> anyhow::Result<()> {
    let socket_path = socket_path.unwrap_or(default_socket_path()?);
    let state_path = default_state_path()?;
    let state = PersistedState::load_or_create(&state_path)?;
    let listener = bind_socket(&socket_path)?;
    let hub = Arc::new(Hub::new(state));

    for stream in listener.incoming() {
        let stream = stream.context("accepting local daemon connection")?;
        let hub = Arc::clone(&hub);
        thread::spawn(move || {
            if let Err(err) = handle_connection(&hub, stream) {
                eprintln!("beams daemon: connection error: {err}");
            }
        });
    }
    Ok(())
}

fn bind_socket(path: &Path) -> anyhow::Result<UnixListener> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    if path.exists() {
        if UnixStream::connect(path).is_ok() {
            bail!("a beams daemon is already listening on {}", path.display());
        }
        fs::remove_file(path)
            .with_context(|| format!("removing stale socket {}", path.display()))?;
    }
    let listener =
        UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("setting permissions on {}", path.display()))?;
    Ok(listener)
}

fn handle_connection(hub: &Arc<Hub>, mut stream: UnixStream) -> anyhow::Result<()> {
    stream
        .set_read_timeout(Some(HANDSHAKE_TIMEOUT))
        .context("setting frontend handshake timeout")?;
    let read_half = stream.try_clone().context("cloning connection")?;
    let mut reader = BufReader::new(read_half);

    let hello = match read_json::<ClientMessage, _>(&mut reader) {
        Ok(Some(message)) => message,
        Ok(None) => return Ok(()),
        Err(err) => {
            let _ = write_json(
                &mut stream,
                &ServerEvent::Error {
                    message: err.to_string(),
                },
            );
            return Ok(());
        }
    };
    match hello {
        ClientMessage::Hello {
            protocol,
            version,
            role,
        } if protocol == PROTOCOL && version == PROTOCOL_VERSION && role == "frontend" => {}
        _ => {
            write_json(
                &mut stream,
                &ServerEvent::Error {
                    message: format!(
                        "expected {PROTOCOL} protocol version {PROTOCOL_VERSION} frontend handshake"
                    ),
                },
            )?;
            return Ok(());
        }
    }
    stream
        .set_read_timeout(None)
        .context("clearing frontend handshake timeout")?;

    let client_id = hub.state.lock().unwrap().client_id.clone();
    let (tx, rx) = hub.register();
    let mut write_half = stream;
    thread::spawn(move || {
        for event in rx {
            if write_json(&mut write_half, &event).is_err() {
                return;
            }
        }
    });

    let _ = tx.try_send(ServerEvent::Hello {
        protocol: PROTOCOL.to_string(),
        version: PROTOCOL_VERSION,
        client_id,
    });
    let _ = tx.try_send(hub.snapshot());
    hub.broadcast(ServerEvent::DaemonStatus {
        frontends: hub.frontends.load(Ordering::SeqCst),
    });

    let mut rate = RateLimiter::new();
    loop {
        match read_json::<ClientMessage, _>(&mut reader) {
            Ok(Some(message)) => {
                if !rate.allow() {
                    let _ = tx.try_send(ServerEvent::Error {
                        message: format!(
                            "local protocol rate limit exceeded ({MAX_MESSAGES_PER_WINDOW}/s)"
                        ),
                    });
                    break;
                }
                match message {
                    ClientMessage::Snapshot => {
                        let _ = tx.try_send(hub.snapshot());
                    }
                    ClientMessage::Ping => {
                        let _ = tx.try_send(ServerEvent::Pong);
                    }
                    ClientMessage::Hello { .. } => {
                        let _ = tx.try_send(ServerEvent::Error {
                            message: "handshake already completed".to_string(),
                        });
                    }
                }
            }
            Ok(None) => break,
            Err(err) => {
                let _ = tx.try_send(ServerEvent::Error {
                    message: err.to_string(),
                });
                break;
            }
        }
    }
    hub.unregister();
    Ok(())
}

struct RateLimiter {
    window_started: Instant,
    messages: usize,
}

impl RateLimiter {
    fn new() -> Self {
        Self {
            window_started: Instant::now(),
            messages: 0,
        }
    }

    fn allow(&mut self) -> bool {
        let now = Instant::now();
        if now.duration_since(self.window_started) >= RATE_WINDOW {
            self.window_started = now;
            self.messages = 0;
        }
        self.messages += 1;
        self.messages <= MAX_MESSAGES_PER_WINDOW
    }
}

fn write_json<T: Serialize>(stream: &mut UnixStream, value: &T) -> anyhow::Result<()> {
    serde_json::to_writer(&mut *stream, value).context("serializing daemon message")?;
    stream.write_all(b"\n").context("writing daemon message")?;
    stream.flush().context("flushing daemon message")
}

fn read_json<T: for<'de> Deserialize<'de>, R: BufRead>(
    reader: &mut R,
) -> anyhow::Result<Option<T>> {
    let Some(frame) = read_frame(reader)? else {
        return Ok(None);
    };
    serde_json::from_slice(&frame)
        .context("parsing daemon message")
        .map(Some)
}

fn read_frame<R: BufRead>(reader: &mut R) -> anyhow::Result<Option<Vec<u8>>> {
    let mut frame = Vec::new();
    loop {
        let available = reader.fill_buf().context("reading daemon message")?;
        if available.is_empty() {
            return if frame.is_empty() {
                Ok(None)
            } else {
                Ok(Some(frame))
            };
        }
        let end = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|i| i + 1)
            .unwrap_or(available.len());
        if frame.len() + end > MAX_FRAME_BYTES {
            bail!("daemon message exceeds {MAX_FRAME_BYTES} byte limit");
        }
        frame.extend_from_slice(&available[..end]);
        reader.consume(end);
        if frame.last() == Some(&b'\n') {
            frame.pop();
            return Ok(Some(frame));
        }
    }
}

pub fn default_socket_path() -> anyhow::Result<PathBuf> {
    if let Ok(runtime) = std::env::var("XDG_RUNTIME_DIR") {
        if !runtime.is_empty() {
            return Ok(PathBuf::from(runtime).join("beams-client/daemon.sock"));
        }
    }
    Ok(config_dir()?.join("beams-client/daemon.sock"))
}

fn default_state_path() -> anyhow::Result<PathBuf> {
    Ok(config_dir()?.join("beams-client/daemon-state.json"))
}

fn default_log_path() -> anyhow::Result<PathBuf> {
    Ok(config_dir()?.join("beams-client/daemon.log"))
}

fn config_dir() -> anyhow::Result<PathBuf> {
    match std::env::var("XDG_CONFIG_HOME") {
        Ok(dir) if !dir.is_empty() => Ok(PathBuf::from(dir)),
        _ => Ok(PathBuf::from(std::env::var("HOME").context("HOME is not set")?).join(".config")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("{name}-{}-{}", std::process::id(), Uuid::new_v4()))
    }

    #[test]
    fn request_transitions_are_explicit() {
        assert_eq!(
            RequestStatus::PendingApproval.transition(RequestStatus::Approved),
            Ok(RequestStatus::Approved)
        );
        assert_eq!(
            RequestStatus::Approved.transition(RequestStatus::Starting),
            Ok(RequestStatus::Starting)
        );
        assert_eq!(
            RequestStatus::Starting.transition(RequestStatus::Running),
            Ok(RequestStatus::Running)
        );
        assert!(RequestStatus::Rejected
            .transition(RequestStatus::Running)
            .is_err());
        assert!(RequestStatus::Running
            .transition(RequestStatus::Approved)
            .is_err());
    }

    #[test]
    fn service_transitions_are_explicit() {
        assert_eq!(
            ServiceStatus::Starting.transition(ServiceStatus::Running),
            Ok(ServiceStatus::Running)
        );
        assert_eq!(
            ServiceStatus::Running.transition(ServiceStatus::Stopping),
            Ok(ServiceStatus::Stopping)
        );
        assert_eq!(
            ServiceStatus::Stopping.transition(ServiceStatus::Stopped),
            Ok(ServiceStatus::Stopped)
        );
        assert!(ServiceStatus::Stopped
            .transition(ServiceStatus::Running)
            .is_err());
    }

    #[test]
    fn state_creation_is_stable_atomic_and_private() {
        let dir = temp_path("beams-clientd-state");
        let path = dir.join("state.json");
        let first = PersistedState::load_or_create(&path).unwrap();
        let second = PersistedState::load_or_create(&path).unwrap();
        assert_eq!(first.client_id, second.client_id);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::read_dir(&dir).unwrap().count(),
            1,
            "atomic temp file should have been renamed away"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn socket_is_owner_only_and_live_socket_is_not_stolen() {
        let dir = temp_path("beams-clientd-socket");
        let path = dir.join("daemon.sock");
        let _listener = bind_socket(&path).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(bind_socket(&path).is_err());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn frame_reader_enforces_the_wire_limit() {
        let mut valid = Cursor::new(vec![b'a'; MAX_FRAME_BYTES - 1]);
        assert_eq!(
            read_frame(&mut valid).unwrap().unwrap().len(),
            MAX_FRAME_BYTES - 1
        );

        let mut oversized = Cursor::new(vec![b'a'; MAX_FRAME_BYTES + 1]);
        assert!(read_frame(&mut oversized).is_err());
    }

    #[test]
    fn rate_limiter_bounds_each_connection() {
        let mut rate = RateLimiter::new();
        for _ in 0..MAX_MESSAGES_PER_WINDOW {
            assert!(rate.allow());
        }
        assert!(!rate.allow());
    }

    #[test]
    fn typed_handshake_and_snapshot_work_over_a_real_unix_stream() {
        let (server, client) = UnixStream::pair().unwrap();
        let state = PersistedState::default();
        let expected_client_id = state.client_id.clone();
        let hub = Arc::new(Hub::new(state));
        let server_hub = Arc::clone(&hub);
        let server_thread = thread::spawn(move || handle_connection(&server_hub, server).unwrap());

        let mut frontend = Frontend::from_stream(client).unwrap();
        assert_eq!(frontend.client_id, expected_client_id);
        let snapshot = frontend
            .events
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        match snapshot {
            ServerEvent::Snapshot {
                client_id,
                requests,
                services,
                ..
            } => {
                assert_eq!(client_id, expected_client_id);
                assert!(requests.is_empty());
                assert!(services.is_empty());
            }
            other => panic!("expected snapshot, got {other:?}"),
        }

        frontend.request_snapshot().unwrap();
        drop(frontend);
        server_thread.join().unwrap();
    }
}
