//! The bootstrap daemon: a long-lived process reachable over a UDS
//! socket, driving on-demand task execution for whoever's connected (a
//! future TUI, the CLI, eventually a Mac app - see `docs/designs/DESIGN.md`'s
//! "Architecture" and "Wire protocol" sections).
//!
//! Protocol: newline-delimited JSON, one request per line in, any number
//! of lines back out. Every connection is both a requester and a
//! broadcast subscriber - a `run_task` from *any* connection emits
//! `task_status` events to *every* connected client, not just the one
//! that asked, so e.g. a TUI can watch progress for a task someone else
//! kicked off from the CLI. There's no `subscribe` op: connecting is
//! enough.
//!
//! Supported requests today (see the module doc comment for what's
//! deliberately not here yet - auth/multi-daemon concerns, resolving a
//! `run_task`/manifest against capabilities from a *previous* run rather
//! than just the instances named in the same request):
//!
//! ```json
//! {"request_id": "1", "op": "list_tasks"}
//! {"request_id": "2", "op": "run_task", "task": "clone-git-repo", "params": {"repo": "...", "dest": "..."}}
//! {"request_id": "3", "op": "run_manifest", "tasks": [
//!   {"template": "clone-git-repo", "params": {"repo": "...", "dest": "~/projects/core"}},
//!   {"template": "checkout-pr-worktree", "params": {"base": "~/projects/core", "pr": 1234, "dest": "..."}}
//! ]}
//! ```
//!
//! `list_tasks`'s reply carries each template's param schema too (`tasks:
//! [{"template": "clone-git-repo", "params": [{"name": "repo", "kind":
//! "string", "required": true}, ...]}, ...]`, see `tasks::TemplateInfo`)
//! - enough for a client to build an input field per param without
//! hardcoding per-template knowledge.
//!
//! `run_manifest` is `run_task` generalized to several task instances at
//! once (see docs/designs/DESIGN.md's "Seeding manifest") - the entries are resolved
//! against each other by capability (`executor::plan`/`executor::run` -
//! see docs/designs/DESIGN.md's "Concurrent execution"), so listing them out of
//! dependency order, as above, is fine. Each entry starts as soon as the
//! entries it depends on have finished; independent entries run
//! concurrently. Its `accepted` reply's `tasks` field is that plan: a
//! list of `{"task_id", "depends_on": [task_id, ...]}` in a valid
//! topological order. A failed entry blocks only the entries that depend
//! on it (reported with a `"blocked"` `task_status`); everything else
//! keeps running. A manifest entry can only depend on a capability
//! provided by *another entry in the same manifest* - not on one left
//! over from a previous, separate run. Because every task here is
//! idempotent, the fix is simply to list the full chain every time (e.g.
//! always include `clone-git-repo` alongside a `checkout-pr-worktree`
//! that needs it): the redundant entry just skips if its work is already
//! done.
//!
//! `run_task`/`run_manifest` don't hold up anything else: each one
//! replies `accepted` and then runs on its own thread, so the same
//! connection can keep submitting, and separate submissions run
//! concurrently with each other. What *does* serialize across
//! submissions is the daemon-wide `executor::Resources` (`Hub::resources`):
//! two tasks sharing an `exclusive_resources` entry (e.g. `apt`), or two
//! submissions of the very same task id, never overlap.
//!
//! Direct replies (only to the requester) carry the same `request_id`
//! back; broadcast `task_status`/`manifest_status` events don't carry
//! one, since they aren't a reply to anything in particular. A
//! `task_status` broadcast carries a `manifest_id` when the task ran as
//! part of a manifest (`null` for a standalone `run_task`).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::Context;
use beams_protocol::{
    ActionRequestProjection, BeamToClient, ClientToBeam, FetchSource, RequestProjection, RequestStatus,
    ServiceActionRequest, ServiceProjection, ServiceRequest, ServiceRequestKind, ServiceStatus,
    CAPABILITY_FILE_FETCH, CAPABILITY_SSH_AGENT, ManifestParam, ManifestTask, MAX_FETCH_SOURCES, MAX_FRAME_BYTES, PROTOCOL, PROTOCOL_VERSION,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::agentproxy::{self, AgentBridges, AgentState};
use crate::executor;
use crate::tasks::{self, Task};

pub fn default_socket_path() -> String {
    "~/.beam-bootstrap/daemon.sock".to_string()
}

/// State shared by every connection: who's listening for broadcast
/// events, and the exclusive-resource locks every task run through this
/// daemon shares (see the module doc comment).
struct Hub {
    subscribers: Mutex<Vec<(u64, Sender<String>)>>,
    resources: executor::Resources,
    manifest_counter: AtomicU64,
    connection_counter: AtomicU64,
    bridges: Mutex<BTreeMap<String, BridgeRegistration>>,
    client_requests: Mutex<BTreeMap<String, ClientRequestProjection>>,
    client_action_requests: Mutex<BTreeMap<String, ClientActionProjection>>,
    client_services: Mutex<BTreeMap<String, OwnedServiceProjection>>,
    /// Running workstation-submitted manifests: manifest id -> the
    /// `client_id` that submitted it. Routes its tasks' SSH key use
    /// (`route_agent`) while it runs.
    manifest_owners: Mutex<HashMap<String, String>>,
    agent: AgentState,
    /// Accepted launches (`launch::daemon`).
    launches: crate::launch::daemon::Launches,
}

impl Default for Hub {
    fn default() -> Self {
        Self {
            subscribers: Mutex::new(Vec::new()),
            resources: executor::Resources::default(),
            manifest_counter: AtomicU64::new(0),
            connection_counter: AtomicU64::new(0),
            bridges: Mutex::new(BTreeMap::new()),
            client_requests: Mutex::new(BTreeMap::new()),
            client_action_requests: Mutex::new(BTreeMap::new()),
            client_services: Mutex::new(BTreeMap::new()),
            manifest_owners: Mutex::new(HashMap::new()),
            agent: AgentState::default(),
            launches: crate::launch::daemon::Launches::new(
                crate::pathutil::expand_tilde("~/.beam-bootstrap/launch-state"),
                home_dir(),
            ),
        }
    }
}

fn home_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".to_string()))
}

impl crate::launch::daemon::Host for Hub {
    fn broadcast(&self, line: &str) {
        Hub::broadcast(self, line);
    }
    fn resources(&self) -> &executor::Resources {
        &self.resources
    }
    fn launches(&self) -> &crate::launch::daemon::Launches {
        &self.launches
    }
}

impl AgentBridges for Hub {
    fn agent_state(&self) -> &AgentState {
        &self.agent
    }

    /// A shell's session token wins; else a task of a running
    /// workstation-submitted manifest goes to the client that submitted
    /// it; else the only connected client. The run tag is advisory (any
    /// process can claim one), but routing on it gives a spoofer nothing
    /// the last rule doesn't: the chosen workstation still evaluates and
    /// approves the request itself.
    fn route_agent(&self, session_token: Option<&str>, task_run: Option<&str>) -> Result<String, String> {
        let owner = task_run
            .and_then(|run| run.split_once('/').map(|(manifest, _)| manifest))
            .and_then(|manifest| self.manifest_owners.lock().unwrap().get(manifest).cloned());
        let bridges = self.bridges.lock().unwrap();
        let capable: Vec<&BridgeRegistration> = bridges
            .values()
            .filter(|bridge| bridge.ready && bridge.capabilities.contains(CAPABILITY_SSH_AGENT))
            .collect();
        let only = |keep: &dyn Fn(&BridgeRegistration) -> bool| match capable
            .iter()
            .filter(|bridge| keep(bridge))
            .collect::<Vec<_>>()
            .as_slice()
        {
            [bridge] => Some(bridge.bridge_id.clone()),
            _ => None,
        };
        if let Some(token) = session_token {
            if let Some(bridge_id) = only(&|bridge| bridge.session_tokens.contains(token)) {
                return Ok(bridge_id);
            }
        }
        if let Some(owner) = owner {
            if let Some(bridge_id) = only(&|bridge| bridge.client_id == owner) {
                return Ok(bridge_id);
            }
        }
        match capable.as_slice() {
            [bridge] => Ok(bridge.bridge_id.clone()),
            [] => Err("no managed beams client is connected".to_string()),
            _ => Err("several beams clients are connected and none owns this shell's session or task".to_string()),
        }
    }

    fn send_agent(&self, bridge_id: &str, message: &BeamToClient) -> bool {
        let sender = self.bridges.lock().unwrap().get(bridge_id).map(|bridge| bridge.sender.clone());
        sender.is_some_and(|sender| send_bridge_event(&sender, message))
    }
}

#[derive(Clone)]
struct BridgeRegistration {
    client_id: String,
    bridge_id: String,
    session_tokens: HashSet<String>,
    ready: bool,
    sender: Sender<String>,
    /// Optional features the workstation advertised (see
    /// `beams_protocol::CAPABILITIES`).
    capabilities: HashSet<String>,
}

#[derive(Clone, Debug, Serialize)]
struct ClientRequestProjection {
    request: ServiceRequest,
    owner_client_id: String,
    bridge_id: String,
    status: RequestStatus,
    message: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
struct OwnedServiceProjection {
    owner_client_id: String,
    service: ServiceProjection,
}

#[derive(Clone, Debug, Serialize)]
struct ClientActionProjection {
    request: ServiceActionRequest,
    owner_client_id: String,
    bridge_id: String,
    status: RequestStatus,
    message: Option<String>,
}

#[derive(Default)]
struct ConnectionContext {
    bridge_id: Option<String>,
    /// Requests this connection submitted that are tied to it: if it goes
    /// away first (Ctrl-C in `beams fetch`), they're withdrawn.
    tied_requests: Vec<String>,
}

impl Hub {
    /// Registers a new connection, returning the sender it should use
    /// for *both* its own direct replies and this hub's broadcasts (both
    /// end up as lines on the same socket - see the module doc comment)
    /// and the receiver its dedicated writer thread should drain.
    fn register(&self) -> (u64, Sender<String>, Receiver<String>) {
        let (tx, rx) = mpsc::channel();
        let id = self.connection_counter.fetch_add(1, Ordering::Relaxed);
        self.subscribers.lock().unwrap().push((id, tx.clone()));
        (id, tx, rx)
    }

    fn unregister(&self, connection_id: u64) {
        self.subscribers.lock().unwrap().retain(|(id, _)| *id != connection_id);
    }

