//! Workstation-side daemon and its local frontend protocol.
//!
//! It owns the authority boundary for client services: one process per user,
//! a permissioned UDS, persistent client identity/request state, reconnecting
//! per-Beam control bridges, and local-only approval decisions. Phase 3 routes
//! port-forward proposals; Phase 4 will create the actual forwarding process.

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
use std::sync::{Arc, Mutex, Weak};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use beams_protocol::{
    BeamToClient, ClientToBeam, RequestProjection, RequestStatus, ServiceProjection,
    ServiceRequest, ServiceStatus, MAX_FRAME_BYTES, PROTOCOL, PROTOCOL_VERSION,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const START_TIMEOUT: Duration = Duration::from_secs(3);
const START_RETRY: Duration = Duration::from_millis(50);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
const EVENT_QUEUE_CAPACITY: usize = 256;
const BRIDGE_EVENT_QUEUE_CAPACITY: usize = 256;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const RATE_WINDOW: Duration = Duration::from_secs(1);
const MAX_MESSAGES_PER_WINDOW: usize = 120;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RequestRecord {
    pub request_id: String,
    pub beam_uuid: String,
    pub kind: String,
    pub status: RequestStatus,
    #[serde(default)]
    pub request: Option<ServiceRequest>,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ServiceRecord {
    pub service_id: String,
    pub beam_uuid: String,
    pub kind: String,
    pub status: ServiceStatus,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BridgeRecord {
    pub beam_id: String,
    pub beam_uuid: String,
    pub login: String,
    pub status: String,
    pub session_count: usize,
    #[serde(default)]
    pub bridge_id: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
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
    EnsureBridge {
        beam_id: String,
        beam_uuid: String,
        login: String,
        session_token: String,
    },
    DecideRequest {
        request_id: String,
        approve: bool,
    },
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
        bridges: Vec<BridgeRecord>,
    },
    DaemonStatus {
        frontends: usize,
    },
    RequestChanged {
        request: RequestRecord,
    },
    BridgeStatus {
        bridge: BridgeRecord,
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

    fn expire_pending_after_restart(&mut self) -> bool {
        let mut changed = false;
        for request in self.requests.values_mut() {
            if request.status == RequestStatus::PendingApproval {
                request.status = RequestStatus::Expired;
                request.message = Some("local Beams daemon restarted before approval".to_string());
                changed = true;
            }
        }
        changed
    }
}

struct Hub {
    state: Mutex<PersistedState>,
    state_path: Option<PathBuf>,
    subscribers: Mutex<Vec<SyncSender<ServerEvent>>>,
    frontends: AtomicUsize,
    bridges: Mutex<BTreeMap<String, BridgeRuntime>>,
}

struct BridgeRuntime {
    record: BridgeRecord,
    commands: SyncSender<ClientToBeam>,
}

impl Hub {
    #[cfg(test)]
    fn new(state: PersistedState) -> Self {
        Self::with_state_path(state, None)
    }

    fn with_state_path(state: PersistedState, state_path: Option<PathBuf>) -> Self {
        Self {
            state: Mutex::new(state),
            state_path,
            subscribers: Mutex::new(Vec::new()),
            frontends: AtomicUsize::new(0),
            bridges: Mutex::new(BTreeMap::new()),
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
            bridges: self
                .bridges
                .lock()
                .unwrap()
                .values()
                .map(|bridge| bridge.record.clone())
                .collect(),
        }
    }

    fn save_state(&self) -> anyhow::Result<()> {
        if let Some(path) = &self.state_path {
            self.state.lock().unwrap().save(path)?;
        }
        Ok(())
    }

    fn ensure_bridge(
        self: &Arc<Self>,
        target: crate::bridge::Target,
        session_token: String,
    ) -> anyhow::Result<()> {
        if target.beam_uuid.is_empty() || target.login.is_empty() || session_token.is_empty() {
            bail!("beam UUID, login, and managed-session token must not be empty");
        }

        let mut bridges = self.bridges.lock().unwrap();
        if let Some(existing) = bridges.get_mut(&target.beam_uuid) {
            if existing.record.login != target.login {
                bail!(
                    "Beam {} already has a bridge for login {:?}, not {:?}",
                    target.beam_uuid,
                    existing.record.login,
                    target.login
                );
            }
            existing
                .commands
                .try_send(ClientToBeam::AddSessionToken { session_token })
                .map_err(|err| anyhow::anyhow!("adding managed session to bridge: {err}"))?;
            existing.record.session_count += 1;
            let record = existing.record.clone();
            drop(bridges);
            self.broadcast(ServerEvent::BridgeStatus { bridge: record });
            return Ok(());
        }

        let client_id = self.state.lock().unwrap().client_id.clone();
        let (event_tx, event_rx) = mpsc::sync_channel(BRIDGE_EVENT_QUEUE_CAPACITY);
        let commands = crate::bridge::spawn(target.clone(), client_id, session_token, event_tx);
        let record = BridgeRecord {
            beam_id: target.beam_id,
            beam_uuid: target.beam_uuid.clone(),
            login: target.login,
            status: "connecting".to_string(),
            session_count: 1,
            bridge_id: None,
            message: None,
        };
        bridges.insert(
            target.beam_uuid,
            BridgeRuntime {
                record: record.clone(),
                commands,
            },
        );
        drop(bridges);
        self.broadcast(ServerEvent::BridgeStatus { bridge: record });

        let weak = Arc::downgrade(self);
        thread::spawn(move || handle_bridge_events(weak, event_rx));
        Ok(())
    }

    fn decide_request(&self, request_id: &str, approve: bool) -> anyhow::Result<RequestRecord> {
        let next = if approve {
            RequestStatus::Approved
        } else {
            RequestStatus::Rejected
        };
        let beam_uuid = self
            .state
            .lock()
            .unwrap()
            .requests
            .get(request_id)
            .map(|request| request.beam_uuid.clone())
            .with_context(|| format!("unknown request {request_id:?}"))?;
        let commands = self
            .bridges
            .lock()
            .unwrap()
            .get(&beam_uuid)
            .map(|bridge| bridge.commands.clone())
            .with_context(|| format!("no live bridge for Beam {beam_uuid}"))?;
        let updated = {
            let mut state = self.state.lock().unwrap();
            let request = state
                .requests
                .get_mut(request_id)
                .with_context(|| format!("unknown request {request_id:?}"))?;
            request.status = request
                .status
                .transition(next)
                .map_err(anyhow::Error::msg)?;
            request.message = Some(if approve {
                "approved by local user".to_string()
            } else {
                "rejected by local user".to_string()
            });
            let updated = request.clone();
            if let Err(err) = commands.try_send(ClientToBeam::RequestStatus {
                request_id: request_id.to_string(),
                status: updated.status,
                message: updated.message.clone(),
            }) {
                request.status = RequestStatus::PendingApproval;
                request.message = None;
                return Err(anyhow::anyhow!("sending request decision to bridge: {err}"));
            }
            updated
        };
        self.save_state()?;
        self.broadcast(ServerEvent::RequestChanged {
            request: updated.clone(),
        });
        Ok(updated)
    }

    fn expire_request(&self, request_id: &str) {
        let updated = {
            let mut state = self.state.lock().unwrap();
            let Some(request) = state.requests.get_mut(request_id) else {
                return;
            };
            if request.status != RequestStatus::PendingApproval {
                return;
            }
            request.status = RequestStatus::Expired;
            request.message = Some("local approval timed out".to_string());
            request.clone()
        };
        if let Some(commands) = self
            .bridges
            .lock()
            .unwrap()
            .get(&updated.beam_uuid)
            .map(|bridge| bridge.commands.clone())
        {
            let _ = commands.try_send(ClientToBeam::RequestStatus {
                request_id: updated.request_id.clone(),
                status: updated.status,
                message: updated.message.clone(),
            });
        }
        let _ = self.save_state();
        self.broadcast(ServerEvent::RequestChanged { request: updated });
    }
}

fn handle_bridge_events(hub: Weak<Hub>, events: Receiver<crate::bridge::Event>) {
    while let Ok(event) = events.recv() {
        let Some(hub) = hub.upgrade() else {
            return;
        };
        handle_bridge_event(&hub, event);
    }
}

fn handle_bridge_event(hub: &Arc<Hub>, event: crate::bridge::Event) {
    match event {
        crate::bridge::Event::Connected {
            beam_uuid,
            bridge_id,
        } => {
            let (record, commands) = {
                let mut bridges = hub.bridges.lock().unwrap();
                let Some(bridge) = bridges.get_mut(&beam_uuid) else {
                    return;
                };
                bridge.record.status = "connected".to_string();
                bridge.record.bridge_id = Some(bridge_id);
                bridge.record.message = None;
                (bridge.record.clone(), bridge.commands.clone())
            };
            let requests = hub
                .state
                .lock()
                .unwrap()
                .requests
                .values()
                .filter(|request| request.beam_uuid == beam_uuid)
                .filter_map(|request| {
                    request
                        .request
                        .clone()
                        .map(|service_request| RequestProjection {
                            request: service_request,
                            status: request.status,
                            message: request.message.clone(),
                        })
                })
                .collect();
            let services: Vec<ServiceProjection> = Vec::new();
            let _ = commands.send(ClientToBeam::RequestSnapshot { requests });
            let _ = commands.send(ClientToBeam::ServiceSnapshot { services });
            let _ = commands.send(ClientToBeam::Ready);
            hub.broadcast(ServerEvent::BridgeStatus { bridge: record });
        }
        crate::bridge::Event::Disconnected { beam_uuid, message } => {
            let record = {
                let mut bridges = hub.bridges.lock().unwrap();
                let Some(bridge) = bridges.get_mut(&beam_uuid) else {
                    return;
                };
                bridge.record.status = "reconnecting".to_string();
                bridge.record.bridge_id = None;
                bridge.record.message = Some(message);
                bridge.record.clone()
            };
            hub.broadcast(ServerEvent::BridgeStatus { bridge: record });
        }
        crate::bridge::Event::Message { beam_uuid, message } => match message {
            BeamToClient::RequestSubmitted { request } => {
                let record = RequestRecord {
                    request_id: request.request_id.clone(),
                    beam_uuid: beam_uuid.clone(),
                    kind: request.kind.label().to_string(),
                    status: RequestStatus::PendingApproval,
                    request: Some(request),
                    message: None,
                };
                let inserted = {
                    let mut state = hub.state.lock().unwrap();
                    if state.requests.contains_key(&record.request_id) {
                        false
                    } else {
                        state
                            .requests
                            .insert(record.request_id.clone(), record.clone());
                        true
                    }
                };
                if inserted {
                    let _ = hub.save_state();
                    hub.broadcast(ServerEvent::RequestChanged {
                        request: record.clone(),
                    });
                    let request_id = record.request_id;
                    let weak = Arc::downgrade(hub);
                    thread::spawn(move || {
                        thread::sleep(REQUEST_TIMEOUT);
                        if let Some(hub) = weak.upgrade() {
                            hub.expire_request(&request_id);
                        }
                    });
                }
            }
            BeamToClient::Error { message } => {
                let record = {
                    let mut bridges = hub.bridges.lock().unwrap();
                    let Some(bridge) = bridges.get_mut(&beam_uuid) else {
                        return;
                    };
                    bridge.record.message = Some(message);
                    bridge.record.clone()
                };
                hub.broadcast(ServerEvent::BridgeStatus { bridge: record });
            }
            BeamToClient::Pong { .. } | BeamToClient::Hello { .. } => {}
        },
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

    pub fn ensure_bridge(
        &mut self,
        target: &crate::bridge::Target,
        session_token: &str,
    ) -> anyhow::Result<()> {
        write_json(
            &mut self.write_half,
            &ClientMessage::EnsureBridge {
                beam_id: target.beam_id.clone(),
                beam_uuid: target.beam_uuid.clone(),
                login: target.login.clone(),
                session_token: session_token.to_string(),
            },
        )
        .context("requesting Beam bridge")
    }

    pub fn decide_request(&mut self, request_id: &str, approve: bool) -> anyhow::Result<()> {
        write_json(
            &mut self.write_half,
            &ClientMessage::DecideRequest {
                request_id: request_id.to_string(),
                approve,
            },
        )
        .context("sending request decision")
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
    let mut state = PersistedState::load_or_create(&state_path)?;
    if state.expire_pending_after_restart() {
        state.save(&state_path)?;
    }
    let listener = bind_socket(&socket_path)?;
    let hub = Arc::new(Hub::with_state_path(state, Some(state_path)));

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
                    ClientMessage::EnsureBridge {
                        beam_id,
                        beam_uuid,
                        login,
                        session_token,
                    } => {
                        let target = crate::bridge::Target {
                            beam_id,
                            beam_uuid,
                            login,
                        };
                        if let Err(err) = hub.ensure_bridge(target, session_token) {
                            let _ = tx.try_send(ServerEvent::Error {
                                message: format!("starting Beam bridge: {err:#}"),
                            });
                        }
                    }
                    ClientMessage::DecideRequest {
                        request_id,
                        approve,
                    } => {
                        if let Err(err) = hub.decide_request(&request_id, approve) {
                            let _ = tx.try_send(ServerEvent::Error {
                                message: format!("deciding request: {err:#}"),
                            });
                        }
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

    fn hub_with_bridge(beam_uuid: &str) -> (Arc<Hub>, Receiver<ClientToBeam>) {
        let hub = Arc::new(Hub::new(PersistedState::default()));
        let (commands, command_rx) = mpsc::sync_channel(16);
        hub.bridges.lock().unwrap().insert(
            beam_uuid.to_string(),
            BridgeRuntime {
                record: BridgeRecord {
                    beam_id: "beam-name".to_string(),
                    beam_uuid: beam_uuid.to_string(),
                    login: "beams".to_string(),
                    status: "connected".to_string(),
                    session_count: 1,
                    bridge_id: Some("bridge-id".to_string()),
                    message: None,
                },
                commands,
            },
        );
        (hub, command_rx)
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
    fn daemon_restart_expires_orphaned_pending_approvals() {
        let mut state = PersistedState::default();
        state.requests.insert(
            "pending".to_string(),
            RequestRecord {
                request_id: "pending".to_string(),
                beam_uuid: "beam-uuid".to_string(),
                kind: "local_port_forward".to_string(),
                status: RequestStatus::PendingApproval,
                request: None,
                message: None,
            },
        );
        assert!(state.expire_pending_after_restart());
        assert_eq!(state.requests["pending"].status, RequestStatus::Expired);
        assert!(!state.expire_pending_after_restart());
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

    #[test]
    fn remote_request_is_pending_until_one_local_decision_wins() {
        let (hub, commands) = hub_with_bridge("beam-uuid");
        let request = ServiceRequest {
            request_id: "request-1".to_string(),
            kind: beams_protocol::ServiceRequestKind::LocalPortForward {
                remote_host: "127.0.0.1".to_string(),
                remote_port: 8080,
                local_port: Some(18080),
            },
        };
        handle_bridge_event(
            &hub,
            crate::bridge::Event::Message {
                beam_uuid: "beam-uuid".to_string(),
                message: BeamToClient::RequestSubmitted {
                    request: request.clone(),
                },
            },
        );
        let stored = hub
            .state
            .lock()
            .unwrap()
            .requests
            .get("request-1")
            .cloned()
            .unwrap();
        assert_eq!(stored.status, RequestStatus::PendingApproval);
        assert_eq!(stored.request, Some(request));

        let decided = hub.decide_request("request-1", false).unwrap();
        assert_eq!(decided.status, RequestStatus::Rejected);
        assert!(matches!(
            commands.recv_timeout(Duration::from_secs(1)).unwrap(),
            ClientToBeam::RequestStatus {
                request_id,
                status: RequestStatus::Rejected,
                ..
            } if request_id == "request-1"
        ));
        assert!(hub.decide_request("request-1", true).is_err());
    }

    #[test]
    fn connected_bridge_receives_authoritative_request_and_service_snapshots() {
        let (hub, commands) = hub_with_bridge("beam-uuid");
        hub.state.lock().unwrap().requests.insert(
            "request-2".to_string(),
            RequestRecord {
                request_id: "request-2".to_string(),
                beam_uuid: "beam-uuid".to_string(),
                kind: "local_port_forward".to_string(),
                status: RequestStatus::Rejected,
                request: Some(ServiceRequest {
                    request_id: "request-2".to_string(),
                    kind: beams_protocol::ServiceRequestKind::LocalPortForward {
                        remote_host: "localhost".to_string(),
                        remote_port: 3000,
                        local_port: None,
                    },
                }),
                message: Some("rejected by local user".to_string()),
            },
        );

        handle_bridge_event(
            &hub,
            crate::bridge::Event::Connected {
                beam_uuid: "beam-uuid".to_string(),
                bridge_id: "new-bridge-id".to_string(),
            },
        );
        let request_snapshot = commands.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(matches!(
            request_snapshot,
            ClientToBeam::RequestSnapshot { requests }
                if requests.len() == 1
                    && requests[0].status == RequestStatus::Rejected
        ));
        assert!(matches!(
            commands.recv_timeout(Duration::from_secs(1)).unwrap(),
            ClientToBeam::ServiceSnapshot { services } if services.is_empty()
        ));
        assert!(matches!(
            commands.recv_timeout(Duration::from_secs(1)).unwrap(),
            ClientToBeam::Ready
        ));
    }

    #[test]
    fn second_frontend_for_the_same_beam_reuses_one_bridge_and_adds_its_token() {
        let (hub, commands) = hub_with_bridge("beam-uuid");
        hub.ensure_bridge(
            crate::bridge::Target {
                beam_id: "beam-name".to_string(),
                beam_uuid: "beam-uuid".to_string(),
                login: "beams".to_string(),
            },
            "second-token".to_string(),
        )
        .unwrap();
        assert_eq!(hub.bridges.lock().unwrap().len(), 1);
        assert!(matches!(
            commands.recv_timeout(Duration::from_secs(1)).unwrap(),
            ClientToBeam::AddSessionToken { session_token }
                if session_token == "second-token"
        ));
    }

    #[test]
    fn pending_request_can_expire_without_starting_a_service() {
        let (hub, commands) = hub_with_bridge("beam-uuid");
        hub.state.lock().unwrap().requests.insert(
            "request-3".to_string(),
            RequestRecord {
                request_id: "request-3".to_string(),
                beam_uuid: "beam-uuid".to_string(),
                kind: "local_port_forward".to_string(),
                status: RequestStatus::PendingApproval,
                request: None,
                message: None,
            },
        );
        hub.expire_request("request-3");
        assert_eq!(
            hub.state.lock().unwrap().requests["request-3"].status,
            RequestStatus::Expired
        );
        assert!(matches!(
            commands.recv_timeout(Duration::from_secs(1)).unwrap(),
            ClientToBeam::RequestStatus {
                status: RequestStatus::Expired,
                ..
            }
        ));
        assert!(hub.state.lock().unwrap().services.is_empty());
    }
}
