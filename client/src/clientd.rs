//! Workstation-side daemon and its local frontend protocol.
//!
//! It owns the authority boundary for client services: one process per user,
//! a permissioned UDS, persistent client identity/request state, reconnecting
//! per-Beam control bridges, local-only approval decisions, and supervised
//! workstation-owned service processes.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, Weak};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use beams_protocol::{
    ActionRequestProjection, BeamToClient, ClientToBeam, RequestProjection, RequestStatus,
    ServiceAction, ServiceActionRequest, ServiceProjection, ServiceRequest, ServiceRequestKind,
    ServiceStatus, TransferProgress, CAPABILITY_SSH_AGENT, CAPABILITY_STARTUP_MANIFEST, MAX_FRAME_BYTES,
    PROTOCOL, PROTOCOL_VERSION,
};
use crate::startup::{GrantConfig, GrantRecord, StartupConfig};
use serde::{Deserialize, Serialize};
use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
use uuid::Uuid;

const START_TIMEOUT: Duration = Duration::from_secs(3);
const START_RETRY: Duration = Duration::from_millis(50);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
const EVENT_QUEUE_CAPACITY: usize = 256;
/// The message on a request its Beam-side requester withdrew.
const WITHDRAWN: &str = "withdrawn on the Beam";
const BRIDGE_EVENT_QUEUE_CAPACITY: usize = 256;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const RATE_WINDOW: Duration = Duration::from_secs(1);
const MAX_MESSAGES_PER_WINDOW: usize = 120;
/// How long a bridge must have been failing before the daemon asks `tsh`
/// whether its Beam still exists, and how often it re-asks after that.
const BRIDGE_GONE_CHECK_AFTER: Duration = Duration::from_secs(20);
const BRIDGE_GONE_CHECK_INTERVAL: Duration = Duration::from_secs(60);
/// How often the daemon checks whether its own executable was replaced.
const EXE_CHECK_INTERVAL: Duration = Duration::from_secs(15);
/// Set (to "1") when a daemon re-execs itself after an update, so the new
/// process restores the port forwards the old one was running.
const RESTORE_SERVICES_ENV: &str = "BEAMS_DAEMON_RESTORE_SERVICES";
/// The listening socket's fd, inherited across a self-restart so the socket
/// never disappears and no frontend can race in a second daemon.
const LISTEN_FD_ENV: &str = "BEAMS_DAEMON_LISTEN_FD";

/// A frontend and daemon disagree on protocol. Distinguished from other
/// connection errors so `connect_or_start` only replaces a daemon that
/// really is incompatible, never one that was merely slow to answer.
#[derive(Debug)]
pub struct ProtocolMismatch(String);

impl std::fmt::Display for ProtocolMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "local daemon protocol mismatch: {}", self.0)
    }
}

impl std::error::Error for ProtocolMismatch {}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RequestRecord {
    pub request_id: String,
    pub beam_uuid: String,
    pub kind: String,
    pub status: RequestStatus,
    #[serde(default)]
    pub request: Option<ServiceRequest>,
    #[serde(default)]
    pub action: Option<ServiceActionRequest>,
    #[serde(default)]
    pub message: Option<String>,
    /// Workstation-only approved path. Never included in Beam projections.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_path: Option<String>,
    /// Set for SSH agent sign requests (`kind` "ssh_agent"). These live only
    /// in memory, never in the persisted state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<crate::agent::AgentPrompt>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ServiceRecord {
    pub service_id: String,
    #[serde(default)]
    pub request_id: String,
    #[serde(default)]
    pub beam_id: String,
    pub beam_uuid: String,
    #[serde(default = "default_login")]
    pub login: String,
    pub kind: String,
    #[serde(default)]
    pub spec: Option<ServiceRequestKind>,
    pub status: ServiceStatus,
    #[serde(default)]
    pub desired_running: bool,
    #[serde(default)]
    pub message: Option<String>,
    /// Workstation-only approved path. Never included in Beam projections.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_path: Option<String>,
    /// Workstation-only Mutagen session identity. Never included in Beam projections.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mutagen_session: Option<String>,
    /// Bytes and items copied so far, for transfers that report it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<TransferProgress>,
}

fn default_login() -> String {
    "beams".to_string()
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
        /// The frontend's own `SSH_AUTH_SOCK`: the agent that signs the SSH
        /// agent requests this frontend approves.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ssh_auth_sock: Option<String>,
    },
    DecideRequest {
        request_id: String,
        approve: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        local_path: Option<String>,
    },
    CreatePortForward {
        beam_id: String,
        beam_uuid: String,
        login: String,
        remote_host: String,
        remote_port: u16,
        local_port: Option<u16>,
    },
    CreateFileCopy {
        request_id: String,
        beam_id: String,
        beam_uuid: String,
        login: String,
        direction: beams_protocol::FileCopyDirection,
        local_path: String,
        remote_path: String,
        overwrite: beams_protocol::OverwritePolicy,
    },
    CreateFolderSync {
        request_id: String,
        beam_id: String,
        beam_uuid: String,
        login: String,
        local_path: String,
        remote_path: String,
        mode: beams_protocol::FolderSyncMode,
        ignore_vcs: bool,
        ignores: Vec<String>,
    },
    ControlService {
        service_id: String,
        action: ServiceAction,
    },
    /// End a startup SSH key grant now.
    RevokeGrant {
        grant_id: String,
    },
    /// `[ssh_agent] allowed_keys` changed: push keys to every Beam again.
    RefreshAgentKeys,
    /// Version-independent control op (see `daemonctl`): accepted as the
    /// first message on a connection, without a hello. Never change it.
    Status,
    /// Version-independent control op (see `daemonctl`). Never change it.
    Shutdown {
        #[serde(default)]
        force: bool,
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
        /// Usable startup SSH key grants.
        #[serde(default)]
        grants: Vec<GrantRecord>,
    },
    /// A grant was issued, used, or ended (`ended` set).
    GrantChanged {
        grant: GrantRecord,
    },
    DaemonStatus {
        frontends: usize,
    },
    RequestChanged {
        request: RequestRecord,
    },
    ServiceChanged {
        service: ServiceRecord,
    },
    BridgeStatus {
        bridge: BridgeRecord,
    },
    Pong,
    Error {
        message: String,
    },
    /// Reply to the `status` control op. Never change its shape.
    DaemonInfo {
        #[serde(flatten)]
        info: crate::daemonctl::DaemonInfo,
    },
    /// Reply to an accepted `shutdown` control op.
    ShuttingDown,
    /// Reply to a `shutdown` refused because stopping would lose work.
    ShutdownRefused {
        busy: Vec<String>,
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
    /// Beams that were already sent the startup manifest. Each Beam gets it
    /// (and its SSH grant) at most once, however often its bridge or daemon
    /// restarts.
    #[serde(default)]
    startup_submitted: BTreeSet<String>,
}

