//! Launches accepted by the daemon: durable acceptance keyed by launch UUID
//! and plan digest, one worker per launch (pre, then post) on the daemon's
//! shared executor resources, external conditions that wait to be released,
//! and task snapshots ordered against live events by a sequence number.
//!
//! Ops (see `docs/development/launches.md`):
//!
//! ```json
//! {"op": "launch_submit", "launch": "/abs/launch.json", "staging": "/abs/stage", "gate": "interactive"}
//! {"op": "launch_signal", "launch_id": "...", "condition": "interface-ready"}
//! {"op": "launch_snapshot", "launch_id": "..."}
//! {"op": "launch_retry", "launch_id": "...", "retry_interrupted": false}
//! {"op": "launch_ready", "launch_id": "...", "component": "shell"}
//! {"op": "launch_receive", "launch_id": "...", "phase": "pre", "archive": "/abs/bundle.tar.gz", "descriptor": "/abs/descriptor.json"}
//! ```
//!
//! A workstation does the same over its bridge (`bridge`): it uploads the
//! declaration and each phase's bundle in chunks (`upload`), submits, and
//! receives `LaunchStatus`/`LaunchCondition` for the launches it submitted.
//! A complete bundle is verified and staged here, which releases
//! `artifact-ready:<phase>`; no separate signal is needed.
//!
//! An interactive launch's `interface-ready` is released once both the
//! initial shell and the TUI have reported `launch_ready` (or by an explicit
//! `launch_signal`); a headless one releases it when pre succeeds.
//!
//! Every launch event carries a daemon-wide `seq`. A reply carrying launch
//! state (`launch_snapshot`, `launch_accepted`, `launch_retrying`) is
//! queued on the requesting connection while holding the same lock under
//! which events are numbered and broadcast, so it reflects exactly the
//! events with `seq` up to its own; every later one arrives after it on
//! that connection. A client applies such a reply, then ignores events
//! with `seq` at or below it.

use super::runner::{self, Conditions, Verified};
use super::upload::{self, Chunk, Uploads};
use crate::executor;
use anyhow::{bail, Context as _};
pub use beams_protocol::launch::{Gate, LaunchStatus};
use beams_protocol::launch::{Blob, BundleDescriptor, Declaration, Phase, MAX_DECLARATION_BYTES};
use beams_protocol::{BeamToClient, ClientToBeam};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

/// Progress lines kept per node, and their maximum length.
const LOG_LINES: usize = 10;
const LOG_LINE_CHARS: usize = 200;

/// What the daemon provides to launches.
pub trait Host: Send + Sync + 'static {
    fn broadcast(&self, line: &str);
    fn resources(&self) -> &executor::Resources;
    fn launches(&self) -> &Launches;
    /// Sends `message` to the workstation `client_id`'s connected bridges
    /// that support launches.
    fn notify(&self, client_id: &str, message: &BeamToClient);
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NodeStatus {
    Pending,
    Waiting,
    Running,
    Ok,
    Skip,
    Error,
    Blocked,
    Interrupted,
}

