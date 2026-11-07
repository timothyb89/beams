//! TUI state and rendering. Deliberately its own module from `client.rs`
//! (transport) and `mod.rs` (terminal setup/event loop) - this is just
//! "given what the daemon has told us so far, and what's been typed,
//! what does the screen look like and what does a keypress do".

use std::collections::BTreeMap;

use beams_protocol::{
    RequestStatus, ServiceAction, ServiceActionRequest, ServiceProjection, ServiceRequest,
    ServiceRequestKind, ServiceStatus,
};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Gauge, List, ListItem, ListState, Paragraph};
use ratatui::Frame;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

#[derive(Clone, Deserialize)]
pub struct RemoteRequest {
    pub request: ServiceRequest,
    pub owner_client_id: String,
    pub status: RequestStatus,
    pub message: Option<String>,
}

#[derive(Clone, Deserialize)]
pub struct RemoteActionRequest {
    pub request: ServiceActionRequest,
    pub owner_client_id: String,
    pub status: RequestStatus,
    pub message: Option<String>,
}

#[derive(Clone, Deserialize)]
pub struct RemoteService {
    pub owner_client_id: String,
    pub service: ServiceProjection,
}

#[derive(Clone, Deserialize)]
pub struct ParamSchema {
    pub name: String,
    pub kind: String,
    pub required: bool,
}

#[derive(Clone, Deserialize)]
pub struct TemplateInfo {
    pub template: String,
    #[serde(default)]
    pub params: Vec<ParamSchema>,
    /// See `tasks::TemplateInfo::run_once` - hidden from the template
    /// list, since startup already ran it.
    #[serde(default)]
    pub run_once: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskState {
    Pending,
    Running,
    Ok,
    Skip,
    Error,
    /// Never started: something it depends on failed (`"blocked"`
    /// task_status - see `executor::Event::Blocked`).
    Blocked,
    /// A launch condition waiting to be released (a bundle, the interface).
    Waiting,
    /// The daemon stopped while it ran (a launch snapshot after a restart).
    Interrupted,
}

impl TaskState {
    fn is_terminal(self) -> bool {
        matches!(
            self,
            TaskState::Ok | TaskState::Skip | TaskState::Error | TaskState::Blocked | TaskState::Interrupted
        )
    }

    fn glyph(self, spinner_frame: usize) -> &'static str {
        const SPINNER: [&str; 4] = ["⠋", "⠙", "⠸", "⠴"];
        match self {
            TaskState::Pending => "·",
            TaskState::Running => SPINNER[spinner_frame % SPINNER.len()],
            TaskState::Ok => "✔",
            TaskState::Skip => "○",
            TaskState::Error => "✘",
            TaskState::Blocked => "⊘",
            TaskState::Waiting => "◷",
            TaskState::Interrupted => "↯",
        }
    }

    fn color(self) -> Color {
        match self {
            TaskState::Pending => Color::DarkGray,
            TaskState::Running => Color::Yellow,
            TaskState::Ok => Color::Green,
            TaskState::Skip => Color::Blue,
            TaskState::Error => Color::Red,
            TaskState::Blocked => Color::Magenta,
            TaskState::Waiting => Color::Cyan,
            TaskState::Interrupted => Color::Magenta,
        }
    }

    /// A launch snapshot node's status (`launch::daemon::NodeStatus`).
    fn from_node_status(status: &str) -> Option<Self> {
        Some(match status {
            "pending" => TaskState::Pending,
            "waiting" => TaskState::Waiting,
            "running" => TaskState::Running,
            "ok" => TaskState::Ok,
            "skip" => TaskState::Skip,
            "error" => TaskState::Error,
            "blocked" => TaskState::Blocked,
            "interrupted" => TaskState::Interrupted,
            _ => return None,
        })
    }
}

/// Whether this TUI still owes its launch (`BEAMS_LAUNCH_ID`) a
/// `launch_ready` report: one is due after the first frame drawn with that
/// launch's snapshot applied, while its interface is not yet released.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadyReport {
    Idle,
    AwaitingFrame,
    Due,
    Sent,
}

pub struct TaskRow {
    pub task_id: String,
    pub state: TaskState,
    pub message: Option<String>,
    /// Task ids (from the same manifest) this row waits on - from the
    /// `accepted` reply's plan. Shown on a pending row's second line.
    pub depends_on: Vec<String>,
    /// Shown instead of `task_id` (a launch node's phase and label).
    pub label: Option<String>,
}

/// Which pane is focused for keyboard/scroll purposes - just the two
/// permanent panes now that param entry is a modal (see `App`'s doc
/// comment) rather than a third pane competing for the sidebar's scarce
/// horizontal width: `editing_params` intercepts input ahead of this
/// entirely (see `handle_key`/`handle_mouse`), so there's no "focus" state
/// for the modal itself to be in.
#[derive(PartialEq, Eq)]
pub enum Focus {
    Templates,
    Log,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Tasks,
    Services,
    Requests,
}

/// Which action a submitted set of params should turn into. Kept tiny
/// on purpose: the TUI only ever asks the daemon to run exactly one
/// task at a time today (`run_task`), same single-task granularity as
/// `beam-bootstrap run-task`. Building and submitting a multi-task
/// manifest from the TUI (e.g. selecting several templates at once) is
/// a real gap, not implemented yet - see `TODO.md`.
pub struct App {
    pub tab: Tab,
    pub templates: Vec<TemplateInfo>,
    pub selected_template: usize,

    pub editing_params: bool,
    pub param_values: Vec<String>,
    pub focused_param: usize,

    pub rows: Vec<TaskRow>,
    pub log_scroll: usize,

    pub focus: Focus,
    pub services: BTreeMap<String, RemoteService>,
    pub service_selection: usize,
    pub requests: BTreeMap<String, RemoteRequest>,
    pub action_requests: BTreeMap<String, RemoteActionRequest>,
    session_token: Option<String>,
    client_id: Option<String>,
    pub status_line: String,
    pub spinner_frame: usize,
    /// Set while the daemon connection is down (`mod.rs`'s `run_loop`
    /// detects this via the event channel's `Disconnected` error and
    /// drives the actual reconnect attempts/backoff - this is just the
    /// bit of state `draw` needs to say so). Was the TUI's known gap
    /// from the daemon-restart E2E test in TODO.md: previously the TUI
    /// just went quiet on a dropped connection, showing stale state
    /// forever with no indication anything was wrong.
    pub disconnected: bool,

    next_request_id: u64,
    pub should_quit: bool,
    /// Set by the key handler when a request should be sent; drained by
    /// `mod.rs`'s event loop right after, so `App` itself never needs a
    /// `&mut DaemonClient`.
    pub pending_request: Option<Value>,
    pub needs_client_state: bool,
    /// Ask for `launch_snapshot`: at start, after a reconnect, and when a
    /// launch is (re)accepted, so its pending rows appear before they run.
    pub needs_launch_snapshot: bool,
    /// The last applied snapshot's `seq`: launch events at or below it are
    /// already reflected in it.
    launch_seq: u64,
    /// `BEAMS_LAUNCH_ID`: the launch this TUI is part of the interface of.
    launch_id: Option<String>,
    pub ready_report: ReadyReport,

    /// The on-screen area the template list's rows were last drawn into
    /// (including its border) - set at the end of `draw_templates` each
    /// frame, read by `handle_mouse`'s click hit-testing. `Rect::default()`
    /// (zero-sized) before the first `draw`, which `row_within` treats as
    /// "never inside" - a click can't resolve to a row before anything's
    /// been rendered.
    templates_area: Rect,
    /// Same idea as `templates_area`, for the task-progress list.
    progress_area: Rect,
    /// First row index visible in the progress list as of the last draw
    /// (ratatui scrolls it to keep the selection visible) - needed to map
    /// a mouse click back to a row.
    progress_offset: usize,
}

impl App {
    pub fn new() -> Self {
        App {
            tab: Tab::Tasks,
            templates: Vec::new(),
            selected_template: 0,
            editing_params: false,
            param_values: Vec::new(),
            focused_param: 0,
            rows: Vec::new(),
            log_scroll: 0,
            focus: Focus::Templates,
            services: BTreeMap::new(),
            service_selection: 0,
            requests: BTreeMap::new(),
            action_requests: BTreeMap::new(),
            session_token: std::env::var("BEAMS_SESSION_TOKEN")
                .ok()
                .filter(|value| !value.is_empty()),
            client_id: std::env::var("BEAMS_CLIENT_ID")
                .ok()
                .filter(|value| !value.is_empty()),
            status_line: "connecting...".to_string(),
            spinner_frame: 0,
            disconnected: false,
            next_request_id: 1,
            should_quit: false,
            pending_request: None,
            needs_client_state: false,
            needs_launch_snapshot: true,
            launch_seq: 0,
            launch_id: std::env::var("BEAMS_LAUNCH_ID").ok().filter(|value| !value.is_empty()),
            ready_report: ReadyReport::Idle,
            templates_area: Rect::default(),
            progress_area: Rect::default(),
            progress_offset: 0,
        }
    }

