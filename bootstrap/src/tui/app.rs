//! TUI state and rendering. Deliberately its own module from `client.rs`
//! (transport) and `mod.rs` (terminal setup/event loop) - this is just
//! "given what the daemon has told us so far, and what's been typed,
//! what does the screen look like and what does a keypress do".

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Gauge, List, ListItem, ListState, Paragraph};
use ratatui::Frame;
use serde::Deserialize;
use serde_json::{json, Value};

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
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Pending,
    Running,
    Ok,
    Skip,
    Error,
}

impl TaskState {
    fn is_terminal(self) -> bool {
        matches!(self, TaskState::Ok | TaskState::Skip | TaskState::Error)
    }

    fn glyph(self, spinner_frame: usize) -> &'static str {
        const SPINNER: [&str; 4] = ["⠋", "⠙", "⠸", "⠴"];
        match self {
            TaskState::Pending => "·",
            TaskState::Running => SPINNER[spinner_frame % SPINNER.len()],
            TaskState::Ok => "✔",
            TaskState::Skip => "○",
            TaskState::Error => "✘",
        }
    }

    fn color(self) -> Color {
        match self {
            TaskState::Pending => Color::DarkGray,
            TaskState::Running => Color::Yellow,
            TaskState::Ok => Color::Green,
            TaskState::Skip => Color::Blue,
            TaskState::Error => Color::Red,
        }
    }
}

pub struct TaskRow {
    pub task_id: String,
    pub state: TaskState,
    pub message: Option<String>,
    pub wave: Option<u64>,
}

#[derive(PartialEq, Eq)]
pub enum Focus {
    Templates,
    Params,
    Log,
}

/// Which action a submitted set of params should turn into. Kept tiny
/// on purpose: the TUI only ever asks the daemon to run exactly one
/// task at a time today (`run_task`), same single-task granularity as
/// `beam-bootstrap run-task`. Building and submitting a multi-task
/// manifest from the TUI (e.g. selecting several templates at once) is
/// a real gap, not implemented yet - see `TODO.md`.
pub struct App {
    pub templates: Vec<TemplateInfo>,
    pub selected_template: usize,

    pub editing_params: bool,
    pub param_values: Vec<String>,
    pub focused_param: usize,

    pub rows: Vec<TaskRow>,
    pub log_scroll: usize,

    pub focus: Focus,
    pub status_line: String,
    pub spinner_frame: usize,

    next_request_id: u64,
    pub should_quit: bool,
    /// Set by the key handler when a request should be sent; drained by
    /// `mod.rs`'s event loop right after, so `App` itself never needs a
    /// `&mut DaemonClient`.
    pub pending_request: Option<Value>,
}

impl App {
    pub fn new() -> Self {
        App {
            templates: Vec::new(),
            selected_template: 0,
            editing_params: false,
            param_values: Vec::new(),
            focused_param: 0,
            rows: Vec::new(),
            log_scroll: 0,
            focus: Focus::Templates,
            status_line: "connecting...".to_string(),
            spinner_frame: 0,
            next_request_id: 1,
            should_quit: false,
            pending_request: None,
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

    fn row_index(&self, task_id: &str) -> Option<usize> {
        self.rows.iter().position(|r| r.task_id == task_id)
    }

    fn upsert_row(&mut self, task_id: &str, state: TaskState, message: Option<String>, wave: Option<u64>) {
        match self.row_index(task_id) {
            Some(i) => {
                self.rows[i].state = state;
                if message.is_some() {
                    self.rows[i].message = message;
                }
                if wave.is_some() {
                    self.rows[i].wave = wave;
                }
            }
            None => self.rows.push(TaskRow { task_id: task_id.to_string(), state, message, wave }),
        }
    }

    /// Handles one line already parsed off the daemon socket - see
    /// `daemon.rs`'s module doc comment for the shapes being matched
    /// here.
    pub fn handle_daemon_event(&mut self, value: Value) {
        match value.get("type").and_then(Value::as_str) {
            Some("tasks") => {
                if let Some(templates) = value.get("tasks").and_then(|v| serde_json::from_value(v.clone()).ok())
                {
                    self.templates = templates;
                    self.status_line = format!("{} template(s) available", self.templates.len());
                }
            }
            Some("accepted") => {
                if let Some(task_id) = value.get("task_id").and_then(Value::as_str) {
                    self.upsert_row(task_id, TaskState::Pending, None, None);
                    self.status_line = format!("submitted {task_id}");
                } else if let Some(waves) = value.get("waves").and_then(Value::as_array) {
                    for (wave, ids) in waves.iter().enumerate() {
                        for id in ids.as_array().into_iter().flatten() {
                            if let Some(id) = id.as_str() {
                                self.upsert_row(id, TaskState::Pending, None, Some(wave as u64));
                            }
                        }
                    }
                    self.status_line = "manifest submitted".to_string();
                }
            }
            Some("task_status") => {
                let Some(task_id) = value.get("task_id").and_then(Value::as_str) else { return };
                let status = value.get("status").and_then(Value::as_str).unwrap_or_default();
                let message = value.get("message").and_then(Value::as_str).map(str::to_string);
                let wave = value.get("wave").and_then(Value::as_u64);
                let state = match status {
                    "start" => TaskState::Running,
                    "ok" => TaskState::Ok,
                    "skip" => TaskState::Skip,
                    "error" => TaskState::Error,
                    _ => return,
                };
                self.upsert_row(task_id, state, message, wave);
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
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Templates | Focus::Params => Focus::Log,
                    Focus::Log => Focus::Templates,
                };
            }
            KeyCode::Char('r') => self.request_task_list(),
            _ => match self.focus {
                Focus::Templates => self.handle_template_list_key(key.code),
                Focus::Log => self.handle_log_key(key.code),
                Focus::Params => {}
            },
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
                    self.focus = Focus::Params;
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

    pub fn draw(&mut self, frame: &mut Frame) {
        let [top, bottom] =
            Layout::vertical([Constraint::Percentage(35), Constraint::Percentage(65)]).areas(frame.area());
        self.draw_top(frame, top);
        self.draw_bottom(frame, bottom);
    }

    fn draw_top(&mut self, frame: &mut Frame, area: Rect) {
        let [list_area, form_area] =
            Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)]).areas(area);

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
                    .title("tasks (↑/↓, enter to fill in params)")
                    .border_style(focus_style(self.focus == Focus::Templates)),
            )
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        frame.render_stateful_widget(list, list_area, &mut list_state);

