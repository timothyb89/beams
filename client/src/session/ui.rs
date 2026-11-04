//! The local menu renderer, shared by both presentation modes.
//!
//! The default takeover mode clears the physical terminal and draws the
//! dialog on a blank background; `--terminal-overlay` paints the `vt100`
//! shadow screen first (dimmed) and draws the same dialog on top. Either
//! way there is exactly one dialog layout per screen (home, or a form), so
//! the two modes can't drift apart.

use beams_protocol::{
    FileCopyDirection, FolderSyncMode, OverwritePolicy, ServiceAction, ServiceRequestKind,
    ServiceStatus,
};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Padding, Paragraph};
use ratatui::Frame;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::form::Input;
use super::{ConnectionInfo, DaemonView, EscapeKey, Screen};
use crate::clientd::{RequestRecord, ServiceRecord};
use crate::terminal_model::TerminalModel;

const DIALOG_WIDTH: u16 = 84;

pub mod theme {
    use ratatui::style::{Color, Modifier, Style};

    pub fn accent() -> Style {
        Style::default().fg(Color::Cyan)
    }
    pub fn title() -> Style {
        accent().add_modifier(Modifier::BOLD)
    }
    pub fn heading() -> Style {
        Style::default().add_modifier(Modifier::BOLD)
    }
    pub fn label() -> Style {
        Style::default()
    }
    pub fn label_focused() -> Style {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    }
    // Input boxes and the selected row set both colors explicitly so they
    // read the same on light and dark terminal themes.
    pub fn input() -> Style {
        Style::default()
            .fg(Color::Indexed(252))
            .bg(Color::Indexed(236))
    }
    pub fn input_focused() -> Style {
        Style::default()
            .fg(Color::Indexed(255))
            .bg(Color::Indexed(238))
    }
    pub fn placeholder() -> Style {
        Style::default()
            .fg(Color::Indexed(245))
            .add_modifier(Modifier::ITALIC)
    }
    pub fn dim() -> Style {
        Style::default().fg(Color::DarkGray)
    }
    pub fn key() -> Style {
        accent().add_modifier(Modifier::BOLD)
    }
    pub fn ok() -> Style {
        Style::default().fg(Color::Green)
    }
    pub fn warn() -> Style {
        Style::default().fg(Color::Yellow)
    }
    pub fn error() -> Style {
        Style::default().fg(Color::Red)
    }
    pub fn selected_row() -> Style {
        Style::default()
            .fg(Color::Indexed(252))
            .bg(Color::Indexed(236))
    }
}

/// Word-wraps `text` to `width` columns, hard-breaking words that don't fit.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut used = 0;
    for word in text.split(' ') {
        let word_width = word.width();
        if used > 0 && used + 1 + word_width > width {
            lines.push(std::mem::take(&mut line));
            used = 0;
        }
        if used > 0 {
            line.push(' ');
            used += 1;
        }
        for ch in word.chars() {
            let w = ch.width().unwrap_or(0);
            if used + w > width {
                lines.push(std::mem::take(&mut line));
                used = 0;
            }
            line.push(ch);
            used += w;
        }
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    lines
}

/// Truncates `text` to `width` columns, ending in `…` when cut.
fn ellipsize(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = ch.width().unwrap_or(0);
        if used + w + 1 > width {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

pub fn direction_label(direction: FileCopyDirection) -> &'static str {
    match direction {
        FileCopyDirection::Upload => "upload",
        FileCopyDirection::Download => "download",
    }
}

pub fn mode_label(mode: FolderSyncMode) -> &'static str {
    match mode {
        FolderSyncMode::TwoWaySafe => "two-way, safe",
        FolderSyncMode::TwoWayResolved => "two-way, workstation wins",
        FolderSyncMode::OneWaySafe => "one-way to Beam, safe",
        FolderSyncMode::OneWayReplica => "one-way to Beam, replica",
    }
}

pub fn action_label(action: ServiceAction) -> &'static str {
    match action {
        ServiceAction::Stop => "stop",
        ServiceAction::Restart => "restart",
        ServiceAction::Pause => "pause",
        ServiceAction::Resume => "resume",
        ServiceAction::Terminate => "terminate",
    }
}

fn status_label(status: ServiceStatus) -> (&'static str, Style) {
    match status {
        ServiceStatus::Starting => ("starting", theme::warn()),
        ServiceStatus::Running => ("running", theme::ok()),
        ServiceStatus::Paused => ("paused", theme::warn()),
        ServiceStatus::Stopping => ("stopping", theme::warn()),
        ServiceStatus::Stopped => ("stopped", theme::dim()),
        ServiceStatus::Degraded => ("degraded", theme::warn()),
        ServiceStatus::Unknown => ("unknown", theme::dim()),
        ServiceStatus::Error => ("error", theme::error()),
    }
}