    fn next_id(&mut self) -> String {
        let id = self.next_request_id;
        self.next_request_id += 1;
        id.to_string()
    }

    pub fn request_task_list(&mut self) {
        let id = self.next_id();
        self.pending_request = Some(json!({"request_id": id, "op": "list_tasks"}));
    }

    pub fn request_client_state(&mut self) {
        let id = self.next_id();
        self.pending_request = Some(json!({"request_id": id, "op": "list_client_state"}));
        self.needs_client_state = false;
    }

    /// Queues the next deferred request once the single request slot is
    /// free: client state, then a launch snapshot, then a due readiness
    /// report.
    pub fn queue_deferred(&mut self) {
        if self.pending_request.is_some() || self.disconnected {
            return;
        }
        if self.needs_client_state {
            self.request_client_state();
        } else if self.needs_launch_snapshot {
            self.needs_launch_snapshot = false;
            let id = self.next_id();
            self.pending_request = Some(json!({"request_id": id, "op": "launch_snapshot"}));
        } else if self.ready_report == ReadyReport::Due {
            if let Some(launch_id) = self.launch_id.clone() {
                let id = self.next_id();
                self.pending_request =
                    Some(json!({"request_id": id, "op": "launch_ready", "launch_id": launch_id, "component": "tui"}));
            }
            self.ready_report = ReadyReport::Sent;
        }
    }

    /// Called after each drawn frame: a readiness report waiting on one is
    /// now due.
    pub fn frame_drawn(&mut self) {
        if self.ready_report == ReadyReport::AwaitingFrame {
            self.ready_report = ReadyReport::Due;
        }
    }

    /// Replaces every launch's rows with the snapshot's nodes (in plan
    /// order) and records its `seq` boundary.
    fn apply_launch_snapshot(&mut self, value: &Value) {
        let Some(launches) = value.get("launches").and_then(Value::as_array) else { return };
        for launch in launches {
            let Some(launch_id) = launch.get("launch_id").and_then(Value::as_str) else { continue };
            let prefix = format!("launch:{launch_id}:");
            self.rows.retain(|row| !row.task_id.starts_with(&prefix));
            for node in launch.get("nodes").and_then(Value::as_array).into_iter().flatten() {
                let Some(node_id) = node.get("id").and_then(Value::as_str) else { continue };
                let state = node
                    .get("status")
                    .and_then(Value::as_str)
                    .and_then(TaskState::from_node_status)
                    .unwrap_or(TaskState::Pending);
                let label = node.get("label").and_then(Value::as_str).unwrap_or(node_id);
                let phase = node.get("phase").and_then(Value::as_str).unwrap_or_default();
                self.rows.push(TaskRow {
                    task_id: format!("{prefix}{node_id}"),
                    state,
                    message: node.get("message").and_then(Value::as_str).map(str::to_string),
                    depends_on: Vec::new(),
                    label: Some(if phase.is_empty() { label.to_string() } else { format!("{phase} · {label}") }),
                });
            }
            if self.launch_id.as_deref() == Some(launch_id) {
                let listed = |field: &str, item: &str| {
                    launch
                        .get(field)
                        .and_then(Value::as_array)
                        .is_some_and(|items| items.iter().any(|v| v.as_str() == Some(item)))
                };
                let owed = !listed("released", "interface-ready") && !listed("ready", "tui");
                if owed && matches!(self.ready_report, ReadyReport::Idle | ReadyReport::Sent) {
                    self.ready_report = ReadyReport::AwaitingFrame;
                }
            }
        }
        if let Some(seq) = value.get("seq").and_then(Value::as_u64) {
            self.launch_seq = seq;
        }
    }

    /// The daemon connection just dropped - `run_loop` calls this the
    /// moment the event channel reports `Disconnected`, before it starts
    /// retrying `DaemonClient::reconnect`.
    pub fn mark_disconnected(&mut self) {
        self.disconnected = true;
        self.status_line = "daemon connection lost - reconnecting...".to_string();
    }

    /// `run_loop`'s reconnect attempt just succeeded - existing rows are
    /// left as-is (whatever happened to their tasks while disconnected
    /// is simply unknown, same gap as any other missed broadcast) but a
    /// fresh `list_tasks` request is still worth queuing here so the
    /// template list reflects the daemon's current state rather than
    /// whatever it was before the connection dropped.
    pub fn mark_reconnected(&mut self) {
        self.disconnected = false;
        self.status_line = "reconnected".to_string();
        // A restarted daemon numbers events from scratch.
        self.launch_seq = 0;
        self.needs_launch_snapshot = true;
        self.request_task_list();
    }

    fn row_index(&self, task_id: &str) -> Option<usize> {
        self.rows.iter().position(|r| r.task_id == task_id)
    }

    fn upsert_row(&mut self, task_id: &str, state: TaskState, message: Option<String>) {
        match self.row_index(task_id) {
            Some(i) => {
                self.rows[i].state = state;
                if message.is_some() {
                    self.rows[i].message = message;
                }
            }
            None => self.rows.push(TaskRow {
                task_id: task_id.to_string(),
                state,
                message,
                depends_on: Vec::new(),
                label: None,
            }),
        }
    }

    /// Resets (or creates) a row for a freshly submitted task: `Pending`,
    /// no message left over from an earlier run of the same task id.
    fn reset_row(&mut self, task_id: &str, depends_on: Vec<String>) {
        match self.row_index(task_id) {
            Some(i) => {
                self.rows[i].state = TaskState::Pending;
                self.rows[i].message = None;
                self.rows[i].depends_on = depends_on;
            }
            None => self.rows.push(TaskRow {
                task_id: task_id.to_string(),
                state: TaskState::Pending,
                message: None,
                depends_on,
                label: None,
            }),
        }
    }

    /// Refreshes an existing row's `message` without touching its
    /// `state` - see the "progress" branch of `handle_daemon_event`'s
    /// doc comment on why this is a separate path from `upsert_row`.
    fn update_progress_message(&mut self, task_id: &str, message: Option<String>) {
        match self.row_index(task_id) {
            Some(i) => {
                if message.is_some() {
                    self.rows[i].message = message;
                }
            }
            None => self.rows.push(TaskRow {
                task_id: task_id.to_string(),
                state: TaskState::Running,
                message,
                depends_on: Vec::new(),
                label: None,
            }),
        }
    }

    /// What a row's second line shows: the task's latest message, or for
    /// a still-pending row, which of its dependencies haven't finished.
    fn row_detail(&self, row: &TaskRow) -> String {
        if row.state == TaskState::Pending {
            let waiting: Vec<&str> = row
                .depends_on
                .iter()
                .filter(|dep| {
                    self.row_index(dep).map_or(true, |i| !matches!(self.rows[i].state, TaskState::Ok | TaskState::Skip))
                })
                .map(String::as_str)
                .collect();
            if !waiting.is_empty() {
                return format!("waiting on {}", waiting.join(", "));
            }
            return "queued".to_string();
        }
        row.message.clone().unwrap_or_default()
    }