impl NodeStatus {
    fn terminal(self) -> bool {
        matches!(self, Self::Ok | Self::Skip | Self::Error | Self::Blocked | Self::Interrupted)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct NodeState {
    pub id: String,
    pub label: String,
    pub phase: Phase,
    pub status: NodeStatus,
    pub message: Option<String>,
    /// The last `LOG_LINES` progress lines.
    pub log: VecDeque<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct LaunchSnapshot {
    pub launch_id: String,
    pub plan_digest: String,
    pub gate: Gate,
    pub status: LaunchStatus,
    pub message: Option<String>,
    pub nodes: Vec<NodeState>,
    /// Interface components that reported `launch_ready` (`shell`, `tui`).
    #[serde(default)]
    pub ready: Vec<String>,
    /// External conditions released so far, in release order.
    #[serde(default)]
    pub released: Vec<String>,
}

/// The interface components an interactive launch waits for.
pub const READY_COMPONENTS: [&str; 2] = ["shell", "tui"];

const INTERFACE_READY: &str = "interface-ready";

/// Readiness reports for launches not accepted yet (the shell can start
/// before a slow submission lands); applied at acceptance. Bounded.
const EARLY_READY_LIMIT: usize = 32;

/// Persisted before any effect, as `<root>/<launch id>/accepted.json`.
#[derive(Clone, Deserialize, Serialize)]
struct Accepted {
    declaration: Declaration,
    staging: PathBuf,
    gate: Gate,
    accepted_at_ms: u64,
    /// The workstation that submitted it over its bridge; it gets the
    /// launch's status and condition updates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owner_client_id: Option<String>,
}

struct Entry {
    accepted: Accepted,
    snapshot: LaunchSnapshot,
    conditions: Arc<DaemonConditions>,
    worker: bool,
}

#[derive(Default)]
struct Table {
    seq: u64,
    entries: BTreeMap<String, Entry>,
    early_ready: BTreeMap<String, Vec<String>>,
}

/// The daemon's launches. State lives under `root` (shared with the
/// foreground `run-phase`, so receipts, locks and hook history agree).
pub struct Launches {
    root: PathBuf,
    home: PathBuf,
    table: Mutex<Table>,
    /// Bridge uploads, in `launch-uploads/` next to `root`.
    uploads: Uploads,
}

impl Launches {
    pub fn new(root: PathBuf, home: PathBuf) -> Self {
        let uploads = Uploads::new(upload::root_for(&root));
        Self { root, home, table: Mutex::default(), uploads }
    }

    /// Loads accepted launches persisted by an earlier daemon. Anything that
    /// hadn't finished is marked interrupted, not resumed: an old installer
    /// may still be running, and `launch_retry` is the explicit way back.
    pub fn load(&self) {
        let Ok(dirs) = fs::read_dir(&self.root) else { return };
        let mut table = self.table.lock().unwrap();
        for dir in dirs.flatten() {
            let path = dir.path();
            if !path.join("accepted.json").exists() {
                continue;
            }
            let loaded = (|| -> anyhow::Result<(Accepted, LaunchSnapshot)> {
                let accepted: Accepted = beams_artifacts::read_json(&path.join("accepted.json"))?;
                let snapshot = match beams_artifacts::read_json(&path.join("snapshot.json")) {
                    Ok(snapshot) => snapshot,
                    Err(_) => initial_snapshot(&accepted),
                };
                Ok((accepted, snapshot))
            })();
            let (accepted, mut snapshot) = match loaded {
                Ok(loaded) => loaded,
                Err(err) => {
                    eprintln!("daemon: ignoring launch state {}: {err:#}", path.display());
                    continue;
                }
            };
            if matches!(snapshot.status, LaunchStatus::Accepted | LaunchStatus::Running) {
                snapshot.status = LaunchStatus::Interrupted;
                snapshot.message = Some("the daemon stopped while this launch ran".into());
                for node in snapshot.nodes.iter_mut().filter(|n| !n.status.terminal()) {
                    node.status = NodeStatus::Interrupted;
                }
                let _ = beams_artifacts::write_json(&path.join("snapshot.json"), &snapshot);
            }
            let id = accepted.declaration.launch_id.clone();
            let conditions = DaemonConditions::new(&accepted);
            table.entries.insert(id, Entry { accepted, snapshot, conditions, worker: false });
        }
    }

    fn dir(&self, launch_id: &str) -> PathBuf {
        self.root.join(launch_id)
    }
}

/// Handles one `launch_*` op. `send` queues a reply on the requesting
/// connection (the caller adds its `request_id`).
pub fn handle<H: Host>(host: &Arc<H>, op: &str, request: &Value, send: &dyn Fn(Value)) {
    let result = match op {
        "launch_submit" => submit(host, request, send),
        "launch_signal" => signal(host, request).map(|reply| send(reply)),
        "launch_snapshot" => snapshot(host, request, send),
        "launch_retry" => retry(host, request, send),
        "launch_ready" => ready(host, request).map(|reply| send(reply)),
        "launch_receive" => receive_op(host, request).map(|reply| send(reply)),
        _ => Err(anyhow::anyhow!("unknown launch op {op:?}")),
    };
    if let Err(err) = result {
        send(json!({"type": "error", "message": format!("{err:#}")}));
    }
}

fn str_field<'a>(request: &'a Value, name: &str) -> anyhow::Result<&'a str> {
    request
        .get(name)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .with_context(|| format!("{name} is required"))
}

fn absolute(request: &Value, name: &str) -> anyhow::Result<PathBuf> {
    let path = PathBuf::from(str_field(request, name)?);
    if !path.is_absolute() {
        bail!("{name} must be an absolute path");
    }
    Ok(path)
}

fn submit<H: Host>(host: &Arc<H>, request: &Value, send: &dyn Fn(Value)) -> anyhow::Result<()> {
    let launch = absolute(request, "launch")?;
    let staging = absolute(request, "staging")?;
    let gate: Gate = match request.get("gate") {
        None | Some(Value::Null) => Gate::default(),
        Some(gate) => serde_json::from_value(gate.clone()).context("gate must be interactive or headless")?,
    };
    let declaration: Declaration = beams_artifacts::read_json(&launch)?;
    accept(host, declaration, staging, gate, None, &|existing, seq, snapshot| {
        send(json!({"type": "launch_accepted", "existing": existing, "seq": seq, "launch": snapshot}));
    })
}

/// Accepts `declaration` (or finds it already accepted with the same
/// digest), calling `reply(existing, seq, snapshot)` while event numbering
/// is held, so the reply precedes anything the new worker reports.
fn accept<H: Host>(
    host: &Arc<H>,
    declaration: Declaration,
    staging: PathBuf,
    gate: Gate,
    owner_client_id: Option<String>,
    reply: &dyn Fn(bool, u64, &LaunchSnapshot),
) -> anyhow::Result<()> {
    runner::validate(&declaration)?;
    let launches = host.launches();
    let id = declaration.launch_id.clone();
    {
        let mut table = launches.table.lock().unwrap();
        if let Some(entry) = table.entries.get(&id) {
            if entry.accepted.declaration.plan_digest != declaration.plan_digest {
                bail!("launch {id} was already accepted with a different plan digest");
            }
            // An identical retry of the submission refers to the same launch.
            reply(true, table.seq, &entry.snapshot);
            return Ok(());
        }
        let accepted = Accepted { declaration, staging, gate, accepted_at_ms: now_ms(), owner_client_id };
        let dir = launches.dir(&id);
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        beams_artifacts::set_mode(&dir, 0o700)?;
        beams_artifacts::write_json(&dir.join("accepted.json"), &accepted)?;
        let mut snapshot = initial_snapshot(&accepted);
        snapshot.ready = table.early_ready.remove(&id).unwrap_or_default();
        beams_artifacts::write_json(&dir.join("snapshot.json"), &snapshot)?;
        let conditions = DaemonConditions::new(&accepted);
        table.entries.insert(id.clone(), Entry { accepted, snapshot: snapshot.clone(), conditions, worker: true });
        table.seq += 1;
        // The reply, like a snapshot, follows the event it includes, and
        // both are queued before anything the worker reports.
        host.broadcast(&json!({"type": "launch_status", "launch_id": id, "status": "accepted", "seq": table.seq}).to_string());
        reply(false, table.seq, &snapshot);
    }
    spawn_worker(host, id, false);
    Ok(())
}

fn signal<H: Host>(host: &Arc<H>, request: &Value) -> anyhow::Result<Value> {
    let id = str_field(request, "launch_id")?;
    let condition = str_field(request, "condition")?;
    let conditions = {
        let table = host.launches().table.lock().unwrap();
        Arc::clone(&table.entries.get(id).with_context(|| format!("no launch {id}"))?.conditions)
    };
    let (released, message) = if condition == INTERFACE_READY {
        release_interface(&**host, id, &conditions, "released by launch_signal");
        (true, "interface-ready released".to_string())
    } else {
        let phase = runner::condition_phase(condition)?;
        match probe(&**host, id, &conditions, phase) {
            Ok(true) => (true, format!("verified the {} staging", phase.name())),
            Ok(false) => (false, format!("nothing is staged for {} yet", phase.name())),
            Err(why) => bail!("{} staging failed verification: {why}", phase.name()),
        }
    };
    Ok(json!({"type": "launch_signaled", "launch_id": id, "condition": condition, "released": released, "message": message}))
}

fn snapshot<H: Host>(host: &Arc<H>, request: &Value, send: &dyn Fn(Value)) -> anyhow::Result<()> {
    let only = request.get("launch_id").and_then(Value::as_str);
    let table = host.launches().table.lock().unwrap();
    if let Some(id) = only {
        if !table.entries.contains_key(id) {
            bail!("no launch {id}");
        }
    }
    let launches: Vec<&LaunchSnapshot> = table
        .entries
        .values()
        .filter(|e| only.is_none_or(|id| e.snapshot.launch_id == id))
        .map(|e| &e.snapshot)
        .collect();
    // Sent while numbering is held: see the module doc comment.
    send(json!({"type": "launch_snapshot", "seq": table.seq, "launches": launches}));
    Ok(())
}

fn retry<H: Host>(host: &Arc<H>, request: &Value, send: &dyn Fn(Value)) -> anyhow::Result<()> {
    let id = str_field(request, "launch_id")?.to_string();
    let retry_interrupted = request.get("retry_interrupted").and_then(Value::as_bool).unwrap_or(false);
    {
        let launches = host.launches();
        let mut table = launches.table.lock().unwrap();
        let Table { seq, entries, .. } = &mut *table;
        let entry = entries.get_mut(&id).with_context(|| format!("no launch {id}"))?;
        if entry.worker || !matches!(entry.snapshot.status, LaunchStatus::Failed | LaunchStatus::Interrupted) {
            bail!("launch {id} is {:?}; only failed or interrupted launches can be retried", entry.snapshot.status);
        }
        // The interface outlives the attempt: keep what it reported.
        let previous = std::mem::replace(&mut entry.snapshot, initial_snapshot(&entry.accepted));
        entry.snapshot.ready = previous.ready;
        entry.conditions = DaemonConditions::new(&entry.accepted);
        if previous.released.iter().any(|c| c == INTERFACE_READY) || interface_reported(&entry.snapshot) {
            entry.snapshot.released.push(INTERFACE_READY.to_string());
            entry.conditions.release_interface();
        }
        entry.worker = true;
        *seq += 1;
        let _ = beams_artifacts::write_json(&launches.dir(&id).join("snapshot.json"), &entry.snapshot);
        host.broadcast(&json!({"type": "launch_status", "launch_id": id, "status": "accepted", "seq": *seq}).to_string());
        send(json!({"type": "launch_retrying", "seq": *seq, "launch": entry.snapshot}));
    }
    spawn_worker(host, id, retry_interrupted);
    Ok(())
}

/// Records that an interface component is up; once both are (interactive
/// gate), releases `interface-ready`. Repeats are no-ops.
fn ready<H: Host>(host: &Arc<H>, request: &Value) -> anyhow::Result<Value> {
    let id = str_field(request, "launch_id")?;
    let component = str_field(request, "component")?;
    if !READY_COMPONENTS.contains(&component) {
        bail!("component must be one of {}", READY_COMPONENTS.join(", "));
    }
    let release = {
        let launches = host.launches();
        let mut table = launches.table.lock().unwrap();
        let Table { seq, entries, early_ready } = &mut *table;
        let Some(entry) = entries.get_mut(id) else {
            if early_ready.len() >= EARLY_READY_LIMIT && !early_ready.contains_key(id) {
                bail!("no launch {id}");
            }
            let seen = early_ready.entry(id.to_string()).or_default();
            if !seen.iter().any(|c| c == component) {
                seen.push(component.to_string());
            }
            return Ok(json!({"type": "launch_ready", "launch_id": id, "component": component, "accepted": false}));
        };
        let snapshot = &mut entry.snapshot;
        if !snapshot.ready.iter().any(|c| c == component) {
            snapshot.ready.push(component.to_string());
            let waiting = interface_waiting(snapshot);
            if let Some(node) = snapshot.nodes.iter_mut().find(|n| n.id == INTERFACE_READY && n.status == NodeStatus::Waiting) {
                node.message = Some(waiting);
            }
            *seq += 1;
            if let Err(err) = beams_artifacts::write_json(&launches.dir(id).join("snapshot.json"), &*snapshot) {
                eprintln!("daemon: saving launch {id} snapshot: {err:#}");
            }
            host.broadcast(
                &json!({"type": "launch_ready", "launch_id": id, "component": component, "ready": snapshot.ready, "seq": *seq})
                    .to_string(),
            );
        }
        let release = entry.accepted.gate == Gate::Interactive
            && interface_reported(&entry.snapshot)
            && !entry.snapshot.released.iter().any(|c| c == INTERFACE_READY);
        release.then(|| Arc::clone(&entry.conditions))
    };
    if let Some(conditions) = release {
        release_interface(&**host, id, &conditions, "the shell and TUI are up");
    }
    Ok(json!({"type": "launch_ready", "launch_id": id, "component": component, "accepted": true}))
}

fn interface_reported(snapshot: &LaunchSnapshot) -> bool {
    READY_COMPONENTS.iter().all(|c| snapshot.ready.iter().any(|r| r == c))
}

/// What a waiting `interface-ready` node says it is waiting for.
fn interface_waiting(snapshot: &LaunchSnapshot) -> String {
    let missing: Vec<&str> = READY_COMPONENTS
        .iter()
        .copied()
        .filter(|c| !snapshot.ready.iter().any(|r| r == c))
        .map(|c| if c == "tui" { "the TUI" } else { "the shell" })
        .collect();
    match missing.as_slice() {
        [] => "the shell and TUI are up".to_string(),
        [one] => format!("waiting for {one} to start"),
        _ => "waiting for the shell and TUI to start (or `beam-bootstrap launch signal --condition interface-ready`)"
            .to_string(),
    }
}

/// Releases `interface-ready` for launch `id` and records it.
fn release_interface<H: Host>(host: &H, id: &str, conditions: &DaemonConditions, why: &str) {
    conditions.release_interface();
    condition_changed(host, id, INTERFACE_READY, true, Some(why.to_string()));
}

/// Probes `phase`'s staging for launch `id`, recording a release or a
/// verification failure.
fn probe<H: Host>(host: &H, id: &str, conditions: &DaemonConditions, phase: Phase) -> Result<bool, String> {
    let result = conditions.probe(phase);
    let condition = format!("artifact-ready:{}", phase.name());
    match &result {
        Ok(true) => condition_changed(host, id, &condition, true, None),
        Err(why) => condition_changed(host, id, &condition, false, Some(why.clone())),
        Ok(false) => {}
    }
    result
}

/// Numbers and broadcasts a condition transition (`launch_condition`). A
/// release is recorded once; a failure is reported each time it happens.
fn condition_changed<H: Host>(host: &H, id: &str, condition: &str, released: bool, message: Option<String>) {
    let notify = BeamToClient::LaunchCondition {
        launch_id: id.to_string(),
        condition: condition.to_string(),
        released,
        message: message.clone(),
    };
    update_and_notify(host, id, true, Some(notify), |snapshot| {
        if released {
            if snapshot.released.iter().any(|c| c == condition) {
                return None;
            }
            snapshot.released.push(condition.to_string());
        }
        Some(json!({
            "type": "launch_condition",
            "launch_id": id,
            "condition": condition,
            "released": released,
            "message": message,
        }))
    });
}

fn initial_snapshot(accepted: &Accepted) -> LaunchSnapshot {
    let declaration = &accepted.declaration;
    LaunchSnapshot {
        launch_id: declaration.launch_id.clone(),
        plan_digest: declaration.plan_digest.clone(),
        gate: accepted.gate,
        status: LaunchStatus::Accepted,
        message: None,
        nodes: declaration
            .plan
            .nodes
            .iter()
            .map(|node| NodeState {
                id: node.id.clone(),
                label: node.label.clone(),
                phase: node.phase,
                status: NodeStatus::Pending,
                message: None,
                log: VecDeque::new(),
            })
            .collect(),
        ready: Vec::new(),
        released: Vec::new(),
    }
}

/// Applies `change` to launch `id`'s snapshot, numbers it, broadcasts
/// `event` (given the new `seq`), and persists the snapshot when `persist`.
fn update<H: Host>(
    host: &H,
    id: &str,
    persist: bool,
    change: impl FnOnce(&mut LaunchSnapshot) -> Option<Value>,
) {
    update_and_notify(host, id, persist, None, change);
}

/// `update`, also sending `notify` to the launch's owning workstation when
/// the change produced an event (in event order, since both happen under
/// the table lock).
fn update_and_notify<H: Host>(
    host: &H,
    id: &str,
    persist: bool,
    notify: Option<BeamToClient>,
    change: impl FnOnce(&mut LaunchSnapshot) -> Option<Value>,
) {
    let launches = host.launches();
    let mut table = launches.table.lock().unwrap();
    let Table { seq, entries, .. } = &mut *table;
    let Some(entry) = entries.get_mut(id) else { return };
    let Some(mut event) = change(&mut entry.snapshot) else { return };
    *seq += 1;
    event["seq"] = json!(*seq);
    if persist {
        if let Err(err) = beams_artifacts::write_json(&launches.dir(id).join("snapshot.json"), &entry.snapshot) {
            eprintln!("daemon: saving launch {id} snapshot: {err:#}");
        }
    }
    host.broadcast(&event.to_string());
    if let (Some(owner), Some(message)) = (&entry.accepted.owner_client_id, notify) {
        host.notify(owner, &message);
    }
}

fn set_status<H: Host>(host: &H, id: &str, status: LaunchStatus, message: Option<String>) {
    let notify = BeamToClient::LaunchStatus { launch_id: id.to_string(), status, message: message.clone() };
    update_and_notify(host, id, true, Some(notify), |snapshot| {
        snapshot.status = status;
        snapshot.message = message.clone();
        Some(json!({"type": "launch_status", "launch_id": id, "status": status, "message": message}))
    });
}

/// Records one executor event for a launch node and broadcasts it in the
/// `task_status` shape (with `manifest_id` = launch id) the TUI already
/// renders.
fn record_event<H: Host>(host: &H, id: &str, event: executor::Event) {
    let (task_id, wire, status, message, persist) = match &event {
        executor::Event::Start { task_id } => (task_id, "start", NodeStatus::Running, None, true),
        executor::Event::Progress { task_id, message } => (task_id, "progress", NodeStatus::Running, Some(message), false),
        executor::Event::Ok { task_id, message } => (task_id, "ok", NodeStatus::Ok, Some(message), true),
        executor::Event::Skip { task_id } => (task_id, "skip", NodeStatus::Skip, None, true),
        executor::Event::Error { task_id, message } => (task_id, "error", NodeStatus::Error, Some(message), true),
        executor::Event::Blocked { task_id, message } => (task_id, "blocked", NodeStatus::Blocked, Some(message), true),
    };
    let prefix = format!("launch:{id}:");
    let Some(node_id) = task_id.strip_prefix(&prefix).map(str::to_string) else { return };
    let progress = matches!(event, executor::Event::Progress { .. });
    let line = json!({
        "type": "task_status",
        "task_id": task_id,
        "status": wire,
        "message": message,
        "manifest_id": id,
    });
    update(host, id, persist, |snapshot| {
        let node = snapshot.nodes.iter_mut().find(|n| n.id == node_id)?;
        // A waiting condition stays waiting until it ends.
        if !(progress && node.status == NodeStatus::Waiting) {
            node.status = status;
        }
        // A terminal event without a message (skip) clears a stale one,
        // such as a released condition's "waiting for …".
        if message.is_none() && status.terminal() {
            node.message = None;
        }
        if let Some(message) = message {
            node.message = Some(message.clone());
            if progress {
                node.log.push_back(message.chars().take(LOG_LINE_CHARS).collect());
                while node.log.len() > LOG_LINES {
                    node.log.pop_front();
                }
            }
        }
        let mut line = line;
        line["launch_id"] = json!(id);
        line["node"] = json!(node_id);
        Some(line)
    });
}

fn mark_waiting<H: Host>(host: &H, id: &str, node_id: &str, message: &str) {
    update(host, id, true, |snapshot| {
        let message =
            if node_id == INTERFACE_READY { interface_waiting(snapshot) } else { message.to_string() };
        let node = snapshot.nodes.iter_mut().find(|n| n.id == node_id)?;
        node.status = NodeStatus::Waiting;
        node.message = Some(message.clone());
        Some(json!({
            "type": "task_status",
            "task_id": format!("launch:{id}:{node_id}"),
            "status": "progress",
            "message": message,
            "manifest_id": id,
            "launch_id": id,
            "node": node_id,
            "waiting": true,
        }))
    });
}

/// Marks every still-pending node of `phase` blocked (its phase never ran).
fn block_phase<H: Host>(host: &H, id: &str, phase: Phase, why: &str) {
    update(host, id, true, |snapshot| {
        let mut nodes = Vec::new();
        for node in snapshot.nodes.iter_mut().filter(|n| n.phase == phase && n.status == NodeStatus::Pending) {
            node.status = NodeStatus::Blocked;
            node.message = Some(why.to_string());
            nodes.push(node.id.clone());
        }
        (!nodes.is_empty()).then(|| {
            json!({"type": "launch_nodes_blocked", "launch_id": id, "phase": phase, "nodes": nodes, "message": why})
        })
    });
}

fn spawn_worker<H: Host>(host: &Arc<H>, id: String, retry_interrupted: bool) {
    let host = Arc::clone(host);
    thread::spawn(move || {
        let (accepted, conditions) = {
            let table = host.launches().table.lock().unwrap();
            let entry = &table.entries[&id];
            (entry.accepted.clone(), Arc::clone(&entry.conditions))
        };
        conditions.attach(Arc::clone(&host), id.clone());
        // Staging unpacked before submission releases right away.
        for phase in [Phase::Pre, Phase::Post] {
            let _ = probe(&*host, &id, &conditions, phase);
        }
        set_status(&*host, &id, LaunchStatus::Running, None);
        let outcome = run_phases(&*host, &id, &accepted, &conditions, retry_interrupted);
        let (status, message) = match outcome {
            Ok(()) => (LaunchStatus::Ok, None),
            Err(err) => (LaunchStatus::Failed, Some(format!("{err:#}"))),
        };
        {
            let mut table = host.launches().table.lock().unwrap();
            if let Some(entry) = table.entries.get_mut(&id) {
                entry.worker = false;
            }
        }
        set_status(&*host, &id, status, message);
    });
}

fn run_phases<H: Host>(
    host: &H,
    id: &str,
    accepted: &Accepted,
    conditions: &Arc<DaemonConditions>,
    retry_interrupted: bool,
) -> anyhow::Result<()> {
    let launches = host.launches();
    for phase in [Phase::Pre, Phase::Post] {
        if phase == Phase::Post && accepted.gate == Gate::Headless {
            release_interface(host, id, conditions, "headless: pre succeeded");
        }
        let options = runner::Options {
            home: launches.home.clone(),
            state: launches.root.clone(),
            phase,
            retry_interrupted,
        };
        let shared: Arc<dyn Conditions> = Arc::clone(conditions) as Arc<dyn Conditions>;
        let result = runner::run(
            accepted.declaration.clone(),
            options,
            host.resources(),
            shared,
            &|event| record_event(host, id, event),
        );
        let failure = match result {
            Ok(true) => continue,
            Ok(false) => format!("{} did not complete; see failed or blocked nodes", phase.name()),
            Err(err) => format!("{}: {err:#}", phase.name()),
        };
        if phase == Phase::Pre {
            block_phase(host, id, Phase::Post, "not run: pre did not complete");
        }
        bail!(failure);
    }
    Ok(())
}

#[derive(Default)]
struct ConditionState {
    interface: bool,
    verified: BTreeMap<Phase, Verified>,
    failed: BTreeMap<Phase, String>,
    /// Phases whose bundle is being received right now.
    receiving: BTreeSet<Phase>,
}

type Waiting = Box<dyn Fn(&str, &str) + Send + Sync>;

/// Releases a launch's conditions on signals instead of checking once.
pub struct DaemonConditions {
    declaration: Declaration,
    staging: PathBuf,
    state: Mutex<ConditionState>,
    changed: Condvar,
    /// Marks a condition node as waiting (set once a worker owns this).
    on_waiting: Mutex<Option<Waiting>>,
}

impl DaemonConditions {
    fn new(accepted: &Accepted) -> Arc<Self> {
        Arc::new(Self {
            declaration: accepted.declaration.clone(),
            staging: accepted.staging.clone(),
            state: Mutex::default(),
            changed: Condvar::new(),
            on_waiting: Mutex::new(None),
        })
    }

