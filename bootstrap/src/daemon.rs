//! The bootstrap daemon: a long-lived process reachable over a UDS
//! socket, driving on-demand task execution for whoever's connected (a
//! future TUI, the CLI, eventually a Mac app - see `DESIGN.md`'s
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
//! once (see DESIGN.md's "Seeding manifest") - the entries are resolved
//! against each other into dependency waves (`executor::plan`, the same
//! thing `executor::run` uses to actually run them - see DESIGN.md's
//! "Concurrent execution"), so listing them out of dependency order, as
//! above, is fine, and independent entries run concurrently rather than
//! waiting on each other. Its `accepted` reply's `waves` field is that
//! plan: a list of task-id lists, one per wave, in the order they'll
//! run. Stops at the first wave containing a failure (not the first
//! failing *task* - the rest of that wave still finishes), same
//! semantics as the fixed startup sequence (`steps::run`). A manifest
//! entry can only depend on a capability provided by *another entry in
//! the same manifest* - not on one left over from a previous, separate
//! run. Because every task here is idempotent, the fix is simply to
//! list the full chain every time (e.g. always include `clone-git-repo`
//! alongside a `checkout-pr-worktree` that needs it): the redundant
//! entry just skips if its work is already done.
//!
//! Direct replies (only to the requester) carry the same `request_id`
//! back; broadcast `task_status`/`manifest_status` events don't carry
//! one, since they aren't a reply to anything in particular. A
//! `task_status` broadcast carries a `manifest_id` and a `wave` index
//! (into that manifest's `accepted` reply's `waves`) when the task ran
//! as part of a manifest - both `null` when it ran via a standalone
//! `run_task`. Every task sharing a `wave` value ran (or at least
//! started) concurrently - that's what a client should group progress
//! rows by, rather than inferring concurrency from event timing.

use std::collections::{BTreeMap, HashSet};
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
    BeamToClient, ClientToBeam, RequestProjection, RequestStatus, ServiceProjection, ServiceRequest,
    ServiceRequestKind, PROTOCOL, PROTOCOL_VERSION,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::executor;
use crate::tasks::{self, Task};

pub fn default_socket_path() -> String {
    "~/.beam-bootstrap/daemon.sock".to_string()
}

/// State shared by every connection: who's listening for broadcast
/// events, and a lock serializing task execution so only one task runs
/// at a time system-wide - the same one-thing-at-a-time semantics the
/// fixed startup sequence already has (see `steps::run`).
struct Hub {
    subscribers: Mutex<Vec<(u64, Sender<String>)>>,
    run_lock: Mutex<()>,
    manifest_counter: AtomicU64,
    connection_counter: AtomicU64,
    bridges: Mutex<BTreeMap<String, BridgeRegistration>>,
    client_requests: Mutex<BTreeMap<String, ClientRequestProjection>>,
    client_services: Mutex<BTreeMap<String, OwnedServiceProjection>>,
}

impl Default for Hub {
    fn default() -> Self {
        Self {
            subscribers: Mutex::new(Vec::new()),
            run_lock: Mutex::new(()),
            manifest_counter: AtomicU64::new(0),
            connection_counter: AtomicU64::new(0),
            bridges: Mutex::new(BTreeMap::new()),
            client_requests: Mutex::new(BTreeMap::new()),
            client_services: Mutex::new(BTreeMap::new()),
        }
    }
}

#[derive(Clone)]
struct BridgeRegistration {
    client_id: String,
    bridge_id: String,
    session_tokens: HashSet<String>,
    ready: bool,
    sender: Sender<String>,
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

#[derive(Default)]
struct ConnectionContext {
    bridge_id: Option<String>,
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