impl Default for PersistedState {
    fn default() -> Self {
        Self {
            version: 1,
            client_id: Uuid::new_v4().to_string(),
            requests: BTreeMap::new(),
            services: BTreeMap::new(),
            startup_submitted: BTreeSet::new(),
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

    fn mark_process_services_stopped_after_restart(&mut self) -> bool {
        let mut changed = false;
        for service in self.services.values_mut() {
            if matches!(
                service.status,
                ServiceStatus::Starting
                    | ServiceStatus::Running
                    | ServiceStatus::Paused
                    | ServiceStatus::Stopping
                    | ServiceStatus::Degraded
                    | ServiceStatus::Unknown
            ) {
                match service.spec {
                    Some(ServiceRequestKind::FolderSync { .. }) if service.desired_running => {
                        service.status = ServiceStatus::Unknown;
                        service.message = Some(
                            "local daemon restarted; waiting to adopt Mutagen session".to_string(),
                        );
                    }
                    Some(ServiceRequestKind::FileCopy { .. }) => {
                        service.status = ServiceStatus::Stopped;
                        service.desired_running = false;
                        service.message =
                            Some("local daemon restarted; file copy was interrupted".to_string());
                    }
                    _ => {
                        service.status = ServiceStatus::Stopped;
                        service.message = Some(
                            "local daemon restarted; waiting to restore the forward".to_string(),
                        );
                    }
                }
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
    service_runtimes: Mutex<BTreeMap<String, ServiceRuntime>>,
    forward_program: String,
    mutagen_program: String,
    mutagen_root: PathBuf,
    beam_lister: BeamLister,
    /// Set by a termination signal or the `shutdown` control op.
    terminating: Arc<AtomicBool>,
    started_at: Instant,
    /// `[ssh_agent]` config, re-read on use so edits apply without a restart.
    agent_config: Box<dyn Fn() -> crate::agent::Config + Send + Sync>,
    /// The user's known-hosts text, for naming verified host keys.
    agent_known_hosts: Box<dyn Fn() -> String + Send + Sync>,
    agent: Mutex<AgentRuntime>,
    frontend_ids: AtomicU64,
    /// `[startup]` config, read when a Beam first connects.
    startup_config: Box<dyn Fn() -> StartupConfig + Send + Sync>,
}

/// SSH agent relay state (see `crate::agent`). Nothing here is persisted:
/// a pending sign request can't outlive the Beam-side connection waiting
/// for it anyway.
#[derive(Default)]
struct AgentRuntime {
    /// Frontend connection id -> (Beam UUID, that frontend's SSH_AUTH_SOCK).
    frontends: BTreeMap<u64, (String, Option<String>)>,
    binds: crate::agent::BindStore,
    /// Local request id -> waiting sign request.
    pending: BTreeMap<String, PendingAgent>,
    /// Beams whose bridge advertised `CAPABILITY_SSH_AGENT`.
    capable: HashSet<String>,
    /// Usable startup grants by id.
    grants: BTreeMap<String, GrantRecord>,
    /// Startup manifests sent but not yet accepted: request id ->
    /// (Beam UUID, grant to issue once accepted, its use budget).
    startup_requests: BTreeMap<String, (String, Option<GrantConfig>, u32)>,
}

struct PendingAgent {
    beam_request_id: String,
    body: Vec<u8>,
    record: RequestRecord,
    /// See `crate::agent::fold_key`.
    fold_key: Option<String>,
}

use crate::agent::now_ms;

fn load_startup_config() -> StartupConfig {
    StartupConfig::load().unwrap_or_else(|err| {
        eprintln!("startup: config ignored, using the default tasks: {err:#}");
        StartupConfig::default()
    })
}

fn load_agent_config() -> crate::agent::Config {
    crate::agent::Config::load().unwrap_or_else(|err| {
        eprintln!("ssh-agent: config ignored, proxy disabled: {err:#}");
        crate::agent::Config::default()
    })
}

struct BridgeRuntime {
    record: BridgeRecord,
    commands: SyncSender<ClientToBeam>,
    /// When the current run of failed connection attempts began.
    failing_since: Option<Instant>,
    last_existence_check: Option<Instant>,
}

impl BridgeRuntime {
    fn new(record: BridgeRecord, commands: SyncSender<ClientToBeam>) -> Self {
        Self {
            record,
            commands,
            failing_since: None,
            last_existence_check: None,
        }
    }
}

/// Lists the UUIDs of the Beams that currently exist. `Err` means "can't
/// tell right now" (offline, logged out), never "none exist".
type BeamLister = Box<dyn Fn() -> anyhow::Result<Vec<String>> + Send + Sync>;

fn list_beam_uuids() -> anyhow::Result<Vec<String>> {
    Ok(crate::tsh::list_beams()?
        .into_iter()
        .map(|beam| beam.uuid)
        .collect())
}

struct ServiceRuntime {
    actions: RuntimeActions,
}

enum RuntimeActions {
    Forward(SyncSender<crate::forward::Action>),
    Copy(SyncSender<crate::copy::Action>),
    Sync(SyncSender<crate::sync::Action>),
}

impl Hub {
    #[cfg(test)]
    fn new(state: PersistedState) -> Self {
        Self::with_state_path_and_program(state, None, "tsh".to_string())
    }

    #[cfg(test)]
    fn with_state_path_and_program(
        state: PersistedState,
        state_path: Option<PathBuf>,
        forward_program: String,
    ) -> Self {
        let mutagen_root = std::env::temp_dir().join(format!("beams-mutagen-{}", Uuid::new_v4()));
        let mut hub = Self::with_programs(
            state,
            state_path,
            forward_program,
            "mutagen".to_string(),
            mutagen_root,
        );
        // Never the developer's real config or known_hosts.
        hub.agent_config = Box::new(crate::agent::Config::default);
        hub.agent_known_hosts = Box::new(String::new);
        hub.startup_config = Box::new(|| StartupConfig { tasks: Vec::new(), ssh_grant: None });
        hub
    }

    fn with_programs(
        state: PersistedState,
        state_path: Option<PathBuf>,
        forward_program: String,
        mutagen_program: String,
        mutagen_root: PathBuf,
    ) -> Self {
        Self {
            state: Mutex::new(state),
            state_path,
            subscribers: Mutex::new(Vec::new()),
            frontends: AtomicUsize::new(0),
            bridges: Mutex::new(BTreeMap::new()),
            service_runtimes: Mutex::new(BTreeMap::new()),
            forward_program,
            mutagen_program,
            mutagen_root,
            beam_lister: Box::new(list_beam_uuids),
            terminating: Arc::new(AtomicBool::new(false)),
            started_at: Instant::now(),
            agent_config: Box::new(load_agent_config),
            agent_known_hosts: Box::new(crate::agent::read_known_hosts),
            agent: Mutex::new(AgentRuntime::default()),
            frontend_ids: AtomicU64::new(1),
            startup_config: Box::new(load_startup_config),
        }
    }

    /// Why stopping the daemon right now would lose something. Forwards and
    /// syncs aren't listed: a restart restores forwards and re-adopts syncs.
    fn busy_reasons(&self) -> Vec<String> {
        let state = self.state.lock().unwrap();
        let copies = state
            .services
            .values()
            .filter(|service| {
                matches!(service.spec, Some(ServiceRequestKind::FileCopy { .. }))
                    && matches!(
                        service.status,
                        ServiceStatus::Starting | ServiceStatus::Running | ServiceStatus::Stopping
                    )
            })
            .count();
        // SSH key requests aren't persisted, so a restart would drop them.
        let pending = state
            .requests
            .values()
            .filter(|request| request.status == RequestStatus::PendingApproval)
            .count()
            + self.agent.lock().unwrap().pending.len();
        let mut reasons = Vec::new();
        if copies > 0 {
            reasons.push(format!("{copies} file copy job(s) in progress"));
        }
        if pending > 0 {
            reasons.push(format!("{pending} request(s) awaiting approval"));
        }
        reasons
    }

    fn daemon_info(&self) -> crate::daemonctl::DaemonInfo {
        let exe = crate::daemonctl::startup_exe();
        crate::daemonctl::DaemonInfo {
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            protocol_version: PROTOCOL_VERSION,
            uptime_secs: self.started_at.elapsed().as_secs(),
            exe: exe.cloned(),
            binary_replaced: exe.is_some_and(|exe| exe.replacement().is_some()),
            busy: self.busy_reasons(),
            frontends: self.frontends.load(Ordering::SeqCst),
            bridges: self.bridges.lock().unwrap().len(),
            services: self.state.lock().unwrap().services.len(),
        }
    }

    /// Handles the `shutdown` control op, returning the reply to send.
    fn request_shutdown(&self, force: bool) -> ServerEvent {
        let busy = self.busy_reasons();
        if !force && !busy.is_empty() {
            return ServerEvent::ShutdownRefused { busy };
        }
        eprintln!("beams daemon: shutdown requested{}", if force { " (forced)" } else { "" });
        self.terminating.store(true, Ordering::SeqCst);
        ServerEvent::ShuttingDown
    }

    /// After a self-restart, start every forward the previous process was
    /// running, rather than waiting for a `beams ssh` session to re-ensure
    /// its Beam's bridge. (Syncs are re-adopted separately; interrupted
    /// copies stay stopped.)
    fn restore_forwards(self: &Arc<Self>) {
        let targets = self
            .state
            .lock()
            .unwrap()
            .services
            .values()
            .filter(|service| {
                service.desired_running
                    && matches!(service.spec, Some(ServiceRequestKind::LocalPortForward { .. }))
            })
            .map(|service| crate::bridge::Target {
                beam_id: service.beam_id.clone(),
                beam_uuid: service.beam_uuid.clone(),
                login: service.login.clone(),
            })
            .collect::<Vec<_>>();
        let mut seen = std::collections::BTreeSet::new();
        for target in targets {
            if seen.insert(target.beam_uuid.clone()) {
                self.recover_services(&target);
            }
        }
    }

    fn recover_folder_syncs(self: &Arc<Self>) {
        let services = self
            .state
            .lock()
            .unwrap()
            .services
            .values()
            .filter(|service| {
                service.desired_running
                    && matches!(service.spec, Some(ServiceRequestKind::FolderSync { .. }))
            })
            .cloned()
            .collect::<Vec<_>>();
        for mut service in services {
            if self
                .service_runtimes
                .lock()
                .unwrap()
                .contains_key(&service.service_id)
            {
                continue;
            }
            service.status = ServiceStatus::Starting;
            service.message =
                Some("adopting Mutagen session after local daemon restart".to_string());
            self.state
                .lock()
                .unwrap()
                .services
                .insert(service.service_id.clone(), service.clone());
            let _ = self.save_state();
            self.publish_service(&service);
            let target = crate::bridge::Target {
                beam_id: service.beam_id.clone(),
                beam_uuid: service.beam_uuid.clone(),
                login: service.login.clone(),
            };
            if let Err(err) = self.spawn_service_worker(&service, &target) {
                handle_service_update(
                    self,
                    service.service_id.clone(),
                    service.spec.clone().expect("filtered above"),
                    ServiceStatus::Error,
                    Some(format!("adopting folder sync: {err:#}")),
                );
            }
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
        let mut requests: Vec<RequestRecord> = state.requests.values().cloned().collect();
        let grants = {
            let agent = self.agent.lock().unwrap();
            requests.extend(agent.pending.values().map(|p| p.record.clone()));
            agent.grants.values().cloned().collect()
        };
        ServerEvent::Snapshot {
            client_id: state.client_id.clone(),
            frontends: self.frontends.load(Ordering::SeqCst),
            grants,
            requests,
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
            self.recover_services(&target);
            return Ok(());
        }

        let client_id = self.state.lock().unwrap().client_id.clone();
        let (event_tx, event_rx) = mpsc::sync_channel(BRIDGE_EVENT_QUEUE_CAPACITY);
        let commands = crate::bridge::spawn(target.clone(), client_id, session_token, event_tx);
        let record = BridgeRecord {
            beam_id: target.beam_id.clone(),
            beam_uuid: target.beam_uuid.clone(),
            login: target.login.clone(),
            status: "connecting".to_string(),
            session_count: 1,
            bridge_id: None,
            message: None,
        };
        bridges.insert(
            target.beam_uuid.clone(),
            BridgeRuntime::new(record.clone(), commands),
        );
        drop(bridges);
        self.broadcast(ServerEvent::BridgeStatus { bridge: record });
        self.recover_services(&target);

        let weak = Arc::downgrade(self);
        thread::spawn(move || handle_bridge_events(weak, event_rx));
        Ok(())
    }

    /// Bridges otherwise live for the daemon's whole lifetime and retry
    /// forever, so a Beam deleted after a `beams ssh` session would leave a
    /// supervisor spawning `tsh ssh` against it indefinitely. Once a bridge
    /// has been failing for a while, retire it if its Beam no longer exists:
    /// drop the bridge (its supervisor exits when the command channel
    /// closes), stop that Beam's forwards and copies, and pause its syncs
    /// (pausing is reversible). A later `beams ssh` to the same Beam simply
    /// creates a fresh bridge, so a wrong call here heals itself.
    fn retire_bridge_if_beam_gone(self: &Arc<Self>, beam_uuid: &str) {
        let Ok(existing) = (self.beam_lister)() else {
            return;
        };
        if existing.iter().any(|uuid| uuid == beam_uuid) {
            return;
        }
        let record = {
            let mut bridges = self.bridges.lock().unwrap();
            // It may have reconnected while `tsh beams ls` ran.
            if !bridges
                .get(beam_uuid)
                .is_some_and(|bridge| bridge.failing_since.is_some())
            {
                return;
            }
            let mut bridge = bridges.remove(beam_uuid).expect("checked above");
            bridge.record.status = "retired".to_string();
            bridge.record.bridge_id = None;
            bridge.record.message = Some("Beam no longer exists; bridge stopped".to_string());
            bridge.record
        };
        self.broadcast(ServerEvent::BridgeStatus { bridge: record });

        let services = self
            .state
            .lock()
            .unwrap()
            .services
            .values()
            .filter(|service| service.beam_uuid == beam_uuid && service.desired_running)
            .filter_map(|service| {
                let action = match service.spec.as_ref()? {
                    ServiceRequestKind::LocalPortForward { .. }
                    | ServiceRequestKind::FileCopy { .. }
                    | ServiceRequestKind::FileFetch { .. } => ServiceAction::Stop,
                    ServiceRequestKind::FolderSync { .. } => ServiceAction::Pause,
                };
                Some((service.service_id.clone(), action))
            })
            .collect::<Vec<_>>();
        for (service_id, action) in services {
            let _ = self.control_service(&service_id, action);
        }
    }

    fn recover_services(self: &Arc<Self>, target: &crate::bridge::Target) {
        let services = self
            .state
            .lock()
            .unwrap()
            .services
            .values()
            .filter(|service| {
                service.beam_uuid == target.beam_uuid
                    && service.desired_running
                    && service.spec.is_some()
            })
            .cloned()
            .collect::<Vec<_>>();
        for mut service in services {
            if self
                .service_runtimes
                .lock()
                .unwrap()
                .contains_key(&service.service_id)
            {
                continue;
            }
            service.status = ServiceStatus::Starting;
            service.message = Some("restoring after local daemon restart".to_string());
            self.state
                .lock()
                .unwrap()
                .services
                .insert(service.service_id.clone(), service.clone());
            let _ = self.save_state();
            self.publish_service(&service);
            if let Err(err) = self.spawn_service_worker(&service, target) {
                handle_service_event(
                    self,
                    crate::forward::Event {
                        service_id: service.service_id.clone(),
                        kind: service.spec.clone().expect("filtered above"),
                        status: ServiceStatus::Error,
                        message: Some(format!("restoring service: {err:#}")),
                    },
                );
            }
        }
    }

    fn decide_request(
        self: &Arc<Self>,
        request_id: &str,
        approve: bool,
        local_path: Option<String>,
    ) -> anyhow::Result<RequestRecord> {
        let next = if approve {
            RequestStatus::Approved
        } else {
            RequestStatus::Rejected
        };
        let (beam_uuid, is_action) = self
            .state
            .lock()
            .unwrap()
            .requests
            .get(request_id)
            .map(|request| (request.beam_uuid.clone(), request.action.is_some()))
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
            if approve
                && matches!(
                    request.request.as_ref().map(|request| &request.kind),
                    Some(
                        ServiceRequestKind::FileCopy { .. }
                            | ServiceRequestKind::FileFetch { .. }
                            | ServiceRequestKind::FolderSync { .. }
                    )
                )
            {
                let path = local_path
                    .as_deref()
                    .context("approval requires a workstation path")?;
                let path = normalize_local_path(path)?;
                if matches!(
                    request.request.as_ref().map(|request| &request.kind),
                    Some(ServiceRequestKind::FolderSync { .. })
                ) && !Path::new(&path).is_dir()
                {
                    bail!("folder-sync workstation path must be an existing directory");
                }
                request.local_path = Some(path);
            }
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
            let decision = if is_action {
                ClientToBeam::ServiceActionStatus {
                    request_id: request_id.to_string(),
                    status: updated.status,
                    message: updated.message.clone(),
                }
            } else {
                ClientToBeam::RequestStatus {
                    request_id: request_id.to_string(),
                    status: updated.status,
                    message: updated.message.clone(),
                }
            };
            if let Err(err) = commands.try_send(decision) {
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
        if approve {
            let result = if is_action {
                self.start_action_request(request_id)
            } else {
                self.start_service_request(request_id)
            };
            if let Err(err) = result {
                self.fail_request(request_id, is_action, &format!("{err:#}"));
                return Err(err);
            }
        }
        Ok(updated)
    }

    fn fail_request(&self, request_id: &str, action: bool, message: &str) {
        let updated = {
            let mut state = self.state.lock().unwrap();
            let Some(request) = state.requests.get_mut(request_id) else {
                return;
            };
            if !matches!(
                request.status,
                RequestStatus::Approved | RequestStatus::Starting
            ) {
                return;
            }
            request.status = RequestStatus::Failed;
            request.message = Some(message.to_string());
            request.clone()
        };
        let _ = self.save_state();
        self.publish_request(&updated, action);
    }

    fn create_port_forward(
        self: &Arc<Self>,
        target: crate::bridge::Target,
        remote_host: String,
        remote_port: u16,
        local_port: Option<u16>,
    ) -> anyhow::Result<RequestRecord> {
        validate_port_forward(&remote_host, remote_port, local_port)?;
        if target.beam_uuid.is_empty() || target.login.is_empty() {
            bail!("Beam UUID and login must not be empty");
        }
        let request = ServiceRequest {
            request_id: Uuid::new_v4().to_string(),
            kind: ServiceRequestKind::LocalPortForward {
                remote_host,
                remote_port,
                local_port,
            },
        };
        let record = RequestRecord {
            request_id: request.request_id.clone(),
            beam_uuid: target.beam_uuid.clone(),
            kind: request.kind.label().to_string(),
            status: RequestStatus::Approved,
            request: Some(request),
            action: None,
            message: Some("created by local user".to_string()),
            local_path: None,
            agent: None,
        };
        {
            let mut state = self.state.lock().unwrap();
            state
                .requests
                .insert(record.request_id.clone(), record.clone());
        }
        self.save_state()?;
        self.broadcast(ServerEvent::RequestChanged {
            request: record.clone(),
        });
        if let Err(err) = self.start_service_request_with_target(&record.request_id, target) {
            self.fail_request(&record.request_id, false, &format!("{err:#}"));
            return Err(err);
        }
        Ok(record)
    }

    fn create_file_copy(
        self: &Arc<Self>,
        request_id: String,
        target: crate::bridge::Target,
        direction: beams_protocol::FileCopyDirection,
        local_path: String,
        remote_path: String,
        overwrite: beams_protocol::OverwritePolicy,
    ) -> anyhow::Result<RequestRecord> {
        if target.beam_uuid.is_empty() || target.login.is_empty() {
            bail!("Beam UUID and login must not be empty");
        }
        if remote_path.is_empty() {
            bail!("remote path must not be empty");
        }
        let local_path = normalize_local_path(&local_path)?;
        let local_label = Path::new(&local_path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("workstation file")
            .to_string();
        if request_id.is_empty() {
            bail!("request ID must not be empty");
        }
        let request = ServiceRequest {
            request_id,
            kind: ServiceRequestKind::FileCopy {
                direction,
                remote_path,
                local_path_hint: Some(local_label),
                overwrite,
            },
        };
        let record = RequestRecord {
            request_id: request.request_id.clone(),
            beam_uuid: target.beam_uuid.clone(),
            kind: request.kind.label().to_string(),
            status: RequestStatus::Approved,
            request: Some(request),
            action: None,
            message: Some("created by local user".to_string()),
            local_path: Some(local_path),
            agent: None,
        };
        self.state
            .lock()
            .unwrap()
            .requests
            .insert(record.request_id.clone(), record.clone());
        self.save_state()?;
        self.broadcast(ServerEvent::RequestChanged {
            request: record.clone(),
        });
        if let Err(err) = self.start_service_request_with_target(&record.request_id, target) {
            self.fail_request(&record.request_id, false, &format!("{err:#}"));
            return Err(err);
        }
        Ok(record)
    }

    #[allow(clippy::too_many_arguments)]
    fn create_folder_sync(
        self: &Arc<Self>,
        request_id: String,
        target: crate::bridge::Target,
        local_path: String,
        remote_path: String,
        mode: beams_protocol::FolderSyncMode,
        ignore_vcs: bool,
        ignores: Vec<String>,
    ) -> anyhow::Result<RequestRecord> {
        if request_id.is_empty() || target.beam_uuid.is_empty() || target.login.is_empty() {
            bail!("request ID, Beam UUID, and login must not be empty");
        }
        if remote_path.is_empty() {
            bail!("remote path must not be empty");
        }
        if ignores.iter().any(|ignore| ignore.is_empty()) {
            bail!("ignore rules must not be empty");
        }
        let local_path = normalize_local_path(&local_path)?;
        if !Path::new(&local_path).is_dir() {
            bail!("folder-sync workstation path must be an existing directory");
        }
        let local_label = Path::new(&local_path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("workstation directory")
            .to_string();
        let request = ServiceRequest {
            request_id,
            kind: ServiceRequestKind::FolderSync {
                remote_path,
                local_path_hint: Some(local_label),
                mode,
                ignore_vcs,
                ignores,
            },
        };
        let record = RequestRecord {
            request_id: request.request_id.clone(),
            beam_uuid: target.beam_uuid.clone(),
            kind: request.kind.label().to_string(),
            status: RequestStatus::Approved,
            request: Some(request),
            action: None,
            message: Some("created by local user".to_string()),
            local_path: Some(local_path),
            agent: None,
        };
        self.state
            .lock()
            .unwrap()
            .requests
            .insert(record.request_id.clone(), record.clone());
        self.save_state()?;
        self.broadcast(ServerEvent::RequestChanged {
            request: record.clone(),
        });
        if let Err(err) = self.start_service_request_with_target(&record.request_id, target) {
            self.fail_request(&record.request_id, false, &format!("{err:#}"));
            return Err(err);
        }
        Ok(record)
    }

    fn start_service_request(self: &Arc<Self>, request_id: &str) -> anyhow::Result<()> {
        let beam_uuid = self
            .state
            .lock()
            .unwrap()
            .requests
            .get(request_id)
            .map(|request| request.beam_uuid.clone())
            .with_context(|| format!("unknown request {request_id:?}"))?;
        let target = {
            let bridges = self.bridges.lock().unwrap();
            let bridge = bridges
                .get(&beam_uuid)
                .with_context(|| format!("no bridge target for Beam {beam_uuid}"))?;
            crate::bridge::Target {
                beam_id: bridge.record.beam_id.clone(),
                beam_uuid: bridge.record.beam_uuid.clone(),
                login: bridge.record.login.clone(),
            }
        };
        self.start_service_request_with_target(request_id, target)
    }

    fn start_service_request_with_target(
        self: &Arc<Self>,
        request_id: &str,
        target: crate::bridge::Target,
    ) -> anyhow::Result<()> {
        let (request, service) = {
            let mut state = self.state.lock().unwrap();
            let request = state
                .requests
                .get_mut(request_id)
                .with_context(|| format!("unknown request {request_id:?}"))?;
            let service_request = request
                .request
                .clone()
                .context("request does not contain a service specification")?;
            request.status = request
                .status
                .transition(RequestStatus::Starting)
                .map_err(anyhow::Error::msg)?;
            request.message = Some("starting workstation service".to_string());
            let updated_request = request.clone();
            let desired_running = matches!(
                service_request.kind,
                ServiceRequestKind::LocalPortForward { .. } | ServiceRequestKind::FolderSync { .. }
            );
            let service_id = Uuid::new_v4().to_string();
            let mutagen_session =
                matches!(service_request.kind, ServiceRequestKind::FolderSync { .. })
                    .then(|| crate::sync::session_name(&target.beam_uuid, &service_id));
            let service = ServiceRecord {
                service_id,
                request_id: request_id.to_string(),
                beam_id: target.beam_id.clone(),
                beam_uuid: target.beam_uuid.clone(),
                login: target.login.clone(),
                kind: service_request.kind.label().to_string(),
                spec: Some(service_request.kind),
                status: ServiceStatus::Starting,
                desired_running,
                message: Some("starting workstation service".to_string()),
                local_path: request.local_path.clone(),
                mutagen_session,
                progress: None,
            };
            state
                .services
                .insert(service.service_id.clone(), service.clone());
            (updated_request, service)
        };
        self.save_state()?;
        self.publish_request(&request, false);
        self.publish_service(&service);

        self.spawn_service_worker(&service, &target)
    }

    fn spawn_service_worker(
        self: &Arc<Self>,
        service: &ServiceRecord,
        target: &crate::bridge::Target,
    ) -> anyhow::Result<()> {
        let spec = service
            .spec
            .clone()
            .context("service specification disappeared before start")?;
        match &spec {
            ServiceRequestKind::LocalPortForward { .. } => {
                let (event_tx, event_rx) = mpsc::sync_channel(EVENT_QUEUE_CAPACITY);
                let actions = crate::forward::spawn(
                    self.forward_program.clone(),
                    crate::forward::Target {
                        beam_uuid: target.beam_uuid.clone(),
                        login: target.login.clone(),
                    },
                    service.service_id.clone(),
                    spec,
                    event_tx,
                );
                self.service_runtimes.lock().unwrap().insert(
                    service.service_id.clone(),
                    ServiceRuntime {
                        actions: RuntimeActions::Forward(actions),
                    },
                );
                let weak = Arc::downgrade(self);
                thread::spawn(move || handle_service_events(weak, event_rx));
            }
            ServiceRequestKind::FileCopy { .. } => {
                let local_path = service
                    .local_path
                    .as_deref()
                    .context("file-copy service has no approved workstation path")?;
                let (event_tx, event_rx) = mpsc::sync_channel(EVENT_QUEUE_CAPACITY);
                let actions = crate::copy::spawn(
                    self.forward_program.clone(),
                    crate::copy::Target {
                        beam_uuid: target.beam_uuid.clone(),
                        login: target.login.clone(),
                    },
                    service.service_id.clone(),
                    spec,
                    PathBuf::from(local_path),
                    event_tx,
                );
                self.service_runtimes.lock().unwrap().insert(
                    service.service_id.clone(),
                    ServiceRuntime {
                        actions: RuntimeActions::Copy(actions),
                    },
                );
                let weak = Arc::downgrade(self);
                thread::spawn(move || handle_copy_events(weak, event_rx));
            }
            ServiceRequestKind::FileFetch { .. } => {
                let local_path = service
                    .local_path
                    .as_deref()
                    .context("fetch has no approved workstation destination")?;
                let (event_tx, event_rx) = mpsc::sync_channel(EVENT_QUEUE_CAPACITY);
                let actions = crate::copy::spawn_fetch(
                    self.forward_program.clone(),
                    crate::copy::Target {
                        beam_uuid: target.beam_uuid.clone(),
                        login: target.login.clone(),
                    },
                    service.service_id.clone(),
                    spec,
                    PathBuf::from(local_path),
                    event_tx,
                );
                self.service_runtimes.lock().unwrap().insert(
                    service.service_id.clone(),
                    ServiceRuntime {
                        actions: RuntimeActions::Copy(actions),
                    },
                );
                let weak = Arc::downgrade(self);
                thread::spawn(move || handle_copy_events(weak, event_rx));
            }
            ServiceRequestKind::FolderSync { .. } => {
                let local_path = service
                    .local_path
                    .as_deref()
                    .context("folder-sync service has no approved workstation path")?;
                let session_name = service
                    .mutagen_session
                    .clone()
                    .context("folder-sync service has no persisted Mutagen session name")?;
                let (event_tx, event_rx) = mpsc::sync_channel(EVENT_QUEUE_CAPACITY);
                let actions = crate::sync::spawn(
                    self.mutagen_program.clone(),
                    self.forward_program.clone(),
                    self.mutagen_root.clone(),
                    crate::sync::Target {
                        beam_uuid: target.beam_uuid.clone(),
                        login: target.login.clone(),
                    },
                    service.service_id.clone(),
                    session_name,
                    spec,
                    PathBuf::from(local_path),
                    event_tx,
                );
                self.service_runtimes.lock().unwrap().insert(
                    service.service_id.clone(),
                    ServiceRuntime {
                        actions: RuntimeActions::Sync(actions),
                    },
                );
                let weak = Arc::downgrade(self);
                thread::spawn(move || handle_sync_events(weak, event_rx));
            }
        }
        Ok(())
    }

    fn control_service(
        &self,
        service_id: &str,
        action: ServiceAction,
    ) -> anyhow::Result<ServiceRecord> {
        let spec = self
            .state
            .lock()
            .unwrap()
            .services
            .get(service_id)
            .and_then(|service| service.spec.clone())
            .with_context(|| format!("unknown service {service_id:?}"))?;
        match (&spec, action) {
            (
                ServiceRequestKind::LocalPortForward { .. },
                ServiceAction::Stop | ServiceAction::Restart,
            )
            | (
                ServiceRequestKind::FileCopy { .. } | ServiceRequestKind::FileFetch { .. },
                ServiceAction::Stop,
            )
            | (
                ServiceRequestKind::FolderSync { .. },
                ServiceAction::Pause | ServiceAction::Resume | ServiceAction::Terminate,
            ) => {}
            (ServiceRequestKind::FileCopy { .. } | ServiceRequestKind::FileFetch { .. }, _) => {
                bail!("file-copy jobs only support cancellation")
            }
            (ServiceRequestKind::LocalPortForward { .. }, _) => {
                bail!("port forwards only support stop and restart")
            }
            (
                ServiceRequestKind::FolderSync { .. },
                ServiceAction::Stop | ServiceAction::Restart,
            ) => {
                bail!("folder syncs use pause/resume/terminate; terminated syncs must be recreated")
            }
        }
        let actions = self
            .service_runtimes
            .lock()
            .unwrap()
            .get(service_id)
            .map(|runtime| match &runtime.actions {
                RuntimeActions::Forward(actions) => RuntimeActions::Forward(actions.clone()),
                RuntimeActions::Copy(actions) => RuntimeActions::Copy(actions.clone()),
                RuntimeActions::Sync(actions) => RuntimeActions::Sync(actions.clone()),
            })
            .with_context(|| {
                format!("service {service_id:?} is not owned by this daemon process")
            })?;
        let updated = {
            let mut state = self.state.lock().unwrap();
            let service = state
                .services
                .get_mut(service_id)
                .with_context(|| format!("unknown service {service_id:?}"))?;
            match action {
                ServiceAction::Stop => {
                    if service.status != ServiceStatus::Stopping {
                        service.status = service
                            .status
                            .transition(ServiceStatus::Stopping)
                            .map_err(anyhow::Error::msg)?;
                    }
                    service.desired_running = false;
                    service.message = Some("stopping".to_string());
                }
                ServiceAction::Restart => {
                    if matches!(
                        service.status,
                        ServiceStatus::Running | ServiceStatus::Degraded | ServiceStatus::Unknown
                    ) {
                        service.status = service
                            .status
                            .transition(ServiceStatus::Stopping)
                            .map_err(anyhow::Error::msg)?;
                    }
                    service.desired_running = true;
                    service.message = Some("restart requested".to_string());
                }
                ServiceAction::Pause => {
                    if service.status == ServiceStatus::Paused {
                        bail!("folder sync is already paused");
                    }
                    service.desired_running = true;
                    service.message = Some("pause requested".to_string());
                }
                ServiceAction::Resume => {
                    if service.status != ServiceStatus::Paused {
                        bail!("folder sync is not paused");
                    }
                    service.desired_running = true;
                    service.message = Some("resume requested".to_string());
                }
                ServiceAction::Terminate => {
                    if service.status != ServiceStatus::Stopping {
                        service.status = service
                            .status
                            .transition(ServiceStatus::Stopping)
                            .map_err(anyhow::Error::msg)?;
                    }
                    service.desired_running = false;
                    service.message = Some("terminating".to_string());
                }
            }
            service.clone()
        };
        match (actions, action) {
            (RuntimeActions::Forward(actions), ServiceAction::Stop) => actions
                .try_send(crate::forward::Action::Stop)
                .map_err(|err| anyhow::anyhow!("sending service action: {err}"))?,
            (RuntimeActions::Forward(actions), ServiceAction::Restart) => actions
                .try_send(crate::forward::Action::Restart)
                .map_err(|err| anyhow::anyhow!("sending service action: {err}"))?,
            (RuntimeActions::Copy(actions), ServiceAction::Stop) => actions
                .try_send(crate::copy::Action::Stop)
                .map_err(|err| anyhow::anyhow!("sending copy cancellation: {err}"))?,
            (RuntimeActions::Copy(_), ServiceAction::Restart) => {
                bail!("completed or interrupted file-copy jobs cannot be restarted")
            }
            (RuntimeActions::Sync(actions), ServiceAction::Pause) => actions
                .try_send(crate::sync::Action::Pause)
                .map_err(|err| anyhow::anyhow!("sending sync pause: {err}"))?,
            (RuntimeActions::Sync(actions), ServiceAction::Resume) => actions
                .try_send(crate::sync::Action::Resume)
                .map_err(|err| anyhow::anyhow!("sending sync resume: {err}"))?,
            (RuntimeActions::Sync(actions), ServiceAction::Terminate) => {
                actions
                    .try_send(crate::sync::Action::Terminate)
                    .map_err(|err| anyhow::anyhow!("sending sync termination: {err}"))?
            }
            (RuntimeActions::Sync(_), ServiceAction::Restart)
            | (RuntimeActions::Forward(_), ServiceAction::Pause)
            | (RuntimeActions::Forward(_), ServiceAction::Resume)
            | (RuntimeActions::Forward(_), ServiceAction::Terminate)
            | (RuntimeActions::Copy(_), ServiceAction::Pause)
            | (RuntimeActions::Copy(_), ServiceAction::Resume)
            | (RuntimeActions::Copy(_), ServiceAction::Terminate)
            | (RuntimeActions::Sync(_), ServiceAction::Stop) => {
                unreachable!("service/action compatibility validated above")
            }
        }
        self.save_state()?;
        self.publish_service(&updated);
        Ok(updated)
    }

    fn start_action_request(self: &Arc<Self>, request_id: &str) -> anyhow::Result<()> {
        let action = {
            let mut state = self.state.lock().unwrap();
            let request = state
                .requests
                .get_mut(request_id)
                .with_context(|| format!("unknown action request {request_id:?}"))?;
            let action = request
                .action
                .clone()
                .context("request does not contain a service action")?;
            request.status = request
                .status
                .transition(RequestStatus::Starting)
                .map_err(anyhow::Error::msg)?;
            request.message = Some("performing service action".to_string());
            action
        };
        self.save_state()?;
        let request = self.state.lock().unwrap().requests[request_id].clone();
        self.publish_request(&request, true);
        self.control_service(&action.service_id, action.action)?;
        Ok(())
    }

    fn publish_request(&self, request: &RequestRecord, action: bool) {
        if let Some(commands) = self
            .bridges
            .lock()
            .unwrap()
            .get(&request.beam_uuid)
            .map(|bridge| bridge.commands.clone())
        {
            let message = if action {
                ClientToBeam::ServiceActionStatus {
                    request_id: request.request_id.clone(),
                    status: request.status,
                    message: request.message.clone(),
                }
            } else {
                ClientToBeam::RequestStatus {
                    request_id: request.request_id.clone(),
                    status: request.status,
                    message: request.message.clone(),
                }
            };
            let _ = commands.try_send(message);
        }
        self.broadcast(ServerEvent::RequestChanged {
            request: request.clone(),
        });
    }

    fn publish_service(&self, service: &ServiceRecord) {
        if let Some(projection) = service_projection(service) {
            if let Some(commands) = self
                .bridges
                .lock()
                .unwrap()
                .get(&service.beam_uuid)
                .map(|bridge| bridge.commands.clone())
            {
                let _ = commands.try_send(ClientToBeam::ServiceStatus {
                    service: projection,
                });
            }
        }
        self.broadcast(ServerEvent::ServiceChanged {
            service: service.clone(),
        });
    }

    fn shutdown(&self) {
        let actions = self
            .service_runtimes
            .lock()
            .unwrap()
            .values()
            .map(|runtime| match &runtime.actions {
                RuntimeActions::Forward(actions) => RuntimeActions::Forward(actions.clone()),
                RuntimeActions::Copy(actions) => RuntimeActions::Copy(actions.clone()),
                RuntimeActions::Sync(actions) => RuntimeActions::Sync(actions.clone()),
            })
            .collect::<Vec<_>>();
        for actions in actions {
            match actions {
                RuntimeActions::Forward(actions) => {
                    let _ = actions.try_send(crate::forward::Action::Stop);
                }
                RuntimeActions::Copy(actions) => {
                    let _ = actions.try_send(crate::copy::Action::Stop);
                }
                RuntimeActions::Sync(actions) => {
                    let _ = actions.try_send(crate::sync::Action::Detach);
                }
            }
        }
        // Dropping every bridge command sender makes each supervisor kill and
        // reap its current SSH child instead of reconnecting.
        self.bridges.lock().unwrap().clear();

        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            let active = self.state.lock().unwrap().services.values().any(|service| {
                matches!(
                    service.status,
                    ServiceStatus::Starting
                        | ServiceStatus::Running
                        | ServiceStatus::Paused
                        | ServiceStatus::Stopping
                        | ServiceStatus::Degraded
                        | ServiceStatus::Unknown
                )
            });
            if !active {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// The Beam-side requester gave up (`beams fetch` interrupted): drop a
    /// pending request, or stop the one-shot service it started.
    fn withdraw_request(self: &Arc<Self>, beam_uuid: &str, request_id: &str) {
        let (pending, service) = {
            let mut state = self.state.lock().unwrap();
            let Some(request) = state
                .requests
                .get_mut(request_id)
                .filter(|request| request.beam_uuid == beam_uuid)
            else {
                return;
            };
            if request.status == RequestStatus::PendingApproval {
                request.status = RequestStatus::Expired;
                request.message = Some(WITHDRAWN.to_string());
                (Some(request.clone()), None)
            } else {
                let service = state
                    .services
                    .values_mut()
                    .find(|service| {
                        service.request_id == request_id
                            && matches!(
                                service.spec,
                                Some(ServiceRequestKind::FileFetch { .. } | ServiceRequestKind::FileCopy { .. })
                            )
                            && matches!(service.status, ServiceStatus::Starting | ServiceStatus::Running)
                    })
                    .map(|service| service.service_id.clone());
                (None, service)
            }
        };
        if let Some(updated) = pending {
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
        } else if let Some(service_id) = service {
            let _ = self.control_service(&service_id, ServiceAction::Stop);
        }
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
            let message = if updated.action.is_some() {
                ClientToBeam::ServiceActionStatus {
                    request_id: updated.request_id.clone(),
                    status: updated.status,
                    message: updated.message.clone(),
                }
            } else {
                ClientToBeam::RequestStatus {
                    request_id: updated.request_id.clone(),
                    status: updated.status,
                    message: updated.message.clone(),
                }
            };
            let _ = commands.try_send(message);
        }
        let _ = self.save_state();
        self.broadcast(ServerEvent::RequestChanged { request: updated });
    }
}

impl Hub {
    fn agent_frontend_attached(&self, frontend: u64, beam_uuid: &str, sock: Option<String>) {
        self.agent
            .lock()
            .unwrap()
            .frontends
            .insert(frontend, (beam_uuid.to_string(), sock));
        self.refresh_agent_identities(beam_uuid);
    }

    fn agent_frontend_detached(&self, frontend: u64) {
        let beam = self.agent.lock().unwrap().frontends.remove(&frontend).map(|(beam, _)| beam);
        if let Some(beam) = beam {
            self.refresh_agent_identities(&beam);
        }
    }

    fn has_agent_frontend(&self, beam_uuid: &str) -> bool {
        self.agent
            .lock()
            .unwrap()
            .frontends
            .values()
            .any(|(beam, _)| beam == beam_uuid)
    }

    /// The agent socket of `preferred` if it's attached to `beam_uuid` and
    /// has one, else the most recently attached such frontend's.
    fn agent_socket_for(&self, beam_uuid: &str, preferred: Option<u64>) -> Option<String> {
        let agent = self.agent.lock().unwrap();
        if let Some((beam, Some(sock))) = preferred.and_then(|id| agent.frontends.get(&id)) {
            if beam == beam_uuid {
                return Some(sock.clone());
            }
        }
        agent
            .frontends
            .values()
            .rev()
            .find_map(|(beam, sock)| (beam == beam_uuid).then(|| sock.clone()).flatten())
    }

    /// Pushes the keys `beam_uuid` may list: the allowlisted keys of its
    /// newest attached frontend's agent, or none.
    fn refresh_agent_identities(&self, beam_uuid: &str) {
        if !self.agent.lock().unwrap().capable.contains(beam_uuid) {
            return;
        }
        let Some(commands) = self
            .bridges
            .lock()
            .unwrap()
            .get(beam_uuid)
            .map(|bridge| bridge.commands.clone())
        else {
            return;
        };
        let config = (self.agent_config)();
        let keys = match self.agent_socket_for(beam_uuid, None) {
            Some(sock) if config.enabled() => crate::agent::allowed_keys(&config, &sock).unwrap_or_else(|err| {
                eprintln!("ssh-agent: {beam_uuid}: listing local keys: {err:#}");
                Vec::new()
            }),
            _ => Vec::new(),
        };
        let _ = commands.try_send(ClientToBeam::AgentIdentities { keys });
    }

    fn send_agent_response(&self, beam_uuid: &str, beam_request_id: &str, body: &[u8], note: Option<String>) {
        let commands = self
            .bridges
            .lock()
            .unwrap()
            .get(beam_uuid)
            .map(|bridge| bridge.commands.clone());
        if let Some(commands) = commands {
            let _ = commands.try_send(ClientToBeam::AgentResponse {
                request_id: beam_request_id.to_string(),
                message: beams_protocol::agent::b64_encode(body),
                note,
            });
        }
    }

    fn deny_agent(&self, beam_uuid: &str, beam_request_id: &str, why: &str) {
        eprintln!("ssh-agent: {beam_uuid}: denied without asking: {why}");
        self.send_agent_response(beam_uuid, beam_request_id, &beams_protocol::agent::failure(), Some(why.to_string()));
    }

    /// A relayed sign request: refuse it outright, or queue it for the user.
    fn handle_agent_sign(
        self: &Arc<Self>,
        beam_uuid: &str,
        beam_request_id: String,
        conn_id: &str,
        message: &str,
        context: &beams_protocol::AgentContext,
    ) {
        let config = (self.agent_config)();
        if !config.enabled() {
            return self.deny_agent(beam_uuid, &beam_request_id, "the SSH agent proxy is disabled on the workstation");
        }
        if !self.has_agent_frontend(beam_uuid) {
            return self.deny_agent(
                beam_uuid,
                &beam_request_id,
                "no interactive `beams ssh` session is attached to approve it",
            );
        }
        let (binds, queued) = {
            let agent = self.agent.lock().unwrap();
            let queued = agent.pending.values().filter(|p| p.record.beam_uuid == beam_uuid).count();
            (agent.binds.binds(beam_uuid, conn_id), queued)
        };
        if queued >= crate::agent::MAX_PENDING_PER_BEAM {
            return self.deny_agent(beam_uuid, &beam_request_id, "too many SSH key requests are already waiting");
        }
        let local_keys = self
            .agent_socket_for(beam_uuid, None)
            .and_then(|sock| crate::agent::local_identities(&sock).ok())
            .unwrap_or_default();
        let timeout = config.prompt_timeout();
        let expires_at_ms = now_ms() + timeout.as_millis() as u64;
        let known_hosts = (self.agent_known_hosts)();
        let (body, prompt) = match crate::agent::evaluate(
            &config,
            message,
            &binds,
            context,
            &known_hosts,
            &local_keys,
            expires_at_ms,
        ) {
            Ok(evaluated) => evaluated,
            Err(why) => return self.deny_agent(beam_uuid, &beam_request_id, &why),
        };
        if let Some(grant) = self.take_grant_use(beam_uuid, &prompt) {
            return self.sign_with_grant(beam_uuid, beam_request_id, body, prompt, grant);
        }
        let fold_key = crate::agent::fold_key(&prompt, context);
        let record = RequestRecord {
            request_id: Uuid::new_v4().to_string(),
            beam_uuid: beam_uuid.to_string(),
            kind: "ssh_agent".to_string(),
            status: RequestStatus::PendingApproval,
            request: None,
            action: None,
            message: None,
            local_path: None,
            agent: Some(prompt),
        };
        eprintln!(
            "ssh-agent: {beam_uuid}: asking: {} (request {})",
            record.agent.as_ref().map(|p| p.summary()).unwrap_or_default(),
            record.request_id
        );
        let request_id = record.request_id.clone();
        self.agent.lock().unwrap().pending.insert(
            request_id.clone(),
            PendingAgent { beam_request_id, body, record: record.clone(), fold_key },
        );
        self.broadcast(ServerEvent::RequestChanged { request: record });
        let weak = Arc::downgrade(self);
        thread::spawn(move || {
            thread::sleep(timeout);
            if let Some(hub) = weak.upgrade() {
                hub.finish_agent_request(&request_id, RequestStatus::Expired, "approval timed out");
            }
        });
    }

    /// Sends `[startup]` to a Beam that hasn't had it yet.
    fn submit_startup(self: &Arc<Self>, beam_uuid: &str) {
        let first_time = self.state.lock().unwrap().startup_submitted.insert(beam_uuid.to_string());
        if !first_time {
            return;
        }
        let _ = self.save_state();
        let config = (self.startup_config)();
        if config.tasks.is_empty() {
            return;
        }
        let Some(commands) = self
            .bridges
            .lock()
            .unwrap()
            .get(beam_uuid)
            .map(|bridge| bridge.commands.clone())
        else {
            return;
        };
        let request_id = Uuid::new_v4().to_string();
        let budget = config.ssh_grant.as_ref().map(|g| g.max_uses_for(&config.tasks)).unwrap_or(0);
        self.agent.lock().unwrap().startup_requests.insert(
            request_id.clone(),
            (beam_uuid.to_string(), config.ssh_grant.clone().filter(|g| !g.hosts.is_empty()), budget),
        );
        let tasks = config.manifest_tasks();
        eprintln!("startup: {beam_uuid}: submitting {} task(s)", tasks.len());
        let _ = commands.try_send(ClientToBeam::RunManifest { request_id, tasks });
    }

    /// The Beam started our startup manifest: issue its grant, if any.
    fn startup_accepted(self: &Arc<Self>, beam_uuid: &str, request_id: &str, manifest_id: &str, tasks: usize) {
        let Some((owner, grant, max_uses)) = self.agent.lock().unwrap().startup_requests.remove(request_id) else {
            return; // not one of ours
        };
        if owner != beam_uuid {
            return;
        }
        eprintln!("startup: {beam_uuid}: {manifest_id} running {tasks} task(s)");
        let Some(grant) = grant else {
            return;
        };
        let window = grant.window();
        let record = GrantRecord {
            grant_id: Uuid::new_v4().to_string(),
            beam_uuid: beam_uuid.to_string(),
            manifest_id: manifest_id.to_string(),
            hosts: grant.hosts,
            user: grant.user,
            uses: 0,
            max_uses,
            expires_at_ms: now_ms() + window.as_millis() as u64,
            ended: None,
        };
        eprintln!("startup: {beam_uuid}: grant {} issued: {}", record.grant_id, record.summary());
        let grant_id = record.grant_id.clone();
        self.agent.lock().unwrap().grants.insert(grant_id.clone(), record.clone());
        self.broadcast(ServerEvent::GrantChanged { grant: record });
        let weak = Arc::downgrade(self);
        thread::spawn(move || {
            thread::sleep(window);
            if let Some(hub) = weak.upgrade() {
                hub.end_grants_where(|g| g.grant_id == grant_id, "its time window ended");
            }
        });
    }

    fn end_grants_for(&self, beam_uuid: &str, why: &str) {
        self.end_grants_where(|g| g.beam_uuid == beam_uuid, why);
    }

    fn end_grants_where(&self, matches: impl Fn(&GrantRecord) -> bool, why: &str) {
        let ended: Vec<GrantRecord> = {
            let mut agent = self.agent.lock().unwrap();
            let ids: Vec<String> = agent.grants.values().filter(|g| matches(g)).map(|g| g.grant_id.clone()).collect();
            ids.iter().filter_map(|id| agent.grants.remove(id)).collect()
        };
        for mut grant in ended {
            eprintln!("startup: {}: grant {} ended: {why}", grant.beam_uuid, grant.grant_id);
            grant.ended = Some(why.to_string());
            self.broadcast(ServerEvent::GrantChanged { grant });
        }
    }

    /// Spends one use of a grant covering `prompt`, if there is one.
    fn take_grant_use(&self, beam_uuid: &str, prompt: &crate::agent::AgentPrompt) -> Option<GrantRecord> {
        let now = now_ms();
        let mut agent = self.agent.lock().unwrap();
        let grant = agent
            .grants
            .values_mut()
            .find(|g| g.beam_uuid == beam_uuid && g.usable(now) && g.covers(prompt))?;
        grant.uses += 1;
        Some(grant.clone())
    }

    /// Signs without asking under `grant`, with a passive notice.
    fn sign_with_grant(
        self: &Arc<Self>,
        beam_uuid: &str,
        beam_request_id: String,
        body: Vec<u8>,
        mut prompt: crate::agent::AgentPrompt,
        grant: GrantRecord,
    ) {
        prompt.granted = Some(format!("startup grant, {} of {}", grant.uses, grant.max_uses));
        let mut record = RequestRecord {
            request_id: Uuid::new_v4().to_string(),
            beam_uuid: beam_uuid.to_string(),
            kind: "ssh_agent".to_string(),
            status: RequestStatus::Approved,
            request: None,
            action: None,
            message: Some("approved by the startup grant".to_string()),
            local_path: None,
            agent: Some(prompt),
        };
        let summary = record.agent.as_ref().map(|p| p.summary()).unwrap_or_default();
        eprintln!("ssh-agent: {beam_uuid}: granted ({}): {summary}", record.agent.as_ref().and_then(|p| p.granted.clone()).unwrap_or_default());
        self.broadcast(ServerEvent::GrantChanged { grant: grant.clone() });
        if grant.uses >= grant.max_uses {
            self.end_grants_where(|g| g.grant_id == grant.grant_id, "all its uses were spent");
        }
        let Some(sock) = self.agent_socket_for(beam_uuid, None) else {
            record.status = RequestStatus::Failed;
            record.message = Some("no attached terminal has an SSH agent".to_string());
            self.send_agent_response(beam_uuid, &beam_request_id, &beams_protocol::agent::failure(), record.message.clone());
            self.broadcast(ServerEvent::RequestChanged { request: record });
            return;
        };
        let hub = Arc::clone(self);
        let beam_uuid = beam_uuid.to_string();
        thread::spawn(move || {
            match crate::agent::local_sign(&sock, &body) {
                Ok(reply) => {
                    record.status = RequestStatus::Running;
                    record.message = Some("signed (startup grant)".to_string());
                    hub.send_agent_response(&beam_uuid, &beam_request_id, &reply, None);
                }
                Err(err) => {
                    record.status = RequestStatus::Failed;
                    record.message = Some(format!("{err:#}"));
                    hub.send_agent_response(&beam_uuid, &beam_request_id, &beams_protocol::agent::failure(), record.message.clone());
                }
            }
            hub.broadcast(ServerEvent::RequestChanged { request: record });
        });
    }

    fn is_agent_request(&self, request_id: &str) -> bool {
        self.agent.lock().unwrap().pending.contains_key(request_id)
    }

    /// Ends a still-pending request without signing.
    fn finish_agent_request(&self, request_id: &str, status: RequestStatus, why: &str) {
        let Some(mut pending) = self.agent.lock().unwrap().pending.remove(request_id) else {
            return;
        };
        pending.record.status = status;
        pending.record.message = Some(why.to_string());
        eprintln!("ssh-agent: {}: {why} (request {request_id})", pending.record.beam_uuid);
        self.send_agent_response(
            &pending.record.beam_uuid,
            &pending.beam_request_id,
            &beams_protocol::agent::failure(),
            Some(why.to_string()),
        );
        self.broadcast(ServerEvent::RequestChanged { request: pending.record });
    }

    fn expire_agent_requests_for(&self, beam_uuid: &str, why: &str) {
        let ids: Vec<String> = self
            .agent
            .lock()
            .unwrap()
            .pending
            .iter()
            .filter(|(_, p)| p.record.beam_uuid == beam_uuid)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            self.finish_agent_request(&id, RequestStatus::Expired, why);
        }
    }

    /// A local decision on an agent request. The first decision wins; the
    /// signature comes from the deciding frontend's own agent.
    /// Decides `request_id` and every pending request folded with it (see
    /// `crate::agent::fold_key`).
    fn decide_agent(self: &Arc<Self>, request_id: &str, approve: bool, frontend: u64) -> anyhow::Result<()> {
        let group: Vec<String> = {
            let agent = self.agent.lock().unwrap();
            let Some(chosen) = agent.pending.get(request_id) else {
                bail!("that SSH key request was already decided or has expired");
            };
            match &chosen.fold_key {
                Some(key) => agent
                    .pending
                    .iter()
                    .filter(|(_, other)| {
                        other.fold_key.as_ref() == Some(key) && other.record.beam_uuid == chosen.record.beam_uuid
                    })
                    .map(|(id, _)| id.clone())
                    .collect(),
                None => vec![request_id.to_string()],
            }
        };
        let mut result = Ok(());
        for id in group {
            let decided = self.decide_one_agent(&id, approve, frontend);
            if id == request_id {
                result = decided;
            }
        }
        result
    }

    fn decide_one_agent(self: &Arc<Self>, request_id: &str, approve: bool, frontend: u64) -> anyhow::Result<()> {
        if !approve {
            self.finish_agent_request(request_id, RequestStatus::Rejected, "rejected by local user");
            return Ok(());
        }
        let Some(mut pending) = self.agent.lock().unwrap().pending.remove(request_id) else {
            bail!("that SSH key request was already decided or has expired");
        };
        let beam_uuid = pending.record.beam_uuid.clone();
        let Some(sock) = self.agent_socket_for(&beam_uuid, Some(frontend)) else {
            let why = "this terminal has no SSH_AUTH_SOCK, so there is no agent to sign with";
            pending.record.status = RequestStatus::Failed;
            pending.record.message = Some(why.to_string());
            self.send_agent_response(&beam_uuid, &pending.beam_request_id, &beams_protocol::agent::failure(), Some(why.to_string()));
            self.broadcast(ServerEvent::RequestChanged { request: pending.record });
            bail!("{why}");
        };
        pending.record.status = RequestStatus::Approved;
        pending.record.message = Some("approved by local user".to_string());
        self.broadcast(ServerEvent::RequestChanged { request: pending.record.clone() });
        // The local agent may itself prompt (confirm, hardware touch), so
        // sign off this frontend's connection thread.
        let PendingAgent { beam_request_id, body, mut record, .. } = pending;
        let summary = record.agent.as_ref().map(|p| p.summary()).unwrap_or_default();
        let hub = Arc::clone(self);
        let request_id = request_id.to_string();
        thread::spawn(move || match crate::agent::local_sign(&sock, &body) {
            Ok(reply) => {
                eprintln!("ssh-agent: {beam_uuid}: signed: {summary} (request {request_id})");
                record.status = RequestStatus::Running;
                record.message = Some("signed".to_string());
                hub.send_agent_response(&beam_uuid, &beam_request_id, &reply, None);
                hub.broadcast(ServerEvent::RequestChanged { request: record });
            }
            Err(err) => {
                // Shown to the user through the request's message.
                let why = format!("{err:#}");
                eprintln!("ssh-agent: {beam_uuid}: signing failed: {why} (request {request_id})");
                record.status = RequestStatus::Failed;
                record.message = Some(why.clone());
                hub.send_agent_response(&beam_uuid, &beam_request_id, &beams_protocol::agent::failure(), Some(why));
                hub.broadcast(ServerEvent::RequestChanged { request: record });
            }
        });
        Ok(())
    }
}

fn validate_port_forward(
    remote_host: &str,
    remote_port: u16,
    local_port: Option<u16>,
) -> anyhow::Result<()> {
    if remote_host.is_empty() || remote_port == 0 {
        bail!("remote host must not be empty and remote port must be non-zero");
    }
    if local_port == Some(0) {
        bail!("an explicit local port must be non-zero; omit it to select one automatically");
    }
    Ok(())
}

fn normalize_local_path(path: &str) -> anyhow::Result<String> {
    if path.is_empty() {
        bail!("workstation path must not be empty");
    }
    let expanded = if let Some(rest) = path.strip_prefix("~/") {
        PathBuf::from(std::env::var("HOME").context("HOME is not set")?).join(rest)
    } else {
        PathBuf::from(path)
    };
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir()
            .context("reading workstation current directory")?
            .join(expanded)
    };
    Ok(absolute.to_string_lossy().into_owned())
}

fn service_projection(service: &ServiceRecord) -> Option<ServiceProjection> {
    let kind = service.spec.clone()?;
    Some(ServiceProjection {
        service_id: service.service_id.clone(),
        request_id: service.request_id.clone(),
        kind: kind.clone(),
        status: service.status,
        progress: service.progress,
        message: match kind {
            ServiceRequestKind::LocalPortForward { .. } => service.message.clone(),
            // Like plain copies, never echo workstation paths to the Beam.
            ServiceRequestKind::FileFetch { .. } => Some(
                match service.status {
                    ServiceStatus::Starting => "preparing fetch",
                    ServiceStatus::Running => "fetch in progress",
                    ServiceStatus::Stopping => "cancelling fetch",
                    ServiceStatus::Stopped => {
                        match service.message.as_deref() {
                            Some("cancelled locally") => "fetch cancelled on the workstation",
                            Some(WITHDRAWN) => "fetch withdrawn",
                            Some(message) if message.contains("interrupted") => "fetch interrupted",
                            _ => "fetch completed",
                        }
                    }
                    ServiceStatus::Error => {
                        if service
                            .message
                            .as_deref()
                            .is_some_and(|message| message.contains("already exists"))
                        {
                            "something with that name already exists at the destination (retry with --overwrite to replace it)"
                        } else {
                            "fetch failed; see the workstation's local menu for details"
                        }
                    }
                    ServiceStatus::Degraded => "fetch degraded",
                    ServiceStatus::Unknown => "fetch status unknown",
                    ServiceStatus::Paused => "fetch paused",
                }
                .to_string(),
            ),
            ServiceRequestKind::FileCopy { .. } => Some(
                match service.status {
                    ServiceStatus::Starting => "preparing file copy",
                    ServiceStatus::Running => "file copy in progress",
                    ServiceStatus::Stopping => "cancelling file copy",
                    ServiceStatus::Stopped => {
                        if service.message.as_deref() == Some("cancelled locally") {
                            "file copy cancelled"
                        } else if service
                            .message
                            .as_deref()
                            .is_some_and(|message| message.contains("interrupted"))
                        {
                            "file copy interrupted"
                        } else {
                            "file copy completed"
                        }
                    }
                    ServiceStatus::Error => {
                        "file copy failed; inspect the workstation client for details"
                    }
                    ServiceStatus::Degraded => "file copy degraded",
                    ServiceStatus::Unknown => "file copy status unknown",
                    ServiceStatus::Paused => "file copy paused",
                }
                .to_string(),
            ),
            ServiceRequestKind::FolderSync { .. } => Some(match service.status {
                ServiceStatus::Starting => "preparing folder sync".to_string(),
                ServiceStatus::Running => service
                    .message
                    .clone()
                    .unwrap_or_else(|| "folder sync is running".to_string()),
                ServiceStatus::Paused => "folder sync is paused".to_string(),
                ServiceStatus::Stopping => "terminating folder sync".to_string(),
                ServiceStatus::Stopped => "folder sync terminated".to_string(),
                ServiceStatus::Degraded => service
                    .message
                    .clone()
                    .unwrap_or_else(|| "folder sync requires attention".to_string()),
                ServiceStatus::Unknown => "folder sync status unknown".to_string(),
                ServiceStatus::Error => {
                    "folder sync failed; inspect the workstation client for details".to_string()
                }
            }),
        },
    })
}

fn handle_service_events(hub: Weak<Hub>, events: Receiver<crate::forward::Event>) {
    while let Ok(event) = events.recv() {
        let Some(hub) = hub.upgrade() else {
            return;
        };
        handle_service_update(
            &hub,
            event.service_id,
            event.kind,
            event.status,
            event.message,
        );
    }
}

fn handle_copy_events(hub: Weak<Hub>, events: Receiver<crate::copy::Event>) {
    while let Ok(event) = events.recv() {
        let Some(hub) = hub.upgrade() else {
            return;
        };
        if let Some(progress) = event.progress {
            // Progress ticks while running are published but not persisted
            // (they arrive several times a second).
            let tick = {
                let mut state = hub.state.lock().unwrap();
                state.services.get_mut(&event.service_id).and_then(|service| {
                    service.progress = Some(progress);
                    (service.status == ServiceStatus::Running && event.status == ServiceStatus::Running)
                        .then(|| {
                            service.message = event.message.clone();
                            service.clone()
                        })
                })
            };
            if let Some(service) = tick {
                hub.publish_service(&service);
                continue;
            }
        }
        handle_service_update(
            &hub,
            event.service_id,
            event.kind,
            event.status,
            event.message,
        );
    }
}

fn handle_sync_events(hub: Weak<Hub>, events: Receiver<crate::sync::Event>) {
    while let Ok(event) = events.recv() {
        let Some(hub) = hub.upgrade() else {
            return;
        };
        handle_service_update(
            &hub,
            event.service_id,
            event.kind,
            event.status,
            event.message,
        );
    }
}

fn handle_service_event(hub: &Arc<Hub>, event: crate::forward::Event) {
    handle_service_update(
        hub,
        event.service_id,
        event.kind,
        event.status,
        event.message,
    );
}

fn handle_service_update(
    hub: &Arc<Hub>,
    service_id: String,
    kind: ServiceRequestKind,
    status: ServiceStatus,
    message: Option<String>,
) {
    let (service, completed_actions, request_changed) = {
        let mut state = hub.state.lock().unwrap();
        let Some(service) = state.services.get_mut(&service_id) else {
            return;
        };
        if service.status != status {
            match service.status.transition(status) {
                Ok(next) => service.status = next,
                Err(_) if status == ServiceStatus::Starting => {
                    service.status = ServiceStatus::Starting;
                }
                Err(_) => return,
            }
        }
        service.spec = Some(kind);
        service.message = message;
        if status == ServiceStatus::Error
            && matches!(service.spec, Some(ServiceRequestKind::FolderSync { .. }))
        {
            service.desired_running = false;
        }
        let service = service.clone();
        let request_status_before = state
            .requests
            .get(&service.request_id)
            .map(|request| request.status);

        if matches!(
            service.status,
            ServiceStatus::Running | ServiceStatus::Paused
        ) {
            if let Some(request) = state.requests.get_mut(&service.request_id) {
                if request.status == RequestStatus::Starting {
                    request.status = RequestStatus::Running;
                    request.message = Some("workstation service is running".to_string());
                }
            }
        } else if service.status == ServiceStatus::Error {
            if let Some(request) = state.requests.get_mut(&service.request_id) {
                if matches!(
                    request.status,
                    RequestStatus::Approved | RequestStatus::Starting
                ) {
                    request.status = RequestStatus::Failed;
                    request.message = service.message.clone();
                }
            }
        }

        let completed_actions = state
            .requests
            .values_mut()
            .filter_map(|request| {
                let action = request.action.as_ref()?;
                if action.service_id != service.service_id
                    || request.status != RequestStatus::Starting
                {
                    return None;
                }
                let completed = matches!(
                    (action.action, service.status),
                    (ServiceAction::Stop, ServiceStatus::Stopped)
                        | (ServiceAction::Restart, ServiceStatus::Running)
                        | (ServiceAction::Pause, ServiceStatus::Paused)
                        | (ServiceAction::Resume, ServiceStatus::Running)
                        | (ServiceAction::Terminate, ServiceStatus::Stopped)
                );
                if completed {
                    request.status = RequestStatus::Running;
                    request.message = Some("service action completed".to_string());
                    Some(request.clone())
                } else if service.status == ServiceStatus::Error {
                    request.status = RequestStatus::Failed;
                    request.message = service.message.clone();
                    Some(request.clone())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        let request_changed = state
            .requests
            .get(&service.request_id)
            .map(|request| request.status)
            != request_status_before;
        (service, completed_actions, request_changed)
    };
    let _ = hub.save_state();
    hub.publish_service(&service);
    // Only on a real change: the Beam rejects a repeated status (e.g.
    // running -> running) as an invalid transition.
    let request = request_changed
        .then(|| {
            hub.state
                .lock()
                .unwrap()
                .requests
                .get(&service.request_id)
                .cloned()
        })
        .flatten();
    if let Some(request) = request {
        hub.publish_request(&request, false);
    }
    for request in completed_actions {
        hub.publish_request(&request, true);
    }
    if matches!(
        service.spec,
        Some(
            ServiceRequestKind::FileCopy { .. }
                | ServiceRequestKind::FileFetch { .. }
                | ServiceRequestKind::FolderSync { .. }
        )
    ) && matches!(
        service.status,
        ServiceStatus::Stopped | ServiceStatus::Error
    ) {
        hub.service_runtimes
            .lock()
            .unwrap()
            .remove(&service.service_id);
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
            capabilities,
        } => {
            {
                let mut agent = hub.agent.lock().unwrap();
                agent.binds.clear_beam(&beam_uuid);
                if capabilities.iter().any(|c| c == CAPABILITY_SSH_AGENT) {
                    agent.capable.insert(beam_uuid.clone());
                } else {
                    agent.capable.remove(&beam_uuid);
                }
            }
            // A reconnect means a new Beam-side daemon session; whatever
            // manifest a grant belonged to may be gone.
            hub.end_grants_for(&beam_uuid, "the Beam bridge reconnected");
            let (record, commands) = {
                let mut bridges = hub.bridges.lock().unwrap();
                let Some(bridge) = bridges.get_mut(&beam_uuid) else {
                    return;
                };
                bridge.record.status = "connected".to_string();
                bridge.record.bridge_id = Some(bridge_id);
                bridge.record.message = None;
                bridge.failing_since = None;
                bridge.last_existence_check = None;
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
            let action_requests = hub
                .state
                .lock()
                .unwrap()
                .requests
                .values()
                .filter(|request| request.beam_uuid == beam_uuid)
                .filter_map(|request| {
                    request
                        .action
                        .clone()
                        .map(|action_request| ActionRequestProjection {
                            request: action_request,
                            status: request.status,
                            message: request.message.clone(),
                        })
                })
                .collect();
            let services = hub
                .state
                .lock()
                .unwrap()
                .services
                .values()
                .filter(|service| service.beam_uuid == beam_uuid)
                .filter_map(service_projection)
                .collect();
            let _ = commands.send(ClientToBeam::RequestSnapshot { requests });
            let _ = commands.send(ClientToBeam::ActionRequestSnapshot {
                requests: action_requests,
            });
            let _ = commands.send(ClientToBeam::ServiceSnapshot { services });
            let _ = commands.send(ClientToBeam::Ready);
            hub.broadcast(ServerEvent::BridgeStatus { bridge: record });
            hub.refresh_agent_identities(&beam_uuid);
            if capabilities.iter().any(|c| c == CAPABILITY_STARTUP_MANIFEST) {
                hub.submit_startup(&beam_uuid);
            }
        }
        crate::bridge::Event::Disconnected { beam_uuid, message } => {
            // The Beam fails its waiting sign requests when the bridge goes.
            hub.expire_agent_requests_for(&beam_uuid, "the Beam bridge disconnected");
            hub.end_grants_for(&beam_uuid, "the Beam bridge disconnected");
            let (record, check_existence) = {
                let mut bridges = hub.bridges.lock().unwrap();
                let Some(bridge) = bridges.get_mut(&beam_uuid) else {
                    return;
                };
                bridge.record.status = "reconnecting".to_string();
                bridge.record.bridge_id = None;
                bridge.record.message = Some(message);
                let now = Instant::now();
                let since = *bridge.failing_since.get_or_insert(now);
                let check = now.duration_since(since) >= BRIDGE_GONE_CHECK_AFTER
                    && bridge
                        .last_existence_check
                        .is_none_or(|last| now.duration_since(last) >= BRIDGE_GONE_CHECK_INTERVAL);
                if check {
                    bridge.last_existence_check = Some(now);
                }
                (bridge.record.clone(), check)
            };
            hub.broadcast(ServerEvent::BridgeStatus { bridge: record });
            if check_existence {
                // `tsh beams ls` can be slow; keep it off the event thread.
                let weak = Arc::downgrade(hub);
                thread::spawn(move || {
                    if let Some(hub) = weak.upgrade() {
                        hub.retire_bridge_if_beam_gone(&beam_uuid);
                    }
                });
            }
        }
        crate::bridge::Event::Message { beam_uuid, message } => match message {
            BeamToClient::RequestSubmitted { request } => {
                let record = RequestRecord {
                    request_id: request.request_id.clone(),
                    beam_uuid: beam_uuid.clone(),
                    kind: request.kind.label().to_string(),
                    status: RequestStatus::PendingApproval,
                    request: Some(request),
                    action: None,
                    message: None,
                    local_path: None,
                    agent: None,
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
            BeamToClient::RequestWithdrawn { request_id } => {
                hub.withdraw_request(&beam_uuid, &request_id);
            }
            BeamToClient::ServiceActionRequested { request } => {
                let beam_matches = hub
                    .state
                    .lock()
                    .unwrap()
                    .services
                    .get(&request.service_id)
                    .is_some_and(|service| service.beam_uuid == beam_uuid);
                if !beam_matches {
                    return;
                }
                let record = RequestRecord {
                    request_id: request.request_id.clone(),
                    beam_uuid: beam_uuid.clone(),
                    kind: format!("{:?}", request.action).to_lowercase(),
                    status: RequestStatus::PendingApproval,
                    request: None,
                    action: Some(request),
                    message: None,
                    local_path: None,
                    agent: None,
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
            BeamToClient::AgentSessionBind { conn_id, message } => {
                if let Err(err) = hub.agent.lock().unwrap().binds.add(&beam_uuid, &conn_id, &message) {
                    eprintln!("ssh-agent: {beam_uuid} {conn_id}: session-bind ignored: {err}");
                }
            }
            BeamToClient::AgentConnClosed { conn_id } => {
                hub.agent.lock().unwrap().binds.close(&beam_uuid, &conn_id);
            }
            BeamToClient::AgentSignRequest {
                request_id,
                conn_id,
                message,
                context,
            } => hub.handle_agent_sign(&beam_uuid, request_id, &conn_id, &message, &context),
            BeamToClient::ManifestAccepted { request_id, manifest_id, task_ids } => {
                hub.startup_accepted(&beam_uuid, &request_id, &manifest_id, task_ids.len())
            }
            BeamToClient::ManifestRejected { request_id, message } => {
                hub.agent.lock().unwrap().startup_requests.remove(&request_id);
                eprintln!("startup: {beam_uuid}: manifest rejected: {message}");
            }
            BeamToClient::ManifestFinished { manifest_id, ok } => {
                eprintln!("startup: {beam_uuid}: {manifest_id} finished ({})", if ok { "ok" } else { "with errors" });
                hub.end_grants_where(|g| g.beam_uuid == beam_uuid && g.manifest_id == manifest_id, "startup finished");
            }
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
            ServerEvent::Error { message } => {
                return Err(ProtocolMismatch(format!("daemon rejected handshake: {message}")).into())
            }
            ServerEvent::Hello {
                protocol, version, ..
            } => {
                return Err(ProtocolMismatch(format!(
                    "daemon speaks {protocol} version {version}, not {PROTOCOL} version {PROTOCOL_VERSION}"
                ))
                .into())
            }
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
                ssh_auth_sock: std::env::var("SSH_AUTH_SOCK").ok().filter(|sock| !sock.is_empty()),
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
                local_path: None,
            },
        )
        .context("sending request decision")
    }

    pub fn decide_file_copy(
        &mut self,
        request_id: &str,
        approve: bool,
        local_path: Option<&str>,
    ) -> anyhow::Result<()> {
        write_json(
            &mut self.write_half,
            &ClientMessage::DecideRequest {
                request_id: request_id.to_string(),
                approve,
                local_path: local_path.map(str::to_string),
            },
        )
        .context("sending file-copy decision")
    }

    pub fn decide_folder_sync(
        &mut self,
        request_id: &str,
        approve: bool,
        local_path: Option<&str>,
    ) -> anyhow::Result<()> {
        write_json(
            &mut self.write_half,
            &ClientMessage::DecideRequest {
                request_id: request_id.to_string(),
                approve,
                local_path: local_path.map(str::to_string),
            },
        )
        .context("sending folder-sync decision")
    }

    pub fn create_port_forward(
        &mut self,
        target: &crate::bridge::Target,
        remote_host: &str,
        remote_port: u16,
        local_port: Option<u16>,
    ) -> anyhow::Result<()> {
        write_json(
            &mut self.write_half,
            &ClientMessage::CreatePortForward {
                beam_id: target.beam_id.clone(),
                beam_uuid: target.beam_uuid.clone(),
                login: target.login.clone(),
                remote_host: remote_host.to_string(),
                remote_port,
                local_port,
            },
        )
        .context("creating local port forward")
    }

    pub fn create_file_copy(
        &mut self,
        target: &crate::bridge::Target,
        direction: beams_protocol::FileCopyDirection,
        local_path: &str,
        remote_path: &str,
        overwrite: beams_protocol::OverwritePolicy,
    ) -> anyhow::Result<String> {
        let request_id = Uuid::new_v4().to_string();
        write_json(
            &mut self.write_half,
            &ClientMessage::CreateFileCopy {
                request_id: request_id.clone(),
                beam_id: target.beam_id.clone(),
                beam_uuid: target.beam_uuid.clone(),
                login: target.login.clone(),
                direction,
                local_path: local_path.to_string(),
                remote_path: remote_path.to_string(),
                overwrite,
            },
        )
        .context("creating file-copy job")?;
        Ok(request_id)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_folder_sync(
        &mut self,
        target: &crate::bridge::Target,
        local_path: &str,
        remote_path: &str,
        mode: beams_protocol::FolderSyncMode,
        ignore_vcs: bool,
        ignores: Vec<String>,
    ) -> anyhow::Result<String> {
        let request_id = Uuid::new_v4().to_string();
        write_json(
            &mut self.write_half,
            &ClientMessage::CreateFolderSync {
                request_id: request_id.clone(),
                beam_id: target.beam_id.clone(),
                beam_uuid: target.beam_uuid.clone(),
                login: target.login.clone(),
                local_path: local_path.to_string(),
                remote_path: remote_path.to_string(),
                mode,
                ignore_vcs,
                ignores,
            },
        )
        .context("creating folder sync")?;
        Ok(request_id)
    }

    pub fn refresh_agent_keys(&mut self) -> anyhow::Result<()> {
        write_json(&mut self.write_half, &ClientMessage::RefreshAgentKeys).context("refreshing shared SSH keys")
    }

    pub fn revoke_grant(&mut self, grant_id: &str) -> anyhow::Result<()> {
        write_json(&mut self.write_half, &ClientMessage::RevokeGrant { grant_id: grant_id.to_string() })
            .context("revoking SSH key grant")
    }

    pub fn control_service(
        &mut self,
        service_id: &str,
        action: ServiceAction,
    ) -> anyhow::Result<()> {
        write_json(
            &mut self.write_half,
            &ClientMessage::ControlService {
                service_id: service_id.to_string(),
                action,
            },
        )
        .context("controlling local service")
    }
}

/// Connects to the local daemon, starting it in the background if needed.
pub fn connect_or_start() -> anyhow::Result<Frontend> {
    let socket_path = default_socket_path()?;
    match Frontend::connect(&socket_path) {
        Ok(frontend) => return Ok(frontend),
        Err(err) if err.downcast_ref::<ProtocolMismatch>().is_some() => {
            // A daemon from a different build is listening. Replace it with
            // this binary's - unless this process is itself outdated, in
            // which case the daemon is likely the newer one.
            if crate::daemonctl::this_process_is_stale() {
                return Err(err).context(
                    "this beams binary was replaced since it started; start a new `beams ssh` to reconnect",
                );
            }
            match crate::daemonctl::stop(&socket_path, false)? {
                crate::daemonctl::StopOutcome::Refused(busy) => bail!(
                    "the local daemon is from a different beams build but busy ({}); \
                     run `beams daemon restart` once it's idle",
                    busy.join("; ")
                ),
                _ => eprintln!("beams: replaced a local daemon from a different build"),
            }
        }
        Err(_) => {}
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

pub fn spawn_daemon(socket_path: &Path) -> anyhow::Result<()> {
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
    let approvals_changed = state.expire_pending_after_restart();
    let services_changed = state.mark_process_services_stopped_after_restart();
    if approvals_changed || services_changed {
        state.save(&state_path)?;
    }
    let inherited_fd = std::env::var(LISTEN_FD_ENV)
        .ok()
        .and_then(|fd| fd.parse::<std::os::unix::io::RawFd>().ok());
    std::env::remove_var(LISTEN_FD_ENV);
    let restore_services = std::env::var_os(RESTORE_SERVICES_ENV).is_some();
    std::env::remove_var(RESTORE_SERVICES_ENV);
    let listener = match inherited_fd {
        // Safety: the fd was handed over by the previous daemon process,
        // which exec'd into this one with it open.
        Some(fd) => unsafe {
            use std::os::unix::io::FromRawFd;
            UnixListener::from_raw_fd(fd)
        },
        None => bind_socket(&socket_path)?,
    };
    listener
        .set_nonblocking(true)
        .context("making daemon listener nonblocking")?;
    let forward_program = std::env::var("BEAMS_TSH").unwrap_or_else(|_| "tsh".to_string());
    let mutagen_program = std::env::var("BEAMS_MUTAGEN").unwrap_or_else(|_| "mutagen".to_string());
    let mutagen_root = state_path
        .parent()
        .context("daemon state path has no parent")?
        .join("mutagen");
    let hub = Arc::new(Hub::with_programs(
        state,
        Some(state_path),
        forward_program,
        mutagen_program,
        mutagen_root,
    ));
    hub.recover_folder_syncs();
    if restore_services {
        eprintln!("beams daemon: restarted after an update; restoring forwards");
        hub.restore_forwards();
    }

    let terminating = Arc::clone(&hub.terminating);
    for signal in [SIGHUP, SIGINT, SIGQUIT, SIGTERM] {
        signal_hook::flag::register(signal, Arc::clone(&terminating))
            .context("installing daemon termination handler")?;
    }

    let mut update = UpdateWatch::default();
    let mut restart_into = None;
    while !terminating.load(Ordering::Relaxed) {
        if let Some(exe) = update.poll(&hub) {
            restart_into = Some(exe);
            break;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                let hub = Arc::clone(&hub);
                thread::spawn(move || {
                    if let Err(err) = handle_connection(&hub, stream) {
                        eprintln!("beams daemon: connection error: {err}");
                    }
                });
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(err) => return Err(err).context("accepting local daemon connection"),
        }
    }
    hub.shutdown();
    if let Some(exe) = restart_into {
        // Give bridge supervisors a moment to reap their SSH children.
        thread::sleep(Duration::from_millis(250));
        let err = exec_restart(&exe, &socket_path, &listener);
        eprintln!("beams daemon: could not restart into {}: {err:#}", exe.display());
    }
    drop(listener);
    let _ = fs::remove_file(&socket_path);
    Ok(())
}

/// Watches the daemon's own executable for replacement (a rebuild or
/// reinstall) and decides when to restart into the new one: once the new
/// file has stayed unchanged for a full check interval (so a half-written
/// binary is never exec'd) and the daemon is idle.
#[derive(Default)]
struct UpdateWatch {
    next_check: Option<Instant>,
    candidate: Option<crate::daemonctl::ExeIdentity>,
    reported_busy: bool,
}

impl UpdateWatch {
    fn poll(&mut self, hub: &Hub) -> Option<PathBuf> {
        let now = Instant::now();
        if self.next_check.is_some_and(|next| now < next) {
            return None;
        }
        self.next_check = Some(now + EXE_CHECK_INTERVAL);
        let exe = crate::daemonctl::startup_exe()?;
        let Some(current) = exe.replacement() else {
            self.candidate = None;
            return None;
        };
        if self.candidate.as_ref() != Some(&current) {
            eprintln!(
                "beams daemon: {} was replaced; restarting once it's stable and idle",
                exe.path.display()
            );
            self.candidate = Some(current);
            self.reported_busy = false;
            return None;
        }
        let busy = hub.busy_reasons();
        if !busy.is_empty() {
            if !self.reported_busy {
                eprintln!("beams daemon: update waiting: {}", busy.join("; "));
                self.reported_busy = true;
            }
            return None;
        }
        eprintln!("beams daemon: restarting into the updated binary");
        Some(exe.path.clone())
    }
}

/// Replaces this process with `exe daemon --socket <path>`, handing over the
/// listening socket. Only returns on failure.
fn exec_restart(exe: &Path, socket_path: &Path, listener: &UnixListener) -> anyhow::Error {
    use std::os::unix::io::AsRawFd;
    let fd = listener.as_raw_fd();
    // Rust opens sockets close-on-exec; clear it for this one fd only.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, 0) } != 0 {
        return anyhow::Error::from(std::io::Error::last_os_error())
            .context("clearing close-on-exec on the daemon socket");
    }
    let err = Command::new(exe)
        .arg("daemon")
        .arg("--socket")
        .arg(socket_path)
        .env(LISTEN_FD_ENV, fd.to_string())
        .env(RESTORE_SERVICES_ENV, "1")
        .exec();
    anyhow::Error::from(err).context("exec")
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
    // The listener is nonblocking so `serve` can notice termination signals.
    // Accepted-socket inheritance differs across Unix platforms; macOS can
    // leave this stream nonblocking, making the first idle post-handshake read
    // fail with WouldBlock. Each frontend has its own thread and must block.
    stream
        .set_nonblocking(false)
        .context("making frontend connection blocking")?;
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
        ClientMessage::Status => {
            write_json(
                &mut stream,
                &ServerEvent::DaemonInfo {
                    info: hub.daemon_info(),
                },
            )?;
            return Ok(());
        }
        ClientMessage::Shutdown { force } => {
            write_json(&mut stream, &hub.request_shutdown(force))?;
            return Ok(());
        }
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
    let frontend_id = hub.frontend_ids.fetch_add(1, Ordering::Relaxed);
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
                    ClientMessage::Status => {
                        let _ = tx.try_send(ServerEvent::DaemonInfo {
                            info: hub.daemon_info(),
                        });
                    }
                    ClientMessage::Shutdown { force } => {
                        let _ = tx.try_send(hub.request_shutdown(force));
                    }
                    ClientMessage::EnsureBridge {
                        beam_id,
                        beam_uuid,
                        login,
                        session_token,
                        ssh_auth_sock,
                    } => {
                        let target = crate::bridge::Target {
                            beam_id,
                            beam_uuid: beam_uuid.clone(),
                            login,
                        };
                        match hub.ensure_bridge(target, session_token) {
                            Ok(()) => hub.agent_frontend_attached(frontend_id, &beam_uuid, ssh_auth_sock),
                            Err(err) => {
                                let _ = tx.try_send(ServerEvent::Error {
                                    message: format!("starting Beam bridge: {err:#}"),
                                });
                            }
                        }
                    }
                    ClientMessage::DecideRequest {
                        request_id,
                        approve,
                        local_path,
                    } => {
                        let result = if hub.is_agent_request(&request_id) {
                            hub.decide_agent(&request_id, approve, frontend_id)
                        } else {
                            hub.decide_request(&request_id, approve, local_path).map(|_| ())
                        };
                        if let Err(err) = result {
                            let _ = tx.try_send(ServerEvent::Error {
                                message: format!("deciding request: {err:#}"),
                            });
                        }
                    }
                    ClientMessage::CreateFileCopy {
                        request_id,
                        beam_id,
                        beam_uuid,
                        login,
                        direction,
                        local_path,
                        remote_path,
                        overwrite,
                    } => {
                        let target = crate::bridge::Target {
                            beam_id,
                            beam_uuid,
                            login,
                        };
                        if let Err(err) = hub.create_file_copy(
                            request_id,
                            target,
                            direction,
                            local_path,
                            remote_path,
                            overwrite,
                        ) {
                            let _ = tx.try_send(ServerEvent::Error {
                                message: format!("creating file-copy job: {err:#}"),
                            });
                        }
                    }
                    ClientMessage::CreateFolderSync {
                        request_id,
                        beam_id,
                        beam_uuid,
                        login,
                        local_path,
                        remote_path,
                        mode,
                        ignore_vcs,
                        ignores,
                    } => {
                        let target = crate::bridge::Target {
                            beam_id,
                            beam_uuid,
                            login,
                        };
                        if let Err(err) = hub.create_folder_sync(
                            request_id,
                            target,
                            local_path,
                            remote_path,
                            mode,
                            ignore_vcs,
                            ignores,
                        ) {
                            let _ = tx.try_send(ServerEvent::Error {
                                message: format!("creating folder sync: {err:#}"),
                            });
                        }
                    }
                    ClientMessage::CreatePortForward {
                        beam_id,
                        beam_uuid,
                        login,
                        remote_host,
                        remote_port,
                        local_port,
                    } => {
                        let target = crate::bridge::Target {
                            beam_id,
                            beam_uuid,
                            login,
                        };
                        if let Err(err) =
                            hub.create_port_forward(target, remote_host, remote_port, local_port)
                        {
                            let _ = tx.try_send(ServerEvent::Error {
                                message: format!("creating port forward: {err:#}"),
                            });
                        }
                    }
                    ClientMessage::ControlService { service_id, action } => {
                        if let Err(err) = hub.control_service(&service_id, action) {
                            let _ = tx.try_send(ServerEvent::Error {
                                message: format!("controlling service: {err:#}"),
                            });
                        }
                    }
                    ClientMessage::RevokeGrant { grant_id } => {
                        hub.end_grants_where(|g| g.grant_id == grant_id, "revoked by local user");
                    }
                    ClientMessage::RefreshAgentKeys => {
                        let beams: Vec<String> = hub.agent.lock().unwrap().capable.iter().cloned().collect();
                        for beam in beams {
                            hub.refresh_agent_identities(&beam);
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
                    message: format!("{err:#}"),
                });
                break;
            }
        }
    }
    hub.agent_frontend_detached(frontend_id);
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

pub(crate) fn config_dir() -> anyhow::Result<PathBuf> {
    match std::env::var("XDG_CONFIG_HOME") {
        Ok(dir) if !dir.is_empty() => Ok(PathBuf::from(dir)),
        _ => Ok(PathBuf::from(std::env::var("HOME").context("HOME is not set")?).join(".config")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::os::unix::fs::PermissionsExt;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("{name}-{}-{}", std::process::id(), Uuid::new_v4()))
    }

    fn hub_with_bridge(beam_uuid: &str) -> (Arc<Hub>, Receiver<ClientToBeam>) {
        hub_with_bridge_and_program(beam_uuid, "tsh".to_string())
    }

    fn hub_with_bridge_and_program(
        beam_uuid: &str,
        program: String,
    ) -> (Arc<Hub>, Receiver<ClientToBeam>) {
        let hub = Arc::new(Hub::with_state_path_and_program(
            PersistedState::default(),
            None,
            program,
        ));
        let (commands, command_rx) = mpsc::sync_channel(16);
        hub.bridges.lock().unwrap().insert(
            beam_uuid.to_string(),
            BridgeRuntime::new(
                BridgeRecord {
                    beam_id: "beam-name".to_string(),
                    beam_uuid: beam_uuid.to_string(),
                    login: "beams".to_string(),
                    status: "connected".to_string(),
                    session_count: 1,
                    bridge_id: Some("bridge-id".to_string()),
                    message: None,
                },
                commands,
            ),
        );
        (hub, command_rx)
    }

    fn fake_forward_program() -> PathBuf {
        let path = temp_path("beams-clientd-fake-forward");
        fs::write(
            &path,
            "#!/bin/sh\ntrap 'exit 0' TERM INT\nwhile :; do sleep 1; done\n",
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    fn fake_copy_program() -> PathBuf {
        let path = temp_path("beams-clientd-fake-copy");
        fs::write(
            &path,
            "#!/bin/sh\n\
             if [ \"$1\" = scp ]; then\n\
               for arg do destination=$arg; done\n\
               printf daemon-copy > \"$destination\"\n\
             fi\n",
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + timeout;
        while !condition() {
            assert!(Instant::now() < deadline, "condition did not become true");
            thread::sleep(Duration::from_millis(20));
        }
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
                action: None,
                message: None,
                local_path: None,
                agent: None,
            },
        );
        assert!(state.expire_pending_after_restart());
        assert_eq!(state.requests["pending"].status, RequestStatus::Expired);
        assert!(!state.expire_pending_after_restart());
    }

    #[test]
    fn daemon_restart_marks_file_copy_interrupted_without_restoring_it() {
        let mut state = PersistedState::default();
        state.services.insert(
            "copy".to_string(),
            ServiceRecord {
                service_id: "copy".to_string(),
                request_id: "request".to_string(),
                beam_id: "beam".to_string(),
                beam_uuid: "beam-uuid".to_string(),
                login: "beams".to_string(),
                kind: "file_copy".to_string(),
                spec: Some(ServiceRequestKind::FileCopy {
                    direction: beams_protocol::FileCopyDirection::Download,
                    remote_path: "/tmp/source".to_string(),
                    local_path_hint: Some("destination".to_string()),
                    overwrite: beams_protocol::OverwritePolicy::Deny,
                }),
                status: ServiceStatus::Running,
                desired_running: true,
                message: None,
                local_path: Some("/tmp/destination".to_string()),
                mutagen_session: None,
                progress: None,
            },
        );
        assert!(state.mark_process_services_stopped_after_restart());
        let copy = &state.services["copy"];
        assert_eq!(copy.status, ServiceStatus::Stopped);
        assert!(!copy.desired_running);
        assert!(copy.message.as_deref().unwrap().contains("interrupted"));
    }

    #[test]
    fn daemon_restart_preserves_folder_sync_for_name_based_adoption() {
        let mut state = PersistedState::default();
        state.services.insert(
            "sync".to_string(),
            ServiceRecord {
                service_id: "sync".to_string(),
                request_id: "request".to_string(),
                beam_id: "beam".to_string(),
                beam_uuid: "beam-uuid".to_string(),
                login: "beams".to_string(),
                kind: "folder_sync".to_string(),
                spec: Some(ServiceRequestKind::FolderSync {
                    remote_path: "/home/beams/project".to_string(),
                    local_path_hint: Some("project".to_string()),
                    mode: beams_protocol::FolderSyncMode::TwoWaySafe,
                    ignore_vcs: true,
                    ignores: Vec::new(),
                }),
                status: ServiceStatus::Paused,
                desired_running: true,
                message: None,
                local_path: Some("/tmp/project".to_string()),
                mutagen_session: Some("beams-beam-uuid-sync".to_string()),
                progress: None,
            },
        );
        assert!(state.mark_process_services_stopped_after_restart());
        let sync = &state.services["sync"];
        assert_eq!(sync.status, ServiceStatus::Unknown);
        assert!(sync.desired_running);
        assert_eq!(
            sync.mutagen_session.as_deref(),
            Some("beams-beam-uuid-sync")
        );
    }

    #[test]
    fn folder_sync_projection_redacts_workstation_paths_and_errors() {
        let local_path = "/Users/example/secret-project";
        let service = ServiceRecord {
            service_id: "sync".to_string(),
            request_id: "request".to_string(),
            beam_id: "beam".to_string(),
            beam_uuid: "beam-uuid".to_string(),
            login: "beams".to_string(),
            kind: "folder_sync".to_string(),
            spec: Some(ServiceRequestKind::FolderSync {
                remote_path: "/home/beams/project".to_string(),
                local_path_hint: Some("secret-project".to_string()),
                mode: beams_protocol::FolderSyncMode::TwoWaySafe,
                ignore_vcs: true,
                ignores: Vec::new(),
            }),
            status: ServiceStatus::Error,
            desired_running: false,
            message: Some(format!("failed while opening {local_path}")),
            local_path: Some(local_path.to_string()),
            mutagen_session: Some("session".to_string()),
            progress: None,
        };
        let projection = service_projection(&service).unwrap();
        let encoded = serde_json::to_string(&projection).unwrap();
        assert!(!encoded.contains(local_path));
        assert!(projection
            .message
            .as_deref()
            .unwrap()
            .contains("inspect the workstation"));
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
        // Models platforms where accept inherits the daemon listener's
        // nonblocking mode.
        server.set_nonblocking(true).unwrap();
        let state = PersistedState::default();
        let expected_client_id = state.client_id.clone();
        let hub = Arc::new(Hub::new(state));
        let server_hub = Arc::clone(&hub);
        let server_thread = thread::spawn(move || handle_connection(&server_hub, server).unwrap());

        thread::sleep(Duration::from_millis(20));
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

        let decided = hub.decide_request("request-1", false, None).unwrap();
        assert_eq!(decided.status, RequestStatus::Rejected);
        assert!(matches!(
            commands.recv_timeout(Duration::from_secs(1)).unwrap(),
            ClientToBeam::RequestStatus {
                request_id,
                status: RequestStatus::Rejected,
                ..
            } if request_id == "request-1"
        ));
        assert!(hub.decide_request("request-1", true, None).is_err());
    }

    #[test]
    fn concurrent_frontends_racing_one_decision_have_exactly_one_winner() {
        let (hub, _commands) = hub_with_bridge("beam-uuid");
        handle_bridge_event(
            &hub,
            crate::bridge::Event::Message {
                beam_uuid: "beam-uuid".to_string(),
                message: BeamToClient::RequestSubmitted {
                    request: ServiceRequest {
                        request_id: "raced-request".to_string(),
                        kind: ServiceRequestKind::LocalPortForward {
                            remote_host: "127.0.0.1".to_string(),
                            remote_port: 8080,
                            local_port: Some(18080),
                        },
                    },
                },
            },
        );
        let first = Arc::clone(&hub);
        let second = Arc::clone(&hub);
        let a = thread::spawn(move || first.decide_request("raced-request", false, None).is_ok());
        let b = thread::spawn(move || second.decide_request("raced-request", false, None).is_ok());
        let winners = usize::from(a.join().unwrap()) + usize::from(b.join().unwrap());
        assert_eq!(winners, 1);
        assert_eq!(
            hub.state.lock().unwrap().requests["raced-request"].status,
            RequestStatus::Rejected
        );
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
                action: None,
                message: Some("rejected by local user".to_string()),
                local_path: None,
                agent: None,
            },
        );

        handle_bridge_event(
            &hub,
            crate::bridge::Event::Connected {
                beam_uuid: "beam-uuid".to_string(),
                bridge_id: "new-bridge-id".to_string(),
                capabilities: Vec::new(),
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
            ClientToBeam::ActionRequestSnapshot { requests } if requests.is_empty()
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
                action: None,
                message: None,
                local_path: None,
                agent: None,
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

    #[test]
    fn remote_file_copy_requires_a_local_path_and_keeps_it_off_the_beam() {
        let program = fake_copy_program();
        let dir = temp_path("beams-clientd-copy-destination");
        fs::create_dir_all(&dir).unwrap();
        let destination = dir.join("actual-local-destination");
        let (hub, _commands) =
            hub_with_bridge_and_program("beam-uuid", program.display().to_string());
        handle_bridge_event(
            &hub,
            crate::bridge::Event::Message {
                beam_uuid: "beam-uuid".to_string(),
                message: BeamToClient::RequestSubmitted {
                    request: ServiceRequest {
                        request_id: "request-copy".to_string(),
                        kind: ServiceRequestKind::FileCopy {
                            direction: beams_protocol::FileCopyDirection::Download,
                            remote_path: "/tmp/source".to_string(),
                            local_path_hint: Some("suggested-name".to_string()),
                            overwrite: beams_protocol::OverwritePolicy::Deny,
                        },
                    },
                },
            },
        );

        assert!(hub.decide_request("request-copy", true, None).is_err());
        assert_eq!(
            hub.state.lock().unwrap().requests["request-copy"].status,
            RequestStatus::PendingApproval
        );
        hub.decide_request(
            "request-copy",
            true,
            Some(destination.display().to_string()),
        )
        .unwrap();
        wait_until(Duration::from_secs(2), || {
            hub.state
                .lock()
                .unwrap()
                .services
                .values()
                .any(|service| service.status == ServiceStatus::Stopped)
        });

        let service = hub
            .state
            .lock()
            .unwrap()
            .services
            .values()
            .next()
            .cloned()
            .unwrap();
        assert_eq!(fs::read_to_string(&destination).unwrap(), "daemon-copy");
        assert_eq!(
            service.local_path.as_deref(),
            Some(destination.to_string_lossy().as_ref())
        );
        let projection = serde_json::to_string(&service_projection(&service).unwrap()).unwrap();
        assert!(!projection.contains(&destination.to_string_lossy().to_string()));
        assert!(projection.contains("suggested-name"));
        assert!(!hub
            .service_runtimes
            .lock()
            .unwrap()
            .contains_key(&service.service_id));
        assert!(hub
            .control_service(&service.service_id, ServiceAction::Restart)
            .is_err());

        drop(hub);
        let _ = fs::remove_dir_all(dir);
        let _ = fs::remove_file(program);
    }

    #[test]
    fn approving_remote_forward_runs_and_local_stop_restart_controls_it() {
        let program = fake_forward_program();
        let (hub, _commands) =
            hub_with_bridge_and_program("beam-uuid", program.display().to_string());
        handle_bridge_event(
            &hub,
            crate::bridge::Event::Message {
                beam_uuid: "beam-uuid".to_string(),
                message: BeamToClient::RequestSubmitted {
                    request: ServiceRequest {
                        request_id: "request-forward".to_string(),
                        kind: ServiceRequestKind::LocalPortForward {
                            remote_host: "127.0.0.1".to_string(),
                            remote_port: 8080,
                            local_port: None,
                        },
                    },
                },
            },
        );

        hub.decide_request("request-forward", true, None).unwrap();
        wait_until(Duration::from_secs(3), || {
            hub.state.lock().unwrap().requests["request-forward"].status == RequestStatus::Running
        });
        let service = hub
            .state
            .lock()
            .unwrap()
            .services
            .values()
            .next()
            .cloned()
            .unwrap();
        assert_eq!(service.status, ServiceStatus::Running);
        assert!(matches!(
            service.spec,
            Some(ServiceRequestKind::LocalPortForward {
                local_port: Some(port),
                ..
            }) if port > 0
        ));

        hub.control_service(&service.service_id, ServiceAction::Stop)
            .unwrap();
        wait_until(Duration::from_secs(2), || {
            hub.state.lock().unwrap().services[&service.service_id].status == ServiceStatus::Stopped
        });
        hub.control_service(&service.service_id, ServiceAction::Restart)
            .unwrap();
        wait_until(Duration::from_secs(3), || {
            hub.state.lock().unwrap().services[&service.service_id].status == ServiceStatus::Running
        });

        drop(hub);
        let _ = fs::remove_file(program);
    }

    fn failing_bridge(beam_uuid: &str, existing: Vec<&'static str>) -> (Arc<Hub>, Receiver<ClientToBeam>) {
        let (mut hub, command_rx) = hub_with_bridge(beam_uuid);
        let existing: Vec<String> = existing.into_iter().map(str::to_string).collect();
        Arc::get_mut(&mut hub).unwrap().beam_lister = Box::new(move || Ok(existing.clone()));
        let failed_at = Instant::now() - BRIDGE_GONE_CHECK_AFTER - Duration::from_secs(1);
        hub.bridges.lock().unwrap().get_mut(beam_uuid).unwrap().failing_since = Some(failed_at);
        (hub, command_rx)
    }

    #[test]
    fn bridges_to_deleted_beams_are_retired_after_failing_for_a_while() {
        let (hub, command_rx) = failing_bridge("gone-uuid", vec!["other-uuid"]);
        handle_bridge_event(
            &hub,
            crate::bridge::Event::Disconnected {
                beam_uuid: "gone-uuid".to_string(),
                message: "bridge exited with exit status: 1".to_string(),
            },
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        while hub.bridges.lock().unwrap().contains_key("gone-uuid") && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!hub.bridges.lock().unwrap().contains_key("gone-uuid"));
        // Dropping the command sender is what stops the bridge supervisor.
        assert!(matches!(
            command_rx.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ));
    }

    #[test]
    fn bridges_are_kept_when_the_beam_exists_or_existence_is_unknown() {
        let (hub, _rx) = failing_bridge("live-uuid", vec!["live-uuid"]);
        hub.retire_bridge_if_beam_gone("live-uuid");
        assert!(hub.bridges.lock().unwrap().contains_key("live-uuid"));

        let (mut hub, _rx) = failing_bridge("unknown-uuid", vec![]);
        Arc::get_mut(&mut hub).unwrap().beam_lister = Box::new(|| anyhow::bail!("not logged in"));
        hub.retire_bridge_if_beam_gone("unknown-uuid");
        assert!(hub.bridges.lock().unwrap().contains_key("unknown-uuid"));

        // A bridge that reconnected while `tsh beams ls` ran is kept too.
        let (hub, _rx) = failing_bridge("back-uuid", vec![]);
        hub.bridges.lock().unwrap().get_mut("back-uuid").unwrap().failing_since = None;
        hub.retire_bridge_if_beam_gone("back-uuid");
        assert!(hub.bridges.lock().unwrap().contains_key("back-uuid"));
    }

    #[test]
    fn recently_failed_bridges_are_not_checked_yet() {
        let (hub, _rx) = failing_bridge("new-uuid", vec![]);
        hub.bridges.lock().unwrap().get_mut("new-uuid").unwrap().failing_since = None;
        handle_bridge_event(
            &hub,
            crate::bridge::Event::Disconnected {
                beam_uuid: "new-uuid".to_string(),
                message: "bridge exited".to_string(),
            },
        );
        thread::sleep(Duration::from_millis(100));
        let bridges = hub.bridges.lock().unwrap();
        let bridge = bridges.get("new-uuid").expect("not retired");
        assert!(bridge.failing_since.is_some());
        assert!(bridge.last_existence_check.is_none());
    }

    fn control_op(hub: &Arc<Hub>, request: &str) -> serde_json::Value {
        let (mut client, server) = UnixStream::pair().unwrap();
        let hub = Arc::clone(hub);
        let handler = thread::spawn(move || handle_connection(&hub, server));
        client.write_all(request.as_bytes()).unwrap();
        client.write_all(b"\n").unwrap();
        let mut reply = String::new();
        BufReader::new(client).read_line(&mut reply).unwrap();
        handler.join().unwrap().unwrap();
        serde_json::from_str(&reply).unwrap()
    }

    fn submit_forward_request(hub: &Arc<Hub>, beam_uuid: &str, request_id: &str) {
        handle_bridge_event(
            hub,
            crate::bridge::Event::Message {
                beam_uuid: beam_uuid.to_string(),
                message: BeamToClient::RequestSubmitted {
                    request: ServiceRequest {
                        request_id: request_id.to_string(),
                        kind: ServiceRequestKind::LocalPortForward {
                            remote_host: "127.0.0.1".to_string(),
                            remote_port: 8080,
                            local_port: None,
                        },
                    },
                },
            },
        );
    }

    #[test]
    fn control_ops_work_without_a_hello_and_shutdown_refuses_when_busy() {
        let (hub, _commands) = hub_with_bridge("beam-uuid");
        let status = control_op(&hub, r#"{"op":"status"}"#);
        assert_eq!(status["type"], "daemon_info");
        assert_eq!(status["pid"], std::process::id());
        assert_eq!(status["bridges"], 1);
        assert_eq!(status["busy"], serde_json::json!([]));

        submit_forward_request(&hub, "beam-uuid", "pending-request");
        let status = control_op(&hub, r#"{"op":"status"}"#);
        assert_eq!(status["busy"], serde_json::json!(["1 request(s) awaiting approval"]));

        let refused = control_op(&hub, r#"{"op":"shutdown"}"#);
        assert_eq!(refused["type"], "shutdown_refused");
        assert!(!hub.terminating.load(Ordering::SeqCst));

        let accepted = control_op(&hub, r#"{"op":"shutdown","force":true}"#);
        assert_eq!(accepted["type"], "shutting_down");
        assert!(hub.terminating.load(Ordering::SeqCst));
    }

    #[test]
    fn a_self_restarted_daemon_restores_running_forwards_without_a_session() {
        let program = fake_forward_program();
        let (hub, _commands) =
            hub_with_bridge_and_program("beam-uuid", program.display().to_string());
        submit_forward_request(&hub, "beam-uuid", "request-forward");
        hub.decide_request("request-forward", true, None).unwrap();
        wait_until(Duration::from_secs(3), || {
            hub.state.lock().unwrap().services.values().any(|service| service.status == ServiceStatus::Running)
        });
        hub.shutdown();

        // What the restarted process sees: the persisted state, with
        // process-backed services marked stopped, and no bridges at all.
        let mut state = hub.state.lock().unwrap().clone();
        state.mark_process_services_stopped_after_restart();
        let restarted = Arc::new(Hub::with_state_path_and_program(
            state,
            None,
            program.display().to_string(),
        ));
        restarted.restore_forwards();
        wait_until(Duration::from_secs(3), || {
            restarted
                .state
                .lock()
                .unwrap()
                .services
                .values()
                .any(|service| service.status == ServiceStatus::Running)
        });
        restarted.shutdown();
        let _ = fs::remove_file(program);
    }

    /// A stand-in for the workstation's ssh-agent: lists `key_blob`, signs
    /// anything with a fixed signature.
    fn fake_local_agent(key_blob: Vec<u8>) -> String {
        use beams_protocol::agent as wire;
        let path = temp_path("beams-fake-agent");
        let listener = UnixListener::bind(&path).unwrap();
        thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { return };
                let key_blob = key_blob.clone();
                thread::spawn(move || {
                    while let Ok(Some(body)) = wire::read_message(&mut conn) {
                        let reply = match body[0] {
                            wire::msg::REQUEST_IDENTITIES => wire::encode_identities_answer(&[wire::Identity {
                                key_blob: key_blob.clone(),
                                comment: "test key".into(),
                            }]),
                            wire::msg::SIGN_REQUEST => {
                                let mut reply = vec![wire::msg::SIGN_RESPONSE];
                                reply.extend_from_slice(&3u32.to_be_bytes());
                                reply.extend_from_slice(b"sig");
                                reply
                            }
                            _ => wire::failure(),
                        };
                        if wire::write_message(&mut conn, &reply).is_err() {
                            return;
                        }
                    }
                });
            }
        });
        path.display().to_string()
    }

    fn agent_hub(allowed: &str) -> (Arc<Hub>, Receiver<ClientToBeam>) {
        let mut hub = Hub::with_state_path_and_program(PersistedState::default(), None, "tsh".to_string());
        let allowed = allowed.to_string();
        hub.agent_config = Box::new(move || crate::agent::Config {
            allowed_keys: vec![allowed.clone()],
            prompt_timeout: Some(30),
        });
        let hub = Arc::new(hub);
        let (commands, command_rx) = mpsc::sync_channel(64);
        hub.bridges.lock().unwrap().insert(
            "beam-uuid".to_string(),
            BridgeRuntime::new(
                BridgeRecord {
                    beam_id: "beam-name".to_string(),
                    beam_uuid: "beam-uuid".to_string(),
                    login: "beams".to_string(),
                    status: "connecting".to_string(),
                    session_count: 1,
                    bridge_id: None,
                    message: None,
                },
                commands,
            ),
        );
        (hub, command_rx)
    }

    /// The next agent message sent to the Beam, skipping snapshots etc.
    fn next_agent_message(commands: &Receiver<ClientToBeam>) -> ClientToBeam {
        loop {
            let message = commands.recv_timeout(Duration::from_secs(5)).expect("an agent message");
            if matches!(message, ClientToBeam::AgentIdentities { .. } | ClientToBeam::AgentResponse { .. }) {
                return message;
            }
        }
    }

    fn agent_sign(hub: &Arc<Hub>, beam_request_id: &str, user_key: &[u8]) {
        use beams_protocol::agent as wire;
        let data = wire::encode_userauth_data(&wire::UserAuthRequest {
            session_id: vec![4; 32],
            user: "git".into(),
            service: "ssh-connection".into(),
            method: wire::METHOD_PUBLICKEY.into(),
            algorithm: "ssh-ed25519".into(),
            key_blob: user_key.to_vec(),
            host_key: None,
        });
        handle_bridge_event(
            hub,
            crate::bridge::Event::Message {
                beam_uuid: "beam-uuid".to_string(),
                message: BeamToClient::AgentSignRequest {
                    request_id: beam_request_id.to_string(),
                    conn_id: "agent-1".to_string(),
                    message: wire::b64_encode(&wire::encode_sign_request(user_key, &data, 0)),
                    context: beams_protocol::AgentContext {
                        reason: Some("clone for review".into()),
                        ..Default::default()
                    },
                },
            },
        );
    }

    fn agent_sign_from(hub: &Arc<Hub>, beam_request_id: &str, user_key: &[u8], parent_pid: u32) {
        use beams_protocol::agent as wire;
        let data = wire::encode_userauth_data(&wire::UserAuthRequest {
            session_id: vec![4; 32],
            user: "git".into(),
            service: "ssh-connection".into(),
            method: wire::METHOD_PUBLICKEY.into(),
            algorithm: "ssh-ed25519".into(),
            key_blob: user_key.to_vec(),
            host_key: None,
        });
        let process = |pid: u32, argv: &[&str]| beams_protocol::AgentProcess {
            pid,
            argv: argv.iter().map(|a| a.to_string()).collect(),
            cwd: None,
        };
        handle_bridge_event(
            hub,
            crate::bridge::Event::Message {
                beam_uuid: "beam-uuid".to_string(),
                message: BeamToClient::AgentSignRequest {
                    request_id: beam_request_id.to_string(),
                    conn_id: format!("agent-{beam_request_id}"),
                    message: wire::b64_encode(&wire::encode_sign_request(user_key, &data, 0)),
                    context: beams_protocol::AgentContext {
                        chain: vec![
                            process(parent_pid + 100, &["ssh", "git@github.com"]),
                            process(parent_pid + 50, &["/bin/sh", "-c", "ssh ..."]),
                            process(parent_pid, &["git", "fetch", "--recurse-submodules"]),
                        ],
                        ..Default::default()
                    },
                },
            },
        );
    }

    #[test]
    fn requests_from_one_parent_process_are_decided_together() {
        use beams_protocol::agent as wire;
        use ssh_key::{rand_core::OsRng, Algorithm, PrivateKey};
        let key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap();
        let blob = key.public_key().to_bytes().unwrap();
        let (hub, commands) = agent_hub(&beams_protocol::agent_crypto::fingerprint(&blob).unwrap());
        let sock = fake_local_agent(blob.clone());
        handle_bridge_event(
            &hub,
            crate::bridge::Event::Connected {
                beam_uuid: "beam-uuid".to_string(),
                bridge_id: "bridge-1".to_string(),
                capabilities: vec![CAPABILITY_SSH_AGENT.to_string()],
            },
        );
        hub.agent_frontend_attached(7, "beam-uuid", Some(sock.clone()));
        while !matches!(next_agent_message(&commands), ClientToBeam::AgentIdentities { ref keys } if !keys.is_empty()) {}

        agent_sign_from(&hub, "a", &blob, 1000);
        agent_sign_from(&hub, "b", &blob, 1000);
        agent_sign_from(&hub, "c", &blob, 2000); // another process
        let ids = pending_agent_ids(&hub);
        assert_eq!(ids.len(), 3);
        let first = hub
            .agent
            .lock()
            .unwrap()
            .pending
            .iter()
            .find(|(_, p)| p.beam_request_id == "a")
            .map(|(id, _)| id.clone())
            .unwrap();

        hub.decide_agent(&first, true, 7).unwrap();
        let mut signed = Vec::new();
        for _ in 0..2 {
            match next_agent_message(&commands) {
                ClientToBeam::AgentResponse { request_id, message, .. } => {
                    assert!(wire::parse_sign_response(&wire::b64_decode(&message).unwrap()).unwrap().is_some());
                    signed.push(request_id);
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        signed.sort();
        assert_eq!(signed, vec!["a".to_string(), "b".to_string()]);
        let left = hub.agent.lock().unwrap().pending.values().map(|p| p.beam_request_id.clone()).collect::<Vec<_>>();
        assert_eq!(left, vec!["c".to_string()], "a different process still needs its own decision");
        let _ = fs::remove_file(sock);
    }

    /// A hub whose startup config clones one ssh repo under a github.com
    /// grant, whose known_hosts names `host` as github.com, and whose
    /// frontend 7 has a fake local agent holding `user`.
    fn startup_hub(
        user: &ssh_key::PrivateKey,
        host: &ssh_key::PrivateKey,
        max_uses: u32,
    ) -> (Arc<Hub>, Receiver<ClientToBeam>, String) {
        let blob = user.public_key().to_bytes().unwrap();
        let fingerprint = beams_protocol::agent_crypto::fingerprint(&blob).unwrap();
        let mut hub = Hub::with_state_path_and_program(PersistedState::default(), None, "tsh".to_string());
        hub.agent_config = Box::new(move || crate::agent::Config {
            allowed_keys: vec![fingerprint.clone()],
            prompt_timeout: Some(30),
        });
        let known_hosts = format!("github.com {}\n", host.public_key().to_openssh().unwrap());
        hub.agent_known_hosts = Box::new(move || known_hosts.clone());
        hub.startup_config = Box::new(move || {
            crate::startup::StartupConfig::parse(&format!(
                "[startup]\ntasks = [{{ template = \"clone-git-repo\", params = {{ repo = \"git@github.com:o/r.git\", dest = \"~/r\" }} }}]\n\
                 [startup.ssh_grant]\nhosts = [\"github.com\"]\nmax_uses = {max_uses}\n"
            ))
            .unwrap()
        });
        let hub = Arc::new(hub);
        let (commands, command_rx) = mpsc::sync_channel(64);
        hub.bridges.lock().unwrap().insert(
            "beam-uuid".to_string(),
            BridgeRuntime::new(
                BridgeRecord {
                    beam_id: "beam-name".to_string(),
                    beam_uuid: "beam-uuid".to_string(),
                    login: "beams".to_string(),
                    status: "connecting".to_string(),
                    session_count: 1,
                    bridge_id: None,
                    message: None,
                },
                commands,
            ),
        );
        let sock = fake_local_agent(blob);
        hub.agent_frontend_attached(7, "beam-uuid", Some(sock.clone()));
        (hub, command_rx, sock)
    }

    fn connect_capable(hub: &Arc<Hub>) {
        handle_bridge_event(
            hub,
            crate::bridge::Event::Connected {
                beam_uuid: "beam-uuid".to_string(),
                bridge_id: "bridge-1".to_string(),
                capabilities: vec![CAPABILITY_SSH_AGENT.to_string(), CAPABILITY_STARTUP_MANIFEST.to_string()],
            },
        );
    }

    /// A verified github.com login from agent connection `conn`, attributed
    /// to `task_run`.
    fn github_login(hub: &Arc<Hub>, conn: &str, user: &ssh_key::PrivateKey, host: &ssh_key::PrivateKey, task_run: Option<&str>) {
        use beams_protocol::agent as wire;
        use signature::Signer;
        let blob = user.public_key().to_bytes().unwrap();
        let sid = vec![9u8; 32];
        let sig: ssh_key::Signature = host.key_data().try_sign(&sid).unwrap();
        let bind = wire::encode_session_bind(&wire::SessionBind {
            host_key: host.public_key().to_bytes().unwrap(),
            session_id: sid.clone(),
            signature: Vec::<u8>::try_from(sig).unwrap(),
            is_forwarding: false,
        });
        let msg = |message| crate::bridge::Event::Message { beam_uuid: "beam-uuid".to_string(), message };
        handle_bridge_event(hub, msg(BeamToClient::AgentSessionBind { conn_id: conn.into(), message: wire::b64_encode(&bind) }));
        let data = wire::encode_userauth_data(&wire::UserAuthRequest {
            session_id: sid,
            user: "git".into(),
            service: "ssh-connection".into(),
            method: wire::METHOD_PUBLICKEY.into(),
            algorithm: "ssh-ed25519".into(),
            key_blob: blob.clone(),
            host_key: None,
        });
        handle_bridge_event(
            hub,
            msg(BeamToClient::AgentSignRequest {
                request_id: conn.into(),
                conn_id: conn.into(),
                message: wire::b64_encode(&wire::encode_sign_request(&blob, &data, 0)),
                context: beams_protocol::AgentContext {
                    task_run: task_run.map(str::to_string),
                    host_candidates: vec!["github.com".into()],
                    ..Default::default()
                },
            }),
        );
    }

    fn next_manifest(commands: &Receiver<ClientToBeam>) -> Option<(String, Vec<beams_protocol::ManifestTask>)> {
        while let Ok(message) = commands.recv_timeout(Duration::from_millis(300)) {
            if let ClientToBeam::RunManifest { request_id, tasks } = message {
                return Some((request_id, tasks));
            }
        }
        None
    }

    #[test]
    fn startup_manifest_is_sent_once_and_its_grant_signs_only_its_own_clones() {
        use beams_protocol::agent as wire;
        use ssh_key::{rand_core::OsRng, Algorithm, PrivateKey};
        let (user, host) = (
            PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap(),
            PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap(),
        );
        let (hub, commands, sock) = startup_hub(&user, &host, 2);

        connect_capable(&hub);
        let (request_id, tasks) = next_manifest(&commands).expect("startup manifest sent on first connect");
        let templates: Vec<&str> = tasks.iter().map(|task| task.template.as_str()).collect();
        assert_eq!(templates, vec!["wait-ssh-agent", "clone-git-repo"]);
        connect_capable(&hub);
        assert!(next_manifest(&commands).is_none(), "never resent to the same Beam");

        let msg = |message| crate::bridge::Event::Message { beam_uuid: "beam-uuid".to_string(), message };
        // A grant only comes from our own request id.
        handle_bridge_event(&hub, msg(BeamToClient::ManifestAccepted {
            request_id: "made-up".into(), manifest_id: "manifest-9".into(), task_ids: vec![],
        }));
        assert!(hub.agent.lock().unwrap().grants.is_empty());
        handle_bridge_event(&hub, msg(BeamToClient::ManifestAccepted {
            request_id, manifest_id: "manifest-1".into(), task_ids: vec!["clone-git-repo:dest=/r".into()],
        }));
        let ServerEvent::Snapshot { grants, .. } = hub.snapshot() else { panic!() };
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].max_uses, 2);

        // The manifest's own clone: signed without a prompt.
        github_login(&hub, "c1", &user, &host, Some("manifest-1/clone-git-repo:dest=/r"));
        assert!(pending_agent_ids(&hub).is_empty(), "no prompt for a granted request");
        match next_agent_message(&commands) {
            ClientToBeam::AgentResponse { request_id, message, .. } => {
                assert_eq!(request_id, "c1");
                assert!(wire::parse_sign_response(&wire::b64_decode(&message).unwrap()).unwrap().is_some());
            }
            other => panic!("unexpected {other:?}"),
        }
        // Something else using the key during the window: asks as usual.
        github_login(&hub, "c2", &user, &host, None);
        assert_eq!(pending_agent_ids(&hub).len(), 1);
        github_login(&hub, "c3", &user, &host, Some("manifest-10/x"));
        assert_eq!(pending_agent_ids(&hub).len(), 2, "manifest ids are matched exactly");

        // Last use, then the budget is spent.
        github_login(&hub, "c4", &user, &host, Some("manifest-1/clone-git-repo:dest=/r"));
        assert!(matches!(next_agent_message(&commands), ClientToBeam::AgentResponse { request_id, .. } if request_id == "c4"));
        assert!(hub.agent.lock().unwrap().grants.is_empty(), "all uses spent");
        github_login(&hub, "c5", &user, &host, Some("manifest-1/clone-git-repo:dest=/r"));
        assert_eq!(pending_agent_ids(&hub).len(), 3);
        let _ = fs::remove_file(sock);
    }

    #[test]
    fn startup_grants_end_when_the_manifest_finishes_or_are_revoked() {
        use ssh_key::{rand_core::OsRng, Algorithm, PrivateKey};
        let (user, host) = (
            PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap(),
            PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap(),
        );
        let (hub, commands, sock) = startup_hub(&user, &host, 5);
        connect_capable(&hub);
        let (request_id, _) = next_manifest(&commands).unwrap();
        let msg = |message| crate::bridge::Event::Message { beam_uuid: "beam-uuid".to_string(), message };
        handle_bridge_event(&hub, msg(BeamToClient::ManifestAccepted {
            request_id, manifest_id: "manifest-1".into(), task_ids: vec![],
        }));
        assert_eq!(hub.agent.lock().unwrap().grants.len(), 1);
        handle_bridge_event(&hub, msg(BeamToClient::ManifestFinished { manifest_id: "manifest-1".into(), ok: true }));
        assert!(hub.agent.lock().unwrap().grants.is_empty());
        github_login(&hub, "c1", &user, &host, Some("manifest-1/clone-git-repo:dest=/r"));
        assert_eq!(pending_agent_ids(&hub).len(), 1, "after startup, clones ask");

        // Revoking (simulated: insert another grant, then revoke it).
        let grant = crate::startup::GrantRecord {
            grant_id: "g2".into(), beam_uuid: "beam-uuid".into(), manifest_id: "manifest-2".into(),
            hosts: vec!["github.com".into()], user: "git".into(), uses: 0, max_uses: 3,
            expires_at_ms: crate::agent::now_ms() + 60_000, ended: None,
        };
        hub.agent.lock().unwrap().grants.insert("g2".into(), grant);
        hub.end_grants_where(|g| g.grant_id == "g2", "revoked by local user");
        assert!(hub.agent.lock().unwrap().grants.is_empty());
        let _ = fs::remove_file(sock);
    }

    fn pending_agent_ids(hub: &Hub) -> Vec<String> {
        hub.agent.lock().unwrap().pending.keys().cloned().collect()
    }

    #[test]
    fn agent_requests_need_an_attached_frontend_and_one_local_decision() {
        use beams_protocol::agent as wire;
        use ssh_key::{rand_core::OsRng, Algorithm, PrivateKey};
        let key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap();
        let blob = key.public_key().to_bytes().unwrap();
        let fingerprint = beams_protocol::agent_crypto::fingerprint(&blob).unwrap();
        let (hub, commands) = agent_hub(&fingerprint);
        let sock = fake_local_agent(blob.clone());

        handle_bridge_event(
            &hub,
            crate::bridge::Event::Connected {
                beam_uuid: "beam-uuid".to_string(),
                bridge_id: "bridge-1".to_string(),
                capabilities: vec![CAPABILITY_SSH_AGENT.to_string()],
            },
        );
        // No frontend yet: nothing to list, and signs are denied unasked.
        assert_eq!(next_agent_message(&commands), ClientToBeam::AgentIdentities { keys: vec![] });
        agent_sign(&hub, "beam-req-0", &blob);
        match next_agent_message(&commands) {
            ClientToBeam::AgentResponse { request_id, message, note } => {
                assert_eq!(request_id, "beam-req-0");
                assert_eq!(wire::b64_decode(&message).unwrap(), wire::failure());
                assert!(note.unwrap().contains("no interactive"));
            }
            other => panic!("unexpected {other:?}"),
        }

        hub.agent_frontend_attached(7, "beam-uuid", Some(sock.clone()));
        match next_agent_message(&commands) {
            ClientToBeam::AgentIdentities { keys } => {
                assert_eq!(keys.len(), 1);
                assert_eq!(keys[0].comment, "test key");
                assert_eq!(wire::b64_decode(&keys[0].blob).unwrap(), blob);
            }
            other => panic!("unexpected {other:?}"),
        }

        // Queued for the user, visible in the snapshot, never persisted.
        agent_sign(&hub, "beam-req-1", &blob);
        let ids = pending_agent_ids(&hub);
        assert_eq!(ids.len(), 1);
        let ServerEvent::Snapshot { requests, .. } = hub.snapshot() else { panic!() };
        let record = requests.iter().find(|r| r.request_id == ids[0]).unwrap();
        assert_eq!(record.status, RequestStatus::PendingApproval);
        let prompt = record.agent.as_ref().unwrap();
        assert_eq!(prompt.claimed.reason.as_deref(), Some("clone for review"));
        assert!(matches!(
            &prompt.operation,
            crate::agent::AgentOperation::Login { user, host: crate::agent::HostCheck::Unverified { .. } } if user == "git"
        ));
        assert!(hub.state.lock().unwrap().requests.is_empty());

        hub.decide_agent(&ids[0], true, 7).unwrap();
        match next_agent_message(&commands) {
            ClientToBeam::AgentResponse { request_id, message, note } => {
                assert_eq!(request_id, "beam-req-1");
                assert_eq!(wire::parse_sign_response(&wire::b64_decode(&message).unwrap()).unwrap(), Some(b"sig".to_vec()));
                assert_eq!(note, None);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(hub.decide_agent(&ids[0], true, 7).is_err(), "a request is decided once");

        agent_sign(&hub, "beam-req-2", &blob);
        let id = pending_agent_ids(&hub).pop().unwrap();
        hub.decide_agent(&id, false, 7).unwrap();
        match next_agent_message(&commands) {
            ClientToBeam::AgentResponse { request_id, message, note } => {
                assert_eq!(request_id, "beam-req-2");
                assert_eq!(wire::b64_decode(&message).unwrap(), wire::failure());
                assert!(note.unwrap().contains("rejected"));
            }
            other => panic!("unexpected {other:?}"),
        }

        // A disallowed key is refused without asking.
        let other = PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap();
        agent_sign(&hub, "beam-req-3", &other.public_key().to_bytes().unwrap());
        assert!(matches!(
            next_agent_message(&commands),
            ClientToBeam::AgentResponse { note: Some(note), .. } if note.contains("allowed_keys")
        ));
        assert!(pending_agent_ids(&hub).is_empty());

        // A disconnect fails anything still waiting; detaching withdraws keys.
        agent_sign(&hub, "beam-req-4", &blob);
        assert_eq!(pending_agent_ids(&hub).len(), 1);
        handle_bridge_event(
            &hub,
            crate::bridge::Event::Disconnected { beam_uuid: "beam-uuid".to_string(), message: "gone".to_string() },
        );
        assert!(pending_agent_ids(&hub).is_empty());
        assert!(matches!(next_agent_message(&commands), ClientToBeam::AgentResponse { .. }));
        hub.agent_frontend_detached(7);
        assert_eq!(next_agent_message(&commands), ClientToBeam::AgentIdentities { keys: vec![] });
        let _ = fs::remove_file(sock);
    }
}