    fn attach<H: Host>(&self, host: Arc<H>, id: String) {
        *self.on_waiting.lock().unwrap() =
            Some(Box::new(move |node, message| mark_waiting(&*host, &id, node, message)));
    }

    fn release_interface(&self) {
        self.state.lock().unwrap().interface = true;
        self.changed.notify_all();
    }

    /// Fails `phase`'s artifact condition (its bundle couldn't be staged).
    fn fail(&self, phase: Phase, why: String) {
        self.state.lock().unwrap().failed.insert(phase, why);
        self.changed.notify_all();
    }

    /// Claims `phase` for one receive at a time; false if one is running.
    fn start_receiving(&self, phase: Phase) -> bool {
        self.state.lock().unwrap().receiving.insert(phase)
    }

    fn done_receiving(&self, phase: Phase) {
        self.state.lock().unwrap().receiving.remove(&phase);
    }

    /// Verifies `phase`'s staging if it is there. `Ok(false)`: nothing
    /// staged yet. A verification failure fails that condition.
    fn probe(&self, phase: Phase) -> Result<bool, String> {
        if self.state.lock().unwrap().verified.contains_key(&phase) {
            return Ok(true);
        }
        let result = runner::verify_phase(&self.declaration, &self.staging, phase);
        let mut state = self.state.lock().unwrap();
        let outcome = match result {
            Ok(Some(artifacts)) => {
                state.verified.insert(phase, Arc::new(artifacts));
                state.failed.remove(&phase);
                Ok(true)
            }
            Ok(None) => Ok(false),
            Err(err) => {
                let why = format!("{err:#}");
                state.failed.insert(phase, why.clone());
                Err(why)
            }
        };
        drop(state);
        self.changed.notify_all();
        outcome
    }
}

impl Conditions for DaemonConditions {
    fn prepare(&self, _declaration: &Declaration, _phase: Phase) -> anyhow::Result<()> {
        Ok(())
    }