        let form_block = Block::default()
            .borders(Borders::ALL)
            .title("params (tab to move, enter on last field to submit, esc to cancel)")
            .border_style(focus_style(self.focus == Focus::Params));
        let inner = form_block.inner(form_area);
        frame.render_widget(form_block, form_area);

        if let Some(template) = self.templates.get(self.selected_template) {
            if template.params.is_empty() {
                frame.render_widget(Paragraph::new("(no params - press enter to run)"), inner);
            } else {
                let rows = Layout::vertical(
                    template.params.iter().map(|_| Constraint::Length(1)).collect::<Vec<_>>(),
                )
                .split(inner);
                for (i, (schema, area)) in template.params.iter().zip(rows.iter()).enumerate() {
                    let value = self.param_values.get(i).map(String::as_str).unwrap_or("");
                    let focused = self.editing_params && self.focused_param == i;
                    let marker = if schema.required { "*" } else { " " };
                    let line = Line::from(vec![
                        Span::styled(
                            format!("{marker}{:<10}", schema.name),
                            Style::default().fg(Color::Cyan),
                        ),
                        Span::raw(format!(" [{}] ", schema.kind)),
                        Span::styled(value, if focused { Style::default().add_modifier(Modifier::UNDERLINED) } else { Style::default() }),
                        if focused { Span::raw("▏") } else { Span::raw("") },
                    ]);
                    frame.render_widget(Paragraph::new(line), *area);
                }
            }
        } else {
            frame.render_widget(Paragraph::new("(no templates known yet - press 'r' to refresh)"), inner);
        }
    }

    fn draw_bottom(&mut self, frame: &mut Frame, area: Rect) {
        let [gauge_area, list_area, status_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .areas(area);

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

        let spinner_frame = self.spinner_frame;
        let items: Vec<ListItem> = self
            .rows
            .iter()
            .map(|row| {
                let wave_label = row.wave.map(|w| format!("[w{w}] ")).unwrap_or_default();
                let message = row.message.as_deref().unwrap_or("");
                let line = Line::from(vec![
                    Span::styled(row.state.glyph(spinner_frame), Style::default().fg(row.state.color())),
                    Span::raw(" "),
                    Span::raw(wave_label),
                    Span::styled(row.task_id.clone(), Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(" "),
                    Span::raw(message.to_string()),
                ]);
                ListItem::new(line)
            })
            .collect();
        let list = List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .title("tasks (concurrent rows share a [wN] wave)")
                .border_style(focus_style(self.focus == Focus::Log)),
        );
        let mut list_state = ListState::default();
        if !self.rows.is_empty() {
            let selected = self.log_scroll.min(self.rows.len() - 1);
            self.log_scroll = selected;
            list_state.select(Some(selected));
        }
        frame.render_stateful_widget(list, list_area, &mut list_state);

        frame.render_widget(
            Paragraph::new(self.status_line.as_str()).style(Style::default().fg(Color::DarkGray)),
            status_area,
        );
    }
}

fn focus_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    }
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
    fn accepted_with_task_id_creates_a_pending_row() {
        let mut app = App::new();
        app.handle_daemon_event(json!({"type": "accepted", "task_id": "clone-git-repo:dest=/x"}));
        assert_eq!(app.rows.len(), 1);
        assert_eq!(app.rows[0].task_id, "clone-git-repo:dest=/x");
        assert!(app.rows[0].state == TaskState::Pending);
    }

    #[test]
    fn accepted_with_waves_creates_one_row_per_task_tagged_with_its_wave() {
        let mut app = App::new();
        app.handle_daemon_event(json!({
            "type": "accepted",
            "manifest_id": "manifest-0",
            "waves": [["a"], ["b", "c"]],
        }));
        assert_eq!(app.rows.len(), 3);
        let wave_of = |id: &str| app.rows.iter().find(|r| r.task_id == id).unwrap().wave;
        assert_eq!(wave_of("a"), Some(0));
        assert_eq!(wave_of("b"), Some(1));
        assert_eq!(wave_of("c"), Some(1));
    }

    #[test]
    fn task_status_transitions_an_existing_row_and_keeps_its_wave() {
        let mut app = App::new();
        app.handle_daemon_event(json!({"type": "accepted", "waves": [["a"]]}));
        app.handle_daemon_event(json!({"type": "task_status", "task_id": "a", "status": "start", "wave": 0}));
        assert!(app.rows[0].state == TaskState::Running);

        app.handle_daemon_event(
            json!({"type": "task_status", "task_id": "a", "status": "ok", "message": "done", "wave": 0}),
        );
        assert!(app.rows[0].state == TaskState::Ok);
        assert_eq!(app.rows[0].message.as_deref(), Some("done"));
        assert_eq!(app.rows[0].wave, Some(0));
        assert_eq!(app.rows.len(), 1, "same task_id should update in place, not duplicate");
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
        assert!(app.focus == Focus::Params);
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
}