    /// Sends `line` to every still-live subscriber, dropping any whose
    /// receiver has gone away (its connection closed).
    fn broadcast(&self, line: &str) {
        let mut subs = self.subscribers.lock().unwrap();
        subs.retain(|(_, tx)| tx.send(line.to_string()).is_ok());
    }

    /// A fresh id for a `run_manifest` call - just needs to be unique
    /// within this daemon's lifetime, not globally meaningful.
    fn next_manifest_id(&self) -> String {
        format!("manifest-{}", self.manifest_counter.fetch_add(1, Ordering::Relaxed))
    }

    fn register_bridge(&self, registration: BridgeRegistration) -> Result<(), String> {
        let client_id = registration.client_id.clone();
        let bridge_id = registration.bridge_id.clone();
        let mut bridges = self.bridges.lock().unwrap();
        if bridges.contains_key(&bridge_id) {
            return Err(format!("client bridge {bridge_id:?} is already registered"));
        }
        bridges.insert(bridge_id.clone(), registration);
        drop(bridges);
        self.broadcast(
            &json!({
                "type": "client_status",
                "client_id": client_id,
                "bridge_id": bridge_id,
                "status": "synchronizing",
            })
            .to_string(),
        );
        Ok(())
    }

    /// Tells the owning workstation that the requester gave up on
    /// `service_request_id`, unless the request already ended. The
    /// workstation ignores a withdrawal for a finished service.
    fn withdraw_request(&self, service_request_id: &str) {
        let Some(projection) = self.client_requests.lock().unwrap().get(service_request_id).cloned() else {
            return;
        };
        if matches!(projection.status, RequestStatus::Rejected | RequestStatus::Expired | RequestStatus::Failed) {
            return;
        }
        let sender = self
            .bridges
            .lock()
            .unwrap()
            .get(&projection.bridge_id)
            .filter(|bridge| bridge.capabilities.contains(CAPABILITY_FILE_FETCH))
            .map(|bridge| bridge.sender.clone());
        if let Some(sender) = sender {
            send_bridge_event(&sender, &BeamToClient::RequestWithdrawn { request_id: service_request_id.to_string() });
        }
    }