    fn wait(&self, condition: &str, progress: &dyn Fn(&str)) -> anyhow::Result<()> {
        let phase = if condition == "interface-ready" { None } else { Some(runner::condition_phase(condition)?) };
        let mut announced = false;
        let mut state = self.state.lock().unwrap();
        loop {
            match phase {
                None if state.interface => return Ok(()),
                Some(phase) if state.verified.contains_key(&phase) => return Ok(()),
                Some(phase) => {
                    if let Some(why) = state.failed.get(&phase) {
                        bail!("{} staging failed verification: {why}", phase.name());
                    }
                }
                None => {}
            }
            if !announced {
                announced = true;
                let message = match phase {
                    None => "waiting for the shell and TUI to start".to_string(),
                    Some(phase) => format!(
                        "waiting for the {0} bundle: unpack it, then `beam-bootstrap launch signal --condition artifact-ready:{0}`",
                        phase.name()
                    ),
                };
                drop(state);
                // Marks the node waiting and broadcasts it; without a
                // worker attached (never in practice), plain progress.
                match &*self.on_waiting.lock().unwrap() {
                    Some(on_waiting) => on_waiting(condition, &message),
                    None => progress(&message),
                }
                state = self.state.lock().unwrap();
                continue;
            }
            state = self.changed.wait(state).unwrap();
        }
    }