fn port_or_automatic(port: Option<u16>) -> String {
    port.map(|port| port.to_string())
        .unwrap_or_else(|| "automatic".to_string())
}

/// Human-readable sentences describing a pending request.
pub fn describe_request(request: &RequestRecord) -> Vec<String> {
    let Some(service_request) = &request.request else {
        return vec![match &request.action {
            Some(action) => format!(
                "{} the {} service.",
                capitalize(action_label(action.action)),
                action.service_id
            ),
            None => format!("Start a {} service.", request.kind),
        }];
    };
    match &service_request.kind {
        ServiceRequestKind::LocalPortForward {
            remote_host,
            remote_port,
            local_port,
        } => vec![format!(
            "Forward workstation port {} to {remote_host}:{remote_port} in the Beam.",
            port_or_automatic(*local_port)
        )],
        ServiceRequestKind::FileCopy {
            direction,
            remote_path,
            local_path_hint,
            overwrite,
        } => {
            let local = local_path_hint
                .as_deref()
                .unwrap_or("a workstation path you choose");
            let mut lines = vec![match direction {
                FileCopyDirection::Upload => {
                    format!("Upload {local} from this workstation to {remote_path} in the Beam.")
                }
                FileCopyDirection::Download => {
                    format!("Download {remote_path} from the Beam to {local} on this workstation.")
                }
            }];
            if *overwrite == OverwritePolicy::Replace {
                lines.push("Existing files at the destination will be replaced.".to_string());
            }
            lines
        }
        ServiceRequestKind::FolderSync {
            remote_path,
            local_path_hint,
            mode,
            ignore_vcs,
            ignores,
        } => {
            let local = local_path_hint
                .as_deref()
                .unwrap_or("a workstation directory you choose");
            let mut lines = vec![
                format!("Sync {local} on this workstation with {remote_path} in the Beam."),
                format!(
                    "Mode: {}. {}",
                    mode_label(*mode),
                    super::sync_mode_warning(*mode)
                ),
            ];
            let mut ignore = Vec::new();
            if *ignore_vcs {
                ignore.push("VCS metadata".to_string());
            }
            ignore.extend(ignores.iter().cloned());
            if !ignore.is_empty() {
                lines.push(format!("Ignoring: {}.", ignore.join(", ")));
            }
            lines
        }
    }
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// `(kind, endpoint)` for a service row.
fn describe_service(service: &ServiceRecord) -> (&'static str, String) {
    match &service.spec {
        Some(ServiceRequestKind::LocalPortForward {
            remote_host,
            remote_port,
            local_port,
        }) => (
            "forward",
            format!(
                "localhost:{} → {remote_host}:{remote_port}",
                local_port
                    .map(|port| port.to_string())
                    .unwrap_or_else(|| "?".to_string())
            ),
        ),
        Some(ServiceRequestKind::FileCopy {
            direction,
            remote_path,
            local_path_hint,
            ..
        }) => {
            let local = local_path_hint.as_deref().unwrap_or("?");
            (
                direction_label(*direction),
                match direction {
                    FileCopyDirection::Upload => format!("{local} → {remote_path}"),
                    FileCopyDirection::Download => format!("{remote_path} → {local}"),
                },
            )
        }
        Some(ServiceRequestKind::FolderSync {
            remote_path,
            local_path_hint,
            mode,
            ..
        }) => (
            "sync",
            format!(
                "{} ⇄ {remote_path} ({})",
                local_path_hint.as_deref().unwrap_or("?"),
                mode_label(*mode)
            ),
        ),
        None => ("service", service.kind.clone()),
    }
}

pub fn supported_actions(service: &ServiceRecord) -> &'static [ServiceAction] {
    match service.spec {
        Some(ServiceRequestKind::LocalPortForward { .. }) => {
            &[ServiceAction::Stop, ServiceAction::Restart]
        }
        Some(ServiceRequestKind::FileCopy { .. }) => &[ServiceAction::Stop],
        Some(ServiceRequestKind::FolderSync { .. }) => &[
            ServiceAction::Pause,
            ServiceAction::Resume,
            ServiceAction::Terminate,
        ],
        None => &[],
    }
}

pub fn action_key(action: ServiceAction) -> char {
    match action {
        ServiceAction::Stop => 's',
        ServiceAction::Restart => 'r',
        ServiceAction::Pause => 'p',
        ServiceAction::Resume => 'e',
        ServiceAction::Terminate => 't',
    }
}

