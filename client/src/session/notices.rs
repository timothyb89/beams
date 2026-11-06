//! Decides which toast (if any) the session shows: an incoming request for
//! this Beam, a transfer in progress, or a transfer that just finished.
//! `toast.rs` handles actually drawing it.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use beams_protocol::{RequestStatus, ServiceRequestKind, ServiceStatus};
use ratatui::text::{Line, Span};

use super::toast::{Toast, Tone};
use super::ui::{self, theme};
use super::{DaemonView, EscapeKey};
use crate::clientd::ServiceRecord;

/// How long an ignored request's toast stays up. The request itself stays
/// pending in the menu until the daemon expires it.
const REQUEST_TOAST_FOR: Duration = Duration::from_secs(20);
/// SSH key requests block whatever asked (`git`, `ssh`) until decided, so
/// their toast stays up for as long as they can be approved.
const AGENT_TOAST_FOR: Duration = Duration::from_secs(120);
const DONE_TOAST_FOR: Duration = Duration::from_secs(5);
/// `first_seen` key for the key-setup offer.
const KEY_SETUP_ID: &str = "\u{0}key-setup";
const FAILED_TOAST_FOR: Duration = Duration::from_secs(10);
/// Inner width the toast lines are cut to.
const LINE_WIDTH: usize = 48;
const BAR_WIDTH: usize = 24;

#[derive(Debug, Default)]
pub struct ToastPolicy {
    first_seen: HashMap<String, Instant>,
    /// Requests the user has looked at (opened the menu while they were
    /// pending); they don't get a toast again.
    reviewed: HashSet<String>,
    /// Transfers seen running in this session; only those get a "done" toast.
    running_seen: HashSet<String>,
    finished_at: HashMap<String, Instant>,
}

/// What the toast is about, so the menu prefix can open the right thing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Subject {
    Request(String),
    Service(String),
    /// The one-time "share an SSH key?" offer.
    KeySetup,
}