    fn verified(&self, phase: Phase) -> Option<Verified> {
        self.state.lock().unwrap().verified.get(&phase).cloned()
    }
}

/// `launch_receive`: stage a bundle delivered some other way (a file on
/// this machine) the same way a bridge upload is.
fn receive_op<H: Host>(host: &Arc<H>, request: &Value) -> anyhow::Result<Value> {
    let id = str_field(request, "launch_id")?;
    let phase: Phase = serde_json::from_value(request.get("phase").cloned().unwrap_or(Value::Null))
        .context("phase must be pre or post")?;
    let archive = absolute(request, "archive")?;
    let descriptor: BundleDescriptor = beams_artifacts::read_json(&absolute(request, "descriptor")?)?;
    receive_bundle(host, id, phase, archive, descriptor, false)?;
    Ok(json!({"type": "launch_receiving", "launch_id": id, "phase": phase}))
}

/// Verifies and publishes `archive` as launch `id`'s `phase` staging on a
/// thread, then releases `artifact-ready:<phase>` (or fails it, blocking
/// its consumers). `upload`: the archive is a bridge upload, removed once
/// consumed.
fn receive_bundle<H: Host>(
    host: &Arc<H>,
    id: &str,
    phase: Phase,
    archive: PathBuf,
    descriptor: BundleDescriptor,
    upload: bool,
) -> anyhow::Result<()> {
    let (conditions, staging) = {
        let table = host.launches().table.lock().unwrap();
        let entry = table.entries.get(id).with_context(|| format!("no launch {id}"))?;
        let declaration = &entry.accepted.declaration;
        if descriptor.launch_id != declaration.launch_id
            || descriptor.plan_digest != declaration.plan_digest
            || descriptor.phase != phase
        {
            bail!("that descriptor is for a different launch, plan or phase");
        }
        (Arc::clone(&entry.conditions), entry.accepted.staging.clone())
    };
    if !conditions.start_receiving(phase) {
        return Ok(());
    }
    let host = Arc::clone(host);
    let id = id.to_string();
    thread::spawn(move || {
        let output = staging.join(phase.name());
        let result = fs::create_dir_all(&staging)
            .with_context(|| format!("creating {}", staging.display()))
            .and_then(|()| beams_artifacts::receive(&archive, &descriptor, &output));
        conditions.done_receiving(phase);
        match result {
            Ok(_) => {
                if upload {
                    host.launches().uploads.remove(&id, Blob::of(phase));
                }
                let _ = probe(&*host, &id, &conditions, phase);
            }
            Err(err) => {
                let why = format!("receiving the {} bundle: {err:#}", phase.name());
                conditions.fail(phase, why.clone());
                condition_changed(&*host, &id, &format!("artifact-ready:{}", phase.name()), false, Some(why));
            }
        }
    });
    Ok(())
}

/// Stages every complete bundle upload of an accepted launch that isn't
/// staged yet; forgets ones that already are.
fn receive_uploaded<H: Host>(host: &Arc<H>, id: &str) {
    let launches = host.launches();
    let staging = {
        let table = launches.table.lock().unwrap();
        let Some(entry) = table.entries.get(id) else { return };
        entry.accepted.staging.clone()
    };
    for phase in [Phase::Pre, Phase::Post] {
        let blob = Blob::of(phase);
        let Some((archive, meta)) = launches.uploads.complete(id, blob) else { continue };
        if staging.join(phase.name()).exists() {
            launches.uploads.remove(id, blob);
            continue;
        }
        let Some(descriptor) = meta.descriptor else { continue };
        if let Err(err) = receive_bundle(host, id, phase, archive, descriptor, true) {
            eprintln!("daemon: launch {id}: {err:#}");
        }
    }
}

/// Handles a workstation's launch message from bridge `bridge_id` of
/// `client_id`, replying on that bridge.
pub fn bridge<H: Host>(host: &Arc<H>, client_id: &str, bridge_id: &str, message: ClientToBeam, reply: &dyn Fn(BeamToClient)) {
    match message {
        ClientToBeam::LaunchUploadBegin { launch_id, blob, bytes, sha256, descriptor } => {
            let meta = upload::Meta { bytes, sha256, descriptor };
            match upload_begin(host, bridge_id, &launch_id, blob, meta) {
                Ok(offset) => {
                    reply(BeamToClient::LaunchUploadOffset { launch_id: launch_id.clone(), blob, offset });
                    if offset == bytes {
                        reply(BeamToClient::LaunchUploaded { launch_id: launch_id.clone(), blob });
                        if blob != Blob::Declaration {
                            receive_uploaded(host, &launch_id);
                        }
                    }
                }
                Err(err) => reply(BeamToClient::LaunchUploadFailed { launch_id, blob, message: format!("{err:#}") }),
            }
        }
        ClientToBeam::LaunchUploadChunk { launch_id, blob, offset, data } => {
            let result = beams_protocol::agent::b64_decode(&data)
                .map_err(|_| anyhow::anyhow!("chunk data is not base64"))
                .and_then(|data| host.launches().uploads.chunk(bridge_id, &launch_id, blob, offset, &data));
            match result {
                Ok(Chunk::Offset(offset)) => reply(BeamToClient::LaunchUploadOffset { launch_id, blob, offset }),
                Ok(Chunk::Complete) => {
                    reply(BeamToClient::LaunchUploaded { launch_id: launch_id.clone(), blob });
                    if blob != Blob::Declaration {
                        receive_uploaded(host, &launch_id);
                    }
                }
                Err(err) => reply(BeamToClient::LaunchUploadFailed { launch_id, blob, message: format!("{err:#}") }),
            }
        }
        ClientToBeam::LaunchSubmit { request_id, launch_id, plan_digest, gate } => {
            let rejected = |message: String| BeamToClient::LaunchRejected {
                request_id: request_id.clone(),
                launch_id: launch_id.clone(),
                message,
            };
            let accepted = |existing: bool, _seq: u64, snapshot: &LaunchSnapshot| {
                reply(BeamToClient::LaunchAccepted {
                    request_id: request_id.clone(),
                    launch_id: launch_id.clone(),
                    existing,
                    status: snapshot.status,
                    released: snapshot.released.clone(),
                })
            };
            if let Err(err) = bridge_submit(host, client_id, &launch_id, &plan_digest, gate, &accepted) {
                reply(rejected(format!("{err:#}")));
                return;
            }
            receive_uploaded(host, &launch_id);
        }
        _ => reply(BeamToClient::Error { message: "not a launch message".into() }),
    }
}

/// Checks what an upload may be before storing any of it: a bounded
/// declaration, or a bundle whose descriptor matches it, its launch and
/// (once accepted) its plan. Post waits for acceptance.
fn upload_begin<H: Host>(host: &Arc<H>, bridge_id: &str, id: &str, blob: Blob, meta: upload::Meta) -> anyhow::Result<u64> {
    upload::check_id(id)?;
    let launches = host.launches();
    let (accepted_digest, staged) = {
        let table = launches.table.lock().unwrap();
        match table.entries.get(id) {
            Some(entry) => (
                Some(entry.accepted.declaration.plan_digest.clone()),
                blob.phase().is_some_and(|phase| entry.accepted.staging.join(phase.name()).exists()),
            ),
            None => (None, false),
        }
    };
    match blob {
        Blob::Declaration => {
            if meta.descriptor.is_some() {
                bail!("a declaration upload has no descriptor");
            }
            if meta.bytes > MAX_DECLARATION_BYTES {
                bail!("declaration is larger than {MAX_DECLARATION_BYTES} bytes");
            }
            // Already accepted: nothing more to send.
            if accepted_digest.is_some() {
                return Ok(meta.bytes);
            }
        }
        Blob::Pre | Blob::Post => {
            let descriptor = meta.descriptor.as_ref().context("a bundle upload needs its descriptor")?;
            if descriptor.launch_id != id
                || Some(descriptor.phase) != blob.phase()
                || descriptor.bytes != meta.bytes
                || descriptor.sha256 != meta.sha256
            {
                bail!("the descriptor doesn't match this upload");
            }
            match &accepted_digest {
                Some(digest) if *digest != descriptor.plan_digest => bail!("the descriptor is for a different plan"),
                Some(_) if staged => return Ok(meta.bytes),
                Some(_) => {}
                None if blob == Blob::Post => bail!("submit the launch before uploading its post bundle"),
                None => {}
            }
        }
    }
    launches.uploads.begin(bridge_id, id, blob, meta)
}

fn bridge_submit<H: Host>(
    host: &Arc<H>,
    client_id: &str,
    id: &str,
    plan_digest: &str,
    gate: Gate,
    reply: &dyn Fn(bool, u64, &LaunchSnapshot),
) -> anyhow::Result<()> {
    upload::check_id(id)?;
    let launches = host.launches();
    {
        let table = launches.table.lock().unwrap();
        if let Some(entry) = table.entries.get(id) {
            if entry.accepted.declaration.plan_digest != plan_digest {
                bail!("launch {id} was already accepted with a different plan digest");
            }
            reply(true, table.seq, &entry.snapshot);
            return Ok(());
        }
    }
    let (path, _) = launches
        .uploads
        .complete(id, Blob::Declaration)
        .context("upload the declaration before submitting")?;
    let declaration: Declaration = beams_artifacts::read_json(&path)?;
    if declaration.launch_id != id || declaration.plan_digest != plan_digest {
        bail!("the uploaded declaration is for a different launch or plan");
    }
    let staging = launches.dir(id).join("staging");
    accept(host, declaration, staging, gate, Some(client_id.to_string()), reply)?;
    // accepted.json holds it now.
    launches.uploads.remove(id, Blob::Declaration);
    Ok(())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// The default state root for a daemon listening on `socket`.
pub fn state_root(socket: &Path) -> PathBuf {
    socket.with_file_name("launch-state")
}