fn hints(view: &DaemonView, escape_key: EscapeKey) -> Vec<(String, String)> {
    let mut hints: Vec<(String, String)> = Vec::new();
    let mut add = |key: &str, desc: &str| hints.push((key.to_string(), desc.to_string()));
    match &view.screen {
        Screen::Home => {
            if view.pending_request().is_some() {
                add("y", "approve");
                add("n", "reject");
            }
            add("f", "forward");
            add("u", "upload");
            add("d", "download");
            add("m", "sync");
            let services = view.visible_services();
            if services.len() > 1 {
                add("↑↓", "select");
            }
            if let Some(service) = view.selected_service() {
                for action in supported_actions(service) {
                    add(&action_key(*action).to_string(), action_label(*action));
                }
            }
            if let EscapeKey::Byte(_) = escape_key {
                add(&escape_key.label(), "send literally");
            }
            add("q", "close");
        }
        Screen::Form { form, .. } => {
            if form.fields.len() > 1 {
                add("Tab", "next");
                add("⇧Tab", "previous");
            }
            match form.focused().map(|field| &field.input) {
                Some(Input::Toggle { .. }) => add("Space", "toggle"),
                Some(Input::Choice { .. }) => add("←→", "change"),
                Some(Input::Text { suggestion, .. }) if !suggestion.is_empty() => {
                    add("→", "complete")
                }
                _ => {}
            }
            add("Enter", form.submit_label);
            add("Esc", "cancel");
        }
    }
    hints
}

fn pack_hints(hints: &[(String, String)], width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    for (key, desc) in hints {
        let item_width = key.width() + 1 + desc.width();
        if used > 0 && used + 3 + item_width > width {
            lines.push(Line::from(std::mem::take(&mut spans)));
            used = 0;
        }
        if used > 0 {
            spans.push(Span::styled(" · ", theme::dim()));
            used += 3;
        }
        spans.push(Span::styled(key.clone(), theme::key()));
        spans.push(Span::raw(format!(" {desc}")));
        used += item_width;
    }
    if !spans.is_empty() {
        lines.push(Line::from(spans));
    }
    lines
}

fn home_body(view: &DaemonView, width: usize, max_rows: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    lines.push(Line::from(vec![
        Span::styled("local daemon  ", theme::dim()),
        Span::styled(
            view.message.clone(),
            if view.connected {
                theme::ok()
            } else {
                theme::warn()
            },
        ),
    ]));
    if let Some(client_id) = &view.client_id {
        lines.push(Line::from(Span::styled(
            ellipsize(
                &format!(
                    "client {client_id} · {} bridge(s) connected · {} frontend(s)",
                    view.connected_bridges(),
                    view.frontends
                ),
                width,
            ),
            theme::dim(),
        )));
    }
    if let Some(notice) = &view.notice {
        let style = if notice.error {
            theme::error()
        } else {
            theme::ok()
        };
        for line in wrap(&notice.text, width) {
            lines.push(Line::from(Span::styled(line, style)));
        }
    }

    if let Some(request) = view.pending_request() {
        lines.push(Line::default());
        let pending = view.pending_count();
        let mut heading = vec![Span::styled(
            "Approval needed",
            theme::warn().add_modifier(Modifier::BOLD),
        )];
        if pending > 1 {
            heading.push(Span::styled(format!("  1 of {pending}"), theme::dim()));
        }
        lines.push(Line::from(heading));
        for sentence in describe_request(request) {
            for line in wrap(&sentence, width.saturating_sub(2)) {
                lines.push(Line::from(format!("  {line}")));
            }
        }
    }

    lines.push(Line::default());
    let services = view.visible_services();
    let mut heading = vec![Span::styled("Services", theme::heading())];
    if !services.is_empty() {
        heading.push(Span::styled(format!("  {}", services.len()), theme::dim()));
    }
    lines.push(Line::from(heading));
    if services.is_empty() {
        for line in wrap(
            "Nothing running for this Beam yet. Forward a port, copy a file, or sync a folder below.",
            width,
        ) {
            lines.push(Line::from(Span::styled(line, theme::dim())));
        }
        return lines;
    }

    let selected = view.selected_index();
    let selected_message = services[selected]
        .message
        .as_deref()
        .filter(|message| !message.is_empty());
    let available = max_rows
        .saturating_sub(lines.len())
        .saturating_sub(usize::from(selected_message.is_some()))
        .max(1);
    let start = selected.saturating_sub(available - 1);
    const KIND_WIDTH: usize = 9;
    const STATUS_WIDTH: usize = 9;
    let endpoint_width = width.saturating_sub(2 + KIND_WIDTH + 1 + STATUS_WIDTH);
    for (index, service) in services.iter().enumerate().skip(start).take(available) {
        let is_selected = index == selected;
        let (kind, endpoint) = describe_service(service);
        let (status, status_style) = status_label(service.status);
        let row_style = if is_selected {
            theme::selected_row()
        } else {
            Style::default()
        };
        let endpoint = ellipsize(&endpoint, endpoint_width);
        let pad = endpoint_width.saturating_sub(endpoint.width());
        lines.push(
            Line::from(vec![
                Span::styled(if is_selected { "› " } else { "  " }, theme::accent()),
                Span::styled(format!("{kind:<KIND_WIDTH$}"), theme::accent()),
                Span::raw(endpoint),
                Span::raw(" ".repeat(pad + 1)),
                Span::styled(format!("{status:>STATUS_WIDTH$}"), status_style),
            ])
            .style(row_style),
        );
        if is_selected {
            if let Some(message) = selected_message {
                lines.push(Line::from(Span::styled(
                    ellipsize(&format!("  {}{message}", " ".repeat(KIND_WIDTH)), width),
                    theme::dim(),
                )));
            }
        }
    }
    lines
}