impl ToastPolicy {
    pub fn current(
        &mut self,
        view: &DaemonView,
        escape_key: EscapeKey,
        now: Instant,
    ) -> Option<(Toast, Subject)> {
        let pending = view.pending_requests();
        for request in &pending {
            self.first_seen
                .entry(request.request_id.clone())
                .or_insert(now);
        }
        let mut fresh: Vec<_> = pending
            .iter()
            .filter(|request| !self.reviewed.contains(&request.request_id))
            .filter(|request| {
                self.first_seen
                    .get(&request.request_id)
                    .is_some_and(|seen| {
                        let shown_for = if request.agent.is_some() { AGENT_TOAST_FOR } else { REQUEST_TOAST_FOR };
                        now.saturating_duration_since(*seen) < shown_for
                    })
            })
            .collect();
        fresh.sort_by_key(|request| self.first_seen.get(&request.request_id).copied());
        if let Some(newest) = fresh.last() {
            let review = match escape_key {
                EscapeKey::Byte(_) => escape_key.label(),
                EscapeKey::Disabled => return None,
            };
            let more = pending.len().saturating_sub(1);
            let mut hint = vec![
                Span::styled(review, theme::key()),
                Span::styled(" to review", theme::dim()),
            ];
            // An SSH key request blocks whatever asked until it expires.
            if let Some(prompt) = &newest.agent {
                let left_ms = prompt.expires_at_ms.saturating_sub(crate::agent::now_ms());
                hint.push(Span::styled(format!(" · {}s left", left_ms.div_ceil(1000)), theme::dim()));
            }
            if more > 0 {
                hint.insert(0, Span::styled(format!("+{more} more · "), theme::dim()));
            }
            let toast = Toast {
                tone: Tone::Info,
                title: if pending.len() > 1 {
                    format!("{} Beam requests", pending.len())
                } else {
                    if newest.agent.is_some() { "SSH key request".to_string() } else { "Beam request".to_string() }
                },
                lines: vec![
                    Line::from(Span::styled(
                        ui::ellipsize(&ui::request_summary(newest), LINE_WIDTH),
                        theme::label_focused(),
                    )),
                    Line::from(hint),
                ],
            };
            return Some((toast, Subject::Request(newest.request_id.clone())));
        }

        // Offer to share a key, once, while nothing more urgent is up.
        if view.offer_key_setup {
            let seen = *self.first_seen.entry(KEY_SETUP_ID.to_string()).or_insert(now);
            if now.saturating_duration_since(seen) < REQUEST_TOAST_FOR {
                let open = match escape_key {
                    EscapeKey::Byte(_) => escape_key.label(),
                    EscapeKey::Disabled => return None,
                };
                let toast = Toast {
                    tone: Tone::Info,
                    title: "Share an SSH key with Beams?".to_string(),
                    lines: vec![
                        Line::from(Span::styled(
                            ui::ellipsize("None is shared, so git over ssh won't work here.", LINE_WIDTH),
                            theme::label_focused(),
                        )),
                        Line::from(vec![
                            Span::styled(open, theme::key()),
                            Span::styled(" to choose", theme::dim()),
                        ]),
                    ],
                };
                return Some((toast, Subject::KeySetup));
            }
        }

        // A startup grant just signed something: a passive notice.
        if let Some(record) = view
            .request_records
            .values()
            .filter(|r| r.agent.as_ref().is_some_and(|p| p.granted.is_some()))
            .filter(|r| r.status == RequestStatus::Running && view.beam_uuid.as_deref() == Some(r.beam_uuid.as_str()))
            .filter_map(|r| {
                let seen = *self.first_seen.entry(r.request_id.clone()).or_insert(now);
                (now.saturating_duration_since(seen) < DONE_TOAST_FOR).then_some((seen, r))
            })
            .max_by_key(|(seen, _)| *seen)
            .map(|(_, r)| r)
        {
            let prompt = record.agent.as_ref().expect("filtered above");
            let mut lines = vec![Line::from(Span::styled(
                ui::ellipsize(&prompt.summary(), LINE_WIDTH),
                theme::label_focused(),
            ))];
            // Which repo or task it was for (as the Beam claims it).
            if let Some(task) = prompt.task_label() {
                lines.push(Line::from(Span::styled(
                    ui::ellipsize(&format!("for {task}"), LINE_WIDTH),
                    theme::dim(),
                )));
            }
            lines.push(Line::from(Span::styled(
                ui::ellipsize(prompt.granted.as_deref().unwrap_or_default(), LINE_WIDTH),
                theme::dim(),
            )));
            let toast = Toast { tone: Tone::Info, title: "SSH key used".to_string(), lines };
            return Some((toast, Subject::Request(record.request_id.clone())));
        }

        let transfers = self.observe(view, now);
        if let Some(service) = transfers.iter().find(|service| {
            matches!(
                service.status,
                ServiceStatus::Starting | ServiceStatus::Running | ServiceStatus::Stopping
            )
        }) {
            return Some((
                progress_toast(service),
                Subject::Service(service.service_id.clone()),
            ));
        }
        transfers
            .iter()
            .filter_map(|service| {
                let finished = self.finished_at.get(&service.service_id)?;
                let failed = service.status == ServiceStatus::Error || !completed(service);
                let shown_for = if failed {
                    FAILED_TOAST_FOR
                } else {
                    DONE_TOAST_FOR
                };
                (now.saturating_duration_since(*finished) < shown_for)
                    .then_some((*finished, service))
            })
            .max_by_key(|(finished, _)| *finished)
            .map(|(_, service)| {
                (
                    finished_toast(service),
                    Subject::Service(service.service_id.clone()),
                )
            })
    }

