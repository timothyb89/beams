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

use super::form::{Dropdown, Input};
use super::{ConnectionInfo, DaemonView, EscapeKey, Menu, Row, Screen};
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
pub fn ellipsize(text: &str, width: usize) -> String {
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

/// Human-readable sentences describing a pending request, each flagged
/// when it's a warning (a destructive sync mode, an overwrite).
pub fn describe_request(request: &RequestRecord) -> Vec<(String, bool)> {
    if let Some(prompt) = &request.agent {
        return prompt.describe();
    }
    let Some(service_request) = &request.request else {
        return vec![(
            match &request.action {
                Some(action) => format!(
                    "{} the {} service.",
                    capitalize(action_label(action.action)),
                    action.service_id
                ),
                None => format!("Start a {} service.", request.kind),
            },
            false,
        )];
    };
    match &service_request.kind {
        ServiceRequestKind::LocalPortForward {
            remote_host,
            remote_port,
            local_port,
        } => vec![(
            format!(
                "Forward workstation port {} to {remote_host}:{remote_port} in the Beam.",
                port_or_automatic(*local_port)
            ),
            false,
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
            let mut lines = vec![(
                match direction {
                    FileCopyDirection::Upload => {
                        format!(
                            "Upload {local} from this workstation to {remote_path} in the Beam."
                        )
                    }
                    FileCopyDirection::Download => format!(
                        "Download {remote_path} from the Beam to {local} on this workstation."
                    ),
                },
                false,
            )];
            if *overwrite == OverwritePolicy::Replace {
                lines.push((
                    "Existing files at the destination will be replaced.".to_string(),
                    true,
                ));
            }
            lines
        }
        ServiceRequestKind::FileFetch {
            sources, overwrite, ..
        } => {
            let total: u64 = sources.iter().map(|source| source.size).sum();
            let what = match sources.as_slice() {
                [source] if !source.is_dir => "a file".to_string(),
                [_] => "a directory".to_string(),
                _ => format!("{} items", sources.len()),
            };
            let mut lines = vec![(
                format!(
                    "Copy {what} ({}) from the Beam to this workstation:",
                    human_bytes(total)
                ),
                false,
            )];
            const LISTED: usize = 4;
            for source in sources.iter().take(LISTED) {
                lines.push((
                    format!(
                        "• {}{}  {}",
                        source.path,
                        if source.is_dir { "/" } else { "" },
                        human_bytes(source.size)
                    ),
                    false,
                ));
            }
            if sources.len() > LISTED {
                lines.push((format!("• … and {} more", sources.len() - LISTED), false));
            }
            if *overwrite == OverwritePolicy::Replace {
                lines.push((
                    "Existing files at the destination will be replaced.".to_string(),
                    true,
                ));
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
                (
                    format!("Sync {local} on this workstation with {remote_path} in the Beam."),
                    false,
                ),
                (
                    format!(
                        "Mode: {}. {}",
                        mode_label(*mode),
                        super::sync_mode_warning(*mode)
                    ),
                    *mode != FolderSyncMode::TwoWaySafe,
                ),
            ];
            let mut ignore = Vec::new();
            if *ignore_vcs {
                ignore.push("VCS metadata".to_string());
            }
            ignore.extend(ignores.iter().cloned());
            if !ignore.is_empty() {
                lines.push((format!("Ignoring: {}.", ignore.join(", ")), false));
            }
            lines
        }
    }
}

/// `1.2 MB`-style sizes (decimal units, like `ls -h --si`).
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else if value < 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

/// `41% · 2/5` for a transfer under way.
pub fn progress_label(progress: &beams_protocol::TransferProgress) -> String {
    let percent = if progress.bytes_total == 0 {
        100
    } else {
        progress.bytes_done.min(progress.bytes_total) * 100 / progress.bytes_total
    };
    if progress.files_total > 1 {
        format!(
            "{percent}% · {}/{}",
            progress.files_done, progress.files_total
        )
    } else {
        format!("{percent}%")
    }
}

fn file_name(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

/// What a fetch or copy moves, in a few words ("big.bin", "3 items").
pub fn transfer_subject(kind: &ServiceRequestKind) -> String {
    match kind {
        ServiceRequestKind::FileFetch { sources, .. } => match sources.as_slice() {
            [source] => file_name(&source.path),
            _ => format!("{} items", sources.len()),
        },
        ServiceRequestKind::FileCopy { remote_path, .. } => file_name(remote_path),
        _ => String::new(),
    }
}

/// One line for a toast: what a pending request wants.
pub fn request_summary(request: &RequestRecord) -> String {
    if let Some(prompt) = &request.agent {
        return prompt.summary();
    }
    let Some(service_request) = &request.request else {
        return match &request.action {
            Some(action) => format!("{} a service", capitalize(action_label(action.action))),
            None => "Approve a request".to_string(),
        };
    };
    match &service_request.kind {
        ServiceRequestKind::FileFetch { sources, .. } => {
            let total: u64 = sources.iter().map(|source| source.size).sum();
            format!(
                "Fetch {} ({})",
                transfer_subject(&service_request.kind),
                human_bytes(total)
            )
        }
        ServiceRequestKind::FileCopy {
            direction: FileCopyDirection::Download,
            remote_path,
            ..
        } => format!("Download {}", file_name(remote_path)),
        ServiceRequestKind::FileCopy { remote_path, .. } => format!("Upload to {remote_path}"),
        ServiceRequestKind::LocalPortForward { remote_port, .. } => {
            format!("Forward Beam port {remote_port}")
        }
        ServiceRequestKind::FolderSync { remote_path, .. } => format!("Sync {remote_path}"),
    }
}

/// `█████░░░░░` for a fraction of `width` cells.
pub fn progress_bar(done: u64, total: u64, width: usize) -> String {
    let filled = if total == 0 {
        width
    } else {
        ((done.min(total) as u128 * width as u128) / total as u128) as usize
    };
    format!("{}{}", "█".repeat(filled), "░".repeat(width - filled))
}

pub fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// `(kind, endpoint)` for a service row.
pub fn describe_service(service: &ServiceRecord) -> (&'static str, String) {
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
        Some(ServiceRequestKind::FileFetch { sources, .. }) => {
            let what = match sources.as_slice() {
                [source] => std::path::Path::new(&source.path)
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| source.path.clone()),
                _ => format!("{} items", sources.len()),
            };
            let local = service.local_path.as_deref().unwrap_or("?");
            let mut endpoint = format!("{what} → {local}");
            if let (ServiceStatus::Running, Some(progress)) = (service.status, &service.progress) {
                endpoint = format!("{} {endpoint}", progress_label(progress));
            }
            ("fetch", endpoint)
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

/// One-shot copies (`FileCopy` and `FileFetch`) behave alike in the menu.
fn is_transfer(service: &ServiceRecord) -> bool {
    matches!(
        service.spec,
        Some(ServiceRequestKind::FileCopy { .. } | ServiceRequestKind::FileFetch { .. })
    )
}

pub fn supported_actions(service: &ServiceRecord) -> &'static [ServiceAction] {
    match service.spec {
        Some(ServiceRequestKind::LocalPortForward { .. }) => {
            &[ServiceAction::Stop, ServiceAction::Restart]
        }
        Some(ServiceRequestKind::FileCopy { .. } | ServiceRequestKind::FileFetch { .. }) => {
            &[ServiceAction::Stop]
        }
        Some(ServiceRequestKind::FolderSync { .. }) => &[
            ServiceAction::Pause,
            ServiceAction::Resume,
            ServiceAction::Terminate,
        ],
        None => &[],
    }
}

/// The supported actions that make sense in the service's current state
/// (no "stop" for something already stopped, no "resume" unless paused).
pub fn available_actions(service: &ServiceRecord) -> Vec<ServiceAction> {
    let finished = matches!(
        service.status,
        ServiceStatus::Stopped | ServiceStatus::Error
    );
    let is_copy = is_transfer(service);
    supported_actions(service)
        .iter()
        .copied()
        .filter(|action| match action {
            ServiceAction::Stop => !finished && service.status != ServiceStatus::Stopping,
            ServiceAction::Restart => true,
            ServiceAction::Pause => !finished && service.status != ServiceStatus::Paused,
            ServiceAction::Resume => service.status == ServiceStatus::Paused,
            ServiceAction::Terminate => service.status != ServiceStatus::Stopped,
        })
        .filter(|_| !(is_copy && finished))
        .collect()
}

/// Actions that can't be undone ask first: terminating a sync removes its
/// Mutagen session, and cancelling a copy can't be resumed.
pub fn needs_confirmation(service: &ServiceRecord, action: ServiceAction) -> bool {
    action == ServiceAction::Terminate || (action == ServiceAction::Stop && is_transfer(service))
}

pub fn action_description(service: &ServiceRecord, action: ServiceAction) -> &'static str {
    let is_copy = is_transfer(service);
    match action {
        ServiceAction::Stop if is_copy => "cancel the copy",
        ServiceAction::Stop => "stop the forward",
        ServiceAction::Restart => "restart the forward",
        ServiceAction::Pause => "pause syncing",
        ServiceAction::Resume => "resume syncing",
        ServiceAction::Terminate => "terminate the sync",
    }
}

pub fn confirmation_warning(service: &ServiceRecord, action: ServiceAction) -> &'static str {
    match action {
        ServiceAction::Terminate => {
            "The Mutagen session is removed (files on both sides stay as they are). A terminated sync can't be resumed; create a new one instead."
        }
        _ if is_transfer(service) => {
            "The partial transfer is discarded and the job can't be restarted; start a new copy instead."
        }
        _ => "This can't be undone.",
    }
}

pub fn service_title(service: &ServiceRecord) -> &'static str {
    match &service.spec {
        Some(ServiceRequestKind::LocalPortForward { .. }) => "Port forward",
        Some(ServiceRequestKind::FileCopy {
            direction: FileCopyDirection::Upload,
            ..
        }) => "Upload",
        Some(ServiceRequestKind::FileCopy {
            direction: FileCopyDirection::Download,
            ..
        }) => "Download",
        Some(ServiceRequestKind::FileFetch { .. }) => "Fetch from the Beam",
        Some(ServiceRequestKind::FolderSync { .. }) => "Folder sync",
        None => "Service",
    }
}