    /// Handles one line already parsed off the daemon socket - see
    /// `daemon.rs`'s module doc comment for the shapes being matched
    /// here.
    pub fn handle_daemon_event(&mut self, value: Value) {
        // Already part of the last launch snapshot.
        if value.get("launch_id").is_some()
            && value.get("seq").and_then(Value::as_u64).is_some_and(|seq| seq <= self.launch_seq)
        {
            return;
        }
        match value.get("type").and_then(Value::as_str) {
            Some("launch_snapshot") => self.apply_launch_snapshot(&value),
            Some("launch_status") => {
                let launch_id = value.get("launch_id").and_then(Value::as_str).unwrap_or_default();
                let status = value.get("status").and_then(Value::as_str).unwrap_or_default();
                // A new or retried launch: fetch its plan so pending rows show.
                if status == "accepted" {
                    self.needs_launch_snapshot = true;
                }
                let message = value.get("message").and_then(Value::as_str).map(|m| format!(": {m}")).unwrap_or_default();
                self.status_line = format!("launch {} {status}{message}", short_id(launch_id));
            }
            Some("launch_condition") => {
                let condition = value.get("condition").and_then(Value::as_str).unwrap_or_default();
                if value.get("released").and_then(Value::as_bool) == Some(true) {
                    self.status_line = format!("released {condition}");
                } else {
                    let why = value.get("message").and_then(Value::as_str).unwrap_or("failed");
                    self.status_line = format!("{condition}: {why}");
                }
            }
            Some("launch_nodes_blocked") => {
                let launch_id = value.get("launch_id").and_then(Value::as_str).unwrap_or_default();
                let message = value.get("message").and_then(Value::as_str).map(str::to_string);
                for node in value.get("nodes").and_then(Value::as_array).into_iter().flatten() {
                    if let Some(node) = node.as_str() {
                        self.upsert_row(&format!("launch:{launch_id}:{node}"), TaskState::Blocked, message.clone());
                    }
                }
            }
            Some("tasks") => {
                if let Some(templates) =
                    value.get("tasks").and_then(|v| serde_json::from_value::<Vec<TemplateInfo>>(v.clone()).ok())
                {
                    self.templates = templates.into_iter().filter(|t| !t.run_once).collect();
                    self.selected_template = self.selected_template.min(self.templates.len().saturating_sub(1));
                    self.status_line = format!("{} template(s) available", self.templates.len());
                    self.needs_client_state = true;
                }
            }
            Some("client_state") => {
                self.services = value
                    .get("services")
                    .and_then(|value| serde_json::from_value::<Vec<RemoteService>>(value.clone()).ok())
                    .unwrap_or_default()
                    .into_iter()
                    .map(|service| (service.service.service_id.clone(), service))
                    .collect();
                self.requests = value
                    .get("requests")
                    .and_then(|value| serde_json::from_value::<Vec<RemoteRequest>>(value.clone()).ok())
                    .unwrap_or_default()
                    .into_iter()
                    .map(|request| (request.request.request_id.clone(), request))
                    .collect();
                self.action_requests = value
                    .get("action_requests")
                    .and_then(|value| {
                        serde_json::from_value::<Vec<RemoteActionRequest>>(value.clone()).ok()
                    })
                    .unwrap_or_default()
                    .into_iter()
                    .map(|request| (request.request.request_id.clone(), request))
                    .collect();
                self.status_line = format!(
                    "{} service(s), {} request(s)",
                    self.services.len(),
                    self.requests.len() + self.action_requests.len()
                );
            }
            Some("client_services_snapshot") => {
                let client_id = value
                    .get("client_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                self.services
                    .retain(|_, service| service.owner_client_id != client_id);
                if let Some(services) = value.get("services").and_then(|value| {
                    serde_json::from_value::<Vec<RemoteService>>(value.clone()).ok()
                }) {
                    for service in services {
                        self.services
                            .insert(service.service.service_id.clone(), service);
                    }
                }
            }
            Some("client_service_status") => {
                if let Some(service) = value
                    .get("service")
                    .and_then(|value| serde_json::from_value::<RemoteService>(value.clone()).ok())
                {
                    self.services
                        .insert(service.service.service_id.clone(), service);
                }
            }
            Some("client_requests_snapshot") => {
                let client_id = value
                    .get("client_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                self.requests
                    .retain(|_, request| request.owner_client_id != client_id);
                if let Some(requests) = value.get("requests").and_then(|value| {
                    serde_json::from_value::<Vec<RemoteRequest>>(value.clone()).ok()
                }) {
                    for request in requests {
                        self.requests
                            .insert(request.request.request_id.clone(), request);
                    }
                }
            }
            Some("service_request_status") => {
                if let Some(request) = value
                    .get("request")
                    .and_then(|value| serde_json::from_value::<RemoteRequest>(value.clone()).ok())
                {
                    self.requests
                        .insert(request.request.request_id.clone(), request);
                }
            }
            Some("service_action_request_status") => {
                if let Some(request) = value.get("request").and_then(|value| {
                    serde_json::from_value::<RemoteActionRequest>(value.clone()).ok()
                }) {
                    self.action_requests
                        .insert(request.request.request_id.clone(), request);
                }
            }
            Some("client_action_requests_snapshot") => {
                let client_id = value
                    .get("client_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                self.action_requests
                    .retain(|_, request| request.owner_client_id != client_id);
                if let Some(requests) = value.get("requests").and_then(|value| {
                    serde_json::from_value::<Vec<RemoteActionRequest>>(value.clone()).ok()
                }) {
                    for request in requests {
                        self.action_requests
                            .insert(request.request.request_id.clone(), request);
                    }
                }
            }
            Some("accepted") => {
                if let Some(task_id) = value.get("task_id").and_then(Value::as_str) {
                    self.reset_row(task_id, Vec::new());
                    self.status_line = format!("submitted {task_id}");
                } else if let Some(planned) = value.get("tasks").and_then(Value::as_array) {
                    for entry in planned {
                        let Some(id) = entry.get("task_id").and_then(Value::as_str) else { continue };
                        let depends_on = entry
                            .get("depends_on")
                            .and_then(Value::as_array)
                            .map(|deps| deps.iter().filter_map(Value::as_str).map(str::to_string).collect())
                            .unwrap_or_default();
                        self.reset_row(id, depends_on);
                    }
                    self.status_line = "manifest submitted".to_string();
                }
            }
            Some("task_status") => {
                let Some(task_id) = value.get("task_id").and_then(Value::as_str) else { return };
                let status = value.get("status").and_then(Value::as_str).unwrap_or_default();
                let message = value.get("message").and_then(Value::as_str).map(str::to_string);
                // "progress" (see `executor::Event::Progress`/
                // `daemon.rs`'s `"progress"` task_status) isn't a
                // lifecycle state the row transitions through like the
                // other four - it only ever arrives while a row is
                // already `Running`, so just refresh the message
                // without touching `state` (falling back to `Running` if
                // somehow nothing's been seen for this task_id yet).
                if status == "progress" {
                    if value.get("waiting").and_then(Value::as_bool) == Some(true) {
                        self.upsert_row(task_id, TaskState::Waiting, message);
                    } else {
                        self.update_progress_message(task_id, message);
                    }
                    return;
                }
                let state = match status {
                    "start" => TaskState::Running,
                    "ok" => TaskState::Ok,
                    "skip" => TaskState::Skip,
                    "error" => TaskState::Error,
                    "blocked" => TaskState::Blocked,
                    _ => return,
                };
                self.upsert_row(task_id, state, message);
            }
            Some("manifest_status") => {
                let status = value.get("status").and_then(Value::as_str).unwrap_or_default();
                self.status_line = format!("manifest {status}");
            }
            Some("error") => {
                let message = value.get("message").and_then(Value::as_str).unwrap_or("unknown error");
                self.status_line = format!("error: {message}");
            }
            _ => {}
        }
    }

    pub fn tick(&mut self) {
        self.spinner_frame = self.spinner_frame.wrapping_add(1);
    }

    pub fn handle_key(&mut self, key: ratatui::crossterm::event::KeyEvent) {
        use ratatui::crossterm::event::KeyCode;

        if self.editing_params {
            self.handle_param_key(key);
            return;
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Char('1') => self.tab = Tab::Tasks,
            KeyCode::Char('2') => self.tab = Tab::Services,
            KeyCode::Char('3') => self.tab = Tab::Requests,
            KeyCode::Left => self.tab = match self.tab {
                Tab::Tasks => Tab::Requests,
                Tab::Services => Tab::Tasks,
                Tab::Requests => Tab::Services,
            },
            KeyCode::Right => self.tab = match self.tab {
                Tab::Tasks => Tab::Services,
                Tab::Services => Tab::Requests,
                Tab::Requests => Tab::Tasks,
            },
            _ if self.tab == Tab::Services => self.handle_service_key(key.code),
            _ if self.tab == Tab::Requests => {
                if key.code == KeyCode::Char('r') {
                    self.request_client_state();
                }
            }
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Templates => Focus::Log,
                    Focus::Log => Focus::Templates,
                };
            }
            KeyCode::Char('r') => self.request_task_list(),
            _ => match self.focus {
                Focus::Templates => self.handle_template_list_key(key.code),
                Focus::Log => self.handle_log_key(key.code),
            },
        }
    }

    fn handle_service_key(&mut self, code: ratatui::crossterm::event::KeyCode) {
        use ratatui::crossterm::event::KeyCode;
        match code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.service_selection = self.service_selection.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.service_selection + 1 < self.services.len() {
                    self.service_selection += 1;
                }
            }
            KeyCode::Char('s') => self.request_service_action(ServiceAction::Stop),
            KeyCode::Char('r') => self.request_service_action(ServiceAction::Restart),
            KeyCode::Char('p') => self.request_service_action(ServiceAction::Pause),
            KeyCode::Char('e') => self.request_service_action(ServiceAction::Resume),
            KeyCode::Char('t') => self.request_service_action(ServiceAction::Terminate),
            _ => {}
        }
    }

    fn request_service_action(&mut self, action: ServiceAction) {
        let Some(service) = self.services.values().nth(self.service_selection)
        else {
            self.status_line = "no service selected".to_string();
            return;
        };
        let supported = match &service.service.kind {
            ServiceRequestKind::LocalPortForward { .. } => {
                matches!(action, ServiceAction::Stop | ServiceAction::Restart)
            }
            ServiceRequestKind::FileCopy { .. } | ServiceRequestKind::FileFetch { .. } => {
                action == ServiceAction::Stop
            }
            ServiceRequestKind::FolderSync { .. } => matches!(
                action,
                ServiceAction::Pause | ServiceAction::Resume | ServiceAction::Terminate
            ),
        };
        if !supported {
            self.status_line = "that action is not supported for the selected service".to_string();
            return;
        }
        if action == ServiceAction::Stop
            && matches!(
                service.service.status,
                ServiceStatus::Stopped | ServiceStatus::Error
            )
        {
            self.status_line = "the selected service is already terminal".to_string();
            return;
        }
        let service_id = service.service.service_id.clone();
        let Some(session_token) = self.session_token.clone() else {
            self.status_line =
                "service actions require a managed shell opened with `beams ssh`".to_string();
            return;
        };
        let action_request = ServiceActionRequest {
            request_id: Uuid::new_v4().to_string(),
            service_id: service_id.clone(),
            action,
        };
        let id = self.next_id();
        self.pending_request = Some(json!({
            "request_id": id,
            "op": "request_service_action",
            "session_token": session_token,
            "client_id": self.client_id,
            "request": action_request,
        }));
        self.status_line = format!(
            "requesting local approval to {} {}",
            match action {
                ServiceAction::Stop => "stop",
                ServiceAction::Restart => "restart",
                ServiceAction::Pause => "pause",
                ServiceAction::Resume => "resume",
                ServiceAction::Terminate => "terminate",
            },
            service_id
        );
    }

    /// Scroll wheel moves whichever list/log the up/down arrow keys would
    /// already affect for the current focus (see
    /// `handle_template_list_key`/`handle_log_key`) rather than
    /// hit-testing the cursor's pane. A left click *does* hit-test -
    /// against `templates_area`/`progress_area`, both set at the end of
    /// their respective `draw_*` calls - and moves that list's selection
    /// to the clicked row, focusing that pane; it doesn't act on the
    /// clicked row (opening the param modal still needs `Enter`). Ignored
    /// entirely while the param modal is open - same as `handle_key`'s
    /// `editing_params` guard, so a click on the dimmed list behind the
    /// modal can't change its selection out from under it.
    pub fn handle_mouse(&mut self, mouse: ratatui::crossterm::event::MouseEvent) {
        use ratatui::crossterm::event::{MouseButton, MouseEventKind};

        if self.editing_params {
            return;
        }

        match mouse.kind {
            MouseEventKind::ScrollUp => match self.focus {
                Focus::Templates => self.handle_template_list_key(ratatui::crossterm::event::KeyCode::Up),
                Focus::Log => self.handle_log_key(ratatui::crossterm::event::KeyCode::Up),
            },
            MouseEventKind::ScrollDown => match self.focus {
                Focus::Templates => self.handle_template_list_key(ratatui::crossterm::event::KeyCode::Down),
                Focus::Log => self.handle_log_key(ratatui::crossterm::event::KeyCode::Down),
            },
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(row) = row_within(self.templates_area, mouse.column, mouse.row) {
                    if row < self.templates.len() {
                        self.selected_template = row;
                    }
                    self.focus = Focus::Templates;
                } else if let Some(line) = row_within(self.progress_area, mouse.column, mouse.row) {
                    // Each progress row is two lines tall - see `draw_progress`.
                    let row = self.progress_offset + line / PROGRESS_ROW_HEIGHT;
                    if !self.rows.is_empty() {
                        self.log_scroll = row.min(self.rows.len() - 1);
                    }
                    self.focus = Focus::Log;
                }
            }
            _ => {}
        }
    }

    fn handle_template_list_key(&mut self, code: ratatui::crossterm::event::KeyCode) {
        use ratatui::crossterm::event::KeyCode;
        match code {
            KeyCode::Up => {
                if self.selected_template > 0 {
                    self.selected_template -= 1;
                }
            }
            KeyCode::Down => {
                if self.selected_template + 1 < self.templates.len() {
                    self.selected_template += 1;
                }
            }
            KeyCode::Enter => {
                if let Some(template) = self.templates.get(self.selected_template) {
                    self.param_values = vec![String::new(); template.params.len()];
                    self.focused_param = 0;
                    self.editing_params = true;
                }
            }
            _ => {}
        }
    }

    fn handle_log_key(&mut self, code: ratatui::crossterm::event::KeyCode) {
        use ratatui::crossterm::event::KeyCode;
        match code {
            KeyCode::Up => self.log_scroll = self.log_scroll.saturating_sub(1),
            KeyCode::Down => self.log_scroll = self.log_scroll.saturating_add(1),
            _ => {}
        }
    }

    fn handle_param_key(&mut self, key: ratatui::crossterm::event::KeyEvent) {
        use ratatui::crossterm::event::KeyCode;
        let field_count = self.param_values.len();
        match key.code {
            KeyCode::Esc => {
                self.editing_params = false;
                self.focus = Focus::Templates;
            }
            KeyCode::Tab | KeyCode::Down => {
                if field_count > 0 {
                    self.focused_param = (self.focused_param + 1) % field_count;
                }
            }
            KeyCode::BackTab | KeyCode::Up => {
                if field_count > 0 {
                    self.focused_param = (self.focused_param + field_count - 1) % field_count;
                }
            }
            KeyCode::Backspace => {
                if let Some(value) = self.param_values.get_mut(self.focused_param) {
                    value.pop();
                }
            }
            KeyCode::Char(c) => {
                if let Some(value) = self.param_values.get_mut(self.focused_param) {
                    value.push(c);
                }
            }
            KeyCode::Enter => {
                if self.focused_param + 1 < field_count {
                    self.focused_param += 1;
                } else {
                    self.submit();
                }
            }
            _ => {}
        }
    }

    fn submit(&mut self) {
        let Some(template) = self.templates.get(self.selected_template).cloned() else { return };
        let mut params = serde_json::Map::new();
        for (schema, value) in template.params.iter().zip(self.param_values.iter()) {
            let json_value = if schema.kind == "number" {
                value.parse::<i64>().map(Value::from).unwrap_or_else(|_| Value::String(value.clone()))
            } else {
                Value::String(value.clone())
            };
            params.insert(schema.name.clone(), json_value);
        }

        let id = self.next_id();
        self.pending_request = Some(json!({
            "request_id": id,
            "op": "run_task",
            "task": template.template,
            "params": Value::Object(params),
        }));
        self.editing_params = false;
        self.focus = Focus::Templates;
        self.status_line = format!("submitting {}...", template.template);
    }

    /// Single vertical stack, no row ever split into side-by-side panes -
    /// the sidebar this runs in (a zellij `size="30%"` pane, see
    /// `shell/runtime/zellij/beams-launch.kdl`) is tall and narrow, so horizontal
    /// space is the scarce resource. Param entry used to be a second pane
    /// here, competing for half the already-narrow width; it's now
    /// `draw_param_modal`, drawn on top of everything else only while
    /// `editing_params`.
    pub fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let [tabs_area, content_area, status_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .areas(area);

        let tab = |label: &'static str, selected| {
            if selected {
                Span::styled(
                    label,
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD | Modifier::REVERSED),
                )
            } else {
                Span::raw(label)
            }
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                tab(" 1 Tasks ", self.tab == Tab::Tasks),
                Span::raw(" "),
                tab(" 2 Services ", self.tab == Tab::Services),
                Span::raw(" "),
                tab(" 3 Requests ", self.tab == Tab::Requests),
            ])),
            tabs_area,
        );

        match self.tab {
            Tab::Tasks => {
                let [templates_area, gauge_area, progress_area] = Layout::vertical([
                    Constraint::Percentage(40),
                    Constraint::Length(1),
                    Constraint::Min(0),
                ])
                .areas(content_area);
                self.draw_templates(frame, templates_area);
                self.draw_progress(frame, gauge_area, progress_area);
            }
            Tab::Services => self.draw_services(frame, content_area),
            Tab::Requests => self.draw_requests(frame, content_area),
        }
        frame.render_widget(
            Paragraph::new(self.status_line.as_str()).style(Style::default().fg(Color::DarkGray)),
            status_area,
        );

        if self.editing_params {
            self.draw_param_modal(frame, area);
        }
    }

    fn draw_services(&mut self, frame: &mut Frame, area: Rect) {
        let items = self
            .services
            .values()
            .map(|owned| {
                let service = &owned.service;
                let endpoint = match &service.kind {
                    beams_protocol::ServiceRequestKind::LocalPortForward {
                        remote_host,
                        remote_port,
                        local_port,
                    } => format!(
                        "127.0.0.1:{} -> {}:{}",
                        local_port
                            .map(|port| port.to_string())
                            .unwrap_or_else(|| "?".to_string()),
                        remote_host,
                        remote_port
                    ),
                    beams_protocol::ServiceRequestKind::FileCopy {
                        direction,
                        remote_path,
                        local_path_hint,
                        overwrite,
                    } => format!(
                        "{:?} {} <-> {} ({:?})",
                        direction,
                        local_path_hint.as_deref().unwrap_or("workstation path"),
                        remote_path,
                        overwrite
                    ),
                    beams_protocol::ServiceRequestKind::FileFetch { sources, .. } => {
                        let progress = service
                            .progress
                            .map(|p| format!(" {}/{} files", p.files_done, p.files_total))
                            .unwrap_or_default();
                        match sources.as_slice() {
                            [source] => format!("fetch {}{progress}", source.path),
                            _ => format!("fetch {} items{progress}", sources.len()),
                        }
                    }
                    beams_protocol::ServiceRequestKind::FolderSync {
                        remote_path,
                        local_path_hint,
                        mode,
                        ..
                    } => format!(
                        "sync {} <-> {} ({:?})",
                        local_path_hint.as_deref().unwrap_or("workstation directory"),
                        remote_path,
                        mode
                    ),
                };
                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!("{:?}", service.status),
                        Style::default().fg(service_status_color(service.status)),
                    ),
                    Span::raw(format!(
                        "  {endpoint}  owner={}{}",
                        short_id(&owned.owner_client_id),
                        service
                            .message
                            .as_deref()
                            .map(|message| format!("  {message}"))
                            .unwrap_or_default()
                    )),
                ]))
            })
            .collect::<Vec<_>>();
        let mut state = ListState::default();
        if !items.is_empty() {
            self.service_selection = self.service_selection.min(items.len() - 1);
            state.select(Some(self.service_selection));
        }
        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("services (s/r stop/restart, p/e pause/resume, t terminate; local approval required)"),
            )
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        frame.render_stateful_widget(list, area, &mut state);
    }

    fn draw_requests(&self, frame: &mut Frame, area: Rect) {
        let mut items = self
            .requests
            .values()
            .map(|request| {
                ListItem::new(format!(
                    "{:?}  create {}  owner={}{}",
                    request.status,
                    request.request.kind.label(),
                    short_id(&request.owner_client_id),
                    request
                        .message
                        .as_deref()
                        .map(|message| format!("  {message}"))
                        .unwrap_or_default()
                ))
            })
            .collect::<Vec<_>>();
        items.extend(self.action_requests.values().map(|request| {
            ListItem::new(format!(
                "{:?}  {:?} {}  owner={}{}",
                request.status,
                request.request.action,
                short_id(&request.request.service_id),
                short_id(&request.owner_client_id),
                request
                    .message
                    .as_deref()
                    .map(|message| format!("  {message}"))
                    .unwrap_or_default()
            ))
        }));
        frame.render_widget(
            List::new(items).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("requests (remote proposals and local decisions; r refreshes)"),
            ),
            area,
        );
    }

    fn draw_templates(&mut self, frame: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = self
            .templates
            .iter()
            .map(|t| ListItem::new(t.template.clone()))
            .collect();
        let mut list_state = ListState::default().with_selected(Some(self.selected_template));
        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("tasks (↑/↓ or click, enter to fill in params)")
                    .border_style(focus_style(self.focus == Focus::Templates)),
            )
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        frame.render_stateful_widget(list, area, &mut list_state);
        self.templates_area = area;
    }

    fn draw_progress(&mut self, frame: &mut Frame, gauge_area: Rect, list_area: Rect) {
        let total = self.rows.len();
        let done = self.rows.iter().filter(|r| r.state.is_terminal()).count();
        let running = self.rows.iter().filter(|r| r.state == TaskState::Running).count();
        let ratio = if total == 0 { 0.0 } else { done as f64 / total as f64 };
        let gauge = Gauge::default()
            .block(Block::default().borders(Borders::NONE))
            .gauge_style(Style::default().fg(Color::Green))
            .ratio(ratio)
            .label(format!("{done}/{total} done, {running} running"));
        frame.render_widget(gauge, gauge_area);

        // Two lines per row: status + task id, then the full-width
        // progress/result message (or what a pending row waits on), so a
        // long message isn't cut off behind a long task id.
        let spinner_frame = self.spinner_frame;
        let items: Vec<ListItem> = self
            .rows
            .iter()
            .map(|row| {
                let header = Line::from(vec![
                    Span::styled(row.state.glyph(spinner_frame), Style::default().fg(row.state.color())),
                    Span::raw(" "),
                    Span::styled(
                        row.label.clone().unwrap_or_else(|| abbreviate_home(&row.task_id)),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                ]);
                let detail_style = match row.state {
                    TaskState::Error | TaskState::Blocked => Style::default().fg(row.state.color()),
                    _ => Style::default().fg(Color::Gray),
                };
                let detail = Line::from(vec![Span::raw("  "), Span::styled(self.row_detail(row), detail_style)]);
                ListItem::new(vec![header, detail])
            })
            .collect();
        let list = List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .title("progress")
                .border_style(focus_style(self.focus == Focus::Log)),
        );
        let mut list_state = ListState::default().with_offset(self.progress_offset);
        if !self.rows.is_empty() {
            let selected = self.log_scroll.min(self.rows.len() - 1);
            self.log_scroll = selected;
            list_state.select(Some(selected));
        }
        frame.render_stateful_widget(list, list_area, &mut list_state);
        self.progress_area = list_area;
        self.progress_offset = list_state.offset();
    }

    /// Centered popup over everything else, replacing the old side-by-side
    /// params pane - see `draw`'s doc comment. `Clear` blanks out whatever
    /// `draw_templates`/`draw_progress` already painted into this `Rect`
    /// so the popup doesn't show through it.
    fn draw_param_modal(&mut self, frame: &mut Frame, area: Rect) {
        let Some(template) = self.templates.get(self.selected_template) else { return };
        let height = (template.params.len() as u16 + 2).max(3);
        let popup = centered_rect(area, 70, height);

        frame.render_widget(Clear, popup);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(
                "{} params (tab to move, enter on last field to submit, esc to cancel)",
                template.template
            ))
            .border_style(Style::default().fg(Color::Cyan));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);

        if template.params.is_empty() {
            frame.render_widget(Paragraph::new("(no params - press enter to run)"), inner);
            return;
        }

        let rows =
            Layout::vertical(template.params.iter().map(|_| Constraint::Length(1)).collect::<Vec<_>>())
                .split(inner);
        for (i, (schema, row_area)) in template.params.iter().zip(rows.iter()).enumerate() {
            let value = self.param_values.get(i).map(String::as_str).unwrap_or("");
            let focused = self.focused_param == i;
            let marker = if schema.required { "*" } else { " " };
            let line = Line::from(vec![
                Span::styled(format!("{marker}{:<10}", schema.name), Style::default().fg(Color::Cyan)),
                Span::raw(format!(" [{}] ", schema.kind)),
                Span::styled(
                    value,
                    if focused { Style::default().add_modifier(Modifier::UNDERLINED) } else { Style::default() },
                ),
                if focused { Span::raw("▏") } else { Span::raw("") },
            ]);
            frame.render_widget(Paragraph::new(line), *row_area);
        }
    }
}