    /// Notes request arrivals and transfer state changes without deciding on
    /// a toast (also called while the menu is open). Returns this Beam's
    /// transfers.
    pub fn observe<'a>(&mut self, view: &'a DaemonView, now: Instant) -> Vec<&'a ServiceRecord> {
        for request in view.pending_requests() {
            self.first_seen
                .entry(request.request_id.clone())
                .or_insert(now);
        }
        let transfers: Vec<&ServiceRecord> = view
            .visible_services()
            .into_iter()
            .filter(|service| {
                matches!(
                    service.spec,
                    Some(
                        ServiceRequestKind::FileFetch { .. } | ServiceRequestKind::FileCopy { .. }
                    )
                )
            })
            .collect();
        for service in &transfers {
            match service.status {
                ServiceStatus::Starting | ServiceStatus::Running | ServiceStatus::Stopping => {
                    self.running_seen.insert(service.service_id.clone());
                }
                ServiceStatus::Stopped | ServiceStatus::Error
                    if self.running_seen.contains(&service.service_id) =>
                {
                    self.finished_at
                        .entry(service.service_id.clone())
                        .or_insert(now);
                }
                _ => {}
            }
        }
        transfers
    }

    /// The menu was opened: requests pending now have been seen.
    pub fn mark_reviewed(&mut self, view: &DaemonView) {
        self.reviewed.extend(
            view.pending_requests()
                .into_iter()
                .map(|request| request.request_id.clone()),
        );
    }
}

fn verb(service: &ServiceRecord) -> (&'static str, &'static str) {
    match &service.spec {
        Some(ServiceRequestKind::FileCopy { direction, .. }) => match direction {
            beams_protocol::FileCopyDirection::Upload => ("Uploading", "Uploaded"),
            beams_protocol::FileCopyDirection::Download => ("Downloading", "Downloaded"),
        },
        _ => ("Fetching", "Fetched"),
    }
}

fn subject(service: &ServiceRecord) -> String {
    service
        .spec
        .as_ref()
        .map(ui::transfer_subject)
        .unwrap_or_else(|| service.kind.clone())
}

/// A stopped transfer that copied everything (or, without progress
/// reporting, didn't say it was cancelled or interrupted).
fn completed(service: &ServiceRecord) -> bool {
    if service.status != ServiceStatus::Stopped {
        return false;
    }
    match &service.progress {
        Some(progress) => progress.files_done == progress.files_total,
        None => !service.message.as_deref().is_some_and(|message| {
            message.contains("cancelled") || message.contains("interrupted")
        }),
    }
}

fn progress_toast(service: &ServiceRecord) -> Toast {
    let (doing, _) = verb(service);
    let mut lines = vec![Line::from(Span::styled(
        ui::ellipsize(&subject(service), LINE_WIDTH),
        theme::label_focused(),
    ))];
    match &service.progress {
        Some(progress) => lines.push(Line::from(vec![
            Span::styled(
                ui::progress_bar(progress.bytes_done, progress.bytes_total, BAR_WIDTH),
                theme::accent(),
            ),
            Span::raw(format!(" {}", ui::progress_label(progress))),
        ])),
        None => lines.push(Line::from(Span::styled("copying…", theme::dim()))),
    }
    Toast {
        tone: Tone::Info,
        title: doing.to_string(),
        lines,
    }
}

fn finished_toast(service: &ServiceRecord) -> Toast {
    let (_, done) = verb(service);
    let what = ui::ellipsize(&subject(service), LINE_WIDTH - 2);
    if completed(service) {
        let into = service
            .local_path
            .as_deref()
            .map(|path| format!("into {}", abbreviate_home(path)))
            .unwrap_or_default();
        Toast {
            tone: Tone::Ok,
            title: done.to_string(),
            lines: vec![
                Line::from(vec![Span::styled("✓ ", theme::ok()), Span::raw(what)]),
                Line::from(Span::styled(ui::ellipsize(&into, LINE_WIDTH), theme::dim())),
            ],
        }
    } else {
        let message = service
            .message
            .clone()
            .unwrap_or_else(|| "failed".to_string());
        Toast {
            tone: Tone::Error,
            title: if service.status == ServiceStatus::Error {
                "Transfer failed".to_string()
            } else {
                "Transfer stopped".to_string()
            },
            lines: vec![
                Line::from(vec![Span::styled("✗ ", theme::error()), Span::raw(what)]),
                Line::from(Span::styled(
                    ui::ellipsize(&message, LINE_WIDTH),
                    theme::dim(),
                )),
            ],
        }
    }
}

fn abbreviate_home(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => match path.strip_prefix(&home) {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("~{rest}"),
            _ => path.to_string(),
        },
        _ => path.to_string(),
    }
}