/// Full, untruncated details for a service's action menu.
pub fn service_details(service: &ServiceRecord) -> Vec<String> {
    let (_, endpoint) = describe_service(service);
    let mut lines = vec![
        endpoint,
        format!("Status: {}", status_label(service.status).0),
    ];
    if let Some(message) = service.message.as_deref().filter(|m| !m.is_empty()) {
        lines.push(capitalize(message));
    }
    if let Some(progress) = &service.progress {
        lines.push(format!(
            "Copied {} of {} ({} of {} items).",
            human_bytes(progress.bytes_done.min(progress.bytes_total)),
            human_bytes(progress.bytes_total),
            progress.files_done,
            progress.files_total
        ));
    }
    if let Some(ServiceRequestKind::FileFetch { sources, .. }) = &service.spec {
        for source in sources {
            lines.push(format!("From {}", source.path));
        }
    }
    if let Some(ServiceRequestKind::FolderSync {
        ignore_vcs,
        ignores,
        ..
    }) = &service.spec
    {
        let mut ignore = Vec::new();
        if *ignore_vcs {
            ignore.push("VCS metadata".to_string());
        }
        ignore.extend(ignores.iter().cloned());
        if !ignore.is_empty() {
            lines.push(format!("Ignoring: {}.", ignore.join(", ")));
        }
    }
    lines.push(format!("Service id: {}", service.service_id));
    lines
}