/// `$HOME/...` -> `~/...` anywhere in a task id (e.g. a clone's
/// `dest=/home/beams/projects/core`), purely for display - the sidebar is
/// narrow and the home prefix carries no information.
fn abbreviate_home(task_id: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && home != "/" => task_id.replace(&format!("{home}/"), "~/"),
        _ => task_id.to_string(),
    }
}

/// Lines per row in the progress list - see `draw_progress`.
const PROGRESS_ROW_HEIGHT: usize = 2;

fn focus_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    }
}

fn service_status_color(status: beams_protocol::ServiceStatus) -> Color {
    use beams_protocol::ServiceStatus;
    match status {
        ServiceStatus::Running => Color::Green,
        ServiceStatus::Paused => Color::Blue,
        ServiceStatus::Starting | ServiceStatus::Stopping => Color::Yellow,
        ServiceStatus::Stopped | ServiceStatus::Unknown => Color::DarkGray,
        ServiceStatus::Degraded => Color::Magenta,
        ServiceStatus::Error => Color::Red,
    }
}

fn short_id(value: &str) -> &str {
    value.get(..8).unwrap_or(value)
}

/// A `width_percent`-wide, `height`-tall `Rect` centered within `area` -
/// the usual ratatui modal-popup pattern (clamped to `area` so a huge
/// `height` - e.g. a template with far too many params to ever fit -
/// still renders something instead of panicking on an out-of-bounds
/// `Rect`).
fn centered_rect(area: Rect, width_percent: u16, height: u16) -> Rect {
    let width = area.width.saturating_mul(width_percent) / 100;
    let height = height.min(area.height);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect { x, y, width, height }
}