/// Draws the whole local menu: the background (the dimmed shadow screen in
/// overlay mode, or nothing in takeover mode), then the dialog for the
/// current screen.
pub fn draw(
    frame: &mut Frame<'_>,
    info: &ConnectionInfo<'_>,
    escape_key: EscapeKey,
    view: &DaemonView,
    background: Option<&TerminalModel>,
) {
    let area = frame.area();
    if let Some(model) = background {
        model.render(frame, area);
        frame
            .buffer_mut()
            .set_style(area, Style::default().add_modifier(Modifier::DIM));
    }

    let width = DIALOG_WIDTH.min(area.width);
    // Borders plus one column of padding on each side.
    let inner_width = usize::from(width.saturating_sub(4)).max(1);
    let hint_lines = pack_hints(&hints(view, escape_key), inner_width);
    let chrome = 2 + 1 + hint_lines.len();
    let max_body = usize::from(area.height).saturating_sub(chrome).max(1);

    let (title, body, cursor) = match &view.screen {
        Screen::Home => (
            "Beams local menu".to_string(),
            home_body(view, inner_width, max_body),
            None,
        ),
        Screen::Form { form, .. } => {
            let (body, cursor) = form.render(inner_width);
            (form.title.clone(), body, cursor)
        }
    };

    let height = (body.len().min(max_body) + chrome).min(usize::from(area.height)) as u16;
    let popup = centered_rect(area, width, height);
    let (badge, badge_style) = if view.connected {
        ("● connected", theme::ok())
    } else {
        ("● offline", theme::warn())
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme::accent())
        .padding(Padding::horizontal(1))
        .title(Line::from(Span::styled(
            format!(" {title} "),
            theme::title(),
        )))
        .title_top(Line::from(Span::styled(format!(" {badge} "), badge_style)).right_aligned())
        .title_bottom(
            Line::from(Span::styled(
                format!(" {}@{} ", info.login, info.beam_id),
                theme::dim(),
            ))
            .right_aligned(),
        );
    let inner = block.inner(popup);
    frame.render_widget(Clear, popup);
    frame.render_widget(block, popup);

    let [body_area, rule_area, hint_area] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(hint_lines.len() as u16),
    ])
    .areas(inner);

    // Keep the cursor's row in view if a small terminal clips the body.
    let scroll = cursor
        .map(|(row, _)| row.saturating_sub(body_area.height.saturating_sub(1)))
        .unwrap_or(0);
    frame.render_widget(Paragraph::new(body).scroll((scroll, 0)), body_area);
    frame.render_widget(
        Paragraph::new(Span::styled(
            "─".repeat(usize::from(rule_area.width)),
            theme::dim(),
        )),
        rule_area,
    );
    frame.render_widget(Paragraph::new(hint_lines), hint_area);

    if let Some((row, col)) = cursor {
        let row = row - scroll;
        if row < body_area.height && col < body_area.width {
            frame.set_cursor_position((body_area.x + col, body_area.y + row));
        }
    }
}

fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_breaks_on_words_and_hard_breaks_long_ones() {
        assert_eq!(wrap("one two three", 7), vec!["one two", "three"]);
        assert_eq!(wrap("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        assert_eq!(wrap("", 4), vec![""]);
    }

    #[test]
    fn ellipsize_marks_truncation() {
        assert_eq!(ellipsize("short", 10), "short");
        assert_eq!(ellipsize("a long endpoint", 8), "a long …");
    }

    #[test]
    fn hints_pack_across_lines() {
        let hints = vec![
            ("f".to_string(), "forward".to_string()),
            ("u".to_string(), "upload".to_string()),
            ("q".to_string(), "close".to_string()),
        ];
        assert_eq!(pack_hints(&hints, 80).len(), 1);
        assert_eq!(pack_hints(&hints, 20).len(), 2);
    }
}