fn request_kind_label(request: &RequestRecord) -> &'static str {
    if request.agent.is_some() {
        return "ssh key";
    }
    match (&request.request, &request.action) {
        (Some(service_request), _) => match &service_request.kind {
            ServiceRequestKind::LocalPortForward { .. } => "forward",
            ServiceRequestKind::FileCopy { direction, .. } => direction_label(*direction),
            ServiceRequestKind::FileFetch { .. } => "fetch",
            ServiceRequestKind::FolderSync { .. } => "sync",
        },
        (None, Some(action)) => action_label(action.action),
        (None, None) => "request",
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
            match view.selected_row() {
                Some(Row::Request(_)) => {
                    add("Enter", "review");
                    add("y", "approve");
                    add("n", "reject");
                }
                Some(Row::Service(service)) => {
                    add("Enter", "actions");
                    for action in available_actions(service) {
                        add(&action_key(action).to_string(), action_label(action));
                    }
                    if view.pending_count() > 0 {
                        add("y/n", "oldest request");
                    }
                }
                None => {}
            }
            if view.rows().len() > 1 {
                add("↑↓", "select");
            }
            add("f", "forward");
            add("u", "upload");
            add("d", "download");
            add("m", "sync");
            if let EscapeKey::Byte(_) = escape_key {
                add(&escape_key.label(), "send literally");
            }
            add("q", "close");
        }
        Screen::Form { form, .. } => {
            if let Some(picker) = form.focused().and_then(|field| field.dropdown()) {
                add("↑↓", "choose");
                add("Tab", "complete");
                if picker.highlighted.is_some() {
                    add("Enter", "take");
                } else {
                    add("Enter", form.submit_label);
                }
                add("Esc", "hide list");
            } else {
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
        Screen::Menu(_) => {
            add("↑↓", "select");
            add("Enter", "choose");
            add("Esc", "back");
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

const KIND_WIDTH: usize = 9;
const STATUS_WIDTH: usize = 9;

/// One list row: marker, kind, flexible text, right-aligned status.
fn list_row(
    selected: bool,
    kind: &str,
    text: &str,
    status: (&str, Style),
    width: usize,
) -> Line<'static> {
    let text_width = width.saturating_sub(2 + KIND_WIDTH + 1 + STATUS_WIDTH);
    let text = ellipsize(text, text_width);
    let pad = text_width.saturating_sub(text.width());
    Line::from(vec![
        Span::styled(if selected { "› " } else { "  " }, theme::accent()),
        Span::styled(format!("{kind:<KIND_WIDTH$}"), theme::accent()),
        Span::raw(text),
        Span::raw(" ".repeat(pad + 1)),
        Span::styled(format!("{:>STATUS_WIDTH$}", status.0), status.1),
    ])
    .style(if selected {
        theme::selected_row()
    } else {
        Style::default()
    })
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

    let selected = view.selected_index();
    let requests = view.pending_requests();
    if !requests.is_empty() {
        lines.push(Line::default());
        lines.push(Line::from(vec![
            Span::styled(
                "Awaiting approval",
                theme::warn().add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("  {}", requests.len()), theme::dim()),
        ]));
        for (index, request) in requests.iter().enumerate() {
            let description = describe_request(request);
            let is_selected = index == selected;
            lines.push(list_row(
                is_selected,
                request_kind_label(request),
                description
                    .first()
                    .map(|(text, _)| text.as_str())
                    .unwrap_or_default(),
                ("pending", theme::warn()),
                width,
            ));
            if is_selected {
                // The selected request is shown in full.
                let indent = 2 + KIND_WIDTH;
                let mut sentences = description.iter();
                // Repeat the first sentence only if the row truncated it.
                let first_fits = description.first().is_some_and(|(first, _)| {
                    first.width() <= width.saturating_sub(indent + 1 + STATUS_WIDTH)
                });
                if first_fits {
                    sentences.next();
                }
                for (sentence, warn) in sentences {
                    let style = if *warn { theme::warn() } else { theme::dim() };
                    for line in wrap(sentence, width.saturating_sub(indent)) {
                        lines.push(Line::from(vec![
                            Span::raw(" ".repeat(indent)),
                            Span::styled(line, style),
                        ]));
                    }
                }
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
    let endpoint_width = width.saturating_sub(2 + KIND_WIDTH + 1 + STATUS_WIDTH);
    lines.push(Line::from(Span::styled(
        format!(
            "  {:<KIND_WIDTH$}{:<endpoint_width$} {:>STATUS_WIDTH$}",
            "TYPE", "ENDPOINT", "STATUS"
        ),
        theme::dim(),
    )));

    // Service rows follow the request rows in selection order.
    let selected_service = selected.checked_sub(requests.len());
    let selected_message = selected_service
        .and_then(|index| services.get(index))
        .and_then(|service| service.message.as_deref())
        .filter(|message| !message.is_empty());
    let available = max_rows
        .saturating_sub(lines.len())
        .saturating_sub(usize::from(selected_message.is_some()))
        .max(1);
    let start = selected_service.unwrap_or(0).saturating_sub(available - 1);
    if start > 0 {
        lines.push(Line::from(Span::styled(
            format!("  ↑ {start} more"),
            theme::dim(),
        )));
    }
    let shown_end = (start + available).min(services.len());
    for (index, service) in services
        .iter()
        .enumerate()
        .skip(start)
        .take(shown_end - start)
    {
        let is_selected = Some(index) == selected_service;
        let (kind, endpoint) = describe_service(service);
        lines.push(list_row(
            is_selected,
            kind,
            &endpoint,
            status_label(service.status),
            width,
        ));
        if is_selected {
            if let Some(message) = selected_message {
                lines.push(Line::from(Span::styled(
                    ellipsize(&format!("  {}{message}", " ".repeat(KIND_WIDTH)), width),
                    theme::dim(),
                )));
            }
        }
    }
    if shown_end < services.len() {
        lines.push(Line::from(Span::styled(
            format!("  ↓ {} more", services.len() - shown_end),
            theme::dim(),
        )));
    }
    lines
}

fn menu_body(menu: &Menu, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (text, warn) in &menu.intro {
        let style = if *warn {
            theme::warn()
        } else {
            Style::default()
        };
        // `wrap` drops leading whitespace, so keep an intro line's own
        // indentation (nested detail lines) on every wrapped line.
        let indent = text.len() - text.trim_start().len();
        let pad = " ".repeat(indent.min(width / 2));
        for line in wrap(text.trim_start(), width.saturating_sub(pad.len())) {
            lines.push(Line::from(Span::styled(format!("{pad}{line}"), style)));
        }
    }
    if !menu.intro.is_empty() {
        lines.push(Line::default());
    }
    for (index, item) in menu.items.iter().enumerate() {
        let selected = index == menu.selected;
        let label_style = match (selected, item.danger) {
            (true, true) => theme::error().add_modifier(Modifier::BOLD),
            (true, false) => theme::label_focused(),
            (false, true) => theme::error(),
            (false, false) => Style::default(),
        };
        let key = item.key.map(|key| format!("[{key}]")).unwrap_or_default();
        let label = ellipsize(&item.label, width.saturating_sub(2 + 1 + key.width()));
        let pad = width.saturating_sub(2 + label.width() + key.width());
        lines.push(
            Line::from(vec![
                Span::styled(if selected { "› " } else { "  " }, theme::accent()),
                Span::styled(label, label_style),
                Span::raw(" ".repeat(pad)),
                Span::styled(key, theme::dim()),
            ])
            .style(if selected {
                theme::selected_row()
            } else {
                Style::default()
            }),
        );
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

    let (title, body, cursor, dropdown) = match &view.screen {
        Screen::Home => (
            "Beams local menu".to_string(),
            home_body(view, inner_width, max_body),
            None,
            None,
        ),
        Screen::Form { form, .. } => {
            let rendered = form.render(inner_width);
            (
                form.title.clone(),
                rendered.lines,
                rendered.cursor,
                rendered.dropdown,
            )
        }
        Screen::Menu(menu) => (menu.title.clone(), menu_body(menu, inner_width), None, None),
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
    if let Some(dropdown) = dropdown {
        if let Some(field_row) = dropdown.row.checked_sub(scroll) {
            if field_row < body_area.height {
                draw_dropdown(frame, area, body_area, field_row, &dropdown);
            }
        }
    }
}

/// Draws a field's dropdown as a bordered popup just under the field (or
/// above it when the terminal is too short below), over the rest of the
/// dialog.
fn draw_dropdown(
    frame: &mut Frame<'_>,
    area: Rect,
    body: Rect,
    field_row: u16,
    dropdown: &Dropdown<'_>,
) {
    let field_y = body.y + field_row;
    let wanted = dropdown.wanted_rows() as u16 + 2;
    let below = area.bottom().saturating_sub(field_y + 1);
    let above = field_y.saturating_sub(area.y);
    let (y, height) = if wanted <= below || below >= above {
        (field_y + 1, wanted.min(below))
    } else {
        let height = wanted.min(above);
        (field_y - height, height)
    };
    if height < 3 {
        return;
    }
    // The border sits one column left of the field text so entries line up
    // with what's typed.
    let x = (body.x + dropdown.col).saturating_sub(1).max(area.x);
    let width = (dropdown.width + 2).min(area.right().saturating_sub(x));
    let rect = Rect {
        x,
        y,
        width,
        height,
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme::dim());
    let inner = block.inner(rect);
    frame.render_widget(Clear, rect);
    frame.render_widget(block, rect);
    frame.render_widget(
        Paragraph::new(dropdown.lines(usize::from(inner.height))),
        inner,
    );
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
