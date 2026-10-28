//! Workstation-side daemon and its local frontend protocol.
//!
//! It owns the authority boundary for client services: one process per user,
//! a permissioned UDS, persistent client identity/request state, reconnecting
//! per-Beam control bridges, local-only approval decisions, and supervised
//! workstation-owned service processes.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, Weak};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use beams_protocol::{
    ActionRequestProjection, BeamToClient, ClientToBeam, RequestProjection, RequestStatus,
    ServiceAction, ServiceActionRequest, ServiceProjection, ServiceRequest, ServiceRequestKind,
    ServiceStatus, MAX_FRAME_BYTES, PROTOCOL, PROTOCOL_VERSION,
};
use serde::{Deserialize, Serialize};
use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
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
    pub action: Option<ServiceActionRequest>,
    #[serde(default)]
    pub message: Option<String>,
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
    },
    DecideRequest {
        request_id: String,
        approve: bool,
    },
    CreatePortForward {
        beam_id: String,
        beam_uuid: String,
        login: String,
        remote_host: String,
        remote_port: u16,
        local_port: Option<u16>,
    },
    ControlService {
        service_id: String,
        action: ServiceAction,
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

    fn mark_process_services_stopped_after_restart(&mut self) -> bool {
        let mut changed = false;
        for service in self.services.values_mut() {
            if matches!(
                service.status,
                ServiceStatus::Starting
                    | ServiceStatus::Running
                    | ServiceStatus::Stopping
                    | ServiceStatus::Degraded
                    | ServiceStatus::Unknown
            ) {
                service.status = ServiceStatus::Stopped;
                service.message =
                    Some("local daemon restarted; waiting to restore the forward".to_string());
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
}

struct BridgeRuntime {
    record: BridgeRecord,
    commands: SyncSender<ClientToBeam>,
}

struct ServiceRuntime {
    actions: SyncSender<crate::forward::Action>,
}

impl Hub {
    #[cfg(test)]
    fn new(state: PersistedState) -> Self {
        Self::with_state_path_and_program(state, None, "tsh".to_string())
    }

    fn with_state_path_and_program(
        state: PersistedState,
        state_path: Option<PathBuf>,
        forward_program: String,
    ) -> Self {
        Self {
            state: Mutex::new(state),
            state_path,
            subscribers: Mutex::new(Vec::new()),
            frontends: AtomicUsize::new(0),
            bridges: Mutex::new(BTreeMap::new()),
            service_runtimes: Mutex::new(BTreeMap::new()),
            forward_program,
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
            BridgeRuntime {
                record: record.clone(),
                commands,
            },
        );
        drop(bridges);
        self.broadcast(ServerEvent::BridgeStatus { bridge: record });
        self.recover_services(&target);

        let weak = Arc::downgrade(self);
        thread::spawn(move || handle_bridge_events(weak, event_rx));
        Ok(())
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

    fn decide_request(self: &Arc<Self>, request_id: &str, approve: bool) -> anyhow::Result<RequestRecord> {
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
            if !matches!(request.status, RequestStatus::Approved | RequestStatus::Starting) {
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
            let service = ServiceRecord {
                service_id: Uuid::new_v4().to_string(),
                request_id: request_id.to_string(),
                beam_id: target.beam_id.clone(),
                beam_uuid: target.beam_uuid.clone(),
                login: target.login.clone(),
                kind: service_request.kind.label().to_string(),
                spec: Some(service_request.kind),
                status: ServiceStatus::Starting,
                desired_running: true,
                message: Some("starting SSH local forward".to_string()),
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
        self.service_runtimes
            .lock()
            .unwrap()
            .insert(service.service_id.clone(), ServiceRuntime { actions });
        let weak = Arc::downgrade(self);
        thread::spawn(move || handle_service_events(weak, event_rx));
        Ok(())
    }

    fn control_service(&self, service_id: &str, action: ServiceAction) -> anyhow::Result<ServiceRecord> {
        let runtime_action = match action {
            ServiceAction::Stop => crate::forward::Action::Stop,
            ServiceAction::Restart => crate::forward::Action::Restart,
        };
        let actions = self
            .service_runtimes
            .lock()
            .unwrap()
            .get(service_id)
            .map(|runtime| runtime.actions.clone())
            .with_context(|| format!("service {service_id:?} is not owned by this daemon process"))?;
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
                        ServiceStatus::Running
                            | ServiceStatus::Degraded
                            | ServiceStatus::Unknown
                    ) {
                        service.status = service
                            .status
                            .transition(ServiceStatus::Stopping)
                            .map_err(anyhow::Error::msg)?;
                    }
                    service.desired_running = true;
                    service.message = Some("restart requested".to_string());
                }
            }
            service.clone()
        };
        actions
            .try_send(runtime_action)
            .map_err(|err| anyhow::anyhow!("sending service action: {err}"))?;
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
            .map(|runtime| runtime.actions.clone())
            .collect::<Vec<_>>();
        for actions in actions {
            let _ = actions.try_send(crate::forward::Action::Stop);
        }
        // Dropping every bridge command sender makes each supervisor kill and
        // reap its current SSH child instead of reconnecting.
        self.bridges.lock().unwrap().clear();

        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            let active = self
                .state
                .lock()
                .unwrap()
                .services
                .values()
                .any(|service| {
                    matches!(
                        service.status,
                        ServiceStatus::Starting
                            | ServiceStatus::Running
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

fn service_projection(service: &ServiceRecord) -> Option<ServiceProjection> {
    Some(ServiceProjection {
        service_id: service.service_id.clone(),
        request_id: service.request_id.clone(),
        kind: service.spec.clone()?,
        status: service.status,
        message: service.message.clone(),
    })
}

fn handle_service_events(hub: Weak<Hub>, events: Receiver<crate::forward::Event>) {
    while let Ok(event) = events.recv() {
        let Some(hub) = hub.upgrade() else {
            return;
        };
        handle_service_event(&hub, event);
    }
}

fn handle_service_event(hub: &Arc<Hub>, event: crate::forward::Event) {
    let (service, completed_actions) = {
        let mut state = hub.state.lock().unwrap();
        let Some(service) = state.services.get_mut(&event.service_id) else {
            return;
        };
        if service.status != event.status {
            match service.status.transition(event.status) {
                Ok(next) => service.status = next,
                Err(_) if event.status == ServiceStatus::Starting => {
                    service.status = ServiceStatus::Starting;
                }
                Err(_) => return,
            }
        }
        service.spec = Some(event.kind);
        service.message = event.message;
        let service = service.clone();

        if service.status == ServiceStatus::Running {
            if let Some(request) = state.requests.get_mut(&service.request_id) {
                if request.status == RequestStatus::Starting {
                    request.status = RequestStatus::Running;
                    request.message = Some("workstation service is running".to_string());
                }
            }
        } else if service.status == ServiceStatus::Error {
            if let Some(request) = state.requests.get_mut(&service.request_id) {
                if matches!(request.status, RequestStatus::Approved | RequestStatus::Starting) {
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
                if action.service_id != service.service_id || request.status != RequestStatus::Starting {
                    return None;
                }
                let completed = matches!(
                    (action.action, service.status),
                    (ServiceAction::Stop, ServiceStatus::Stopped)
                        | (ServiceAction::Restart, ServiceStatus::Running)
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
        (service, completed_actions)
    };
    let _ = hub.save_state();
    hub.publish_service(&service);
    if let Some(request) = hub
        .state
        .lock()
        .unwrap()
        .requests
        .get(&service.request_id)
        .cloned()
    {
        hub.publish_request(&request, false);
    }
    for request in completed_actions {
        hub.publish_request(&request, true);
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
                    action: None,
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
    let approvals_changed = state.expire_pending_after_restart();
    let services_changed = state.mark_process_services_stopped_after_restart();
    if approvals_changed || services_changed {
        state.save(&state_path)?;
    }
    let listener = bind_socket(&socket_path)?;
    listener
        .set_nonblocking(true)
        .context("making daemon listener nonblocking")?;
    let forward_program = std::env::var("BEAMS_TSH").unwrap_or_else(|_| "tsh".to_string());
    let hub = Arc::new(Hub::with_state_path_and_program(
        state,
        Some(state_path),
        forward_program,
    ));

    let terminating = Arc::new(AtomicBool::new(false));
    for signal in [SIGHUP, SIGINT, SIGQUIT, SIGTERM] {
        signal_hook::flag::register(signal, Arc::clone(&terminating))
            .context("installing daemon termination handler")?;
    }

    while !terminating.load(Ordering::Relaxed) {
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
    drop(listener);
    let _ = fs::remove_file(&socket_path);
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
                        if let Err(err) = hub.create_port_forward(
                            target,
                            remote_host,
                            remote_port,
                            local_port,
                        ) {
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
        let a = thread::spawn(move || first.decide_request("raced-request", false).is_ok());
        let b = thread::spawn(move || second.decide_request("raced-request", false).is_ok());
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

        hub.decide_request("request-forward", true).unwrap();
        wait_until(Duration::from_secs(3), || {
            hub.state.lock().unwrap().requests["request-forward"].status
                == RequestStatus::Running
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
            hub.state.lock().unwrap().services[&service.service_id].status
                == ServiceStatus::Stopped
        });
        hub.control_service(&service.service_id, ServiceAction::Restart)
            .unwrap();
        wait_until(Duration::from_secs(3), || {
            hub.state.lock().unwrap().services[&service.service_id].status
                == ServiceStatus::Running
        });

        drop(hub);
        let _ = fs::remove_file(program);
    }
}