    fn unregister_bridge(&self, bridge_id: &str) {
        let removed = self.bridges.lock().unwrap().remove(bridge_id);
        if let Some(registration) = removed {
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
    let hub = Arc::new(Hub::default());

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
    hub.unregister(connection_id);
    drop(tx);
    let _ = writer.join();
    Ok(())
}

/// Parses and dispatches one request line, sending its direct reply (and
/// any `run_task` broadcasts) through `reply`.
fn handle_line(hub: &Hub, line: &str, reply: &Sender<String>, context: &mut ConnectionContext) {
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
        "register_client_bridge" => {
            register_client_bridge(hub, request_id, &request, reply, context);
        }
        "client_bridge_message" => {
            client_bridge_message(hub, request_id, &request, reply, context);
        }
        "list_clients" => {
            list_clients(hub, request_id, reply);
        }
        "request_service" => {
            request_service(hub, request_id, &request, reply);
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
    let ClientToBeam::Hello { protocol, version, client_id, bridge_id, session_tokens } = message else {
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
    hub: &Hub,
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

fn request_service(hub: &Hub, request_id: Option<&str>, request: &Value, reply: &Sender<String>) {
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
        _ => {}
    }

    let matching: Vec<BridgeRegistration> = hub
        .bridges
        .lock()
        .unwrap()
        .values()
        .filter(|bridge| bridge.ready && bridge.session_tokens.contains(session_token))
        .cloned()
        .collect();
    let bridge = match matching.as_slice() {
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

fn send_bridge_event(sender: &Sender<String>, message: &BeamToClient) -> bool {
    sender.send(json!({"type": "client_bridge_event", "message": message}).to_string()).is_ok()
}

fn run_task(hub: &Hub, request_id: Option<&str>, name: &str, params: Value, reply: &Sender<String>) {
    let task = match tasks::instantiate(name, params) {
        Ok(task) => task,
        Err(err) => {
            let _ = reply.send(error_reply(request_id, &err.to_string()));
            return;
        }
    };
    let task_id = task.id();
    let _ = reply.send(json!({"request_id": request_id, "type": "accepted", "task_id": task_id}).to_string());

    // Held for the run's duration: only one task executes at a time
    // system-wide (see `Hub::run_lock`'s doc comment). Other connections
    // can still issue `list_tasks`/further `run_task` requests meanwhile
    // (they just queue on this lock) and keep receiving broadcasts.
    let _guard = hub.run_lock.lock().unwrap();
    let _ = run_one_task(hub, task.as_ref(), None);
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

fn run_manifest(hub: &Hub, request_id: Option<&str>, tasks_value: Value, reply: &Sender<String>) {
    let entries: Vec<ManifestEntry> = match serde_json::from_value(tasks_value) {
        Ok(entries) => entries,
        Err(err) => {
            let _ = reply.send(error_reply(request_id, &format!("invalid manifest: {err}")));
            return;
        }
    };

    let instances = match build_manifest(entries) {
        Ok(instances) => instances,
        Err(message) => {
            let _ = reply.send(error_reply(request_id, &message));
            return;
        }
    };

    // Resolved against *this manifest's own* instances only - see the
    // module doc comment on why a dependency on an earlier, separate
    // run's capability doesn't work (and what to do instead). Just for
    // the `accepted` reply's `waves` list below - `executor::run` (which
    // actually runs these) resolves the same graph again itself, per
    // `executor::plan`'s own doc comment.
    let waves: Vec<Vec<String>> = match executor::plan(&instances) {
        Ok(waves) => waves,
        Err(err) => {
            let _ = reply.send(error_reply(request_id, &err.to_string()));
            return;
        }
    };

    let manifest_id = hub.next_manifest_id();
    let _ = reply.send(
        json!({
            "request_id": request_id,
            "type": "accepted",
            "manifest_id": manifest_id,
            "waves": waves,
        })
        .to_string(),
    );

    // Held for the *whole* manifest's duration, not just one task at a
    // time within it - a manifest runs as one atomic unit relative to
    // other connections' `run_task`/`run_manifest` calls, which queue on
    // this same lock (see `Hub::run_lock`'s doc comment). Tasks *within*
    // the manifest still run concurrently where the graph allows it -
    // this lock is about manifests/standalone tasks not interleaving
    // with each other, not about serializing within one.
    let _guard = hub.run_lock.lock().unwrap();
    hub.broadcast(&json!({"type": "manifest_status", "manifest_id": manifest_id, "status": "start"}).to_string());

    let on_event = |event: executor::Event| {
        let line = match event {
            executor::Event::Start { task_id, wave } => {
                task_status(&task_id, "start", None, Some(&manifest_id), Some(wave))
            }
            executor::Event::Progress { task_id, wave, message } => {
                task_status(&task_id, "progress", Some(&message), Some(&manifest_id), Some(wave))
            }
            executor::Event::Ok { task_id, wave, message } => {
                task_status(&task_id, "ok", Some(&message), Some(&manifest_id), Some(wave))
            }
            executor::Event::Skip { task_id, wave } => {
                task_status(&task_id, "skip", None, Some(&manifest_id), Some(wave))
            }
            executor::Event::Error { task_id, wave, message } => {
                task_status(&task_id, "error", Some(&message), Some(&manifest_id), Some(wave))
            }
        };
        hub.broadcast(&line);
    };

    // Already validated above (same instances) - the only way this
    // fails is a graph error, which computing `waves` already ruled out.
    let ok = executor::run(instances, &on_event).expect("graph already validated above");
    let status = if ok { "ok" } else { "error" };
    hub.broadcast(&json!({"type": "manifest_status", "manifest_id": manifest_id, "status": status}).to_string());
}

/// Runs one task, broadcasting its `task_status` lifecycle (`start` then
/// exactly one of `ok`/`skip`/`error`) - shared by `run_task` and
/// `run_manifest` so the broadcast shape can't drift between the two.
/// `manifest_id` tags the events when running as part of a manifest,
/// `None` for a standalone `run_task`. Returns `Err` (with a message
/// naming the failed task) on failure, so `run_manifest` knows to stop.
fn run_one_task(hub: &Hub, task: &dyn Task, manifest_id: Option<&str>) -> Result<(), String> {
    let task_id = task.id();
    hub.broadcast(&task_status(&task_id, "start", None, manifest_id, None));
    let report = |message: &str| {
        hub.broadcast(&task_status(&task_id, "progress", Some(message), manifest_id, None));
    };
    match task.run_with_progress(&report) {
        Ok(Some(message)) => {
            hub.broadcast(&task_status(&task_id, "ok", Some(&message), manifest_id, None));
            Ok(())
        }
        Ok(None) => {
            hub.broadcast(&task_status(&task_id, "skip", None, manifest_id, None));
            Ok(())
        }
        Err(err) => {
            let message = err.to_string();
            hub.broadcast(&task_status(&task_id, "error", Some(&message), manifest_id, None));
            Err(format!("task {task_id} failed: {message}"))
        }
    }
}

/// `wave` is `None` for a standalone `run_task` (no concurrency grouping
/// concept there) and `Some(i)` for a `run_manifest` task, matching the
/// index into that manifest's `accepted` reply's `waves` list - see
/// `executor::Event`'s doc comment on why a client would want this.
fn task_status(
    task_id: &str,
    status: &str,
    message: Option<&str>,
    manifest_id: Option<&str>,
    wave: Option<usize>,
) -> String {
    json!({
        "type": "task_status",
        "task_id": task_id,
        "status": status,
        "message": message,
        "manifest_id": manifest_id,
        "wave": wave,
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
        let (mut stream, mut reader, handle) = connect_to(hub);
        let hello = ClientToBeam::hello(client_id.to_string(), bridge_id.to_string(), vec![token.to_string()]);
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
        let waves = accepted["waves"].as_array().unwrap();
        assert_eq!(waves.len(), 2, "checkout-pr-worktree depends on clone-git-repo, so they're 2 waves: {waves:?}");
        let wave_task = |wave: &serde_json::Value, i: usize| wave[i].as_str().unwrap().to_string();
        assert!(
            wave_task(&waves[0], 0).starts_with("clone-git-repo:"),
            "clone-git-repo should be reordered into the earlier wave, got {waves:?}"
        );
        assert!(wave_task(&waves[1], 0).starts_with("checkout-pr-worktree:"));
        let manifest_id = accepted["manifest_id"].as_str().unwrap().to_string();

        let manifest_start = recv(&mut reader);
        assert_eq!(manifest_start["type"], "manifest_status");
        assert_eq!(manifest_start["status"], "start");
        assert_eq!(manifest_start["manifest_id"], manifest_id);

        for (wave, expected_task) in [(0, "clone-git-repo"), (1, "checkout-pr-worktree")] {
            let start = recv(&mut reader);
            assert_eq!(start["type"], "task_status");
            assert_eq!(start["status"], "start");
            assert_eq!(start["manifest_id"], manifest_id);
            assert_eq!(start["wave"], wave);
            assert!(start["task_id"].as_str().unwrap().starts_with(expected_task));

            let done = recv_skipping_progress(&mut reader);
            assert_eq!(done["type"], "task_status");
            assert_eq!(done["status"], "ok");
            assert_eq!(done["manifest_id"], manifest_id);
            assert_eq!(done["wave"], wave);
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
    fn run_manifest_stops_at_the_first_failing_task() {
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