/// `None` if `(column, row)` falls outside `area` or on its border;
/// otherwise `Some(index)` into whatever list was drawn there, index 0
/// being the first row *inside* the border - matches a bordered `List`
/// widget's own rendering (row 0 of `area` is the top border, row 1 is
/// the first item). A zero-sized `area` (nothing drawn there yet, e.g.
/// before the first `draw`) always returns `None`.
fn row_within(area: Rect, column: u16, row: u16) -> Option<usize> {
    if area.width == 0 || area.height == 0 {
        return None;
    }
    if column < area.x || column >= area.x + area.width {
        return None;
    }
    // Both the top and bottom border rows are excluded - `row > area.y`
    // (not `>=`) skips the top border, and the `area.height - 1` bound
    // below skips the bottom one.
    if row <= area.y || row >= area.y + area.height.saturating_sub(1) {
        return None;
    }
    Some((row - area.y - 1) as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::{KeyCode, KeyEvent};

    fn key(code: KeyCode) -> ratatui::crossterm::event::KeyEvent {
        KeyEvent::new(code, ratatui::crossterm::event::KeyModifiers::NONE)
    }

    fn sample_templates() -> Value {
        json!({"type": "tasks", "tasks": [
            {"template": "clone-git-repo", "params": [
                {"name": "repo", "kind": "string", "required": true},
                {"name": "dest", "kind": "path", "required": true},
            ]},
            {"template": "checkout-pr-worktree", "params": [
                {"name": "base", "kind": "path", "required": true},
                {"name": "pr", "kind": "number", "required": true},
                {"name": "dest", "kind": "path", "required": true},
            ]},
        ]})
    }

    #[test]
    fn tasks_event_populates_templates() {
        let mut app = App::new();
        app.handle_daemon_event(sample_templates());
        assert_eq!(app.templates.len(), 2);
        assert_eq!(app.templates[0].template, "clone-git-repo");
        assert_eq!(app.templates[1].params.len(), 3);
    }

    #[test]
    fn run_once_templates_are_hidden_from_the_template_list() {
        let mut app = App::new();
        app.handle_daemon_event(json!({"type": "tasks", "tasks": [
            {"template": "clone-git-repo", "params": []},
            {"template": "install-toolchain-go", "params": [], "run_once": true},
        ]}));
        let names: Vec<&str> = app.templates.iter().map(|t| t.template.as_str()).collect();
        assert_eq!(names, vec!["clone-git-repo"]);
    }

    #[test]
    fn accepted_with_task_id_creates_a_pending_row() {
        let mut app = App::new();
        app.handle_daemon_event(json!({"type": "accepted", "task_id": "clone-git-repo:dest=/x"}));
        assert_eq!(app.rows.len(), 1);
        assert_eq!(app.rows[0].task_id, "clone-git-repo:dest=/x");
        assert!(app.rows[0].state == TaskState::Pending);
    }

    fn plan(entries: &[(&str, &[&str])]) -> Value {
        let tasks: Vec<Value> =
            entries.iter().map(|(id, deps)| json!({"task_id": id, "depends_on": deps})).collect();
        json!({"type": "accepted", "manifest_id": "manifest-0", "tasks": tasks})
    }

    #[test]
    fn accepted_plan_creates_one_pending_row_per_task_with_its_dependencies() {
        let mut app = App::new();
        app.handle_daemon_event(plan(&[("a", &[]), ("b", &["a"]), ("c", &["a"])]));
        assert_eq!(app.rows.len(), 3);
        assert!(app.rows.iter().all(|r| r.state == TaskState::Pending));
        assert_eq!(app.rows[1].depends_on, vec!["a".to_string()]);
    }

    #[test]
    fn a_pending_rows_detail_names_only_unfinished_dependencies() {
        let mut app = App::new();
        app.handle_daemon_event(plan(&[("a", &[]), ("b", &[]), ("c", &["a", "b"])]));
        assert_eq!(app.row_detail(&app.rows[2]), "waiting on a, b");

        app.handle_daemon_event(json!({"type": "task_status", "task_id": "a", "status": "ok", "message": "done"}));
        assert_eq!(app.row_detail(&app.rows[2]), "waiting on b");
        assert_eq!(app.row_detail(&app.rows[0]), "done");

        app.handle_daemon_event(json!({"type": "task_status", "task_id": "b", "status": "skip"}));
        assert_eq!(app.row_detail(&app.rows[2]), "queued");
    }

    #[test]
    fn a_blocked_status_marks_the_row_terminal_with_its_reason() {
        let mut app = App::new();
        app.handle_daemon_event(plan(&[("a", &[]), ("b", &["a"])]));
        app.handle_daemon_event(json!({"type": "task_status", "task_id": "a", "status": "error", "message": "boom"}));
        app.handle_daemon_event(json!({
            "type": "task_status", "task_id": "b", "status": "blocked",
            "message": "not run: depends on a, which failed",
        }));
        assert!(app.rows[1].state == TaskState::Blocked);
        assert!(app.rows[1].state.is_terminal());
        assert_eq!(app.row_detail(&app.rows[1]), "not run: depends on a, which failed");
    }

    #[test]
    fn task_status_transitions_an_existing_row_in_place() {
        let mut app = App::new();
        app.handle_daemon_event(plan(&[("a", &[])]));
        app.handle_daemon_event(json!({"type": "task_status", "task_id": "a", "status": "start"}));
        assert!(app.rows[0].state == TaskState::Running);

        app.handle_daemon_event(json!({"type": "task_status", "task_id": "a", "status": "ok", "message": "done"}));
        assert!(app.rows[0].state == TaskState::Ok);
        assert_eq!(app.rows[0].message.as_deref(), Some("done"));
        assert_eq!(app.rows.len(), 1, "same task_id should update in place, not duplicate");
    }

    #[test]
    fn resubmitting_a_finished_task_resets_its_row() {
        let mut app = App::new();
        app.handle_daemon_event(json!({"type": "accepted", "task_id": "a"}));
        app.handle_daemon_event(json!({"type": "task_status", "task_id": "a", "status": "ok", "message": "done"}));
        app.handle_daemon_event(json!({"type": "accepted", "task_id": "a"}));
        assert!(app.rows[0].state == TaskState::Pending);
        assert_eq!(app.rows[0].message, None);
    }

    #[test]
    fn template_list_navigation_stays_in_bounds() {
        let mut app = App::new();
        app.handle_daemon_event(sample_templates());

        app.handle_key(key(KeyCode::Up)); // already at 0, should clamp
        assert_eq!(app.selected_template, 0);

        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.selected_template, 1);
        app.handle_key(key(KeyCode::Down)); // only 2 templates, should clamp
        assert_eq!(app.selected_template, 1);

        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.selected_template, 0);
    }

    #[test]
    fn enter_on_a_template_opens_param_editing_with_one_field_per_param() {
        let mut app = App::new();
        app.handle_daemon_event(sample_templates());
        app.handle_key(key(KeyCode::Enter));
        assert!(app.editing_params);
        assert_eq!(app.param_values.len(), 2); // clone-git-repo has 2 params
        // Param entry is a modal now, not a separate focusable pane (see
        // `Focus`'s doc comment) - focus stays wherever it already was.
        assert!(app.focus == Focus::Templates);
    }

    #[test]
    fn typing_and_tab_fills_fields_then_submits_on_enter_at_the_last_field() {
        let mut app = App::new();
        app.handle_daemon_event(sample_templates());
        app.handle_key(key(KeyCode::Enter)); // open clone-git-repo's form

        for c in "myrepo".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Tab));
        for c in "/tmp/dest".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Enter)); // last field - submits

        assert!(!app.editing_params, "should return to the template list after submit");
        let request = app.pending_request.take().expect("submit should queue a request");
        assert_eq!(request["op"], "run_task");
        assert_eq!(request["task"], "clone-git-repo");
        assert_eq!(request["params"]["repo"], "myrepo");
        assert_eq!(request["params"]["dest"], "/tmp/dest");
    }

    #[test]
    fn a_number_param_is_sent_as_a_json_number_not_a_string() {
        let mut app = App::new();
        app.handle_daemon_event(sample_templates());
        app.selected_template = 1; // checkout-pr-worktree: base, pr (number), dest
        app.handle_key(key(KeyCode::Enter));

        for c in "/tmp/base".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Tab));
        for c in "1234".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Tab));
        for c in "/tmp/worktree".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Enter));

        let request = app.pending_request.take().unwrap();
        assert_eq!(request["params"]["pr"], 1234);
        assert!(request["params"]["pr"].is_number());
    }

    #[test]
    fn backspace_removes_the_last_character_of_the_focused_field() {
        let mut app = App::new();
        app.handle_daemon_event(sample_templates());
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(key(KeyCode::Char('x')));
        app.handle_key(key(KeyCode::Char('y')));
        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(app.param_values[0], "x");
    }

    #[test]
    fn esc_while_editing_params_cancels_without_submitting() {
        let mut app = App::new();
        app.handle_daemon_event(sample_templates());
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(key(KeyCode::Char('x')));
        app.handle_key(key(KeyCode::Esc));
        assert!(!app.editing_params);
        assert!(app.focus == Focus::Templates);
        assert!(app.pending_request.is_none());
    }

    #[test]
    fn mark_disconnected_sets_the_flag_and_a_visible_status() {
        let mut app = App::new();
        app.disconnected = false;
        app.mark_disconnected();
        assert!(app.disconnected);
        assert!(app.status_line.contains("reconnecting"));
    }

    #[test]
    fn mark_reconnected_clears_the_flag_and_requests_the_task_list_again() {
        let mut app = App::new();
        app.mark_disconnected();
        app.pending_request = None; // simulate it having already been sent/dropped

        app.mark_reconnected();

        assert!(!app.disconnected);
        assert_eq!(app.status_line, "reconnected");
        let request = app.pending_request.expect("mark_reconnected should re-request the task list");
        assert_eq!(request["op"], "list_tasks");
    }

    fn left_click(column: u16, row: u16) -> ratatui::crossterm::event::MouseEvent {
        ratatui::crossterm::event::MouseEvent {
            kind: ratatui::crossterm::event::MouseEventKind::Down(ratatui::crossterm::event::MouseButton::Left),
            column,
            row,
            modifiers: ratatui::crossterm::event::KeyModifiers::NONE,
        }
    }

    #[test]
    fn row_within_excludes_borders_and_out_of_bounds_clicks() {
        let area = Rect { x: 2, y: 5, width: 10, height: 4 }; // rows 5..9: 5=top border, 6-7=items, 8=bottom border
        assert_eq!(row_within(area, 4, 5), None, "top border row");
        assert_eq!(row_within(area, 4, 8), None, "bottom border row");
        assert_eq!(row_within(area, 4, 6), Some(0), "first item row");
        assert_eq!(row_within(area, 4, 7), Some(1), "second item row");
        assert_eq!(row_within(area, 1, 6), None, "column just left of the area");
        assert_eq!(row_within(Rect::default(), 0, 0), None, "zero-sized area never matches");
    }

    #[test]
    fn clicking_a_template_row_selects_it_and_focuses_that_pane() {
        let mut app = App::new();
        app.handle_daemon_event(sample_templates());
        app.focus = Focus::Log;
        // Matches what `draw_templates` would have set: a bordered list
        // starting at (0, 0), so row 0 is the border and row 2 is the
        // second item.
        app.templates_area = Rect { x: 0, y: 0, width: 40, height: 6 };

        app.handle_mouse(left_click(5, 2));

        assert_eq!(app.selected_template, 1);
        assert!(app.focus == Focus::Templates);
    }

    #[test]
    fn clicking_the_progress_list_moves_its_scroll_and_focuses_it() {
        let mut app = App::new();
        app.handle_daemon_event(plan(&[("a", &[]), ("b", &[]), ("c", &[])]));
        app.focus = Focus::Templates;
        app.progress_area = Rect { x: 0, y: 10, width: 40, height: 8 };

        // Rows are two lines tall: line 0-1 inside the border is "a",
        // 2-3 is "b", 4-5 is "c" - y=16 is line 5, c's detail line.
        app.handle_mouse(left_click(5, 16));
        assert_eq!(app.log_scroll, 2);
        assert!(app.focus == Focus::Log);

        app.handle_mouse(left_click(5, 13)); // line 2: "b"'s header
        assert_eq!(app.log_scroll, 1);

        app.progress_offset = 1; // list scrolled down by one row
        app.handle_mouse(left_click(5, 11)); // line 0, now "b"
        assert_eq!(app.log_scroll, 1);
        assert!(app.focus == Focus::Log);
    }

    #[test]
    fn a_click_outside_either_list_changes_nothing() {
        let mut app = App::new();
        app.handle_daemon_event(sample_templates());
        app.templates_area = Rect { x: 0, y: 0, width: 40, height: 6 };
        app.progress_area = Rect { x: 0, y: 10, width: 40, height: 6 };
        app.focus = Focus::Templates;

        app.handle_mouse(left_click(5, 8)); // the gap between the two areas

        assert_eq!(app.selected_template, 0);
        assert!(app.focus == Focus::Templates);
    }

    #[test]
    fn a_click_is_ignored_entirely_while_the_param_modal_is_open() {
        let mut app = App::new();
        app.handle_daemon_event(sample_templates());
        app.templates_area = Rect { x: 0, y: 0, width: 40, height: 6 };
        app.handle_key(key(KeyCode::Enter)); // opens the modal on template 0
        assert!(app.editing_params);

        app.handle_mouse(left_click(5, 2)); // would otherwise select template 1

        assert_eq!(app.selected_template, 0, "click behind the modal shouldn't change the underlying selection");
        assert!(app.editing_params, "click shouldn't close the modal either");
    }

    #[test]
    fn a_progress_status_updates_the_message_without_changing_an_existing_rows_state() {
        let mut app = App::new();
        app.handle_daemon_event(json!({"type": "accepted", "task_id": "clone-git-repo:dest=/x"}));
        app.handle_daemon_event(
            json!({"type": "task_status", "task_id": "clone-git-repo:dest=/x", "status": "start"}),
        );
        assert!(app.rows[0].state == TaskState::Running);

        app.handle_daemon_event(json!({
            "type": "task_status",
            "task_id": "clone-git-repo:dest=/x",
            "status": "progress",
            "message": "Receiving objects: 50%",
        }));

        assert!(app.rows[0].state == TaskState::Running, "progress shouldn't change state");
        assert_eq!(app.rows[0].message.as_deref(), Some("Receiving objects: 50%"));
        assert_eq!(app.rows.len(), 1, "should update the existing row, not duplicate it");
    }

    #[test]
    fn a_progress_status_for_an_unknown_task_id_creates_a_running_row() {
        let mut app = App::new();
        app.handle_daemon_event(json!({
            "type": "task_status",
            "task_id": "clone-git-repo:dest=/y",
            "status": "progress",
            "message": "Receiving objects: 10%",
        }));

        assert_eq!(app.rows.len(), 1);
        assert!(app.rows[0].state == TaskState::Running);
        assert_eq!(app.rows[0].message.as_deref(), Some("Receiving objects: 10%"));
    }

    #[test]
    fn client_state_populates_services_and_requests_tabs() {
        let mut app = App::new();
        app.handle_daemon_event(json!({
            "type": "client_state",
            "services": [{
                "owner_client_id": "client-a",
                "service": {
                    "service_id": "service-a",
                    "request_id": "request-a",
                    "kind": {
                        "kind": "local_port_forward",
                        "remote_host": "127.0.0.1",
                        "remote_port": 8080,
                        "local_port": 18080
                    },
                    "status": "running",
                    "message": "ready"
                }
            }],
            "requests": [{
                "request": {
                    "request_id": "request-a",
                    "kind": {
                        "kind": "local_port_forward",
                        "remote_host": "127.0.0.1",
                        "remote_port": 8080,
                        "local_port": 18080
                    }
                },
                "owner_client_id": "client-a",
                "bridge_id": "bridge-a",
                "status": "running",
                "message": "ready"
            }],
            "action_requests": []
        }));

        assert_eq!(app.services.len(), 1);
        assert_eq!(app.requests.len(), 1);
        app.handle_key(key(KeyCode::Char('2')));
        assert_eq!(app.tab, Tab::Services);
        app.handle_key(key(KeyCode::Char('3')));
        assert_eq!(app.tab, Tab::Requests);
    }

    #[test]
    fn remote_service_action_is_a_typed_approval_request() {
        let mut app = App::new();
        app.tab = Tab::Services;
        app.session_token = Some("managed-token".to_string());
        app.client_id = Some("client-a".to_string());
        app.handle_daemon_event(json!({
            "type": "client_service_status",
            "service": {
                "owner_client_id": "client-a",
                "service": {
                    "service_id": "service-a",
                    "request_id": "request-a",
                    "kind": {
                        "kind": "local_port_forward",
                        "remote_host": "127.0.0.1",
                        "remote_port": 8080,
                        "local_port": 18080
                    },
                    "status": "running"
                }
            }
        }));

        app.handle_key(key(KeyCode::Char('s')));

        let request = app.pending_request.take().unwrap();
        assert_eq!(request["op"], "request_service_action");
        assert_eq!(request["session_token"], "managed-token");
        assert_eq!(request["request"]["service_id"], "service-a");
        assert_eq!(request["request"]["action"], "stop");
    }

    fn launch_snapshot(seq: u64, ready: &[&str], released: &[&str]) -> Value {
        json!({"type": "launch_snapshot", "request_id": "9", "seq": seq, "launches": [{
            "launch_id": "L1", "plan_digest": "d", "gate": "interactive", "status": "running",
            "ready": ready, "released": released,
            "nodes": [
                {"id": "artifact-ready:pre", "label": "Pre bundle", "phase": "pre", "status": "ok", "message": null, "log": []},
                {"id": "interface-ready", "label": "Shell and first TUI frame ready", "phase": "post", "status": "waiting",
                 "message": "waiting for the TUI to start", "log": []},
                {"id": "tasks:configure", "label": "Configure", "phase": "post", "status": "pending", "message": null, "log": []},
            ],
        }]})
    }

    fn row<'a>(app: &'a App, task_id: &str) -> &'a TaskRow {
        &app.rows[app.row_index(task_id).unwrap_or_else(|| panic!("no row {task_id}"))]
    }

    #[test]
    fn a_launch_snapshot_shows_every_node_including_pending_post_work() {
        let mut app = App::new();
        // A live event that arrived before the snapshot is replaced by it.
        app.handle_daemon_event(json!({"type": "task_status", "task_id": "launch:L1:artifact-ready:pre",
            "status": "start", "launch_id": "L1", "seq": 3}));
        app.handle_daemon_event(launch_snapshot(5, &["shell"], &["artifact-ready:pre"]));
        assert_eq!(app.rows.len(), 3);
        assert!(row(&app, "launch:L1:artifact-ready:pre").state == TaskState::Ok);
        let interface = row(&app, "launch:L1:interface-ready");
        assert!(interface.state == TaskState::Waiting);
        assert_eq!(interface.message.as_deref(), Some("waiting for the TUI to start"));
        let configure = row(&app, "launch:L1:tasks:configure");
        assert!(configure.state == TaskState::Pending);
        assert_eq!(configure.label.as_deref(), Some("post · Configure"));

        // Events the snapshot already covers are ignored; later ones apply.
        app.handle_daemon_event(json!({"type": "task_status", "task_id": "launch:L1:tasks:configure",
            "status": "start", "launch_id": "L1", "seq": 5}));
        assert!(row(&app, "launch:L1:tasks:configure").state == TaskState::Pending);
        app.handle_daemon_event(json!({"type": "task_status", "task_id": "launch:L1:tasks:configure",
            "status": "start", "launch_id": "L1", "seq": 6}));
        assert!(row(&app, "launch:L1:tasks:configure").state == TaskState::Running);

        // A fresh snapshot replaces the launch's rows rather than adding more.
        app.handle_daemon_event(launch_snapshot(9, &["shell", "tui"], &["artifact-ready:pre", "interface-ready"]));
        assert_eq!(app.rows.len(), 3);
    }

    #[test]
    fn waiting_blocked_and_interrupted_launch_nodes_map_to_their_own_states() {
        let mut app = App::new();
        app.handle_daemon_event(json!({"type": "task_status", "task_id": "launch:L1:artifact-ready:post",
            "status": "progress", "waiting": true, "message": "waiting for the post bundle", "launch_id": "L1", "seq": 1}));
        assert!(row(&app, "launch:L1:artifact-ready:post").state == TaskState::Waiting);
        app.handle_daemon_event(json!({"type": "launch_nodes_blocked", "launch_id": "L1", "phase": "post",
            "nodes": ["tasks:a", "tasks:b"], "message": "not run: pre did not complete", "seq": 2}));
        assert!(row(&app, "launch:L1:tasks:b").state == TaskState::Blocked);
        assert_eq!(row(&app, "launch:L1:tasks:a").message.as_deref(), Some("not run: pre did not complete"));
        assert_eq!(TaskState::from_node_status("interrupted"), Some(TaskState::Interrupted));
        assert!(TaskState::Interrupted.is_terminal() && !TaskState::Waiting.is_terminal());
    }

    #[test]
    fn requests_a_snapshot_at_start_after_reconnecting_and_when_a_launch_is_accepted() {
        let mut app = App::new();
        app.pending_request = None;
        app.queue_deferred();
        assert_eq!(app.pending_request.take().unwrap()["op"], "launch_snapshot");
        app.queue_deferred();
        assert!(app.pending_request.is_none(), "only once");

        app.handle_daemon_event(json!({"type": "launch_status", "launch_id": "L2", "status": "accepted", "seq": 1}));
        app.queue_deferred();
        assert_eq!(app.pending_request.take().unwrap()["op"], "launch_snapshot");

        app.handle_daemon_event(launch_snapshot(20, &[], &[]));
        app.mark_disconnected();
        app.mark_reconnected();
        app.pending_request = None; // the task list request
        app.queue_deferred();
        assert_eq!(app.pending_request.take().unwrap()["op"], "launch_snapshot");
        // The restarted daemon's numbering starts over.
        app.handle_daemon_event(json!({"type": "task_status", "task_id": "launch:L1:tasks:configure",
            "status": "start", "launch_id": "L1", "seq": 1}));
        assert!(row(&app, "launch:L1:tasks:configure").state == TaskState::Running);
    }

    #[test]
    fn reports_tui_readiness_for_its_own_launch_once_after_a_frame() {
        let mut app = App::new();
        app.pending_request = None;
        app.needs_launch_snapshot = false;
        app.launch_id = Some("L1".into());
        app.handle_daemon_event(launch_snapshot(5, &["shell"], &["artifact-ready:pre"]));
        app.queue_deferred();
        assert!(app.pending_request.is_none(), "not before a frame is drawn");
        app.frame_drawn();
        app.queue_deferred();
        let request = app.pending_request.take().unwrap();
        assert_eq!(request["op"], "launch_ready");
        assert_eq!(request["launch_id"], "L1");
        assert_eq!(request["component"], "tui");
        app.frame_drawn();
        app.queue_deferred();
        assert!(app.pending_request.is_none(), "only once");

        // A later snapshot that already has it doesn't ask again...
        app.handle_daemon_event(launch_snapshot(6, &["shell", "tui"], &["interface-ready"]));
        app.frame_drawn();
        app.queue_deferred();
        assert!(app.pending_request.is_none());
        // ...but one from a daemon that lost it (after a restart) does.
        app.handle_daemon_event(launch_snapshot(7, &["shell"], &[]));
        app.frame_drawn();
        app.queue_deferred();
        assert_eq!(app.pending_request.take().unwrap()["op"], "launch_ready");
    }

    #[test]
    fn a_tui_outside_a_launch_reports_nothing() {
        let mut app = App::new();
        app.pending_request = None;
        app.needs_launch_snapshot = false;
        app.launch_id = None;
        app.handle_daemon_event(launch_snapshot(5, &[], &[]));
        app.frame_drawn();
        app.queue_deferred();
        assert!(app.pending_request.is_none());
    }
}
