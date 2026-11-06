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
//! ```
//!
//! Every launch event carries a daemon-wide `seq`. A reply carrying launch
//! state (`launch_snapshot`, `launch_accepted`, `launch_retrying`) is
//! queued on the requesting connection while holding the same lock under
//! which events are numbered and broadcast, so it reflects exactly the
//! events with `seq` up to its own; every later one arrives after it on
//! that connection. A client applies such a reply, then ignores events
//! with `seq` at or below it.

use super::runner::{self, Conditions, Verified};
use crate::executor;
use anyhow::{bail, Context as _};
use beams_protocol::launch::{Declaration, Phase};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};
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
}

/// How `interface-ready` is released.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Gate {
    /// By `launch_signal` (the initial shell and TUI, once wired).
    #[default]
    Interactive,
    /// As soon as pre succeeds: explicit headless initialization.
    Headless,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LaunchStatus {
    Accepted,
    Running,
    Ok,
    Failed,
    /// The daemon stopped while it was running; `launch_retry` resumes it.
    Interrupted,
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
}

/// Persisted before any effect, as `<root>/<launch id>/accepted.json`.
#[derive(Clone, Deserialize, Serialize)]
struct Accepted {
    declaration: Declaration,
    staging: PathBuf,
    gate: Gate,
    accepted_at_ms: u64,
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
}

/// The daemon's launches. State lives under `root` (shared with the
/// foreground `run-phase`, so receipts, locks and hook history agree).
pub struct Launches {
    root: PathBuf,
    home: PathBuf,
    table: Mutex<Table>,
}

impl Launches {
    pub fn new(root: PathBuf, home: PathBuf) -> Self {
        Self { root, home, table: Mutex::default() }
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
            send(json!({
                "type": "launch_accepted",
                "existing": true,
                "seq": table.seq,
                "launch": entry.snapshot,
            }));
            return Ok(());
        }
        let accepted = Accepted { declaration, staging, gate, accepted_at_ms: now_ms() };
        let dir = launches.dir(&id);
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        beams_artifacts::set_mode(&dir, 0o700)?;
        beams_artifacts::write_json(&dir.join("accepted.json"), &accepted)?;
        let snapshot = initial_snapshot(&accepted);
        beams_artifacts::write_json(&dir.join("snapshot.json"), &snapshot)?;
        let conditions = DaemonConditions::new(&accepted);
        table.entries.insert(id.clone(), Entry { accepted, snapshot: snapshot.clone(), conditions, worker: true });
        table.seq += 1;
        // The reply, like a snapshot, follows the event it includes, and
        // both are queued before anything the worker reports.
        host.broadcast(&json!({"type": "launch_status", "launch_id": id, "status": "accepted", "seq": table.seq}).to_string());
        send(json!({"type": "launch_accepted", "existing": false, "seq": table.seq, "launch": snapshot}));
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
    let (released, message) = if condition == "interface-ready" {
        conditions.release_interface();
        (true, "interface-ready released".to_string())
    } else {
        let phase = runner::condition_phase(condition)?;
        match conditions.probe(phase) {
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
        let Table { seq, entries } = &mut *table;
        let entry = entries.get_mut(&id).with_context(|| format!("no launch {id}"))?;
        if entry.worker || !matches!(entry.snapshot.status, LaunchStatus::Failed | LaunchStatus::Interrupted) {
            bail!("launch {id} is {:?}; only failed or interrupted launches can be retried", entry.snapshot.status);
        }
        entry.snapshot = initial_snapshot(&entry.accepted);
        entry.conditions = DaemonConditions::new(&entry.accepted);
        entry.worker = true;
        *seq += 1;
        let _ = beams_artifacts::write_json(&launches.dir(&id).join("snapshot.json"), &entry.snapshot);
        host.broadcast(&json!({"type": "launch_status", "launch_id": id, "status": "accepted", "seq": *seq}).to_string());
        send(json!({"type": "launch_retrying", "seq": *seq, "launch": entry.snapshot}));
    }
    spawn_worker(host, id, retry_interrupted);
    Ok(())
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
    let launches = host.launches();
    let mut table = launches.table.lock().unwrap();
    let Table { seq, entries } = &mut *table;
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
}

fn set_status<H: Host>(host: &H, id: &str, status: LaunchStatus, message: Option<String>) {
    update(host, id, true, |snapshot| {
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
        let node = snapshot.nodes.iter_mut().find(|n| n.id == node_id)?;
        node.status = NodeStatus::Waiting;
        node.message = Some(message.to_string());
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
        let mut any = false;
        for node in snapshot.nodes.iter_mut().filter(|n| n.phase == phase && n.status == NodeStatus::Pending) {
            node.status = NodeStatus::Blocked;
            node.message = Some(why.to_string());
            any = true;
        }
        any.then(|| json!({"type": "launch_nodes_blocked", "launch_id": id, "phase": phase, "message": why}))
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
            let _ = conditions.probe(phase);
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
            conditions.release_interface();
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
                    None => "waiting for interface-ready (the shell and TUI, or `beam-bootstrap launch signal`)".to_string(),
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