    fn unregister_bridge(&self, bridge_id: &str) {
        self.agent.forget_bridge(bridge_id);
        let removed = self.bridges.lock().unwrap().remove(bridge_id);
        if let Some(registration) = removed {
            let changed = {
                let mut services = self.client_services.lock().unwrap();
                services
                    .values_mut()
                    .filter(|service| service.owner_client_id == registration.client_id)
                    .filter_map(|service| {
                        if service.service.status == ServiceStatus::Running {
                            service.service.status = ServiceStatus::Unknown;
                            service.service.message =
                                Some("owning workstation bridge disconnected".to_string());
                            Some(service.clone())
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
            };
            for service in changed {
                self.broadcast(
                    &json!({"type": "client_service_status", "service": service}).to_string(),
                );
            }
            self.broadcast(
                &json!({
                    "type": "client_status",
                    "client_id": registration.client_id,
                    "bridge_id": registration.bridge_id,
                    "status": "disconnected",
                })
                .to_string(),
            );
        }
    }
}

/// Binds `socket_path` and serves forever, one thread per connection.
/// Removes a stale socket file left behind by an unclean previous exit
/// (but only after confirming nothing is actually listening on it, so a
/// second daemon invocation doesn't steal the socket out from under a
/// live one).
pub fn serve(socket_path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = socket_path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    if socket_path.exists() {
        if UnixStream::connect(socket_path).is_ok() {
            anyhow::bail!("a daemon is already listening on {} - not starting a second one", socket_path.display());
        }
        fs::remove_file(socket_path).with_context(|| format!("removing stale socket {}", socket_path.display()))?;
    }

    let listener = UnixListener::bind(socket_path).with_context(|| format!("binding {}", socket_path.display()))?;
    let hub = Arc::new(Hub {
        launches: crate::launch::daemon::Launches::new(
            crate::launch::daemon::state_root(socket_path),
            home_dir(),
        ),
        ..Hub::default()
    });
    hub.launches.load();

    // The SSH agent relay lives next to the daemon socket. Failing to start
    // it (say, another agent already owns the path) leaves the rest of the
    // daemon working.
    let agent_socket = socket_path.with_file_name("ssh-agent.sock");
    crate::sshagent::set_relay_socket(agent_socket.clone());
    {
        let hub = Arc::clone(&hub);
        thread::spawn(move || {
            if let Err(err) = agentproxy::serve(&agent_socket, hub) {
                eprintln!("daemon: ssh agent relay not started: {err:#}");
            }
        });
    }

    for stream in listener.incoming() {
        let stream = stream.context("accepting connection")?;
        let hub = Arc::clone(&hub);
        thread::spawn(move || {
            if let Err(err) = handle_connection(&hub, stream) {
                eprintln!("daemon: connection error: {err}");
            }
        });
    }
    Ok(())
}

/// Services one connection until it disconnects (EOF) or errors. Split
/// out from `serve` so tests can exercise the protocol directly over an
/// in-memory `UnixStream::pair()`, without a real socket file or an
/// infinite `accept()` loop to manage.
fn handle_connection(hub: &Arc<Hub>, stream: UnixStream) -> anyhow::Result<()> {
    let write_half = stream.try_clone().context("cloning connection for writing")?;
    let (connection_id, tx, rx) = hub.register();

    // Dedicated writer thread: broadcasts (from *any* connection's
    // run_task) and this connection's own direct replies both arrive as
    // `String`s on `rx` and get written out here, independent of
    // whatever `handle_connection`'s own read loop below is doing (which
    // blocks for the duration of a `run_task` it issued) - this is what
    // lets a requester see live progress for its own task rather than
    // only after it finishes. Ends once every `Sender` for this
    // connection is gone (pruned by a future failed `broadcast`, since
    // the direct-reply `tx` passed to `handle_line` is this same sender).
    let writer = thread::spawn(move || {
        let mut write_half = write_half;
        for line in rx {
            if writeln!(write_half, "{line}").is_err() {
                break;
            }
            let _ = write_half.flush();
        }
    });

    let reader = BufReader::new(stream);
    let mut context = ConnectionContext::default();
    for line in reader.lines() {
        let line = line.context("reading request")?;
        if line.trim().is_empty() {
            continue;
        }
        handle_line(hub, &line, &tx, &mut context);
    }

    if let Some(bridge_id) = context.bridge_id {
        hub.unregister_bridge(&bridge_id);
    }
    for service_request_id in &context.tied_requests {
        hub.withdraw_request(service_request_id);
    }
    hub.unregister(connection_id);
    drop(tx);
    let _ = writer.join();
    Ok(())
}

/// Parses and dispatches one request line, sending its direct reply (and
/// any `run_task` broadcasts) through `reply`.
fn handle_line(hub: &Arc<Hub>, line: &str, reply: &Sender<String>, context: &mut ConnectionContext) {
    let request: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(err) => {
            let _ = reply.send(error_reply(None, &format!("invalid JSON: {err}")));
            return;
        }
    };
    let request_id = request.get("request_id").and_then(Value::as_str);
    let op = request.get("op").and_then(Value::as_str).unwrap_or_default();

    match op {
        "list_tasks" => {
            let _ =
                reply.send(json!({"request_id": request_id, "type": "tasks", "tasks": tasks::templates()}).to_string());
        }
        "run_task" => {
            let name = request.get("task").and_then(Value::as_str).unwrap_or_default();
            let params = request.get("params").cloned().unwrap_or(Value::Null);
            run_task(hub, request_id, name, params, reply);
        }
        "run_manifest" => {
            let tasks_value = request.get("tasks").cloned().unwrap_or(Value::Null);
            run_manifest(hub, request_id, tasks_value, reply);
        }
        "launch_submit" | "launch_signal" | "launch_snapshot" | "launch_retry" | "launch_ready" => {
            crate::launch::daemon::handle(hub, op, &request, &|mut message| {
                message["request_id"] = json!(request_id);
                let _ = reply.send(message.to_string());
            });
        }
        "register_client_bridge" => {
            register_client_bridge(hub, request_id, &request, reply, context);
        }
        "client_bridge_message" => {
            client_bridge_message(hub, request_id, &request, reply, context);
        }
        "list_clients" => {
            list_clients(hub, request_id, reply);
        }
        "list_client_state" => {
            list_client_state(hub, request_id, reply);
        }
        "request_service" => {
            request_service(hub, request_id, &request, reply, context);
        }
        "request_service_action" => {
            request_service_action(hub, request_id, &request, reply);
        }
        other => {
            let _ = reply.send(error_reply(request_id, &format!("unknown op {other:?}")));
        }
    }
}

fn register_client_bridge(
    hub: &Hub,
    request_id: Option<&str>,
    request: &Value,
    reply: &Sender<String>,
    context: &mut ConnectionContext,
) {
    if context.bridge_id.is_some() {
        let _ = reply.send(error_reply(request_id, "this connection is already a client bridge"));
        return;
    }
    let message: ClientToBeam = match serde_json::from_value(request.get("message").cloned().unwrap_or(Value::Null)) {
        Ok(message) => message,
        Err(err) => {
            let _ = reply.send(error_reply(request_id, &format!("invalid bridge hello: {err}")));
            return;
        }
    };
    let ClientToBeam::Hello { protocol, version, client_id, bridge_id, session_tokens, capabilities } = message else {
        let _ = reply.send(error_reply(request_id, "first bridge message must be hello"));
        return;
    };
    if protocol != PROTOCOL || version != PROTOCOL_VERSION {
        let _ = reply.send(error_reply(
            request_id,
            &format!(
                "unsupported bridge protocol {protocol:?} version {version}; expected {PROTOCOL:?} version {PROTOCOL_VERSION}"
            ),
        ));
        return;
    }
    if client_id.is_empty() || bridge_id.is_empty() {
        let _ = reply.send(error_reply(request_id, "client_id and bridge_id must not be empty"));
        return;
    }
    let registration = BridgeRegistration {
        client_id,
        bridge_id: bridge_id.clone(),
        session_tokens: session_tokens.into_iter().filter(|token| !token.is_empty()).collect(),
        ready: false,
        sender: reply.clone(),
        capabilities: capabilities.into_iter().collect(),
    };
    if let Err(err) = hub.register_bridge(registration) {
        let _ = reply.send(error_reply(request_id, &err));
        return;
    }
    context.bridge_id = Some(bridge_id.clone());
    let _ = reply.send(
        json!({
            "request_id": request_id,
            "type": "bridge_registered",
            "message": BeamToClient::hello(bridge_id),
        })
        .to_string(),
    );
}

fn client_bridge_message(
    hub: &Arc<Hub>,
    request_id: Option<&str>,
    request: &Value,
    reply: &Sender<String>,
    context: &ConnectionContext,
) {
    let Some(bridge_id) = context.bridge_id.as_deref() else {
        let _ = reply.send(error_reply(request_id, "register_client_bridge must be called first"));
        return;
    };
    let message: ClientToBeam = match serde_json::from_value(request.get("message").cloned().unwrap_or(Value::Null)) {
        Ok(message) => message,
        Err(err) => {
            let _ = reply.send(error_reply(request_id, &format!("invalid bridge message: {err}")));
            return;
        }
    };

    match message {
        ClientToBeam::Hello { .. } => {
            let _ = reply.send(error_reply(request_id, "bridge hello was already received"));
        }
        ClientToBeam::AddSessionToken { session_token } => {
            if session_token.is_empty() {
                let _ = reply.send(error_reply(request_id, "session token must not be empty"));
                return;
            }
            let mut bridges = hub.bridges.lock().unwrap();
            let Some(bridge) = bridges.get_mut(bridge_id) else {
                let _ = reply.send(error_reply(request_id, "bridge is no longer registered"));
                return;
            };
            bridge.session_tokens.insert(session_token);
            let _ = reply.send(json!({"request_id": request_id, "type": "bridge_message_accepted"}).to_string());
        }
        ClientToBeam::RequestStatus { request_id: service_request_id, status, message } => {
            let client_id = {
                let bridges = hub.bridges.lock().unwrap();
                match bridges.get(bridge_id) {
                    Some(bridge) => bridge.client_id.clone(),
                    None => {
                        let _ = reply.send(error_reply(request_id, "bridge is no longer registered"));
                        return;
                    }
                }
            };
            let projection = {
                let mut requests = hub.client_requests.lock().unwrap();
                let Some(projection) = requests.get_mut(&service_request_id) else {
                    let _ =
                        reply.send(error_reply(request_id, &format!("unknown service request {service_request_id:?}")));
                    return;
                };
                if projection.owner_client_id != client_id || projection.bridge_id != bridge_id {
                    let _ =
                        reply.send(error_reply(request_id, "service request is owned by a different client bridge"));
                    return;
                }
                match projection.status.transition(status) {
                    Ok(next) => projection.status = next,
                    Err(err) => {
                        let _ = reply.send(error_reply(request_id, &err));
                        return;
                    }
                }
                projection.message = message;
                projection.clone()
            };
            hub.broadcast(&json!({"type": "service_request_status", "request": projection}).to_string());
            let _ = reply.send(json!({"request_id": request_id, "type": "bridge_message_accepted"}).to_string());
        }
        ClientToBeam::RequestSnapshot { requests } => {
            let client_id = {
                let bridges = hub.bridges.lock().unwrap();
                match bridges.get(bridge_id) {
                    Some(bridge) => bridge.client_id.clone(),
                    None => {
                        let _ = reply.send(error_reply(request_id, "bridge is no longer registered"));
                        return;
                    }
                }
            };
            {
                let mut current = hub.client_requests.lock().unwrap();
                current.retain(|_, request| request.owner_client_id != client_id);
                for RequestProjection { request, status, message } in requests {
                    current.insert(
                        request.request_id.clone(),
                        ClientRequestProjection {
                            request,
                            owner_client_id: client_id.clone(),
                            bridge_id: bridge_id.to_string(),
                            status,
                            message,
                        },
                    );
                }
            }
            let requests = hub
                .client_requests
                .lock()
                .unwrap()
                .values()
                .filter(|request| request.owner_client_id == client_id)
                .cloned()
                .collect::<Vec<_>>();
            hub.broadcast(
                &json!({
                    "type": "client_requests_snapshot",
                    "client_id": client_id,
                    "requests": requests,
                })
                .to_string(),
            );
            let _ = reply.send(json!({"request_id": request_id, "type": "bridge_message_accepted"}).to_string());
        }
        ClientToBeam::ActionRequestSnapshot { requests } => {
            let client_id = {
                let bridges = hub.bridges.lock().unwrap();
                match bridges.get(bridge_id) {
                    Some(bridge) => bridge.client_id.clone(),
                    None => {
                        let _ = reply.send(error_reply(request_id, "bridge is no longer registered"));
                        return;
                    }
                }
            };
            {
                let mut current = hub.client_action_requests.lock().unwrap();
                current.retain(|_, request| request.owner_client_id != client_id);
                for ActionRequestProjection {
                    request,
                    status,
                    message,
                } in requests
                {
                    current.insert(
                        request.request_id.clone(),
                        ClientActionProjection {
                            request,
                            owner_client_id: client_id.clone(),
                            bridge_id: bridge_id.to_string(),
                            status,
                            message,
                        },
                    );
                }
            }
            let requests = hub
                .client_action_requests
                .lock()
                .unwrap()
                .values()
                .filter(|request| request.owner_client_id == client_id)
                .cloned()
                .collect::<Vec<_>>();
            hub.broadcast(
                &json!({
                    "type": "client_action_requests_snapshot",
                    "client_id": client_id,
                    "requests": requests,
                })
                .to_string(),
            );
            let _ = reply.send(
                json!({"request_id": request_id, "type": "bridge_message_accepted"})
                    .to_string(),
            );
        }
        ClientToBeam::ServiceSnapshot { services } => {
            let client_id = {
                let bridges = hub.bridges.lock().unwrap();
                match bridges.get(bridge_id) {
                    Some(bridge) => bridge.client_id.clone(),
                    None => {
                        let _ = reply.send(error_reply(request_id, "bridge is no longer registered"));
                        return;
                    }
                }
            };
            {
                let mut current = hub.client_services.lock().unwrap();
                current.retain(|_, service| service.owner_client_id != client_id);
                for service in services {
                    current.insert(
                        service.service_id.clone(),
                        OwnedServiceProjection { owner_client_id: client_id.clone(), service },
                    );
                }
            }
            let services = hub
                .client_services
                .lock()
                .unwrap()
                .values()
                .filter(|service| service.owner_client_id == client_id)
                .cloned()
                .collect::<Vec<_>>();
            hub.broadcast(
                &json!({
                    "type": "client_services_snapshot",
                    "client_id": client_id,
                    "services": services,
                })
                .to_string(),
            );
            let _ = reply.send(json!({"request_id": request_id, "type": "bridge_message_accepted"}).to_string());
        }
        ClientToBeam::ServiceStatus { service } => {
            let client_id = {
                let bridges = hub.bridges.lock().unwrap();
                match bridges.get(bridge_id) {
                    Some(bridge) => bridge.client_id.clone(),
                    None => {
                        let _ = reply.send(error_reply(request_id, "bridge is no longer registered"));
                        return;
                    }
                }
            };
            let owned = {
                let mut current = hub.client_services.lock().unwrap();
                match current.get_mut(&service.service_id) {
                    Some(existing) if existing.owner_client_id != client_id => {
                        let _ = reply.send(error_reply(
                            request_id,
                            "service is owned by a different client bridge",
                        ));
                        return;
                    }
                    Some(existing) => {
                        existing.service = service;
                        existing.clone()
                    }
                    None => {
                        let owned = OwnedServiceProjection {
                            owner_client_id: client_id,
                            service,
                        };
                        current.insert(owned.service.service_id.clone(), owned.clone());
                        owned
                    }
                }
            };
            hub.broadcast(
                &json!({"type": "client_service_status", "service": owned}).to_string(),
            );
            let _ = reply.send(
                json!({"request_id": request_id, "type": "bridge_message_accepted"})
                    .to_string(),
            );
        }
        ClientToBeam::ServiceActionStatus {
            request_id: action_request_id,
            status,
            message,
        } => {
            let client_id = {
                let bridges = hub.bridges.lock().unwrap();
                match bridges.get(bridge_id) {
                    Some(bridge) => bridge.client_id.clone(),
                    None => {
                        let _ = reply.send(error_reply(request_id, "bridge is no longer registered"));
                        return;
                    }
                }
            };
            let projection = {
                let mut requests = hub.client_action_requests.lock().unwrap();
                let Some(projection) = requests.get_mut(&action_request_id) else {
                    let _ = reply.send(error_reply(
                        request_id,
                        &format!("unknown service action request {action_request_id:?}"),
                    ));
                    return;
                };
                if projection.owner_client_id != client_id || projection.bridge_id != bridge_id {
                    let _ = reply.send(error_reply(
                        request_id,
                        "service action request is owned by a different client bridge",
                    ));
                    return;
                }
                match projection.status.transition(status) {
                    Ok(next) => projection.status = next,
                    Err(err) => {
                        let _ = reply.send(error_reply(request_id, &err));
                        return;
                    }
                }
                projection.message = message;
                projection.clone()
            };
            hub.broadcast(
                &json!({"type": "service_action_request_status", "request": projection})
                    .to_string(),
            );
            let _ = reply.send(
                json!({"request_id": request_id, "type": "bridge_message_accepted"})
                    .to_string(),
            );
        }
        ClientToBeam::Ready => {
            let (client_id, bridge_id) = {
                let mut bridges = hub.bridges.lock().unwrap();
                let Some(bridge) = bridges.get_mut(bridge_id) else {
                    let _ = reply.send(error_reply(request_id, "bridge is no longer registered"));
                    return;
                };
                bridge.ready = true;
                (bridge.client_id.clone(), bridge.bridge_id.clone())
            };
            hub.broadcast(
                &json!({
                    "type": "client_status",
                    "client_id": client_id,
                    "bridge_id": bridge_id,
                    "status": "connected",
                })
                .to_string(),
            );
            let _ = reply.send(json!({"request_id": request_id, "type": "bridge_message_accepted"}).to_string());
        }
        ClientToBeam::Ping { nonce } => {
            send_bridge_event(reply, &BeamToClient::Pong { nonce });
        }
        ClientToBeam::RunManifest { request_id: manifest_request_id, tasks } => {
            let client_id = hub.bridges.lock().unwrap().get(bridge_id).map(|bridge| bridge.client_id.clone());
            let Some(client_id) = client_id else {
                let _ = reply.send(error_reply(request_id, "bridge is no longer registered"));
                return;
            };
            run_bridge_manifest(hub, &client_id, manifest_request_id, tasks, reply);
            let _ = reply.send(json!({"request_id": request_id, "type": "bridge_message_accepted"}).to_string());
        }
        ClientToBeam::AgentIdentities { keys } => match hub.agent.set_identities(bridge_id, &keys) {
            Ok(()) => {
                let _ = reply.send(json!({"request_id": request_id, "type": "bridge_message_accepted"}).to_string());
            }
            Err(err) => {
                let _ = reply.send(error_reply(request_id, &err));
            }
        },
        ClientToBeam::AgentResponse { request_id: agent_request_id, message, note } => {
            if let Some(note) = note {
                eprintln!("ssh-agent: request {agent_request_id}: {note}");
            }
            match hub.agent.complete(bridge_id, &agent_request_id, &message) {
                Ok(()) => {
                    let _ =
                        reply.send(json!({"request_id": request_id, "type": "bridge_message_accepted"}).to_string());
                }
                Err(err) => {
                    let _ = reply.send(error_reply(request_id, &err));
                }
            }
        }
    }
}

fn list_clients(hub: &Hub, request_id: Option<&str>, reply: &Sender<String>) {
    let clients: Vec<Value> = hub
        .bridges
        .lock()
        .unwrap()
        .values()
        .map(|bridge| {
            json!({
                "client_id": bridge.client_id,
                "bridge_id": bridge.bridge_id,
                "session_count": bridge.session_tokens.len(),
                "status": if bridge.ready { "connected" } else { "synchronizing" },
            })
        })
        .collect();
    let _ = reply.send(json!({"request_id": request_id, "type": "clients", "clients": clients}).to_string());
}

fn list_client_state(hub: &Hub, request_id: Option<&str>, reply: &Sender<String>) {
    let requests = hub
        .client_requests
        .lock()
        .unwrap()
        .values()
        .cloned()
        .collect::<Vec<_>>();
    let action_requests = hub
        .client_action_requests
        .lock()
        .unwrap()
        .values()
        .cloned()
        .collect::<Vec<_>>();
    let services = hub
        .client_services
        .lock()
        .unwrap()
        .values()
        .cloned()
        .collect::<Vec<_>>();
    let _ = reply.send(
        json!({
            "request_id": request_id,
            "type": "client_state",
            "requests": requests,
            "action_requests": action_requests,
            "services": services,
        })
        .to_string(),
    );
}

fn request_service(
    hub: &Hub,
    request_id: Option<&str>,
    request: &Value,
    reply: &Sender<String>,
    context: &mut ConnectionContext,
) {
    let session_token = request.get("session_token").and_then(Value::as_str).unwrap_or_default();
    let client_hint = request.get("client_id").and_then(Value::as_str);
    let service_request: ServiceRequest =
        match serde_json::from_value(request.get("request").cloned().unwrap_or(Value::Null)) {
            Ok(request) => request,
            Err(err) => {
                let _ = reply.send(error_reply(request_id, &format!("invalid service request: {err}")));
                return;
            }
        };
    if session_token.is_empty() {
        let _ = reply.send(error_reply(
            request_id,
            "no managed Beams client is associated with this shell; reconnect with `beams ssh`, then retry the request",
        ));
        return;
    }
    if service_request.request_id.is_empty() {
        let _ = reply.send(error_reply(request_id, "service request_id must not be empty"));
        return;
    }
    match &service_request.kind {
        ServiceRequestKind::LocalPortForward { remote_host, remote_port, .. }
            if remote_host.is_empty() || *remote_port == 0 =>
        {
            let _ =
                reply.send(error_reply(request_id, "remote_host must not be empty and remote_port must be non-zero"));
            return;
        }
        ServiceRequestKind::FileCopy { remote_path, .. }
        | ServiceRequestKind::FolderSync { remote_path, .. }
            if remote_path.is_empty() =>
        {
            let _ = reply.send(error_reply(request_id, "remote_path must not be empty"));
            return;
        }
        ServiceRequestKind::FileFetch { sources, .. } => {
            if let Err(message) = validate_fetch_sources(sources) {
                let _ = reply.send(error_reply(request_id, &message));
                return;
            }
        }
        _ => {}
    }
    // The workstation bridge drops a frame over `MAX_FRAME_BYTES` as a
    // protocol error, so never route one (with headroom for the envelope).
    let encoded = serde_json::to_vec(&BeamToClient::RequestSubmitted { request: service_request.clone() })
        .map(|bytes| bytes.len())
        .unwrap_or(usize::MAX);
    if encoded > MAX_FRAME_BYTES - 1024 {
        let _ = reply.send(error_reply(
            request_id,
            "the request is too large to send to the workstation; fetch fewer files at a time (or their parent directory)",
        ));
        return;
    }
    let needs_fetch = matches!(service_request.kind, ServiceRequestKind::FileFetch { .. });

    let matching: Vec<BridgeRegistration> = hub
        .bridges
        .lock()
        .unwrap()
        .values()
        .filter(|bridge| bridge.ready && bridge.session_tokens.contains(session_token))
        .cloned()
        .collect();
    let bridge = match matching.as_slice() {
        [bridge] if needs_fetch && !bridge.capabilities.contains(CAPABILITY_FILE_FETCH) => {
            let _ = reply.send(error_reply(
                request_id,
                "the beams client on your workstation is too old for `beams fetch`; update it, run `beams daemon restart` there, then reconnect with `beams ssh`",
            ));
            return;
        }
        [bridge] => bridge.clone(),
        [] => {
            let client_connected = client_hint.is_some_and(|client_id| {
                hub.bridges.lock().unwrap().values().any(|bridge| bridge.ready && bridge.client_id == client_id)
            });
            let message = if client_connected {
                format!(
                    "the managed Beams session token for client {} is stale; reconnect with `beams ssh`, then retry the request",
                    client_hint.unwrap_or("?")
                )
            } else {
                "no managed Beams client is associated with this shell; reconnect with `beams ssh`, then retry the request".to_string()
            };
            let _ = reply.send(error_reply(request_id, &message));
            return;
        }
        _ => {
            let _ = reply.send(error_reply(
                request_id,
                "managed session token is registered by more than one client bridge; reconnect with `beams ssh`",
            ));
            return;
        }
    };

    {
        let mut requests = hub.client_requests.lock().unwrap();
        if requests.contains_key(&service_request.request_id) {
            let _ = reply.send(error_reply(
                request_id,
                &format!("service request {:?} already exists", service_request.request_id),
            ));
            return;
        }
        requests.insert(
            service_request.request_id.clone(),
            ClientRequestProjection {
                request: service_request.clone(),
                owner_client_id: bridge.client_id.clone(),
                bridge_id: bridge.bridge_id.clone(),
                status: RequestStatus::PendingApproval,
                message: None,
            },
        );
    }

    if !send_bridge_event(&bridge.sender, &BeamToClient::RequestSubmitted { request: service_request.clone() }) {
        hub.client_requests.lock().unwrap().remove(&service_request.request_id);
        let _ =
            reply.send(error_reply(request_id, "the managed Beams client disconnected while submitting the request"));
        return;
    }

    if needs_fetch {
        context.tied_requests.push(service_request.request_id.clone());
    }
    let projection = hub
        .client_requests
        .lock()
        .unwrap()
        .get(&service_request.request_id)
        .cloned()
        .expect("request was just inserted");
    hub.broadcast(&json!({"type": "service_request_status", "request": projection}).to_string());
    let _ = reply.send(
        json!({
            "request_id": request_id,
            "type": "service_request_accepted",
            "service_request_id": service_request.request_id,
            "client_id": bridge.client_id,
        })
        .to_string(),
    );
}

fn validate_fetch_sources(sources: &[FetchSource]) -> Result<(), String> {
    if sources.is_empty() {
        return Err("a fetch needs at least one source".to_string());
    }
    if sources.len() > MAX_FETCH_SOURCES {
        return Err(format!("a fetch can name at most {MAX_FETCH_SOURCES} sources"));
    }
    let mut names = HashSet::new();
    for source in sources {
        if !source.path.starts_with('/') {
            return Err(format!("fetch source {:?} must be an absolute path", source.path));
        }
        // Every source lands in the destination under its own name.
        let name = source.path.trim_end_matches('/').rsplit('/').next().unwrap_or_default();
        if name.is_empty() || name == "." || name == ".." {
            return Err(format!("fetch source {:?} has no file name", source.path));
        }
        if !names.insert(name.to_string()) {
            return Err(format!("two fetch sources are both named {name:?}"));
        }
    }
    Ok(())
}

fn request_service_action(
    hub: &Hub,
    request_id: Option<&str>,
    request: &Value,
    reply: &Sender<String>,
) {
    let session_token = request
        .get("session_token")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let client_hint = request.get("client_id").and_then(Value::as_str);
    let action_request: ServiceActionRequest = match serde_json::from_value(
        request
            .get("request")
            .cloned()
            .unwrap_or(Value::Null),
    ) {
        Ok(request) => request,
        Err(err) => {
            let _ = reply.send(error_reply(
                request_id,
                &format!("invalid service action request: {err}"),
            ));
            return;
        }
    };
    if session_token.is_empty() {
        let _ = reply.send(error_reply(
            request_id,
            "no managed Beams client is associated with this shell; reconnect with `beams ssh`, then retry the request",
        ));
        return;
    }
    if action_request.request_id.is_empty() || action_request.service_id.is_empty() {
        let _ = reply.send(error_reply(
            request_id,
            "service action request_id and service_id must not be empty",
        ));
        return;
    }
    let owner_client_id = match hub
        .client_services
        .lock()
        .unwrap()
        .get(&action_request.service_id)
        .map(|service| service.owner_client_id.clone())
    {
        Some(owner) => owner,
        None => {
            let _ = reply.send(error_reply(
                request_id,
                &format!("unknown client service {:?}", action_request.service_id),
            ));
            return;
        }
    };
    let matching = hub
        .bridges
        .lock()
        .unwrap()
        .values()
        .filter(|bridge| {
            bridge.ready
                && bridge.client_id == owner_client_id
                && bridge.session_tokens.contains(session_token)
        })
        .cloned()
        .collect::<Vec<_>>();
    let bridge = match matching.as_slice() {
        [bridge] => bridge.clone(),
        [] => {
            let message = if client_hint == Some(owner_client_id.as_str()) {
                format!(
                    "the managed Beams session token for client {owner_client_id} is stale; reconnect with `beams ssh`, then retry the request"
                )
            } else {
                format!(
                    "service {} is owned by client {}; use a managed shell from that client",
                    action_request.service_id, owner_client_id
                )
            };
            let _ = reply.send(error_reply(request_id, &message));
            return;
        }
        _ => {
            let _ = reply.send(error_reply(
                request_id,
                "managed session token is registered by more than one client bridge; reconnect with `beams ssh`",
            ));
            return;
        }
    };

    {
        let mut requests = hub.client_action_requests.lock().unwrap();
        if requests.contains_key(&action_request.request_id) {
            let _ = reply.send(error_reply(
                request_id,
                &format!(
                    "service action request {:?} already exists",
                    action_request.request_id
                ),
            ));
            return;
        }
        requests.insert(
            action_request.request_id.clone(),
            ClientActionProjection {
                request: action_request.clone(),
                owner_client_id: owner_client_id.clone(),
                bridge_id: bridge.bridge_id.clone(),
                status: RequestStatus::PendingApproval,
                message: None,
            },
        );
    }
    if !send_bridge_event(
        &bridge.sender,
        &BeamToClient::ServiceActionRequested {
            request: action_request.clone(),
        },
    ) {
        hub.client_action_requests
            .lock()
            .unwrap()
            .remove(&action_request.request_id);
        let _ = reply.send(error_reply(
            request_id,
            "the owning Beams client disconnected while submitting the action",
        ));
        return;
    }
    let projection = hub
        .client_action_requests
        .lock()
        .unwrap()
        .get(&action_request.request_id)
        .cloned()
        .expect("action request was just inserted");
    hub.broadcast(
        &json!({"type": "service_action_request_status", "request": projection}).to_string(),
    );
    let _ = reply.send(
        json!({
            "request_id": request_id,
            "type": "service_action_request_accepted",
            "service_action_request_id": action_request.request_id,
            "client_id": owner_client_id,
        })
        .to_string(),
    );
}

fn send_bridge_event(sender: &Sender<String>, message: &BeamToClient) -> bool {
    sender.send(json!({"type": "client_bridge_event", "message": message}).to_string()).is_ok()
}

fn run_task(hub: &Arc<Hub>, request_id: Option<&str>, name: &str, params: Value, reply: &Sender<String>) {
    let task = match tasks::instantiate(name, params) {
        Ok(task) => task,
        Err(err) => {
            let _ = reply.send(error_reply(request_id, &err.to_string()));
            return;
        }
    };
    let task_id = task.id();
    let _ = reply.send(json!({"request_id": request_id, "type": "accepted", "task_id": task_id}).to_string());

    // Not resolved through `executor::run_with`'s graph: a standalone
    // task's `requires` is deliberately not checked against anything
    // (see `tasks`' module doc comment), so it would never resolve on
    // its own. Still takes the shared resource locks, though.
    let hub = Arc::clone(hub);
    thread::spawn(move || {
        let on_event = |event: executor::Event| hub.broadcast(&task_event_line(event, None));
        hub.resources.with_held(task.as_ref(), || executor::run_one(task.as_ref(), &on_event));
    });
}

#[derive(Deserialize)]
struct ManifestEntry {
    template: String,
    #[serde(default)]
    params: Value,
}

/// Instantiates every entry, failing (with entry-indexed context) on the
/// first one that doesn't name a known template or match its params -
/// before anything has run, same as an unresolvable manifest graph below.
fn build_manifest(entries: Vec<ManifestEntry>) -> Result<Vec<Box<dyn Task>>, String> {
    entries
        .into_iter()
        .enumerate()
        .map(|(i, entry)| {
            tasks::instantiate(&entry.template, entry.params)
                .map_err(|err| format!("manifest entry {i} ({:?}): {err}", entry.template))
        })
        .collect()
}

/// `RunManifest` from a workstation bridge: the same as a local
/// `run_manifest`, answered with typed bridge messages instead. While it
/// runs, its tasks' SSH key use routes to `client_id` (`route_agent`).
fn run_bridge_manifest(
    hub: &Arc<Hub>,
    client_id: &str,
    request_id: String,
    tasks: Vec<ManifestTask>,
    reply: &Sender<String>,
) {
    let entries = tasks
        .into_iter()
        .map(|task| ManifestEntry {
            template: task.template,
            params: Value::Object(
                task.params
                    .into_iter()
                    .map(|(name, param)| {
                        let value = match param {
                            ManifestParam::Bool(b) => Value::from(b),
                            ManifestParam::Int(i) => Value::from(i),
                            ManifestParam::Str(s) => Value::from(s),
                        };
                        (name, value)
                    })
                    .collect(),
            ),
        })
        .collect();
    let finished_reply = reply.clone();
    let finished_hub = Arc::clone(hub);
    let started = start_manifest(
        hub,
        entries,
        |manifest_id, plan| {
            eprintln!("daemon: workstation startup manifest accepted as {manifest_id}");
            // Before the worker starts (`start_manifest` runs this first).
            hub.manifest_owners.lock().unwrap().insert(manifest_id.to_string(), client_id.to_string());
            send_bridge_event(reply, &BeamToClient::ManifestAccepted {
                request_id: request_id.clone(),
                manifest_id: manifest_id.to_string(),
                task_ids: plan.iter().map(|planned| planned.task_id.clone()).collect(),
            });
        },
        move |manifest_id, ok| {
            finished_hub.manifest_owners.lock().unwrap().remove(manifest_id);
            send_bridge_event(&finished_reply, &BeamToClient::ManifestFinished { manifest_id: manifest_id.to_string(), ok });
        },
    );
    if let Err(message) = started {
        send_bridge_event(reply, &BeamToClient::ManifestRejected { request_id, message });
    }
}

fn run_manifest(hub: &Arc<Hub>, request_id: Option<&str>, tasks_value: Value, reply: &Sender<String>) {
    let entries: Vec<ManifestEntry> = match serde_json::from_value(tasks_value) {
        Ok(entries) => entries,
        Err(err) => {
            let _ = reply.send(error_reply(request_id, &format!("invalid manifest: {err}")));
            return;
        }
    };
    if let Err(message) = start_manifest(
        hub,
        entries,
        |manifest_id, plan| {
            let _ = reply.send(
                json!({
                    "request_id": request_id,
                    "type": "accepted",
                    "manifest_id": manifest_id,
                    "tasks": plan,
                })
                .to_string(),
            );
        },
        |_, _| {},
    ) {
        let _ = reply.send(error_reply(request_id, &message));
    }
}

/// Validates a manifest, synchronously queues acceptance, then starts its
/// worker. No start/progress/completion can overtake that acceptance.
/// `on_finish(manifest_id, ok)` runs once every task is done.
fn start_manifest(
    hub: &Arc<Hub>,
    entries: Vec<ManifestEntry>,
    on_accept: impl FnOnce(&str, &[executor::PlannedTask]),
    on_finish: impl FnOnce(&str, bool) + Send + 'static,
) -> Result<(), String> {
    let instances = build_manifest(entries)?;

    // Resolved against *this manifest's own* instances only - see the
    // module doc comment on why a dependency on an earlier, separate
    // run's capability doesn't work (and what to do instead). Just for
    // the `accepted` reply's task/dependency list - `executor::run` (which
    // actually runs these) resolves the same graph again itself, per
    // `executor::plan`'s own doc comment.
    let plan = executor::plan(&instances).map_err(|err| err.to_string())?;

    let manifest_id = hub.next_manifest_id();
    on_accept(&manifest_id, &plan);
    let hub = Arc::clone(hub);
    let id = manifest_id;
    thread::spawn(move || {
        hub.broadcast(&json!({"type": "manifest_status", "manifest_id": id, "status": "start"}).to_string());
        let on_event = |event: executor::Event| hub.broadcast(&task_event_line(event, Some(&id)));
        // Already validated above (same instances) - the only way this
        // fails is a graph error, which `plan` already ruled out.
        let ok = executor::run_tagged(instances, &hub.resources, Some(&id), &on_event)
            .expect("graph already validated above");
        let status = if ok { "ok" } else { "error" };
        hub.broadcast(&json!({"type": "manifest_status", "manifest_id": id, "status": status}).to_string());
        on_finish(&id, ok);
    });
    Ok(())
}

/// The `task_status` broadcast for one executor event - shared by
/// `run_task` and `run_manifest` so the shape can't drift between them.
pub(crate) fn task_event_line(event: executor::Event, manifest_id: Option<&str>) -> String {
    let (task_id, status, message) = match event {
        executor::Event::Start { task_id } => (task_id, "start", None),
        executor::Event::Progress { task_id, message } => (task_id, "progress", Some(message)),
        executor::Event::Ok { task_id, message } => (task_id, "ok", Some(message)),
        executor::Event::Skip { task_id } => (task_id, "skip", None),
        executor::Event::Error { task_id, message } => (task_id, "error", Some(message)),
        executor::Event::Blocked { task_id, message } => (task_id, "blocked", Some(message)),
    };
    task_status(&task_id, status, message.as_deref(), manifest_id)
}

fn task_status(
    task_id: &str,
    status: &str,
    message: Option<&str>,
    manifest_id: Option<&str>,
) -> String {
    json!({
        "type": "task_status",
        "task_id": task_id,
        "status": status,
        "message": message,
        "manifest_id": manifest_id,
    })
    .to_string()
}

fn error_reply(request_id: Option<&str>, message: &str) -> String {
    json!({"request_id": request_id, "type": "error", "message": message}).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;
    use std::time::Duration;

    /// Connects an in-memory pair, serves one end on a background
    /// thread, and returns the other end for the test to drive - no
    /// socket file, no infinite `accept()` loop to manage, per
    /// `handle_connection`'s doc comment. The returned `BufReader` must
    /// be reused across every `recv` call on this connection, not
    /// rebuilt per call - rebuilding it would silently drop whatever
    /// extra bytes (quite possibly a whole extra line or two, once
    /// several broadcasts land in one read()) its predecessor had
    /// already buffered but not yet handed back via `read_line`.
    fn connect() -> (UnixStream, BufReader<UnixStream>, thread::JoinHandle<()>) {
        connect_to(Arc::new(Hub::default()))
    }

    #[test]
    fn launch_ops_reply_with_their_request_id() {
        let root = std::env::temp_dir().join(format!("daemon-launch-ops-{}", uuid::Uuid::new_v4()));
        let hub = Arc::new(Hub {
            launches: crate::launch::daemon::Launches::new(root.clone(), root.join("home")),
            ..Hub::default()
        });
        let (mut client, mut reader, _server) = connect_to(hub);
        send(&mut client, r#"{"request_id":"s1","op":"launch_snapshot"}"#);
        let empty = recv_type(&mut reader, "launch_snapshot");
        assert_eq!(empty["request_id"], "s1");
        assert_eq!(empty["launches"], json!([]));
        send(&mut client, r#"{"request_id":"s2","op":"launch_submit","launch":"relative.json","staging":"/x"}"#);
        let refused = recv_type(&mut reader, "error");
        assert_eq!(refused["request_id"], "s2");
        assert!(refused["message"].as_str().unwrap().contains("absolute"), "{refused}");
        send(&mut client, r#"{"request_id":"s3","op":"launch_signal","launch_id":"nope","condition":"interface-ready"}"#);
        assert_eq!(recv_type(&mut reader, "error")["request_id"], "s3");
        let _ = fs::remove_dir_all(root);
    }

    fn agent_bridge(hub: &Hub, client_id: &str, bridge_id: &str, token: &str) {
        let (sender, _) = mpsc::channel();
        hub.bridges.lock().unwrap().insert(
            bridge_id.to_string(),
            BridgeRegistration {
                client_id: client_id.to_string(),
                bridge_id: bridge_id.to_string(),
                session_tokens: [token.to_string()].into(),
                ready: true,
                sender,
                capabilities: [CAPABILITY_SSH_AGENT.to_string()].into(),
            },
        );
    }

    #[test]
    fn agent_requests_route_by_session_then_running_manifest_owner() {
        let hub = Hub::default();
        agent_bridge(&hub, "client-a", "bridge-a", "token-a");
        // One client: everything goes to it, tagged or not.
        assert_eq!(hub.route_agent(None, None).unwrap(), "bridge-a");

        agent_bridge(&hub, "client-b", "bridge-b", "token-b");
        assert!(hub.route_agent(None, None).is_err(), "two clients and no hint is ambiguous");
        assert_eq!(hub.route_agent(Some("token-b"), None).unwrap(), "bridge-b");

        let run = "manifest-4/clone-git-repo:dest=/home/beams/x";
        assert!(hub.route_agent(None, Some(run)).is_err(), "not a running manifest");
        hub.manifest_owners.lock().unwrap().insert("manifest-4".into(), "client-b".into());
        assert_eq!(hub.route_agent(None, Some(run)).unwrap(), "bridge-b");
        // A shell's own session still wins over a claimed run tag.
        assert_eq!(hub.route_agent(Some("token-a"), Some(run)).unwrap(), "bridge-a");

        // The owner went away: fall back to the only remaining client.
        hub.bridges.lock().unwrap().remove("bridge-b");
        assert_eq!(hub.route_agent(None, Some(run)).unwrap(), "bridge-a");
    }

    fn connect_to(hub: Arc<Hub>) -> (UnixStream, BufReader<UnixStream>, thread::JoinHandle<()>) {
        let (server_end, client_end) = UnixStream::pair().expect("creating socket pair");
        client_end.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let read_half = client_end.try_clone().expect("cloning client end for reading");
        let handle = thread::spawn(move || {
            let _ = handle_connection(&hub, server_end);
        });
        (client_end, BufReader::new(read_half), handle)
    }

    fn send(stream: &mut UnixStream, line: &str) {
        writeln!(stream, "{line}").unwrap();
    }

    fn recv(reader: &mut BufReader<UnixStream>) -> Value {
        let mut line = String::new();
        reader.read_line(&mut line).expect("reading reply");
        serde_json::from_str(line.trim()).expect("reply should be valid JSON")
    }

    fn recv_type(reader: &mut BufReader<UnixStream>, expected: &str) -> Value {
        loop {
            let value = recv(reader);
            if value.get("type").and_then(Value::as_str) == Some(expected) {
                return value;
            }
        }
    }

    fn register_bridge(
        hub: Arc<Hub>,
        client_id: &str,
        bridge_id: &str,
        token: &str,
    ) -> (UnixStream, BufReader<UnixStream>, thread::JoinHandle<()>) {
        register_bridge_with(hub, client_id, bridge_id, token, true)
    }

    /// `current: false` registers the way a workstation that predates
    /// capabilities does.
    fn register_bridge_with(
        hub: Arc<Hub>,
        client_id: &str,
        bridge_id: &str,
        token: &str,
        current: bool,
    ) -> (UnixStream, BufReader<UnixStream>, thread::JoinHandle<()>) {
        let (mut stream, mut reader, handle) = connect_to(hub);
        let mut hello = ClientToBeam::hello(client_id.to_string(), bridge_id.to_string(), vec![token.to_string()]);
        if !current {
            if let ClientToBeam::Hello { capabilities, .. } = &mut hello {
                capabilities.clear();
            }
        }
        send(
            &mut stream,
            &json!({
                "request_id": "register",
                "op": "register_client_bridge",
                "message": hello,
            })
            .to_string(),
        );
        let registered = recv_type(&mut reader, "bridge_registered");
        assert_eq!(registered["request_id"], "register");
        send(
            &mut stream,
            &json!({
                "request_id": "ready",
                "op": "client_bridge_message",
                "message": ClientToBeam::Ready,
            })
            .to_string(),
        );
        let ready = recv_type(&mut reader, "bridge_message_accepted");
        assert_eq!(ready["request_id"], "ready");
        (stream, reader, handle)
    }

    /// Like `recv`, but discards any number of `task_status` events with
    /// `status: "progress"` first - `clone-git-repo` (via `git clone
    /// --progress`) now emits zero or more of these between its `start`
    /// and its terminal event, and these tests only care about the
    /// terminal one, same as before progress reporting existed.
    fn recv_skipping_progress(reader: &mut BufReader<UnixStream>) -> Value {
        loop {
            let value = recv(reader);
            if value["type"] == "task_status" && value["status"] == "progress" {
                continue;
            }
            return value;
        }
    }

    #[test]
    fn list_tasks_returns_every_template_name_and_its_param_schema() {
        let (mut client, mut reader, _handle) = connect();
        send(&mut client, r#"{"request_id":"1","op":"list_tasks"}"#);
        let reply = recv(&mut reader);
        assert_eq!(reply["request_id"], "1");
        assert_eq!(reply["type"], "tasks");
        assert_eq!(reply["tasks"], serde_json::to_value(tasks::templates()).unwrap());
    }

    fn git(dir: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("running git {args:?}: {e}"));
        assert!(output.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&output.stderr));
    }

    /// A minimal local repo with one commit - usable as a `clone-git-repo`
    /// source with no network access.
    fn make_source_repo(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        git(dir, &["init", "-q", "-b", "main"]);
        git(dir, &["config", "user.email", "t@example.com"]);
        git(dir, &["config", "user.name", "t"]);
        std::fs::write(dir.join("f"), "x").unwrap();
        git(dir, &["add", "f"]);
        git(dir, &["commit", "-q", "-m", "x"]);
    }

    /// Same as `make_source_repo`, plus a second commit stashed under
    /// `refs/pull/<pr>/head` - the shape GitHub exposes a PR's head as,
    /// so `checkout-pr-worktree` has something to fetch without network
    /// access (same trick as `tasks::checkout_pr_worktree`'s own tests).
    fn make_source_repo_with_pr(dir: &Path, pr: u32) {
        make_source_repo(dir);
        git(dir, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(dir.join("f"), "pr change").unwrap();
        git(dir, &["commit", "-q", "-am", "pr change"]);
        git(dir, &["update-ref", &format!("refs/pull/{pr}/head"), "feature"]);
        git(dir, &["checkout", "-q", "main"]);
        git(dir, &["branch", "-D", "feature"]);
    }

    fn scratch_dir(label: &str) -> std::path::PathBuf {
        let tmp = std::env::temp_dir().join(format!("daemon-test-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        tmp
    }

    #[test]
    fn run_task_accepts_then_broadcasts_terminal_status_to_the_requester() {
        let tmp = scratch_dir("run-task");
        let source = tmp.join("source");
        let dest = tmp.join("dest");
        make_source_repo(&source);

        let (mut client, mut reader, _handle) = connect();
        let request = serde_json::json!({
            "request_id": "7",
            "op": "run_task",
            "task": "clone-git-repo",
            "params": {"repo": source.display().to_string(), "dest": dest.display().to_string()},
        });
        send(&mut client, &request.to_string());

        let accepted = recv(&mut reader);
        assert_eq!(accepted["type"], "accepted");
        assert_eq!(accepted["request_id"], "7");

        let start = recv(&mut reader);
        assert_eq!(start["type"], "task_status");
        assert_eq!(start["status"], "start");

        let done = recv_skipping_progress(&mut reader);
        assert_eq!(done["type"], "task_status");
        assert_eq!(done["status"], "ok");
        assert!(dest.join(".git").exists());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_workstation_startup_manifest_runs_and_reports_back_over_the_bridge() {
        let tmp = scratch_dir("bridge-manifest");
        let source = tmp.join("source");
        let dest = tmp.join("dest");
        make_source_repo_with_pr(&source, 1);
        let hub = Arc::new(Hub::default());
        let (mut bridge, mut reader, _handle) = register_bridge(Arc::clone(&hub), "client-a", "bridge-a", "token-a");

        let mut params = std::collections::BTreeMap::new();
        params.insert("repo".to_string(), ManifestParam::Str(source.display().to_string()));
        params.insert("dest".to_string(), ManifestParam::Str(dest.display().to_string()));
        send(
            &mut bridge,
            &json!({
                "request_id": "m1",
                "op": "client_bridge_message",
                "message": ClientToBeam::RunManifest {
                    request_id: "startup-1".into(),
                    tasks: vec![ManifestTask { template: "clone-git-repo".into(), params }],
                },
            })
            .to_string(),
        );
        let bridge_message = |reader: &mut BufReader<UnixStream>| -> BeamToClient {
            loop {
                let value = recv(reader);
                if value["type"] == "client_bridge_event" {
                    return serde_json::from_value(value["message"].clone()).unwrap();
                }
            }
        };
        let BeamToClient::ManifestAccepted { request_id, manifest_id, task_ids } = bridge_message(&mut reader) else {
            panic!("expected ManifestAccepted");
        };
        assert_eq!(request_id, "startup-1");
        assert_eq!(task_ids.len(), 1);
        assert!(task_ids[0].starts_with("clone-git-repo:"));
        assert_eq!(
            bridge_message(&mut reader),
            BeamToClient::ManifestFinished { manifest_id, ok: true }
        );
        assert!(dest.join(".git").exists());

        // An unknown template is rejected up front.
        send(
            &mut bridge,
            &json!({
                "request_id": "m2",
                "op": "client_bridge_message",
                "message": ClientToBeam::RunManifest {
                    request_id: "startup-2".into(),
                    tasks: vec![ManifestTask { template: "no-such-template".into(), params: Default::default() }],
                },
            })
            .to_string(),
        );
        assert!(matches!(
            bridge_message(&mut reader),
            BeamToClient::ManifestRejected { request_id, .. } if request_id == "startup-2"
        ));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn manifest_acceptance_is_queued_before_even_an_empty_manifest_starts() {
        let hub = Arc::new(Hub::default());
        let (_, reply, receiver) = hub.register();
        start_manifest(
            &hub,
            vec![],
            |id, plan| {
                // A slow acceptance path must not let the worker overtake it.
                thread::sleep(Duration::from_millis(25));
                assert!(receiver.try_recv().is_err());
                assert!(plan.is_empty());
                reply.send(json!({"type": "accepted", "manifest_id": id}).to_string()).unwrap();
            },
            |_, _| {},
        ).unwrap();
        let recv = || -> Value {
            serde_json::from_str(&receiver.recv_timeout(Duration::from_secs(2)).unwrap()).unwrap()
        };
        let accepted = recv();
        assert_eq!(accepted["type"], "accepted");
        for status in ["start", "ok"] {
            let event = recv();
            assert_eq!(event["type"], "manifest_status");
            assert_eq!(event["status"], status);
            assert_eq!(event["manifest_id"], accepted["manifest_id"]);
        }
    }

    #[test]
    fn run_manifest_resolves_dependency_order_and_runs_both_tasks() {
        let tmp = scratch_dir("manifest-order");
        let source = tmp.join("source");
        let base = tmp.join("base");
        let worktree = tmp.join("worktree");
        make_source_repo_with_pr(&source, 42);

        let (mut client, mut reader, _handle) = connect();
        // Listed out of dependency order on purpose - checkout-pr-worktree
        // requires clone-git-repo's capability, so the resolver must still
        // run clone-git-repo first.
        let request = serde_json::json!({
            "request_id": "1",
            "op": "run_manifest",
            "tasks": [
                {"template": "checkout-pr-worktree", "params": {
                    "base": base.display().to_string(), "pr": 42, "dest": worktree.display().to_string(),
                }},
                {"template": "clone-git-repo", "params": {
                    "repo": source.display().to_string(), "dest": base.display().to_string(),
                }},
            ],
        });
        send(&mut client, &request.to_string());

        let accepted = recv(&mut reader);
        assert_eq!(accepted["type"], "accepted");
        let planned = accepted["tasks"].as_array().unwrap();
        assert_eq!(planned.len(), 2, "{planned:?}");
        let clone_id = planned[0]["task_id"].as_str().unwrap();
        assert!(clone_id.starts_with("clone-git-repo:"), "clone-git-repo should be planned first, got {planned:?}");
        assert!(planned[0]["depends_on"].as_array().unwrap().is_empty());
        assert!(planned[1]["task_id"].as_str().unwrap().starts_with("checkout-pr-worktree:"));
        assert_eq!(planned[1]["depends_on"], serde_json::json!([clone_id]));
        let manifest_id = accepted["manifest_id"].as_str().unwrap().to_string();

        let manifest_start = recv(&mut reader);
        assert_eq!(manifest_start["type"], "manifest_status");
        assert_eq!(manifest_start["status"], "start");
        assert_eq!(manifest_start["manifest_id"], manifest_id);

        for expected_task in ["clone-git-repo", "checkout-pr-worktree"] {
            let start = recv(&mut reader);
            assert_eq!(start["type"], "task_status");
            assert_eq!(start["status"], "start");
            assert_eq!(start["manifest_id"], manifest_id);
            assert!(start["task_id"].as_str().unwrap().starts_with(expected_task));

            let done = recv_skipping_progress(&mut reader);
            assert_eq!(done["type"], "task_status");
            assert_eq!(done["status"], "ok");
            assert_eq!(done["manifest_id"], manifest_id);
        }

        let manifest_done = recv(&mut reader);
        assert_eq!(manifest_done["type"], "manifest_status");
        assert_eq!(manifest_done["status"], "ok");

        assert_eq!(std::fs::read_to_string(worktree.join("f")).unwrap(), "pr change");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn run_manifest_rejects_a_dependency_not_satisfied_within_the_same_manifest() {
        let tmp = scratch_dir("manifest-missing-dep");
        let base = tmp.join("base"); // deliberately never cloned

        let (mut client, mut reader, _handle) = connect();
        let request = serde_json::json!({
            "request_id": "2",
            "op": "run_manifest",
            "tasks": [
                {"template": "checkout-pr-worktree", "params": {
                    "base": base.display().to_string(), "pr": 1, "dest": tmp.join("worktree").display().to_string(),
                }},
            ],
        });
        send(&mut client, &request.to_string());

        let reply = recv(&mut reader);
        assert_eq!(reply["type"], "error");
        assert_eq!(reply["request_id"], "2");
        assert!(reply["message"].as_str().unwrap().contains("repo-cloned"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn run_manifest_reports_a_failing_task_and_the_manifest_as_failed() {
        let tmp = scratch_dir("manifest-failure");
        let source = tmp.join("source");
        let base = tmp.join("base");
        make_source_repo(&source); // no PR ref - checkout-pr-worktree will fail to fetch it

        let (mut client, mut reader, _handle) = connect();
        let request = serde_json::json!({
            "request_id": "3",
            "op": "run_manifest",
            "tasks": [
                {"template": "clone-git-repo", "params": {
                    "repo": source.display().to_string(), "dest": base.display().to_string(),
                }},
                {"template": "checkout-pr-worktree", "params": {
                    "base": base.display().to_string(), "pr": 999, "dest": tmp.join("worktree").display().to_string(),
                }},
            ],
        });
        send(&mut client, &request.to_string());

        let accepted = recv(&mut reader);
        assert_eq!(accepted["type"], "accepted");

        let manifest_start = recv(&mut reader);
        assert_eq!(manifest_start["status"], "start");

        let clone_start = recv(&mut reader);
        assert_eq!(clone_start["status"], "start");
        let clone_done = recv_skipping_progress(&mut reader);
        assert_eq!(clone_done["status"], "ok");

        let checkout_start = recv(&mut reader);
        assert_eq!(checkout_start["status"], "start");
        let checkout_failed = recv(&mut reader);
        assert_eq!(checkout_failed["type"], "task_status");
        assert_eq!(checkout_failed["status"], "error");

        let manifest_failed = recv(&mut reader);
        assert_eq!(manifest_failed["type"], "manifest_status");
        assert_eq!(manifest_failed["status"], "error");

        assert!(!tmp.join("worktree").join(".git").exists(), "failed checkout shouldn't leave a worktree behind");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn unknown_op_is_a_direct_error_reply() {
        let (mut client, mut reader, _handle) = connect();
        send(&mut client, r#"{"request_id":"9","op":"not_a_real_op"}"#);
        let reply = recv(&mut reader);
        assert_eq!(reply["type"], "error");
        assert_eq!(reply["request_id"], "9");
    }

    fn fetch_request(request_id: &str) -> ServiceRequest {
        ServiceRequest {
            request_id: request_id.to_string(),
            kind: ServiceRequestKind::FileFetch {
                sources: vec![FetchSource { path: "/home/beams/out.log".to_string(), size: 3, is_dir: false }],
                local_path_hint: None,
                overwrite: beams_protocol::OverwritePolicy::Deny,
            },
        }
    }

    fn submit(requester: &mut UnixStream, token: &str, request: &ServiceRequest) {
        send(
            requester,
            &json!({
                "request_id": "submit",
                "op": "request_service",
                "session_token": token,
                "request": request,
            })
            .to_string(),
        );
    }

    #[test]
    fn fetch_requests_need_a_workstation_that_supports_them() {
        let hub = Arc::new(Hub::default());
        let (_bridge, _bridge_reader, _handle) = register_bridge_with(Arc::clone(&hub), "client-a", "bridge-a", "token-a", false);
        let (mut requester, mut reader, _requester_handle) = connect_to(Arc::clone(&hub));
        submit(&mut requester, "token-a", &fetch_request("fetch-1"));
        let error = recv_type(&mut reader, "error");
        assert!(error["message"].as_str().unwrap().contains("too old for `beams fetch`"), "{error}");
        assert!(hub.client_requests.lock().unwrap().is_empty(), "nothing was routed");

        let mut huge = fetch_request("fetch-3");
        if let ServiceRequestKind::FileFetch { sources, .. } = &mut huge.kind {
            *sources = (0..150)
                .map(|index| FetchSource { path: format!("/{}/{index}", "d".repeat(500)), size: 1, is_dir: false })
                .collect();
        }
        submit(&mut requester, "token-a", &huge);
        let error = recv_type(&mut reader, "error");
        assert!(error["message"].as_str().unwrap().contains("too large"), "{error}");

        let mut relative = fetch_request("fetch-2");
        if let ServiceRequestKind::FileFetch { sources, .. } = &mut relative.kind {
            sources[0].path = "out.log".to_string();
        }
        submit(&mut requester, "token-a", &relative);
        let error = recv_type(&mut reader, "error");
        assert!(error["message"].as_str().unwrap().contains("absolute"), "{error}");
    }

    #[test]
    fn a_fetch_is_withdrawn_when_its_requester_disconnects() {
        let hub = Arc::new(Hub::default());
        let (_bridge, mut bridge_reader, _handle) = register_bridge(Arc::clone(&hub), "client-a", "bridge-a", "token-a");
        let (mut requester, mut reader, requester_handle) = connect_to(Arc::clone(&hub));
        submit(&mut requester, "token-a", &fetch_request("fetch-1"));
        recv_type(&mut reader, "service_request_accepted");
        let submitted = recv_type(&mut bridge_reader, "client_bridge_event");
        assert_eq!(submitted["message"]["type"], "request_submitted");

        // Ctrl-C in `beams fetch` closes its connection.
        drop(reader);
        requester.shutdown(std::net::Shutdown::Both).unwrap();
        drop(requester);
        requester_handle.join().unwrap();
        let withdrawn = recv_type(&mut bridge_reader, "client_bridge_event");
        let message: BeamToClient = serde_json::from_value(withdrawn["message"].clone()).unwrap();
        assert_eq!(message, BeamToClient::RequestWithdrawn { request_id: "fetch-1".to_string() });
    }

    #[test]
    fn managed_session_token_routes_a_request_to_exactly_one_bridge() {
        let hub = Arc::new(Hub::default());
        let (_bridge_a, mut bridge_a_reader, _a_handle) =
            register_bridge(Arc::clone(&hub), "client-a", "bridge-a", "token-a");
        let (_bridge_b, mut bridge_b_reader, _b_handle) =
            register_bridge(Arc::clone(&hub), "client-b", "bridge-b", "token-b");
        let (mut requester, mut requester_reader, _requester_handle) = connect_to(Arc::clone(&hub));

        let service_request = ServiceRequest {
            request_id: "service-request-1".to_string(),
            kind: ServiceRequestKind::LocalPortForward {
                remote_host: "127.0.0.1".to_string(),
                remote_port: 8080,
                local_port: Some(18080),
            },
        };
        send(
            &mut requester,
            &json!({
                "request_id": "submit",
                "op": "request_service",
                "session_token": "token-b",
                "client_id": "client-b",
                "request": service_request,
            })
            .to_string(),
        );

        let bridge_event = recv_type(&mut bridge_b_reader, "client_bridge_event");
        let routed: BeamToClient = serde_json::from_value(bridge_event["message"].clone()).unwrap();
        assert!(matches!(
            routed,
            BeamToClient::RequestSubmitted {
                request: ServiceRequest { request_id, .. }
            } if request_id == "service-request-1"
        ));

        let pending = recv_type(&mut requester_reader, "service_request_status");
        assert_eq!(pending["request"]["owner_client_id"], "client-b");
        assert_eq!(pending["request"]["status"], "pending_approval");
        let accepted = recv_type(&mut requester_reader, "service_request_accepted");
        assert_eq!(accepted["client_id"], "client-b");

        bridge_a_reader.get_mut().set_read_timeout(Some(Duration::from_millis(50))).unwrap();
        loop {
            let mut line = String::new();
            match bridge_a_reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    let value: Value = serde_json::from_str(line.trim()).unwrap();
                    assert_ne!(
                        value.get("type").and_then(Value::as_str),
                        Some("client_bridge_event"),
                        "request intended for client-b leaked to client-a"
                    );
                }
                Err(err) if matches!(err.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                    break;
                }
                Err(err) => panic!("reading non-target bridge: {err}"),
            }
        }
    }

    #[test]
    fn service_actions_route_only_to_the_owning_client_and_require_approval() {
        let hub = Arc::new(Hub::default());
        let (_bridge_a, mut bridge_a_reader, _a_handle) =
            register_bridge(Arc::clone(&hub), "client-a", "bridge-a", "token-a");
        let (_bridge_b, mut bridge_b_reader, _b_handle) =
            register_bridge(Arc::clone(&hub), "client-b", "bridge-b", "token-b");
        hub.client_services.lock().unwrap().insert(
            "service-b".to_string(),
            OwnedServiceProjection {
                owner_client_id: "client-b".to_string(),
                service: ServiceProjection {
                    service_id: "service-b".to_string(),
                    request_id: "create-b".to_string(),
                    kind: ServiceRequestKind::LocalPortForward {
                        remote_host: "127.0.0.1".to_string(),
                        remote_port: 8080,
                        local_port: Some(18080),
                    },
                    status: ServiceStatus::Running,
                    message: None,
                    progress: None,
                },
            },
        );
        let (mut requester, mut reader, _handle) = connect_to(Arc::clone(&hub));

        send(
            &mut requester,
            &json!({
                "request_id": "wrong-owner",
                "op": "request_service_action",
                "session_token": "token-a",
                "client_id": "client-a",
                "request": {
                    "request_id": "action-wrong",
                    "service_id": "service-b",
                    "action": "stop"
                }
            })
            .to_string(),
        );
        let error = recv_type(&mut reader, "error");
        assert!(error["message"]
            .as_str()
            .unwrap()
            .contains("owned by client client-b"));

        send(
            &mut requester,
            &json!({
                "request_id": "right-owner",
                "op": "request_service_action",
                "session_token": "token-b",
                "client_id": "client-b",
                "request": {
                    "request_id": "action-right",
                    "service_id": "service-b",
                    "action": "stop"
                }
            })
            .to_string(),
        );
        let event = recv_type(&mut bridge_b_reader, "client_bridge_event");
        assert_eq!(event["message"]["type"], "service_action_requested");
        assert_eq!(event["message"]["request"]["service_id"], "service-b");
        let pending = recv_type(&mut reader, "service_action_request_status");
        assert_eq!(pending["request"]["status"], "pending_approval");
        let accepted = recv_type(&mut reader, "service_action_request_accepted");
        assert_eq!(accepted["client_id"], "client-b");

        bridge_a_reader
            .get_mut()
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        while let Ok(line) = {
            let mut line = String::new();
            bridge_a_reader.read_line(&mut line).map(|_| line)
        } {
            if line.is_empty() {
                break;
            }
            let value: Value = serde_json::from_str(line.trim()).unwrap();
            assert_ne!(
                value.get("type").and_then(Value::as_str),
                Some("client_bridge_event")
            );
        }
    }

    #[test]
    fn bridge_is_not_routable_until_authoritative_snapshots_are_ready() {
        let hub = Arc::new(Hub::default());
        let (mut bridge, mut bridge_reader, _bridge_handle) = connect_to(Arc::clone(&hub));
        send(
            &mut bridge,
            &json!({
                "request_id": "register",
                "op": "register_client_bridge",
                "message": ClientToBeam::hello(
                    "client-a",
                    "bridge-a",
                    vec!["token-a".to_string()],
                ),
            })
            .to_string(),
        );
        let _ = recv_type(&mut bridge_reader, "bridge_registered");

        let (mut requester, mut requester_reader, _requester_handle) = connect_to(Arc::clone(&hub));
        let request = |id: &str| {
            json!({
                "request_id": id,
                "op": "request_service",
                "session_token": "token-a",
                "client_id": "client-a",
                "request": {
                    "request_id": format!("service-{id}"),
                    "kind": {
                        "kind": "local_port_forward",
                        "remote_host": "127.0.0.1",
                        "remote_port": 8080
                    }
                },
            })
        };
        send(&mut requester, &request("before-ready").to_string());
        let error = recv_type(&mut requester_reader, "error");
        assert!(error["message"].as_str().unwrap().contains("no managed Beams client"));

        send(
            &mut bridge,
            &json!({
                "request_id": "ready",
                "op": "client_bridge_message",
                "message": ClientToBeam::Ready,
            })
            .to_string(),
        );
        let _ = recv_type(&mut bridge_reader, "bridge_message_accepted");

        send(&mut requester, &request("after-ready").to_string());
        let event = recv_type(&mut bridge_reader, "client_bridge_event");
        assert_eq!(event["message"]["request"]["request_id"], "service-after-ready");
        let accepted = recv_type(&mut requester_reader, "service_request_accepted");
        assert_eq!(accepted["client_id"], "client-a");
    }

    #[test]
    fn bridge_decision_is_validated_and_broadcast_back_to_the_remote_requester() {
        let hub = Arc::new(Hub::default());
        let (mut bridge, mut bridge_reader, _bridge_handle) =
            register_bridge(Arc::clone(&hub), "client-a", "bridge-a", "token-a");
        let (mut requester, mut requester_reader, _requester_handle) = connect_to(Arc::clone(&hub));
        send(
            &mut requester,
            &json!({
                "request_id": "submit",
                "op": "request_service",
                "session_token": "token-a",
                "request": {
                    "request_id": "service-request-2",
                    "kind": {
                        "kind": "local_port_forward",
                        "remote_host": "localhost",
                        "remote_port": 3000
                    }
                },
            })
            .to_string(),
        );
        let _ = recv_type(&mut bridge_reader, "client_bridge_event");
        let _ = recv_type(&mut requester_reader, "service_request_accepted");

        send(
            &mut bridge,
            &json!({
                "request_id": "decision",
                "op": "client_bridge_message",
                "message": ClientToBeam::RequestStatus {
                    request_id: "service-request-2".to_string(),
                    status: RequestStatus::Rejected,
                    message: Some("rejected by local user".to_string()),
                },
            })
            .to_string(),
        );

        let rejected = loop {
            let value = recv_type(&mut requester_reader, "service_request_status");
            if value["request"]["status"] == "rejected" {
                break value;
            }
        };
        assert_eq!(rejected["request"]["message"], "rejected by local user");
        let decision_reply = recv_type(&mut bridge_reader, "bridge_message_accepted");
        assert_eq!(decision_reply["request_id"], "decision");

        send(
            &mut bridge,
            &json!({
                "request_id": "invalid-decision",
                "op": "client_bridge_message",
                "message": ClientToBeam::RequestStatus {
                    request_id: "service-request-2".to_string(),
                    status: RequestStatus::Approved,
                    message: None,
                },
            })
            .to_string(),
        );
        let error = recv_type(&mut bridge_reader, "error");
        assert!(error["message"].as_str().unwrap().contains("invalid request transition"));
    }

    #[test]
    fn unmanaged_and_stale_sessions_fail_without_routing_a_request() {
        let hub = Arc::new(Hub::default());
        let (_bridge, _bridge_reader, _bridge_handle) =
            register_bridge(Arc::clone(&hub), "client-a", "bridge-a", "current-token");
        let (mut requester, mut reader, _handle) = connect_to(hub);

        for (token, client_id, expected) in [
            ("", None, "no managed Beams client"),
            ("old-token", Some("client-a"), "session token for client client-a is stale"),
        ] {
            send(
                &mut requester,
                &json!({
                    "request_id": token,
                    "op": "request_service",
                    "session_token": token,
                    "client_id": client_id,
                    "request": {
                        "request_id": format!("request-{token}"),
                        "kind": {
                            "kind": "local_port_forward",
                            "remote_host": "127.0.0.1",
                            "remote_port": 8080
                        }
                    },
                })
                .to_string(),
            );
            let error = recv_type(&mut reader, "error");
            assert!(
                error["message"].as_str().unwrap().contains(expected),
                "{} did not contain {expected:?}",
                error["message"]
            );
        }
    }
}
