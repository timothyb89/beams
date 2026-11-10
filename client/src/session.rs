//! Supervised interactive PTY session for `beams ssh`.
//!
//! `tsh ssh -t` used to replace the `beams` process with `exec(2)`, which
//! made the terminal transparent but left no local process capable of opening
//! a service palette or showing approval dialogs. This module keeps `tsh`
//! under a local PTY instead:
//!
//! - normal input/output remains byte-for-byte passthrough;
//! - terminal size changes are propagated to the local PTY (and therefore
//!   through `tsh` to the remote PTY);
//! - a configurable one-byte prefix opens a local ratatui palette;
//! - remote output is drained and bounded while that palette is visible;
//! - observed terminal settings are restored on normal/error/signal exit paths.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::io::{self, IsTerminal, Read, Write};
use std::str::FromStr;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::execute;
use crossterm::terminal::{self, Clear, ClearType};
use portable_pty::{native_pty_system, CommandBuilder, ExitStatus, MasterPty, PtyPair, PtySize};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
use signal_hook::iterator::{Handle as SignalHandle, Signals};

use beams_protocol::{
    FileCopyDirection, FolderSyncMode, OverwritePolicy, RequestStatus, ServiceAction,
    ServiceRequestKind,
};

use crate::clientd::{BridgeRecord, Frontend, RequestRecord, ServerEvent, ServiceRecord};
use crate::terminal_model::{TerminalEvent, TerminalModel};

mod baseline;
mod borrowed;
mod child;
mod form;
mod input;
mod keys;
mod notices;
mod picker;
mod recent;
mod toast;
mod ui;

use form::{ChoiceOption, Completion, Field, Form, FormEvent};
use input::ChildInput;
use keys::{Decoded, Key};

const LOOP_TICK: Duration = Duration::from_millis(25);
const INPUT_SEQUENCE_TIMEOUT: Duration = Duration::from_millis(25);
/// Remote output waits this long for the terminal to answer the entry
/// probes (normally a few milliseconds, ended early by the DA1 sentinel).
const PROBE_WINDOW: Duration = Duration::from_millis(500);
/// Replies to the entry probes are recognized for this long after they
/// were sent, so a slow terminal's answers never reach the remote as input.
const LATE_REPLY_WINDOW: Duration = Duration::from_secs(5);
const MAX_INPUT_SEQUENCE_LEN: usize = 64;
const EXIT_OUTPUT_GRACE: Duration = Duration::from_millis(250);
const REDRAW_RESIZE_DELAY: Duration = Duration::from_millis(40);
/// How long keys are ignored after an SSH key request's menu opens. Each
/// ignored key restarts it, so it ends only after a pause in typing: keys
/// still being typed for the shell when the menu key opened the review
/// can't act on it.
const AGENT_INPUT_GUARD: Duration = Duration::from_millis(500);
const DAEMON_RECONNECT_RETRY: Duration = Duration::from_secs(1);
const DEFERRED_OUTPUT_LIMIT: usize = 1024 * 1024;
const OUTPUT_QUEUE_CHUNKS: usize = 32;
const OUTPUT_CHUNKS_PER_TICK: usize = 16;
type PaletteTerminal = Terminal<CrosstermBackend<io::Stdout>>;

/// How the managed session draws its local UI.
#[derive(Clone, Copy, Debug, Default)]
pub struct DisplayOptions {
    /// Paint the menu over the captured remote screen (`--terminal-overlay`).
    pub overlay: bool,
}

/// The local prefix byte intercepted by the managed SSH session.
///
/// A disabled key is useful for compatibility/debugging and restores pure
/// passthrough behavior (at the cost of making the local palette unreachable).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EscapeKey {
    Byte(u8),
    Disabled,
}

impl EscapeKey {
    fn byte(self) -> Option<u8> {
        match self {
            EscapeKey::Byte(byte) => Some(byte),
            EscapeKey::Disabled => None,
        }
    }

    pub fn label(self) -> String {
        match self {
            EscapeKey::Disabled => "disabled".to_string(),
            EscapeKey::Byte(0x1d) => "Ctrl-]".to_string(),
            EscapeKey::Byte(byte @ 1..=26) => {
                format!("Ctrl-{}", char::from(b'A' + byte - 1))
            }
            EscapeKey::Byte(0) => "Ctrl-@".to_string(),
            EscapeKey::Byte(0x1c) => "Ctrl-\\".to_string(),
            EscapeKey::Byte(0x1e) => "Ctrl-^".to_string(),
            EscapeKey::Byte(0x1f) => "Ctrl-_".to_string(),
            EscapeKey::Byte(0x7f) => "Ctrl-?".to_string(),
            EscapeKey::Byte(byte) if byte.is_ascii_graphic() => char::from(byte).to_string(),
            EscapeKey::Byte(byte) => format!("0x{byte:02x}"),
        }
    }
}

impl Default for EscapeKey {
    fn default() -> Self {
        EscapeKey::Byte(0x1d)
    }
}

impl fmt::Display for EscapeKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.label())
    }
}

impl FromStr for EscapeKey {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim();
        if value.eq_ignore_ascii_case("none") || value.eq_ignore_ascii_case("disabled") {
            return Ok(EscapeKey::Disabled);
        }

        if let Some(hex) = value.strip_prefix("0x") {
            let byte = u8::from_str_radix(hex, 16).map_err(|_| {
                format!("invalid escape key {value:?}: expected one byte such as 0x1d")
            })?;
            return Ok(EscapeKey::Byte(byte));
        }

        let control = value
            .strip_prefix("ctrl-")
            .or_else(|| value.strip_prefix("Ctrl-"))
            .or_else(|| value.strip_prefix("CTRL-"));
        if let Some(control) = control {
            let bytes = control.as_bytes();
            if bytes.len() != 1 || !bytes[0].is_ascii() {
                return Err(format!(
                    "invalid escape key {value:?}: control keys must name one ASCII character"
                ));
            }
            let byte = match bytes[0] {
                b'?' => 0x7f,
                b'@'..=b'_' => bytes[0] & 0x1f,
                b'a'..=b'z' => bytes[0].to_ascii_uppercase() & 0x1f,
                _ => {
                    return Err(format!(
                        "invalid escape key {value:?}: use ctrl-a..ctrl-z, ctrl-], a byte like 0x1d, or none"
                    ))
                }
            };
            return Ok(EscapeKey::Byte(byte));
        }

        let bytes = value.as_bytes();
        if bytes.len() == 1 && bytes[0].is_ascii() {
            return Ok(EscapeKey::Byte(bytes[0]));
        }

        Err(format!(
            "invalid escape key {value:?}: use ctrl-], a byte like 0x1d, one ASCII character, or none"
        ))
    }
}

pub struct ConnectionInfo<'a> {
    pub beam_id: &'a str,
    pub login: &'a str,
}

pub struct LocalDaemon {
    frontend: Option<Frontend>,
    unavailable: Option<String>,
    managed_bridge: Option<ManagedBridge>,
    reconnect_after: Instant,
}

struct ManagedBridge {
    target: crate::bridge::Target,
    session_token: String,
}

impl LocalDaemon {
    pub fn connected(
        frontend: Frontend,
        target: crate::bridge::Target,
        session_token: String,
    ) -> Self {
        Self {
            frontend: Some(frontend),
            unavailable: None,
            managed_bridge: Some(ManagedBridge {
                target,
                session_token,
            }),
            reconnect_after: Instant::now(),
        }
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self {
            frontend: None,
            unavailable: Some(message.into()),
            managed_bridge: None,
            reconnect_after: Instant::now(),
        }
    }
}

/// Runs `program args...` under a local PTY until it exits, returning the
/// child's exit code (clamped to the process-portable 0..=255 range).
pub fn run(
    program: &str,
    args: &[String],
    info: ConnectionInfo<'_>,
    escape_key: EscapeKey,
    mut local_daemon: LocalDaemon,
    display: DisplayOptions,
) -> anyhow::Result<u8> {
    let overlay_enabled = display.overlay;
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("interactive `beams ssh` requires a terminal on stdin and stdout");
    }

    let initial_size = current_size().context("reading local terminal size")?;
    let PtyPair { master, slave } = native_pty_system()
        .openpty(initial_size)
        .context("opening local PTY for tsh")?;

    let mut command = CommandBuilder::new(program);
    command.args(args);
    let mut child = child::ChildGuard::new(
        slave.spawn_command(command)
            .with_context(|| format!("spawning {program} under local PTY"))?,
    );
    drop(slave);

    let mut reader = master
        .try_clone_reader()
        .context("cloning local PTY reader")?;
    let writer = ChildInput::new(master.take_writer().context("opening local PTY writer")?)
        .context("starting local PTY input worker")?;

    let (output_tx, output_rx) = mpsc::sync_channel(OUTPUT_QUEUE_CHUNKS);
    let output_thread = thread::spawn(move || read_pty_output(&mut reader, output_tx));

    let (signal_rx, signal_handle, signal_thread) = signal_watcher()?;

    if let Some(key) = escape_key.byte() {
        eprintln!(
            "beams: {} opens the local menu; press it twice to send it to the remote session",
            EscapeKey::Byte(key).label()
        );
    }

    let mut terminal_state = TerminalState::enter_raw().context("enabling raw terminal mode")?;
    terminal_state.managed = escape_key != EscapeKey::Disabled;
    let mut terminal_model = TerminalModel::new(initial_size);
    let drive_result = (|| {
        // Takeover never paints over remote output, so it neither probes
        // nor needs entry evidence; it keeps its fixed cleanup.
        if terminal_state.managed && overlay_enabled {
            terminal_state.capture_baseline()?;
            if terminal_state.baseline.answered_cpr {
                // Confirmed positioning support: push whatever was visible
                // before the session started into scrollback, so a later
                // paint restoring unseen cells as blank never looks like
                // data loss.
                let mut stdout = io::stdout().lock();
                stdout
                    .write_all(&baseline::entry_scrollback_push(initial_size.rows))
                    .context("pushing pre-session screen into scrollback")?;
                stdout.flush().context("flushing entry scrollback push")?;
            }
            {
                // Before any remote output: reset the few settings no
                // terminal can report, then let the model observe both
                // those resets and the reported settings as evidence.
                let mut stdout = io::stdout().lock();
                stdout
                    .write_all(baseline::ENTRY_RESETS)
                    .context("writing overlay entry resets")?;
                stdout.flush().context("flushing overlay entry resets")?;
                drop(stdout);
                terminal_model.process(baseline::ENTRY_RESETS);
                terminal_model.process(
                    &terminal_state.baseline.entry_evidence(initial_size.rows),
                );
                terminal_model.take_events();
            }
        }
        drive(
            &*master,
            &writer,
            &mut *child,
            &output_rx,
            &signal_rx,
            &info,
            escape_key,
            initial_size,
            &mut terminal_state,
            &mut local_daemon,
            &mut terminal_model,
            overlay_enabled,
        )
    })();

    // Restore the local terminal before waiting/printing any error. The child
    // is explicitly killed on a local driver error; on success it has already
    // exited and `kill` is unnecessary.
    if terminal_model.can_paint() {
        terminal_state.final_screen = terminal_state
            .baseline
            .observed_final_screen(&terminal_model)
            .cloned();
        if terminal_state.overlay_menu && terminal_state.palette_open {
            let _ = restore_terminal_model(&terminal_model, terminal_state.covered_alternate);
            terminal_state.palette_open = false;
        }
    }
    terminal_state.restore();
    if drive_result.is_err() {
        let _ = child.kill();
    }
    drop(writer);
    let _ = child.reap();

    // Closing the signal handle wakes its blocking iterator. Dropping a
    // JoinHandle detaches the thread; neither helper thread owns state needed
    // after `drive` has drained the child's final output, so session teardown
    // must never wait indefinitely for an OS-specific PTY/signal wakeup.
    signal_handle.close();
    drop(signal_thread);
    drop(output_thread);

    drive_result
}

#[allow(clippy::too_many_arguments)]
fn drive(
    master: &dyn MasterPty,
    writer: &ChildInput,
    child: &mut dyn portable_pty::Child,
    output_rx: &Receiver<OutputEvent>,
    signal_rx: &Receiver<i32>,
    info: &ConnectionInfo<'_>,
    escape_key: EscapeKey,
    mut last_size: PtySize,
    terminal_state: &mut TerminalState,
    local_daemon: &mut LocalDaemon,
    terminal_model: &mut TerminalModel,
    overlay_enabled: bool,
) -> anyhow::Result<u8> {
    let mut input_router = InputRouter::default();
    let mut toast_policy = notices::ToastPolicy::default();
    let mut painter = toast::ToastPainter::default();
    // What the toast on screen is about, for the menu prefix.
    let mut toast_subject: Option<notices::Subject> = None;
    // The last notice announced with a bell because it could not be painted.
    let mut belled_subject: Option<notices::Subject> = None;
    macro_rules! clear_toast {
        () => {{
            let clear = painter.clear(terminal_model);
            if !clear.is_empty() {
                let mut stdout = io::stdout().lock();
                let _ = stdout.write_all(&clear);
                let _ = stdout.flush();
            }
        }};
    }
    let mut deferred = DeferredOutput::default();
    let mut output_finished = false;
    let mut daemon_view = DaemonView::new(local_daemon);
    daemon_view.recents = recent::RecentPaths::load_default();
    // Offer to share a key once, if that was never set up and this
    // terminal's agent has keys to offer (only meaningful with a daemon),
    // and keep a hint on the home screen until it is.
    daemon_view.offer_key_setup = local_daemon.frontend.is_some()
        && !crate::agent::Config::configured()
        && crate::agent::own_agent_socket()
            .and_then(|sock| crate::agent::local_identities(&sock).ok())
            .is_some_and(|keys| !keys.is_empty());
    daemon_view.key_setup_hint = daemon_view.offer_key_setup;
    let mut palette_terminal = None;
    macro_rules! redraw_palette {
        () => {
            draw_palette(
                palette_terminal
                    .as_mut()
                    .expect("palette terminal exists while palette is open"),
                info,
                escape_key,
                &daemon_view,
                terminal_model,
                terminal_state.overlay_menu,
            )?
        };
    }

    loop {
        let daemon_changed = daemon_view.drain(local_daemon);
        // New requests get a toast; this only selects the newest on the
        // home screen, for when the menu is opened.
        if escape_key != EscapeKey::Disabled {
            daemon_view.take_approval_attention();
        }
        if daemon_changed && input_router.mode == InputMode::Palette {
            redraw_palette!();
        }

        if let Ok(signal) = signal_rx.try_recv() {
            let _ = child.kill();
            clear_toast!();
            close_palette(
                master,
                terminal_state,
                &mut palette_terminal,
                &mut input_router.mode,
                &mut deferred,
                last_size,
                terminal_model,
            )?;
            return Ok((128 + signal).clamp(1, 255) as u8);
        }

        let output = drain_output(
            output_rx,
            input_router.mode,
            &mut deferred,
            &mut output_finished,
            terminal_model,
            terminal_state.overlay_menu,
            &mut terminal_state.output,
            Some(writer),
            &mut painter,
            &mut terminal_state.borrowed,
        )?;
        let output_changed = output.changed;
        if let Some(resume) = output.resume {
            // The tracker holds the exact bounded prefix. close_palette
            // restores the last model state and replays that prefix before
            // the remaining bytes resume raw passthrough.
            painter.forget();
            if !output.replay_pending {
                terminal_state.output.cancel_pending();
            }
            close_palette(
                master,
                terminal_state,
                &mut palette_terminal,
                &mut input_router.mode,
                &mut deferred,
                last_size,
                terminal_model,
            )?;
            terminal_model.process(&resume);
            terminal_model.take_events();
            terminal_state.output.feed(&resume, Instant::now());
            let mut stdout = io::stdout().lock();
            stdout
                .write_all(&resume)
                .context("resuming oversized terminal sequence")?;
            stdout
                .flush()
                .context("flushing resumed terminal sequence")?;
        }
        if output_changed && input_router.mode == InputMode::Passthrough {
            painter.output_written();
        }
        if output_changed && terminal_state.menu_over_live_output()
            && input_router.mode == InputMode::Palette
        {
            redraw_palette!();
        }

        // Toasts: drawn over the live screen while the menu is closed. While
        // it's open, keep watching transfers so a finish still gets its toast.
        if input_router.mode == InputMode::Palette || terminal_state.palette_open {
            toast_policy.observe(&daemon_view, Instant::now());
        } else {
            let now = Instant::now();
            let current = toast_policy.current(&daemon_view, escape_key, now);
            toast_subject = current.as_ref().map(|(_, subject)| subject.clone());
            // A notice that cannot be painted rings the bell once instead of
            // vanishing; the menu (always available) lists what it was about.
            if let Some((toast, subject)) = &current {
                if belled_subject.as_ref() != Some(subject)
                    && !toast::ToastPainter::can_show(toast, terminal_model)
                    && terminal_state.output.at_boundary(now)
                {
                    let mut stdout = io::stdout().lock();
                    stdout.write_all(b"\x07").context("ringing for an unpainted notice")?;
                    stdout.flush().context("flushing notice bell")?;
                    belled_subject = Some(subject.clone());
                }
            }
            let paint = painter.sync(
                current.as_ref().map(|(toast, _)| toast),
                terminal_model,
                &mut terminal_state.output,
                now,
            );
            if !paint.is_empty() {
                let mut stdout = io::stdout().lock();
                stdout.write_all(&paint).context("drawing toast")?;
                stdout.flush().context("flushing toast")?;
            }
        }

        if let Some(status) = child.try_wait().context("polling tsh child")? {
            clear_toast!();
            close_palette(
                master,
                terminal_state,
                &mut palette_terminal,
                &mut input_router.mode,
                &mut deferred,
                last_size,
                terminal_model,
            )?;
            finish_output(output_rx, &mut output_finished, terminal_model)?;
            return Ok(exit_code(&status));
        }
        writer.check().context("writing to local PTY")?;

        if let Ok(size) = current_size() {
            if size != last_size {
                if terminal_state.menu_over_live_output() && input_router.mode == InputMode::Palette {
                    // Close using the last represented snapshot. The outer
                    // terminal may already have reflowed it: fidelity during
                    // a covered resize remains an explicit compatibility gate.
                    close_palette(master, terminal_state, &mut palette_terminal,
                        &mut input_router.mode, &mut deferred, last_size,
                        terminal_model)?;
                }
                master.resize(size).context("resizing local PTY")?;
                terminal_model.resize(size);
                last_size = size;
                // The remote repaints for the new size; don't "restore"
                // cells at the old toast position.
                painter.forget();
                if input_router.mode == InputMode::Palette {
                    redraw_palette!();
                }
            }
        }

        let now = Instant::now();
        if input_router.mode == InputMode::Passthrough {
            input_router.mouse_utf8 = terminal_model.screen().mouse_protocol_encoding()
                == vt100::MouseProtocolEncoding::Utf8;
        }
        let mut keys_handled = false;
        let mut actions = Vec::new();
        let mut stdin_closed = false;
        {
            // Menu keys are applied as they're decoded, not batched: a key
            // can close the menu, and everything typed after it must then go
            // to the remote session instead.
            let mut on_key = |key: Key| -> bool {
                keys_handled = true;
                daemon_view.handle_key(key, local_daemon)
            };
            let mut initial_input = std::mem::take(&mut terminal_state.initial_input);
            initial_input.extend(terminal_state.expire_late_replies(now));
            if !initial_input.is_empty() {
                actions.extend(input_router.route(escape_key, &initial_input, now, &mut on_key));
            }
            actions.extend(input_router.flush_expired(escape_key, now, &mut on_key));
            let mut poll_timeout = input_router.poll_timeout(now);
            if terminal_state.late_replies.is_some() {
                poll_timeout = poll_timeout.min(LOOP_TICK);
            }
            if stdin_ready(poll_timeout).context("polling terminal input")? {
                let mut bytes = [0u8; 4096];
                let read = io::stdin()
                    .read(&mut bytes)
                    .context("reading terminal input")?;
                if read == 0 {
                    stdin_closed = true;
                } else {
                    let input = terminal_state.filter_late_replies(&bytes[..read], Instant::now());
                    if !input.is_empty() {
                        actions.extend(input_router.route(
                            escape_key,
                            &input,
                            Instant::now(),
                            &mut on_key,
                        ));
                    }
                }
            }
            if !stdin_closed {
                actions.extend(input_router.flush_expired(escape_key, Instant::now(), &mut on_key));
            }
        }
        if stdin_closed {
            let _ = child.kill();
            clear_toast!();
            close_palette(
                master,
                terminal_state,
                &mut palette_terminal,
                &mut input_router.mode,
                &mut deferred,
                last_size,
                terminal_model,
            )?;
            return Ok(0);
        }
        for action in actions {
            match action {
                InputAction::Remote(bytes) => {
                    writer.enqueue(&bytes).context("queueing local PTY input")?;
                }
                InputAction::OpenPalette => {
                    painter.forget();
                    // Overlay only what the model can restore. Otherwise take
                    // the screen over: a menu must always appear, and keys
                    // after the prefix must reach it, never the remote.
                    let can_overlay = overlay_enabled && terminal_model.can_paint();
                    // Unknown cells no longer block the overlay, but borrowing
                    // never touches primary content at all: prefer it over an
                    // overlay that would restore unobserved cells as blank.
                    let borrow = overlay_enabled
                        && borrow_enabled()
                        && terminal_model.can_borrow()
                        && terminal_state.output.at_boundary(Instant::now())
                        && (!can_overlay
                            || terminal_model.doubtful_restore(terminal_model.screen().alternate_screen()));
                    let overlay = can_overlay && !borrow;
                    if overlay {
                        terminal_model.begin_overlay();
                    }
                    terminal_state.open_palette(terminal_model.screen(), overlay, borrow)?;
                    palette_terminal = Some(new_palette_terminal()?);
                    // The prefix while a request's toast is up goes straight
                    // to that request.
                    daemon_view.reviewing_from_toast = false;
                    match toast_subject.take() {
                        Some(notices::Subject::Request(request_id)) => {
                            daemon_view.review_request(&request_id, local_daemon);
                        }
                        Some(notices::Subject::KeySetup) => {
                            daemon_view.reviewing_from_toast = true;
                            daemon_view.open_agent_keys_menu();
                        }
                        _ => {}
                    }
                    toast_policy.mark_reviewed(&daemon_view);
                    redraw_palette!();
                }
                InputAction::ClosePalette => {
                    painter.forget();
                    close_palette(
                        master,
                        terminal_state,
                        &mut palette_terminal,
                        &mut input_router.mode,
                        &mut deferred,
                        last_size,
                        terminal_model,
                    )?;
                }
            }
        }
        if keys_handled && input_router.mode == InputMode::Palette && palette_terminal.is_some() {
            redraw_palette!();
        }
    }
}

struct DaemonView {
    beam_uuid: Option<String>,
    client_id: Option<String>,
    connected: bool,
    frontends: usize,
    requests: usize,
    services: usize,
    request_records: BTreeMap<String, RequestRecord>,
    service_records: BTreeMap<String, ServiceRecord>,
    /// Usable startup SSH key grants, by id.
    grants: BTreeMap<String, crate::startup::GrantRecord>,
    bridge_statuses: BTreeMap<String, String>,
    /// The selected home-screen row (see `selected_index`).
    selected: usize,
    selected_key: Option<RowKey>,
    /// First-seen order of requests, for listing approvals oldest first.
    request_arrival: BTreeMap<String, u64>,
    screen: Screen,
    presented_approval_requests: BTreeSet<String>,
    /// Connection status of the local daemon and this session's bridge.
    message: String,
    /// Feedback from the most recent menu action.
    notice: Option<Notice>,
    /// Workstation directories offered by empty path fields.
    recents: recent::RecentPaths,
    /// The menu was opened from a request toast (see `review_request`).
    reviewing_from_toast: bool,
    /// Keys are ignored until then (except Esc): set when an SSH key
    /// request's menu opens, so typing that was meant for the shell can't
    /// approve a signature.
    input_guard_until: Option<Instant>,
    /// `[ssh_agent]` was never configured and this terminal's agent has
    /// keys: offer (once) to share one.
    offer_key_setup: bool,
    /// The same condition, for a home-screen line that stays until a key
    /// is shared or declined (the toast is easy to miss).
    key_setup_hint: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Notice {
    text: String,
    error: bool,
}

/// What the local menu is currently showing.
enum Screen {
    Home,
    Form { kind: FormKind, form: Form },
    Menu(Menu),
}

/// Which action submitting the current form performs.
#[derive(Clone, Debug, PartialEq, Eq)]
enum FormKind {
    Forward,
    Copy(FileCopyDirection),
    CopyApproval {
        request_id: String,
    },
    /// Choosing where a `beams fetch` lands; `into_dir` when it's several
    /// items (or a directory), which go inside the destination.
    FetchApproval {
        request_id: String,
        into_dir: bool,
    },
    Sync,
    SyncApproval {
        request_id: String,
    },
}

/// One selectable row on the home screen.
#[derive(Clone, Copy)]
enum Row<'a> {
    Request(&'a RequestRecord),
    Grant(&'a crate::startup::GrantRecord),
    Service(&'a ServiceRecord),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RowKey {
    Request(String),
    Grant(String),
    Service(String),
}

impl Row<'_> {
    fn key(&self) -> RowKey {
        match self {
            Row::Request(request) => RowKey::Request(request.request_id.clone()),
            Row::Grant(grant) => RowKey::Grant(grant.grant_id.clone()),
            Row::Service(service) => RowKey::Service(service.service_id.clone()),
        }
    }
}

const ALL_ACTIONS: [ServiceAction; 5] = [
    ServiceAction::Stop,
    ServiceAction::Restart,
    ServiceAction::Pause,
    ServiceAction::Resume,
    ServiceAction::Terminate,
];

/// What choosing a menu item does.
#[derive(Clone, Debug, PartialEq, Eq)]
enum MenuAction {
    Back,
    Approve(String),
    Reject(String),
    RevokeGrant(String),
    /// Let Beams ask to use this key (fingerprint).
    AllowKey(String),
    /// Stop sharing this key.
    DenyKey(String),
    /// Record "share no keys", so the offer isn't made again.
    DeclineKeys,
    Control {
        service_id: String,
        action: ServiceAction,
    },
    /// Ask before running a destructive service action.
    Confirm {
        service_id: String,
        action: ServiceAction,
    },
}

#[derive(Clone, Debug)]
struct MenuItem {
    label: String,
    /// A shortcut that chooses this item directly.
    key: Option<char>,
    danger: bool,
    action: MenuAction,
}

/// A small modal list of choices: request decisions, service actions, and
/// confirmations.
#[derive(Clone, Debug)]
struct Menu {
    title: String,
    /// `(text, warn)` lines above the items.
    intro: Vec<(String, bool)>,
    items: Vec<MenuItem>,
    selected: usize,
}

impl Menu {
    /// Returns the chosen action, if this key chose one.
    fn handle_key(&mut self, key: &Key) -> Option<MenuAction> {
        let count = self.items.len();
        match key {
            Key::Esc | Key::Char('q' | 'Q') => return Some(MenuAction::Back),
            Key::Up | Key::Char('k') | Key::BackTab if count > 0 => {
                self.selected = (self.selected + count - 1) % count
            }
            Key::Down | Key::Char('j') | Key::Tab if count > 0 => {
                self.selected = (self.selected + 1) % count
            }
            Key::Enter => {
                return self
                    .items
                    .get(self.selected)
                    .map(|item| item.action.clone())
            }
            Key::Char(ch) => {
                let ch = ch.to_ascii_lowercase();
                return self
                    .items
                    .iter()
                    .find(|item| item.key == Some(ch))
                    .map(|item| item.action.clone());
            }
            _ => {}
        }
        None
    }
}

fn back_item(label: &str) -> MenuItem {
    MenuItem {
        label: label.to_string(),
        key: None,
        danger: false,
        action: MenuAction::Back,
    }
}

fn request_menu(request: &RequestRecord) -> Menu {
    if let Some(prompt) = &request.agent {
        // Deny is the default: keys typed into the shell while the menu
        // opens must not approve a signature.
        return Menu {
            title: "SSH key request".to_string(),
            intro: prompt.describe(),
            items: vec![
                MenuItem {
                    label: "Allow once".to_string(),
                    key: Some('y'),
                    danger: false,
                    action: MenuAction::Approve(request.request_id.clone()),
                },
                MenuItem {
                    label: "Deny".to_string(),
                    key: Some('n'),
                    danger: false,
                    action: MenuAction::Reject(request.request_id.clone()),
                },
                back_item("Decide later"),
            ],
            selected: 1,
        };
    }
    let needs_path = matches!(
        request.request.as_ref().map(|request| &request.kind),
        Some(ServiceRequestKind::FileCopy { .. } | ServiceRequestKind::FolderSync { .. })
    );
    Menu {
        title: "Remote request".to_string(),
        intro: ui::describe_request(request),
        items: vec![
            MenuItem {
                label: if needs_path {
                    "Approve - confirm the workstation path next".to_string()
                } else {
                    "Approve".to_string()
                },
                key: Some('y'),
                danger: false,
                action: MenuAction::Approve(request.request_id.clone()),
            },
            MenuItem {
                label: "Reject".to_string(),
                key: Some('n'),
                danger: false,
                action: MenuAction::Reject(request.request_id.clone()),
            },
            back_item("Decide later"),
        ],
        selected: 0,
    }
}

/// The SSH keys menu (home `a`, or the setup toast): this terminal's agent
/// keys, each allowable or revocable for Beams.
fn agent_keys_menu() -> Menu {
    let configured = crate::agent::Config::configured();
    let config = crate::agent::Config::load().unwrap_or_default();
    let keys = crate::agent::own_agent_socket()
        .ok_or_else(|| "this terminal has no SSH agent (SSH_AUTH_SOCK is unset)".to_string())
        .and_then(|sock| crate::agent::local_identities(&sock).map_err(|err| format!("{err:#}")));
    let mut intro = vec![(
        "Beams have no SSH keys of their own. Shared keys stay in this terminal's agent, and you approve each use."
            .to_string(),
        false,
    )];
    let mut items = Vec::new();
    match keys {
        Ok(keys) if !keys.is_empty() => {
            for (index, key) in keys.iter().take(9).enumerate() {
                let Some(fingerprint) = beams_protocol::agent_crypto::fingerprint(&key.key_blob) else {
                    continue;
                };
                let name = if key.comment.is_empty() {
                    fingerprint.clone()
                } else {
                    format!("{} ({})", crate::agent::clean(&key.comment), ui::ellipsize(&fingerprint, 20))
                };
                let allowed = config.allowed_keys.contains(&fingerprint);
                items.push(MenuItem {
                    label: if allowed { format!("Stop sharing {name}") } else { format!("Share {name}") },
                    key: char::from_digit(index as u32 + 1, 10),
                    danger: allowed,
                    action: if allowed { MenuAction::DenyKey(fingerprint) } else { MenuAction::AllowKey(fingerprint) },
                });
            }
        }
        Ok(_) => intro.push(("This terminal's agent has no keys; add one with `ssh-add` first.".to_string(), true)),
        Err(why) => intro.push((format!("Can't list keys: {why}."), true)),
    }
    if !configured {
        items.push(MenuItem {
            label: "Don't share any (don't ask again)".to_string(),
            key: Some('x'),
            danger: false,
            action: MenuAction::DeclineKeys,
        });
    }
    items.push(back_item(if configured { "Back" } else { "Not now" }));
    Menu {
        title: "SSH keys for Beams".to_string(),
        intro,
        selected: if configured { items.len() - 1 } else { 0 },
        items,
    }
}

fn grant_menu(grant: &crate::startup::GrantRecord) -> Menu {
    Menu {
        title: "Startup SSH key grant".to_string(),
        intro: grant.describe(crate::agent::now_ms()),
        items: vec![
            MenuItem {
                label: "Revoke now".to_string(),
                key: Some('r'),
                danger: true,
                action: MenuAction::RevokeGrant(grant.grant_id.clone()),
            },
            back_item("Keep"),
        ],
        selected: 1,
    }
}

fn service_menu(service: &ServiceRecord) -> Menu {
    let mut intro: Vec<(String, bool)> = ui::service_details(service)
        .into_iter()
        .map(|line| (line, false))
        .collect();
    let mut items: Vec<MenuItem> = ui::available_actions(service)
        .into_iter()
        .map(|action| MenuItem {
            label: ui::capitalize(ui::action_description(service, action)),
            key: Some(ui::action_key(action)),
            danger: ui::needs_confirmation(service, action),
            action: if ui::needs_confirmation(service, action) {
                MenuAction::Confirm {
                    service_id: service.service_id.clone(),
                    action,
                }
            } else {
                MenuAction::Control {
                    service_id: service.service_id.clone(),
                    action,
                }
            },
        })
        .collect();
    if items.is_empty() {
        intro.push((
            "This service has finished; start a new one instead.".to_string(),
            false,
        ));
    }
    items.push(back_item("Back"));
    Menu {
        title: ui::service_title(service).to_string(),
        intro,
        items,
        selected: 0,
    }
}

/// Destructive actions default to "keep it": Enter alone never destroys.
fn confirm_menu(service: &ServiceRecord, action: ServiceAction) -> Menu {
    let (_, endpoint) = ui::describe_service(service);
    Menu {
        title: format!(
            "{}?",
            ui::capitalize(ui::action_description(service, action))
        ),
        intro: vec![
            (endpoint, false),
            (ui::confirmation_warning(service, action).to_string(), true),
        ],
        items: vec![
            MenuItem {
                label: "Keep it".to_string(),
                key: Some('n'),
                danger: false,
                action: MenuAction::Back,
            },
            MenuItem {
                label: ui::capitalize(ui::action_description(service, action)),
                key: Some('y'),
                danger: true,
                action: MenuAction::Control {
                    service_id: service.service_id.clone(),
                    action,
                },
            },
        ],
        selected: 0,
    }
}

const SYNC_MODES: [FolderSyncMode; 4] = [
    FolderSyncMode::TwoWaySafe,
    FolderSyncMode::TwoWayResolved,
    FolderSyncMode::OneWaySafe,
    FolderSyncMode::OneWayReplica,
];

fn sync_mode_warning(mode: FolderSyncMode) -> &'static str {
    match mode {
        FolderSyncMode::TwoWaySafe => "Conflicting edits are preserved for manual resolution.",
        FolderSyncMode::TwoWayResolved => {
            "Workstation content wins conflicts; remote conflicting edits may be overwritten."
        }
        FolderSyncMode::OneWaySafe => {
            "Changes flow workstation → Beam; Beam-only edits are not propagated back."
        }
        FolderSyncMode::OneWayReplica => {
            "Destructive: the Beam is maintained as a replica; remote changes/deletions may be overwritten."
        }
    }
}

fn forward_form() -> Form {
    Form::new(
        "Forward a Beam port",
        "create",
        vec![
            Field::number("remote_port", "Beam port").placeholder("e.g. 8080"),
            Field::number("local_port", "Workstation port").placeholder("automatic"),
        ],
    )
    .intro(
        "Listen on a workstation port and forward each connection to 127.0.0.1 in the Beam.",
        false,
    )
}

fn copy_form(direction: FileCopyDirection) -> Form {
    let local = Field::text("local_path", "Workstation path")
        .placeholder("e.g. ~/notes.txt")
        .complete(Completion::Paths);
    let remote = Field::text("remote_path", "Beam path").placeholder("e.g. /home/beams/notes.txt");
    let overwrite = Field::toggle(
        "overwrite",
        "Overwrite",
        false,
        "replace an existing destination",
        "fail if the destination exists",
    );
    match direction {
        FileCopyDirection::Upload => Form::new(
            "Upload a file to the Beam",
            "upload",
            vec![local, remote, overwrite],
        ),
        FileCopyDirection::Download => Form::new(
            "Download a file from the Beam",
            "download",
            vec![remote, local, overwrite],
        ),
    }
}

fn sync_form() -> Form {
    let modes = SYNC_MODES
        .iter()
        .map(|mode| ChoiceOption {
            label: ui::mode_label(*mode),
            help: sync_mode_warning(*mode),
            warn: *mode != FolderSyncMode::TwoWaySafe,
        })
        .collect();
    Form::new(
        "Sync a folder with the Beam",
        "create",
        vec![
            Field::text("local_path", "Workstation dir")
                .placeholder("e.g. ~/src/project")
                .complete(Completion::Directories),
            Field::text("remote_path", "Beam dir").placeholder("e.g. /home/beams/projects/project"),
            Field::choice("mode", "Mode", modes, 0),
            Field::toggle(
                "ignore_vcs",
                "Ignore VCS",
                true,
                "skip .git and other VCS metadata",
                "sync VCS metadata too",
            ),
            Field::text("ignores", "Ignore rules")
                .placeholder("comma-separated, e.g. target, node_modules"),
        ],
    )
    .note("Both directories must already exist.")
}

/// The path-confirmation form shown when approving a Beam-proposed copy or
/// sync: the Beam can only suggest a workstation path, never choose it.
fn approval_form(
    request: &RequestRecord,
    title: &str,
    label: &'static str,
    completion: Completion,
) -> Form {
    let hint = match request.request.as_ref().map(|request| &request.kind) {
        Some(
            ServiceRequestKind::FileCopy {
                local_path_hint, ..
            }
            | ServiceRequestKind::FolderSync {
                local_path_hint, ..
            },
        ) => local_path_hint.clone().unwrap_or_default(),
        _ => String::new(),
    };
    let mut form = Form::new(
        title,
        "approve",
        vec![Field::text("local_path", label)
            .value(&hint)
            .complete(completion)],
    );
    for (sentence, warn) in ui::describe_request(request) {
        form = form.intro(sentence, warn);
    }
    form.intro(
        "The Beam can only suggest a workstation path. Confirm or edit the real one before approving.",
        true,
    )
}

/// Where a `beams fetch` should land. Prefilled with the Beam's suggestion,
/// else the most recently used directory, else the directory `beams ssh`
/// was started from; the dropdown helps change it.
fn fetch_approval_form(request: &RequestRecord, into_dir: bool, recents: &[String]) -> Form {
    let hint = match request.request.as_ref().map(|request| &request.kind) {
        Some(ServiceRequestKind::FileFetch {
            local_path_hint: Some(hint),
            ..
        }) => Some(hint.clone()),
        _ => None,
    };
    let suggested = hint.is_some();
    let value = hint
        .or_else(|| recents.first().cloned())
        .unwrap_or_else(|| "./".to_string());
    let mut form = Form::new(
        "Fetch from the Beam",
        "fetch",
        vec![Field::text("local_path", "Save to")
            .value(&value)
            .complete(Completion::Paths)],
    );
    for (sentence, warn) in ui::describe_request(request) {
        form = form.intro(sentence, warn);
    }
    form = form.note(if into_dir {
        "Everything goes inside the destination directory (created if needed)."
    } else {
        "Give a directory to keep the file's name, or a full path to rename it."
    });
    if suggested {
        form = form.note("The destination is the Beam's suggestion; check it before fetching.");
    }
    form
}

fn required(form: &mut Form, id: &str, what: &str) -> String {
    let value = form.text(id);
    if value.is_empty() {
        form.set_error(id, format!("{what} is required"));
    }
    value
}

fn local_path(form: &mut Form, id: &str, what: &str) -> String {
    let value = required(form, id, what);
    if value.is_empty() {
        return value;
    }
    form::resolve_local_path(&value)
        .to_string_lossy()
        .into_owned()
}

fn port(form: &mut Form, id: &str, optional: bool) -> Option<u16> {
    let value = form.text(id);
    if value.is_empty() {
        if !optional {
            form.set_error(id, "a port is required");
        }
        return None;
    }
    match value.parse::<u16>() {
        Ok(port) if port > 0 => Some(port),
        _ => {
            form.set_error(id, "ports are 1-65535");
            None
        }
    }
}

fn session_target(
    local_daemon: &mut LocalDaemon,
) -> Result<(&crate::bridge::Target, &mut Frontend), String> {
    let managed = local_daemon
        .managed_bridge
        .as_ref()
        .ok_or("this session has no managed Beam target")?;
    let frontend = local_daemon
        .frontend
        .as_mut()
        .ok_or("the local daemon connection is unavailable")?;
    Ok((&managed.target, frontend))
}

fn daemon_frontend(local_daemon: &mut LocalDaemon) -> Result<&mut Frontend, String> {
    local_daemon
        .frontend
        .as_mut()
        .ok_or_else(|| "the local daemon connection is unavailable".to_string())
}

/// The workstation path a submitted form used, for the recents list, and
/// whether it names a directory.
fn used_local_path(kind: &FormKind, form: &Form) -> Option<(std::path::PathBuf, bool)> {
    let is_dir = match kind {
        FormKind::Forward => return None,
        FormKind::Copy(_) | FormKind::CopyApproval { .. } => false,
        FormKind::FetchApproval { into_dir, .. } => {
            let text = form.text("local_path");
            *into_dir || text.ends_with('/') || form::resolve_local_path(&text).is_dir()
        }
        FormKind::Sync | FormKind::SyncApproval { .. } => true,
    };
    let text = form.text("local_path");
    (!text.is_empty()).then(|| (form::resolve_local_path(&text), is_dir))
}

/// Validates and submits `form`. `Ok(Some(notice))` closes the form,
/// `Ok(None)` means field errors were set, `Err` is a form-level error.
fn submit_form(
    kind: &FormKind,
    form: &mut Form,
    local_daemon: &mut LocalDaemon,
) -> Result<Option<String>, String> {
    match kind {
        FormKind::Forward => {
            let remote_port = port(form, "remote_port", false);
            let local_port = port(form, "local_port", true);
            let Some(remote_port) = remote_port.filter(|_| !form.has_errors()) else {
                return Ok(None);
            };
            let (target, frontend) = session_target(local_daemon)?;
            frontend
                .create_port_forward(target, "127.0.0.1", remote_port, local_port)
                .map_err(|err| format!("could not create the forward: {err}"))?;
            Ok(Some(format!(
                "forwarding workstation port {} → 127.0.0.1:{remote_port}",
                local_port
                    .map(|port| port.to_string())
                    .unwrap_or_else(|| "(automatic)".to_string())
            )))
        }
        FormKind::Copy(direction) => {
            let local = local_path(form, "local_path", "a workstation path");
            let remote = required(form, "remote_path", "a Beam path");
            if form.has_errors() {
                return Ok(None);
            }
            let overwrite = if form.toggle("overwrite") {
                OverwritePolicy::Replace
            } else {
                OverwritePolicy::Deny
            };
            let (target, frontend) = session_target(local_daemon)?;
            frontend
                .create_file_copy(target, *direction, &local, &remote, overwrite)
                .map_err(|err| format!("could not start the copy: {err}"))?;
            Ok(Some(format!(
                "starting {}",
                ui::direction_label(*direction)
            )))
        }
        FormKind::CopyApproval { request_id } => {
            let local = local_path(form, "local_path", "a workstation path");
            if form.has_errors() {
                return Ok(None);
            }
            daemon_frontend(local_daemon)?
                .decide_file_copy(request_id, true, Some(&local))
                .map_err(|err| format!("could not approve the copy: {err}"))?;
            Ok(Some(format!("approved file copy to {local}")))
        }
        FormKind::FetchApproval { request_id, .. } => {
            let local = local_path(form, "local_path", "a workstation destination");
            if form.has_errors() {
                return Ok(None);
            }
            daemon_frontend(local_daemon)?
                .decide_file_copy(request_id, true, Some(&local))
                .map_err(|err| format!("could not approve the fetch: {err}"))?;
            Ok(Some(format!("fetching to {local}")))
        }
        FormKind::Sync => {
            let local = local_path(form, "local_path", "a workstation directory");
            let remote = required(form, "remote_path", "a Beam directory");
            if form.has_errors() {
                return Ok(None);
            }
            let ignores = form
                .text("ignores")
                .split(',')
                .map(str::trim)
                .filter(|ignore| !ignore.is_empty())
                .map(str::to_string)
                .collect();
            let mode = SYNC_MODES[form.choice("mode").min(SYNC_MODES.len() - 1)];
            let ignore_vcs = form.toggle("ignore_vcs");
            let (target, frontend) = session_target(local_daemon)?;
            frontend
                .create_folder_sync(target, &local, &remote, mode, ignore_vcs, ignores)
                .map_err(|err| format!("could not create the sync: {err}"))?;
            Ok(Some("creating folder sync".to_string()))
        }
        FormKind::SyncApproval { request_id } => {
            let local = local_path(form, "local_path", "a workstation directory");
            if form.has_errors() {
                return Ok(None);
            }
            daemon_frontend(local_daemon)?
                .decide_folder_sync(request_id, true, Some(&local))
                .map_err(|err| format!("could not approve the sync: {err}"))?;
            Ok(Some(format!("approved folder sync with {local}")))
        }
    }
}

impl DaemonView {
    fn new(local_daemon: &LocalDaemon) -> Self {
        let beam_uuid = local_daemon
            .managed_bridge
            .as_ref()
            .map(|managed| managed.target.beam_uuid.clone());
        match &local_daemon.frontend {
            Some(frontend) => Self {
                beam_uuid,
                client_id: Some(frontend.client_id.clone()),
                connected: true,
                frontends: 1,
                requests: 0,
                services: 0,
                request_records: BTreeMap::new(),
                service_records: BTreeMap::new(),
                grants: BTreeMap::new(),
                bridge_statuses: BTreeMap::new(),
                selected: 0,
                selected_key: None,
                request_arrival: BTreeMap::new(),
                screen: Screen::Home,
                presented_approval_requests: BTreeSet::new(),
                notice: None,
                message: "connected".to_string(),
                recents: Default::default(),
                reviewing_from_toast: false,
                input_guard_until: None,
                offer_key_setup: false,
                key_setup_hint: false,
            },
            None => Self {
                beam_uuid,
                client_id: None,
                connected: false,
                frontends: 0,
                requests: 0,
                services: 0,
                request_records: BTreeMap::new(),
                service_records: BTreeMap::new(),
                grants: BTreeMap::new(),
                bridge_statuses: BTreeMap::new(),
                selected: 0,
                selected_key: None,
                request_arrival: BTreeMap::new(),
                screen: Screen::Home,
                presented_approval_requests: BTreeSet::new(),
                notice: None,
                message: local_daemon
                    .unavailable
                    .clone()
                    .unwrap_or_else(|| "unavailable".to_string()),
                recents: Default::default(),
                reviewing_from_toast: false,
                input_guard_until: None,
                offer_key_setup: false,
                key_setup_hint: false,
            },
        }
    }

    /// Applies queued daemon events and reports whether visible state changed.
    fn drain(&mut self, local_daemon: &mut LocalDaemon) -> bool {
        let mut changed = false;
        let reconnect =
            if local_daemon.frontend.is_none() && Instant::now() >= local_daemon.reconnect_after {
                local_daemon
                    .managed_bridge
                    .as_ref()
                    .map(|managed| (managed.target.clone(), managed.session_token.clone()))
            } else {
                None
            };
        if let Some((target, session_token)) = reconnect {
            match crate::clientd::connect_or_start().and_then(|mut frontend| {
                frontend.ensure_bridge(&target, &session_token)?;
                Ok(frontend)
            }) {
                Ok(frontend) => {
                    self.client_id = Some(frontend.client_id.clone());
                    self.connected = true;
                    self.message = "reconnected to local daemon".to_string();
                    local_daemon.frontend = Some(frontend);
                    changed = true;
                }
                Err(err) => {
                    self.connected = false;
                    self.message = format!("reconnecting to local daemon: {err}");
                    local_daemon.reconnect_after = Instant::now() + DAEMON_RECONNECT_RETRY;
                    return true;
                }
            }
        }

        let Some(frontend) = &mut local_daemon.frontend else {
            return changed;
        };
        loop {
            match frontend.events.try_recv() {
                Ok(ServerEvent::Snapshot {
                    client_id,
                    frontends,
                    requests,
                    services,
                    bridges,
                    grants,
                    ..
                }) => {
                    self.grants = grants
                        .into_iter()
                        .map(|grant| (grant.grant_id.clone(), grant))
                        .collect();
                    self.client_id = Some(client_id);
                    self.connected = true;
                    self.frontends = frontends;
                    self.requests = requests.len();
                    self.services = services.len();
                    for request in &requests {
                        self.note_request_arrival(&request.request_id);
                    }
                    self.request_records = requests
                        .into_iter()
                        .map(|request| (request.request_id.clone(), request))
                        .collect();
                    self.service_records = services
                        .into_iter()
                        .map(|service| (service.service_id.clone(), service))
                        .collect();
                    self.bridge_statuses = bridges
                        .into_iter()
                        .map(|bridge| (bridge.beam_uuid, bridge.status))
                        .collect();
                    self.message = "connected".to_string();
                    changed = true;
                }
                Ok(ServerEvent::DaemonStatus { frontends }) => {
                    self.frontends = frontends;
                    self.connected = true;
                    changed = true;
                }
                Ok(ServerEvent::Error { message }) => {
                    self.notify(format!("daemon error: {message}"), true);
                    changed = true;
                }
                Ok(ServerEvent::RequestChanged { request }) => {
                    self.note_request_arrival(&request.request_id);
                    self.request_records
                        .insert(request.request_id.clone(), request);
                    self.requests = self.request_records.len();
                    self.close_stale_approval();
                    changed = true;
                }
                Ok(ServerEvent::ServiceChanged { service }) => {
                    self.service_records
                        .insert(service.service_id.clone(), service);
                    self.services = self.service_records.len();
                    changed = true;
                }
                Ok(ServerEvent::BridgeStatus { bridge }) => {
                    self.apply_bridge_status(bridge);
                    changed = true;
                }
                Ok(ServerEvent::GrantChanged { grant }) => {
                    if grant.ended.is_some() {
                        self.grants.remove(&grant.grant_id);
                    } else {
                        self.grants.insert(grant.grant_id.clone(), grant);
                    }
                    changed = true;
                }
                Ok(
                    ServerEvent::Hello { .. }
                    | ServerEvent::Pong
                    | ServerEvent::DaemonInfo { .. }
                    | ServerEvent::ShuttingDown
                    | ServerEvent::ShutdownRefused { .. }
                    // Shown by `beams config launch`; the session overlay
                    // doesn't list launches yet.
                    | ServerEvent::LaunchChanged { .. },
                ) => {}
                Err(TryRecvError::Empty) => return changed,
                Err(TryRecvError::Disconnected) => {
                    self.connected = false;
                    self.message = "connection lost".to_string();
                    local_daemon.frontend = None;
                    local_daemon.reconnect_after = Instant::now() + DAEMON_RECONNECT_RETRY;
                    return true;
                }
            }
        }
    }

    fn apply_bridge_status(&mut self, bridge: BridgeRecord) {
        if bridge.status == "retired" {
            self.bridge_statuses.remove(&bridge.beam_uuid);
        } else {
            self.bridge_statuses
                .insert(bridge.beam_uuid.clone(), bridge.status.clone());
        }
        // The daemon is shared by every `beams ssh` session and
        // broadcasts every Beam's bridge; only this session's
        // Beam belongs in its status line.
        if self.beam_uuid.as_deref() == Some(bridge.beam_uuid.as_str()) {
            self.message = format!(
                "bridge to {}: {}{}",
                bridge.beam_id,
                bridge.status,
                bridge
                    .message
                    .as_deref()
                    .map(|message| format!(" ({message})"))
                    .unwrap_or_default()
            );
        }
    }

    /// Pending approvals for this session's Beam, oldest first.
    fn pending_requests(&self) -> Vec<&RequestRecord> {
        let mut pending: Vec<&RequestRecord> = self
            .request_records
            .values()
            .filter(|request| {
                request.status == RequestStatus::PendingApproval
                    && self.beam_uuid.as_deref() == Some(request.beam_uuid.as_str())
            })
            .collect();
        pending.sort_by_key(|request| {
            self.request_arrival
                .get(&request.request_id)
                .copied()
                .unwrap_or(u64::MAX)
        });
        pending
    }

    /// The request y/n act on: the selected one, else the oldest pending.
    fn pending_request(&self) -> Option<&RequestRecord> {
        match self.selected_row() {
            Some(Row::Request(request)) => Some(request),
            _ => self.pending_requests().first().copied(),
        }
    }

    fn pending_count(&self) -> usize {
        self.pending_requests().len()
    }

    /// Records first-seen order so pending approvals list oldest first
    /// (request ids are random, so map order means nothing).
    fn note_request_arrival(&mut self, request_id: &str) {
        if !self.request_arrival.contains_key(request_id) {
            let next = self.request_arrival.len() as u64;
            self.request_arrival.insert(request_id.to_string(), next);
        }
    }

    /// Marks newly pending approvals for this session's Beam as presented,
    /// selecting the oldest new one so the menu opens on it. True when
    /// there was one (once per request).
    fn take_approval_attention(&mut self) -> bool {
        let new_requests = self
            .pending_requests()
            .into_iter()
            .filter(|request| {
                !self
                    .presented_approval_requests
                    .contains(&request.request_id)
            })
            .map(|request| request.request_id.clone())
            .collect::<Vec<_>>();
        let Some(first) = new_requests.first() else {
            return false;
        };
        self.selected_key = Some(RowKey::Request(first.clone()));
        self.presented_approval_requests.extend(new_requests);
        // The previous action's feedback is stale by now.
        self.notice = None;
        true
    }

    /// Every selectable home-screen row: pending approvals, startup SSH key
    /// grants, then services.
    fn rows(&self) -> Vec<Row<'_>> {
        self.pending_requests()
            .into_iter()
            .map(Row::Request)
            .chain(self.visible_grants().into_iter().map(Row::Grant))
            .chain(self.visible_services().into_iter().map(Row::Service))
            .collect()
    }

    /// This session's Beam's usable grants.
    fn visible_grants(&self) -> Vec<&crate::startup::GrantRecord> {
        self.grants
            .values()
            .filter(|grant| self.beam_uuid.as_deref() == Some(grant.beam_uuid.as_str()))
            .collect()
    }

    /// The selected row's index. Selection follows the row's identity, so
    /// deciding a request or a service disappearing doesn't silently move it
    /// onto a different row; if the row is gone, the nearest index is used.
    fn selected_index(&self) -> usize {
        let rows = self.rows();
        self.selected_key
            .as_ref()
            .and_then(|key| rows.iter().position(|row| &row.key() == key))
            .unwrap_or(self.selected)
            .min(rows.len().saturating_sub(1))
    }

    fn selected_row(&self) -> Option<Row<'_>> {
        self.rows().get(self.selected_index()).copied()
    }

    fn selected_service(&self) -> Option<&ServiceRecord> {
        match self.selected_row() {
            Some(Row::Service(service)) => Some(service),
            _ => None,
        }
    }

    fn select(&mut self, index: usize) {
        let key = self.rows().get(index).map(Row::key);
        self.selected = index;
        self.selected_key = key;
    }

    fn select_next(&mut self) {
        let count = self.rows().len();
        self.select((self.selected_index() + 1).min(count.saturating_sub(1)));
    }

    fn select_previous(&mut self) {
        self.select(self.selected_index().saturating_sub(1));
    }

    fn notify(&mut self, text: impl Into<String>, error: bool) {
        self.notice = Some(Notice {
            text: text.into(),
            error,
        });
    }

    fn open_form(&mut self, kind: FormKind, mut form: Form) {
        form.set_recents(self.recents.directories());
        self.notice = None;
        self.screen = Screen::Form { kind, form };
    }

    /// Leaves an approval form whose request was decided elsewhere,
    /// expired, or withdrawn by the Beam.
    fn close_stale_approval(&mut self) {
        let Screen::Form { kind, .. } = &self.screen else {
            return;
        };
        let request_id = match kind {
            FormKind::CopyApproval { request_id }
            | FormKind::FetchApproval { request_id, .. }
            | FormKind::SyncApproval { request_id } => request_id,
            _ => return,
        };
        let Some(request) = self.request_records.get(request_id) else {
            return;
        };
        if request.status != RequestStatus::PendingApproval {
            let message = request
                .message
                .clone()
                .unwrap_or_else(|| "the request is no longer pending".to_string());
            self.screen = Screen::Home;
            self.notify(format!("request closed: {message}"), true);
        }
    }

    fn open_menu(&mut self, menu: Menu) {
        self.notice = None;
        self.screen = Screen::Menu(menu);
    }

    fn open_agent_keys_menu(&mut self) {
        self.offer_key_setup = false;
        let menu = agent_keys_menu();
        self.open_menu(menu);
    }

    /// Writes a new `allowed_keys` and has the daemon push keys again.
    fn set_allowed_keys(&mut self, keys: Vec<String>, done: String, local_daemon: &mut LocalDaemon) {
        match crate::agent::Config::write_allowed_keys(&keys) {
            Ok(_) => {
                if let Some(frontend) = &mut local_daemon.frontend {
                    let _ = frontend.refresh_agent_keys();
                }
                self.key_setup_hint = false;
                self.notify(done, false);
            }
            Err(err) => self.notify(format!("could not update the SSH key config: {err:#}"), true),
        }
    }

    /// Opens a request's decision menu, guarding SSH key requests against
    /// stray keys (see `input_guard_until`).
    fn open_request_menu(&mut self, request_id: &str) {
        let Some(request) = self.request_records.get(request_id) else {
            return;
        };
        let guarded = request.agent.is_some();
        let menu = request_menu(request);
        self.open_menu(menu);
        if guarded {
            self.input_guard_until = Some(Instant::now() + AGENT_INPUT_GUARD);
        }
    }

    /// Handles one key while the menu is open. Returns whether the menu
    /// should close.
    fn handle_key(&mut self, key: Key, local_daemon: &mut LocalDaemon) -> bool {
        if let Some(until) = self.input_guard_until {
            let now = Instant::now();
            if now < until && key != Key::Esc {
                self.input_guard_until = Some(now + AGENT_INPUT_GUARD);
                return false;
            }
            self.input_guard_until = None;
        }
        match &mut self.screen {
            Screen::Form { kind, form } => {
                match form.handle_key(&key) {
                    FormEvent::Cancel => self.screen = Screen::Home,
                    FormEvent::Submit => {
                        form.clear_errors();
                        match submit_form(kind, form, local_daemon) {
                            Ok(Some(notice)) => {
                                let used = used_local_path(kind, form);
                                self.screen = Screen::Home;
                                if let Some((path, is_dir)) = used {
                                    self.recents.record(&path, is_dir);
                                }
                                self.notify(notice, false);
                            }
                            Ok(None) => {}
                            Err(err) => form.error = Some(err),
                        }
                    }
                    FormEvent::Handled | FormEvent::Ignored => {}
                }
                return self.leave_after_review();
            }
            Screen::Menu(menu) => {
                if let Some(action) = menu.handle_key(&key) {
                    self.screen = Screen::Home;
                    self.run_menu_action(action, local_daemon);
                }
                return self.leave_after_review();
            }
            Screen::Home => {}
        }

        match key {
            Key::Char('q' | 'Q') | Key::Esc => return true,
            Key::Enter => match self.selected_row() {
                Some(Row::Request(request)) => {
                    let request_id = request.request_id.clone();
                    self.open_request_menu(&request_id);
                }
                Some(Row::Grant(grant)) => {
                    let menu = grant_menu(grant);
                    self.open_menu(menu);
                }
                Some(Row::Service(service)) => {
                    let menu = service_menu(service);
                    self.open_menu(menu);
                }
                None => {}
            },
            Key::Char('y' | 'Y') => match self.pending_request() {
                // An SSH key is only ever approved from its own review menu.
                Some(request) if request.agent.is_some() => {
                    let request_id = request.request_id.clone();
                    self.open_request_menu(&request_id);
                }
                Some(request) => {
                    let request_id = request.request_id.clone();
                    self.approve(&request_id, local_daemon);
                }
                None => self.notify("no request is waiting for approval", true),
            },
            Key::Char('n' | 'N') => match self.pending_request() {
                Some(request) => {
                    let request_id = request.request_id.clone();
                    self.decide(&request_id, false, local_daemon);
                }
                None => self.notify("no request is waiting for approval", true),
            },
            Key::Char('f' | 'F') => self.open_form(FormKind::Forward, forward_form()),
            Key::Char('u' | 'U') => self.open_form(
                FormKind::Copy(FileCopyDirection::Upload),
                copy_form(FileCopyDirection::Upload),
            ),
            Key::Char('d' | 'D') => self.open_form(
                FormKind::Copy(FileCopyDirection::Download),
                copy_form(FileCopyDirection::Download),
            ),
            Key::Char('m' | 'M') => self.open_form(FormKind::Sync, sync_form()),
            Key::Char('a' | 'A') => self.open_agent_keys_menu(),
            Key::Char('j' | 'J') | Key::Down => self.select_next(),
            Key::Char('k' | 'K') | Key::Up => self.select_previous(),
            Key::Home => self.select(0),
            Key::End => self.select(self.rows().len().saturating_sub(1)),
            Key::Char(ch) => {
                let ch = ch.to_ascii_lowercase();
                let Some(action) = ALL_ACTIONS
                    .into_iter()
                    .find(|action| ui::action_key(*action) == ch)
                else {
                    return false;
                };
                let Some(service) = self.selected_service() else {
                    self.notify("select a service first", true);
                    return false;
                };
                if !ui::available_actions(service).contains(&action) {
                    self.notify(
                        format!(
                            "{} isn't available for this service right now",
                            ui::action_label(action)
                        ),
                        true,
                    );
                } else if ui::needs_confirmation(service, action) {
                    let menu = confirm_menu(service, action);
                    self.open_menu(menu);
                } else {
                    let service_id = service.service_id.clone();
                    self.control(&service_id, action, local_daemon);
                }
            }
            _ => {}
        }
        false
    }

    fn run_menu_action(&mut self, action: MenuAction, local_daemon: &mut LocalDaemon) {
        match action {
            MenuAction::Back => {}
            MenuAction::Approve(request_id) => self.approve(&request_id, local_daemon),
            MenuAction::Reject(request_id) => self.decide(&request_id, false, local_daemon),
            MenuAction::AllowKey(fingerprint) => {
                let mut keys = crate::agent::Config::load().unwrap_or_default().allowed_keys;
                if !keys.contains(&fingerprint) {
                    keys.push(fingerprint.clone());
                }
                self.set_allowed_keys(keys, format!("Beams may now ask to use {fingerprint}"), local_daemon);
            }
            MenuAction::DenyKey(fingerprint) => {
                let mut keys = crate::agent::Config::load().unwrap_or_default().allowed_keys;
                keys.retain(|key| key != &fingerprint);
                self.set_allowed_keys(keys, format!("stopped sharing {fingerprint}"), local_daemon);
            }
            MenuAction::DeclineKeys => {
                self.set_allowed_keys(
                    Vec::new(),
                    "no keys shared; press a to change that later".to_string(),
                    local_daemon,
                );
            }
            MenuAction::RevokeGrant(grant_id) => match &mut local_daemon.frontend {
                Some(frontend) => match frontend.revoke_grant(&grant_id) {
                    Ok(()) => self.notify("startup SSH key grant revoked", false),
                    Err(err) => self.notify(format!("could not revoke the grant: {err}"), true),
                },
                None => self.notify("the local daemon connection is unavailable", true),
            },
            MenuAction::Control { service_id, action } => {
                self.control(&service_id, action, local_daemon)
            }
            MenuAction::Confirm { service_id, action } => {
                if let Some(service) = self.service_records.get(&service_id) {
                    let menu = confirm_menu(service, action);
                    self.open_menu(menu);
                }
            }
        }
    }

    /// Approves `request_id`, first asking for the real workstation path
    /// when the Beam could only suggest one.
    fn approve(&mut self, request_id: &str, local_daemon: &mut LocalDaemon) {
        let Some(request) = self.request_records.get(request_id) else {
            self.notify("that request is gone", true);
            return;
        };
        match request.request.as_ref().map(|request| &request.kind) {
            Some(ServiceRequestKind::FileCopy { .. }) => {
                let form = approval_form(
                    request,
                    "Approve file copy",
                    "Workstation path",
                    Completion::Paths,
                );
                self.open_form(
                    FormKind::CopyApproval {
                        request_id: request_id.to_string(),
                    },
                    form,
                );
            }
            Some(ServiceRequestKind::FileFetch { sources, .. }) => {
                let into_dir = !matches!(sources.as_slice(), [source] if !source.is_dir);
                let form = fetch_approval_form(request, into_dir, &self.recents.directories());
                self.open_form(
                    FormKind::FetchApproval {
                        request_id: request_id.to_string(),
                        into_dir,
                    },
                    form,
                );
            }
            Some(ServiceRequestKind::FolderSync { .. }) => {
                let form = approval_form(
                    request,
                    "Approve folder sync",
                    "Workstation dir",
                    Completion::Directories,
                );
                self.open_form(
                    FormKind::SyncApproval {
                        request_id: request_id.to_string(),
                    },
                    form,
                );
            }
            _ => self.decide(request_id, true, local_daemon),
        }
    }

    /// Whether a review opened from a toast just finished (the form or
    /// menu went back to the home screen), so the menu should close and
    /// hand the terminal straight back to the session it came from.
    fn leave_after_review(&mut self) -> bool {
        if self.reviewing_from_toast && matches!(self.screen, Screen::Home) {
            self.reviewing_from_toast = false;
            return true;
        }
        false
    }

    /// Opens `request_id` for a decision: the path form for copies, fetches
    /// and syncs, the approve/reject menu otherwise. Finishing it (or
    /// backing out) closes the menu again.
    fn review_request(&mut self, request_id: &str, local_daemon: &mut LocalDaemon) {
        let Some(request) = self.request_records.get(request_id) else {
            return;
        };
        if request.status != RequestStatus::PendingApproval {
            return;
        }
        self.reviewing_from_toast = true;
        self.selected_key = Some(RowKey::Request(request_id.to_string()));
        let needs_path = matches!(
            request.request.as_ref().map(|request| &request.kind),
            Some(
                ServiceRequestKind::FileCopy { .. }
                    | ServiceRequestKind::FileFetch { .. }
                    | ServiceRequestKind::FolderSync { .. }
            )
        );
        if needs_path {
            self.approve(request_id, local_daemon);
        } else {
            let request_id = request_id.to_string();
            self.open_request_menu(&request_id);
        }
    }

    fn connected_bridges(&self) -> usize {
        self.bridge_statuses
            .values()
            .filter(|status| status.as_str() == "connected")
            .count()
    }

    fn decide(&mut self, request_id: &str, approve: bool, local_daemon: &mut LocalDaemon) {
        let Some(frontend) = &mut local_daemon.frontend else {
            self.notify("the local daemon connection is unavailable", true);
            return;
        };
        match frontend.decide_request(request_id, approve) {
            Ok(()) => self.notify(
                format!("{} request", if approve { "approved" } else { "rejected" }),
                false,
            ),
            Err(err) => self.notify(format!("could not send the decision: {err}"), true),
        }
    }

    /// Services belonging to this session's Beam, in a stable order.
    fn visible_services(&self) -> Vec<&ServiceRecord> {
        self.service_records
            .values()
            .filter(|service| self.beam_uuid.as_deref() == Some(service.beam_uuid.as_str()))
            .collect()
    }

    fn control(&mut self, service_id: &str, action: ServiceAction, local_daemon: &mut LocalDaemon) {
        let Some(frontend) = &mut local_daemon.frontend else {
            self.notify("the local daemon connection is unavailable", true);
            return;
        };
        match frontend.control_service(service_id, action) {
            Ok(()) => self.notify(format!("{} requested", ui::action_label(action)), false),
            Err(err) => self.notify(format!("could not control the service: {err}"), true),
        }
    }
}

fn exit_code(status: &ExitStatus) -> u8 {
    status.exit_code().min(255) as u8
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InputMode {
    Passthrough,
    Palette,
}

/// What the session loop must do with routed input. Keys pressed while the
/// menu is open never appear here: the router hands them straight to the
/// menu (see `InputRouter::route`).
#[derive(Clone, Debug, PartialEq, Eq)]
enum InputAction {
    Remote(Vec<u8>),
    OpenPalette,
    ClosePalette,
}

/// Upper bound on a bracketed paste collected for a menu field.
const MAX_PASTE_LEN: usize = 64 * 1024;

/// Routes terminal input while preserving the original bytes sent to the
/// remote PTY. Modern terminal applications such as zellij can enable the
/// Kitty CSI-u or xterm modifyOtherKeys protocols; in those modes Ctrl-J is
/// represented by a sequence such as `ESC [ 106 ; 5 u`, not byte `0x0a`.
/// In passthrough mode we decode only enough to recognize the menu prefix
/// and otherwise forward each complete sequence exactly as received. While
/// the menu is open, each unit is decoded into a `keys::Key` and handed to
/// the menu's key handler.
#[derive(Debug)]
struct InputRouter {
    mode: InputMode,
    pending: Vec<u8>,
    pending_since: Option<Instant>,
    /// An in-progress bracketed paste while the menu is open.
    paste: Option<Vec<u8>>,
    /// Paste delimiters are also meaningful in passthrough: a prefix byte
    /// in pasted text must never open the menu.
    remote_paste: bool,
    mouse_utf8: bool,
    /// An oversized OSC/DCS reply is streamed without retaining its payload.
    /// Keep consuming it opaquely through its terminator.
    string_tail: Option<(bool, bool)>,
}

impl Default for InputRouter {
    fn default() -> Self {
        Self {
            mode: InputMode::Passthrough,
            pending: Vec::new(),
            pending_since: None,
            paste: None,
            remote_paste: false,
            mouse_utf8: false,
            string_tail: None,
        }
    }
}

impl InputRouter {
    /// Routes `input`. While the menu is open, each decoded key is passed to
    /// `on_key`, which returns whether that key closes the menu; input after
    /// a closing key is routed as passthrough again.
    fn route(
        &mut self,
        escape_key: EscapeKey,
        input: &[u8],
        now: Instant,
        on_key: &mut dyn FnMut(Key) -> bool,
    ) -> Vec<InputAction> {
        if escape_key == EscapeKey::Disabled && self.mode == InputMode::Passthrough {
            return if input.is_empty() { Vec::new() } else { vec![InputAction::Remote(input.to_vec())] };
        }
        let mut bytes = std::mem::take(&mut self.pending);
        self.pending_since = None;
        bytes.extend_from_slice(input);

        let mut actions = Vec::new();
        let mut remote = Vec::new();
        let mut offset = 0;
        while offset < bytes.len() {
            if let Some((osc, escaped)) = self.string_tail {
                let byte = bytes[offset];
                remote.push(byte);
                self.string_tail = if (osc && byte == b'\x07') || (escaped && byte == b'\\') {
                    None
                } else {
                    Some((osc, byte == b'\x1b'))
                };
                offset += 1;
                continue;
            }
            let remaining = &bytes[offset..];
            let parsed = if self.paste.is_some() || self.remote_paste {
                // Paste is arbitrary text. Only its closing delimiter is a
                // key sequence; OSCs and control bytes in it are just text.
                const END: &[u8] = b"\x1b[201~";
                if remaining.starts_with(END) {
                    TerminalKeyParse::Sequence {
                        len: END.len(),
                        logical: None,
                    }
                } else if END.starts_with(remaining) {
                    TerminalKeyParse::Incomplete
                } else {
                    TerminalKeyParse::Byte
                }
            } else {
                parse_terminal_key(remaining, self.mouse_utf8)
            };
            match parsed {
                TerminalKeyParse::Incomplete => {
                    self.pending.extend_from_slice(&bytes[offset..]);
                    self.pending_since = Some(now);
                    break;
                }
                TerminalKeyParse::Sequence { len, logical } => {
                    if len >= MAX_PASTE_LEN && is_string_sequence(remaining) {
                        let osc = remaining[1] == b']';
                        let sequence = &remaining[..len];
                        if !sequence.ends_with(b"\x1b\\") && !(osc && sequence.ends_with(b"\x07")) {
                            self.string_tail = Some((osc, sequence.last() == Some(&b'\x1b')));
                        }
                    }
                    self.route_key(
                        escape_key,
                        &bytes[offset..offset + len],
                        logical,
                        &mut actions,
                        &mut remote,
                        on_key,
                    );
                    offset += len;
                }
                TerminalKeyParse::Byte => {
                    let byte = bytes[offset];
                    self.route_key(
                        escape_key,
                        &bytes[offset..offset + 1],
                        Some(byte),
                        &mut actions,
                        &mut remote,
                        on_key,
                    );
                    offset += 1;
                }
            }
        }
        flush_remote(&mut actions, &mut remote);
        actions
    }

    fn flush_expired(
        &mut self,
        escape_key: EscapeKey,
        now: Instant,
        on_key: &mut dyn FnMut(Key) -> bool,
    ) -> Vec<InputAction> {
        if is_string_sequence(&self.pending)
            || self.pending.starts_with(b"\x1b[M")
            || ((self.paste.is_some() || self.remote_paste)
                && b"\x1b[201~".starts_with(&self.pending))
        {
            // Clipboard/terminal replies can arrive slowly. Waiting for ST
            // keeps their payload from ever becoming menu keys.
            return Vec::new();
        }
        if self
            .pending_since
            .is_none_or(|since| now.duration_since(since) < INPUT_SEQUENCE_TIMEOUT)
        {
            return Vec::new();
        }

        let pending = std::mem::take(&mut self.pending);
        self.pending_since = None;
        let mut actions = Vec::new();
        let mut remote = Vec::new();
        // A partial CSI report must not become menu keystrokes on timeout.
        if pending.len() > 1 && pending.starts_with(b"\x1b") {
            if self.mode == InputMode::Passthrough {
                remote.extend_from_slice(&pending);
            }
            flush_remote(&mut actions, &mut remote);
            return actions;
        }
        for byte in pending {
            self.route_key(
                escape_key,
                &[byte],
                Some(byte),
                &mut actions,
                &mut remote,
                on_key,
            );
        }
        flush_remote(&mut actions, &mut remote);
        actions
    }

    fn poll_timeout(&self, now: Instant) -> Duration {
        if is_string_sequence(&self.pending)
            || self.pending.starts_with(b"\x1b[M")
            || ((self.paste.is_some() || self.remote_paste)
                && b"\x1b[201~".starts_with(&self.pending))
        {
            return LOOP_TICK;
        }
        self.pending_since
            .map(|since| {
                INPUT_SEQUENCE_TIMEOUT
                    .saturating_sub(now.saturating_duration_since(since))
                    .min(LOOP_TICK)
            })
            .unwrap_or(LOOP_TICK)
    }

    fn route_key(
        &mut self,
        escape_key: EscapeKey,
        encoded: &[u8],
        logical: Option<u8>,
        actions: &mut Vec<InputAction>,
        remote: &mut Vec<u8>,
        on_key: &mut dyn FnMut(Key) -> bool,
    ) {
        if self.mode == InputMode::Passthrough {
            match keys::decode(encoded) {
                Some(Decoded::PasteStart) => self.remote_paste = true,
                Some(Decoded::PasteEnd) => self.remote_paste = false,
                _ => {}
            }
            if !self.remote_paste && logical.is_some() && escape_key.byte() == logical {
                // The prefix always opens a menu; the driver decides whether
                // it can overlay the remote screen or must take it over.
                flush_remote(actions, remote);
                self.mode = InputMode::Palette;
                actions.push(InputAction::OpenPalette);
            } else {
                remote.extend_from_slice(encoded);
            }
            return;
        }

        if let Some(paste) = &mut self.paste {
            if keys::decode(encoded) == Some(Decoded::PasteEnd) {
                let text = String::from_utf8_lossy(paste).into_owned();
                self.paste = None;
                self.menu_key(Key::Paste(text), actions, on_key);
            } else {
                let len = encoded.len().min(MAX_PASTE_LEN.saturating_sub(paste.len()));
                paste.extend_from_slice(&encoded[..len]);
            }
            return;
        }
        // Replies belong to the remote application even while the menu owns
        // user input. Keep OSC/DCS payloads opaque to the local key decoder.
        if is_terminal_reply(encoded) {
            remote.extend_from_slice(encoded);
            return;
        }
        if logical.is_some() && escape_key.byte() == logical {
            // The prefix pressed again closes the menu and sends it
            // literally, so the remote side can still receive that key.
            self.mode = InputMode::Passthrough;
            actions.push(InputAction::ClosePalette);
            remote.extend_from_slice(encoded);
            return;
        }
        match keys::decode(encoded) {
            Some(Decoded::Key(key)) => self.menu_key(key, actions, on_key),
            Some(Decoded::PasteStart) => self.paste = Some(Vec::new()),
            // Unsupported keys are dropped rather than leaked remotely.
            Some(Decoded::PasteEnd) | None => {}
        }
    }

    fn menu_key(
        &mut self,
        key: Key,
        actions: &mut Vec<InputAction>,
        on_key: &mut dyn FnMut(Key) -> bool,
    ) {
        if on_key(key) {
            self.mode = InputMode::Passthrough;
            actions.push(InputAction::ClosePalette);
        }
    }
}

fn flush_remote(actions: &mut Vec<InputAction>, remote: &mut Vec<u8>) {
    if !remote.is_empty() {
        actions.push(InputAction::Remote(std::mem::take(remote)));
    }
}

#[derive(Debug, PartialEq, Eq)]
enum TerminalKeyParse {
    Byte,
    Incomplete,
    Sequence { len: usize, logical: Option<u8> },
}

/// Splits one input unit off the front of `input`: a single byte, a
/// complete UTF-8 character, an SS3 (`ESC O x`) key, or a complete CSI
/// sequence. `logical` is the control byte an enhanced-keyboard sequence
/// stands for, used to recognize the menu prefix.
fn parse_terminal_key(input: &[u8], mouse_utf8: bool) -> TerminalKeyParse {
    let Some(&first) = input.first() else {
        return TerminalKeyParse::Incomplete;
    };
    if (0xc2..=0xf4).contains(&first) {
        let need = match first {
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            _ => 4,
        };
        let available = input.len().min(need);
        if !input[1..available].iter().all(|byte| byte & 0xc0 == 0x80) {
            return TerminalKeyParse::Byte;
        }
        if input.len() < need {
            return TerminalKeyParse::Incomplete;
        }
        return TerminalKeyParse::Sequence {
            len: need,
            logical: None,
        };
    }
    if first != b'\x1b' {
        return TerminalKeyParse::Byte;
    }
    if input.len() == 1 {
        return TerminalKeyParse::Incomplete;
    }
    if matches!(input[1], b']' | b'P' | b'_' | b'^' | b'X') {
        let osc = input[1] == b']';
        let end = (2..input.len()).find(|&i| {
            (osc && input[i] == b'\x07') || (input[i] == b'\\' && input[i - 1] == b'\x1b')
        });
        return match end {
            Some(end) => TerminalKeyParse::Sequence {
                len: end + 1,
                logical: None,
            },
            None if input.len() < MAX_PASTE_LEN => TerminalKeyParse::Incomplete,
            // Keep an oversized string opaque rather than interpreting its
            // payload as approval/menu keys.
            None => TerminalKeyParse::Sequence {
                len: input.len(),
                logical: None,
            },
        };
    }
    if input[1] == b'O' {
        return if input.len() < 3 {
            TerminalKeyParse::Incomplete
        } else {
            TerminalKeyParse::Sequence {
                len: 3,
                logical: None,
            }
        };
    }
    if input[1] != b'[' {
        return TerminalKeyParse::Byte;
    }
    if input.starts_with(b"\x1b[M") {
        // X10 / VT200 reports have three payload bytes after the CSI final
        // byte. With mode 1005 the coordinates are UTF-8 characters.
        let mut end = 3;
        for _ in 0..3 {
            let Some(&byte) = input.get(end) else {
                return TerminalKeyParse::Incomplete;
            };
            let len = if mouse_utf8 {
                match byte {
                    0xc2..=0xdf => 2,
                    0xe0..=0xef => 3,
                    0xf0..=0xf4 => 4,
                    _ => 1,
                }
            } else {
                1
            };
            end += len;
        }
        return if input.len() < end {
            TerminalKeyParse::Incomplete
        } else {
            TerminalKeyParse::Sequence {
                len: end,
                logical: None,
            }
        };
    }

    let Some(relative_end) = input[2..]
        .iter()
        .position(|byte| (0x40..=0x7e).contains(byte))
    else {
        return if input.len() < MAX_INPUT_SEQUENCE_LEN {
            TerminalKeyParse::Incomplete
        } else {
            TerminalKeyParse::Byte
        };
    };
    let end = relative_end + 2;
    let final_byte = input[end];
    let parameters = &input[2..end];
    let logical = match final_byte {
        b'u' => parse_kitty_key(parameters),
        b'~' => parse_modify_other_keys(parameters),
        _ => None,
    };
    TerminalKeyParse::Sequence {
        len: end + 1,
        logical,
    }
}

fn is_terminal_reply(encoded: &[u8]) -> bool {
    is_string_sequence(encoded)
        || (encoded.starts_with(b"\x1b[")
            && !encoded.starts_with(b"\x1b[M")
            && (matches!(encoded.last(), Some(b'R' | b'c' | b'n' | b't' | b'y'))
                // Focus in/out reports (mode 1004) belong to the remote.
                || matches!(encoded, b"\x1b[I" | b"\x1b[O")
                || (encoded.starts_with(b"\x1b[?") && encoded.ends_with(b"u"))))
        || (encoded.starts_with(b"\x1b[>") && encoded.ends_with(b"m"))
}

fn is_string_sequence(encoded: &[u8]) -> bool {
    encoded.first() == Some(&b'\x1b')
        && matches!(encoded.get(1), Some(b']' | b'P' | b'_' | b'^' | b'X'))
}

fn parse_kitty_key(parameters: &[u8]) -> Option<u8> {
    let mut fields = parameters.split(|byte| *byte == b';');
    let codepoint = parse_parameter(fields.next()?)?;
    let modifier_field = fields.next();
    if modifier_field
        .and_then(|field| field.split(|byte| *byte == b':').nth(1))
        .is_some_and(|event| event != b"1")
    {
        // A held/released prefix must not open and immediately close the
        // menu when Kitty event-type reporting is enabled.
        return None;
    }
    let modifiers = modifier_field.and_then(parse_parameter).unwrap_or(1);
    decode_terminal_key(codepoint, modifiers)
}

fn parse_modify_other_keys(parameters: &[u8]) -> Option<u8> {
    let mut fields = parameters.split(|byte| *byte == b';');
    if parse_parameter(fields.next()?)? != 27 {
        return None;
    }
    let modifiers = parse_parameter(fields.next()?)?;
    let codepoint = parse_parameter(fields.next()?)?;
    decode_terminal_key(codepoint, modifiers)
}

fn parse_parameter(parameter: &[u8]) -> Option<u32> {
    let primary = parameter.split(|byte| *byte == b':').next()?;
    std::str::from_utf8(primary).ok()?.parse().ok()
}

fn decode_terminal_key(codepoint: u32, modifiers: u32) -> Option<u8> {
    let byte = u8::try_from(codepoint).ok()?;
    let flags = modifiers.saturating_sub(1);
    const SHIFT: u32 = 1;
    const ALT: u32 = 2;
    const CONTROL: u32 = 4;
    const CAPS_LOCK: u32 = 64;
    const NUM_LOCK: u32 = 128;
    if flags & !(SHIFT | CONTROL | CAPS_LOCK | NUM_LOCK) != 0 || flags & ALT != 0 {
        return None;
    }
    if flags & CONTROL == 0 {
        return Some(byte);
    }
    match byte {
        b'?' => Some(0x7f),
        b'@'..=b'_' => Some(byte & 0x1f),
        b'a'..=b'z' => Some(byte.to_ascii_uppercase() & 0x1f),
        _ => None,
    }
}

/// Test stand-in for the menu: records every key and closes on the keys the
/// home screen closes on.
#[cfg(test)]
fn route_input_keys(
    mode: &mut InputMode,
    escape_key: EscapeKey,
    input: &[u8],
) -> (Vec<InputAction>, Vec<Key>) {
    let mut router = InputRouter {
        mode: *mode,
        ..InputRouter::default()
    };
    let mut keys = Vec::new();
    let actions = router.route(escape_key, input, Instant::now(), &mut |key: Key| {
        let close = matches!(key, Key::Char('q') | Key::Esc | Key::Enter);
        keys.push(key);
        close
    });
    assert!(
        router.pending.is_empty(),
        "test input ended in a partial sequence"
    );
    *mode = router.mode;
    (actions, keys)
}

#[cfg(test)]
fn route_input(mode: &mut InputMode, escape_key: EscapeKey, input: &[u8]) -> Vec<InputAction> {
    route_input_keys(mode, escape_key, input).0
}

#[derive(Default)]
struct DeferredOutput {
    bytes: VecDeque<u8>,
    truncated: bool,
}

impl DeferredOutput {
    fn push(&mut self, bytes: &[u8]) {
        if bytes.len() >= DEFERRED_OUTPUT_LIMIT {
            self.bytes.clear();
            self.bytes
                .extend(bytes[bytes.len() - DEFERRED_OUTPUT_LIMIT..].iter().copied());
            self.truncated = true;
            return;
        }

        let overflow = self
            .bytes
            .len()
            .saturating_add(bytes.len())
            .saturating_sub(DEFERRED_OUTPUT_LIMIT);
        if overflow > 0 {
            self.bytes.drain(..overflow);
            self.truncated = true;
        }
        self.bytes.extend(bytes.iter().copied());
    }

    fn flush(&mut self, tracker: &mut toast::OutputTracker) -> anyhow::Result<()> {
        if self.bytes.is_empty() && !self.truncated {
            return Ok(());
        }
        let (front, back) = self.bytes.as_slices();
        tracker.feed(front, Instant::now());
        tracker.feed(back, Instant::now());
        let mut stdout = io::stdout().lock();
        if self.truncated {
            stdout
                .write_all(
                    b"\r\n[beams: remote output was truncated while the local menu was open]\r\n",
                )
                .context("writing deferred-output warning")?;
        }
        let (front, back) = self.bytes.as_slices();
        stdout
            .write_all(front)
            .context("restoring deferred remote output")?;
        stdout
            .write_all(back)
            .context("restoring deferred remote output")?;
        stdout.flush().context("flushing deferred remote output")?;
        self.bytes.clear();
        self.truncated = false;
        Ok(())
    }

    fn clear(&mut self) {
        self.bytes.clear();
        self.truncated = false;
    }
}

enum OutputEvent {
    Data(Vec<u8>),
    Eof,
    Error(io::Error),
}

fn read_pty_output(reader: &mut dyn Read, tx: mpsc::SyncSender<OutputEvent>) {
    let mut bytes = [0u8; 8192];
    loop {
        match reader.read(&mut bytes) {
            Ok(0) => {
                let _ = tx.send(OutputEvent::Eof);
                return;
            }
            Ok(read) => {
                if tx.send(OutputEvent::Data(bytes[..read].to_vec())).is_err() {
                    return;
                }
            }
            // Linux PTY masters commonly report EIO rather than zero after
            // the final slave closes. It is EOF, not a session failure.
            Err(err) if err.raw_os_error() == Some(libc::EIO) => {
                let _ = tx.send(OutputEvent::Eof);
                return;
            }
            Err(err) => {
                let _ = tx.send(OutputEvent::Error(err));
                return;
            }
        }
    }
}

struct OutputUpdate {
    changed: bool,
    /// The chunk tail to forward after closing an unsafe overlay. The bounded
    /// unfinished prefix is held by the physical output tracker.
    resume: Option<Vec<u8>>,
    replay_pending: bool,
}

#[allow(clippy::too_many_arguments)]
fn drain_output(
    rx: &Receiver<OutputEvent>,
    mode: InputMode,
    deferred: &mut DeferredOutput,
    finished: &mut bool,
    terminal_model: &mut TerminalModel,
    overlay_enabled: bool,
    tracker: &mut toast::OutputTracker,
    remote_writer: Option<&ChildInput>,
    painter: &mut toast::ToastPainter,
    borrowed: &mut Option<borrowed::BorrowedMenu>,
) -> anyhow::Result<OutputUpdate> {
    let mut stdout = io::stdout().lock();
    let mut wrote = false;
    let mut changed = false;
    for _ in 0..OUTPUT_CHUNKS_PER_TICK {
        match rx.try_recv() {
            Ok(OutputEvent::Data(bytes)) => {
                changed = true;
                if let Some(borrowed) = borrowed.as_mut() {
                    let update = borrowed.process(terminal_model, &bytes);
                    tracker.feed(&bytes[..update.consumed], Instant::now());
                    if let Some(resume) = update.resume {
                        stdout.flush().context("flushing before borrowed-menu replay")?;
                        return Ok(OutputUpdate {
                            changed,
                            resume: Some(resume),
                            replay_pending: update.replay_pending,
                        });
                    }
                    continue;
                }
                // A shown toast's restoration must come from the last
                // represented state, before this chunk can invalidate it.
                // Whether it's needed is known once the chunk is processed.
                let erase = if mode == InputMode::Passthrough {
                    terminal_model.take_damage();
                    painter.prepare_output(terminal_model)
                } else {
                    None
                };
                let mut offset = 0;
                while offset < bytes.len() {
                    let (consumed, rejected) =
                        terminal_model.process_guarded_with_events(&bytes[offset..],
                            mode == InputMode::Palette && overlay_enabled, |event| {
                            if mode == InputMode::Palette && overlay_enabled {
                                if let TerminalEvent::Reply(reply) = event {
                                    remote_writer
                                        .expect("overlay query dispatch has a child input queue")
                                        .enqueue(&reply)
                                        .context("queueing remote terminal reply")?;
                                } else if let Some(bytes) = event.terminal_bytes() {
                                    stdout
                                        .write_all(&bytes)
                                        .context("relaying terminal effect")?;
                                    wrote = true;
                                }
                            }
                            Ok::<(), anyhow::Error>(())
                        }, true)?;
                    if overlay_enabled || mode == InputMode::Passthrough {
                        tracker.feed(&bytes[offset..offset + consumed], Instant::now());
                    }
                    offset += consumed;
                    if let Some(mut rejected) = rejected {
                        rejected.extend_from_slice(&bytes[offset..]);
                        stdout.flush().context("flushing effects before unsupported output")?;
                        return Ok(OutputUpdate {
                            changed,
                            resume: Some(rejected),
                            replay_pending: false,
                        });
                    }
                    if offset < bytes.len() {
                        terminal_model.discard_oversized_sequence();
                        if mode == InputMode::Palette && overlay_enabled {
                            stdout
                                .flush()
                                .context("flushing effects before overlay recovery")?;
                            return Ok(OutputUpdate {
                                changed,
                                resume: Some(bytes[offset..].to_vec()),
                                replay_pending: true,
                            });
                        }
                    }
                }
                if mode == InputMode::Palette {
                    if !overlay_enabled {
                        deferred.push(&bytes);
                    }
                } else {
                    // Raw output stays raw; a toast is left alone, repainted
                    // atomically with it, or erased ahead of it.
                    let damage = terminal_model.take_damage();
                    let bytes = painter.pass_output(
                        erase, &damage, &bytes, terminal_model, tracker, Instant::now(),
                    );
                    stdout
                        .write_all(&bytes)
                        .context("writing remote PTY output")?;
                    wrote = true;
                }
            }
            Ok(OutputEvent::Eof) | Err(TryRecvError::Disconnected) => {
                *finished = true;
                break;
            }
            Ok(OutputEvent::Error(err)) => return Err(err).context("reading remote PTY output"),
            Err(TryRecvError::Empty) => break,
        }
    }
    if wrote {
        stdout.flush().context("flushing remote PTY output")?;
    }
    Ok(OutputUpdate {
        changed,
        resume: None,
        replay_pending: true,
    })
}

fn finish_output(
    rx: &Receiver<OutputEvent>,
    finished: &mut bool,
    terminal_model: &mut TerminalModel,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + EXIT_OUTPUT_GRACE;
    let mut deferred = DeferredOutput::default();
    while !*finished && Instant::now() < deadline {
        drain_output(
            rx,
            InputMode::Passthrough,
            &mut deferred,
            finished,
            terminal_model,
            false,
            &mut toast::OutputTracker::default(),
            None,
            &mut toast::ToastPainter::default(),
            &mut None,
        )?;
        if !*finished {
            thread::sleep(Duration::from_millis(5));
        }
    }
    Ok(())
}

fn close_palette(
    master: &dyn MasterPty,
    terminal_state: &mut TerminalState,
    palette_terminal: &mut Option<PaletteTerminal>,
    mode: &mut InputMode,
    deferred: &mut DeferredOutput,
    size: PtySize,
    terminal_model: &mut TerminalModel,
) -> anyhow::Result<()> {
    if terminal_state.palette_open {
        palette_terminal.take();
        if let Some(borrowed) = terminal_state.borrowed.as_mut() {
            borrowed.close(&mut io::stdout().lock())
                .context("restoring primary and replaying borrowed-menu output")?;
            terminal_state.borrowed = None;
            terminal_state.palette_open = false;
        } else if terminal_state.overlay_menu {
            terminal_state.close_palette()?;
            deferred.clear();
            // A restore covering cells Beams never actually observed is
            // painted blank; ask the remote to repaint so that blank patch
            // doesn't linger, the same way takeover's close always does.
            let doubtful = terminal_model.doubtful_restore(terminal_state.covered_alternate);
            restore_terminal_model(terminal_model, terminal_state.covered_alternate)?;
            let mut stdout = io::stdout().lock();
            if terminal_state.output.synchronized_update() {
                stdout
                    .write_all(b"\x1b[?2026h")
                    .context("restoring synchronized output")?;
            }
            stdout
                .write_all(terminal_state.output.pending())
                .context("restoring unfinished remote sequence")?;
            stdout
                .flush()
                .context("flushing unfinished remote sequence")?;
            drop(stdout);
            if doubtful {
                force_remote_redraw(master, size)?;
            }
        } else {
            terminal_state.close_palette()?;
            let mut stdout = io::stdout().lock();
            let mut restore = Vec::new();
            if let Some(snapshot) = &terminal_state.menu_snapshot {
                // Deferred bytes start from the entry state, not the model
                // after consuming them. Local drawing never used its save slot.
                crate::terminal_paint::restore(snapshot, false, &mut restore);
            }
            restore.extend_from_slice(crate::terminal_paint::END);
            stdout
                .write_all(&restore)
                .context("restoring takeover drawing state")?;
            stdout
                .write_all(&terminal_state.menu_prefix)
                .context("restoring interrupted remote sequence")?;
            stdout
                .flush()
                .context("flushing interrupted remote sequence")?;
            drop(stdout);
            deferred.flush(&mut terminal_state.output)?;
            let paste_mode = if terminal_model.screen().bracketed_paste() {
                b"\x1b[?2004h"
            } else {
                b"\x1b[?2004l"
            };
            let mut stdout = io::stdout().lock();
            stdout
                .write_all(paste_mode)
                .context("restoring bracketed paste mode")?;
            stdout.flush().context("flushing paste mode")?;
            force_remote_redraw(master, size)?;
        }
        terminal_state.menu_prefix.clear();
        terminal_state.menu_snapshot = None;
    }
    *mode = InputMode::Passthrough;
    terminal_model.end_overlay();
    Ok(())
}

fn restore_terminal_model(model: &TerminalModel, covered_alternate: bool) -> anyhow::Result<()> {
    let mut stdout = io::stdout().lock();
    stdout
        .write_all(&model.restore_bytes(covered_alternate))
        .context("restoring terminal model")?;
    stdout.flush().context("flushing restored terminal model")?;
    Ok(())
}

/// Force a SIGWINCH-driven redraw without injecting Ctrl-L (which could be
/// meaningful input to a non-shell remote application). The transient size is
/// changed in the least disruptive dimension, then restored after enough time
/// for the supervised SSH process to observe both distinct sizes. Restoring
/// immediately lets some SSH clients coalesce the two notifications and leaves
/// the remote full-screen application unaware that it needs to repaint.
fn force_remote_redraw(master: &dyn MasterPty, size: PtySize) -> anyhow::Result<()> {
    let transient = if size.cols > 1 {
        PtySize {
            cols: size.cols - 1,
            ..size
        }
    } else if size.rows > 1 {
        PtySize {
            rows: size.rows - 1,
            ..size
        }
    } else {
        size
    };
    if transient != size {
        master
            .resize(transient)
            .context("triggering remote redraw")?;
        thread::sleep(REDRAW_RESIZE_DELAY);
    }
    master.resize(size).context("restoring remote PTY size")
}

fn current_size() -> io::Result<PtySize> {
    let (cols, rows) = terminal::size()?;
    Ok(PtySize {
        // Some pseudo-terminals (notably `script(1)` without an attached
        // terminal emulator) report 0x0. A real PTY cannot usefully expose
        // that to the child, and several TUIs reject it outright.
        rows: if rows == 0 { 24 } else { rows },
        cols: if cols == 0 { 80 } else { cols },
        pixel_width: 0,
        pixel_height: 0,
    })
}

fn stdin_ready(timeout: Duration) -> io::Result<bool> {
    let mut poll_fd = libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: libc::POLLIN,
        revents: 0,
    };
    let timeout_ms = timeout.as_millis().min(i32::MAX as u128) as i32;
    let result = unsafe { libc::poll(&mut poll_fd, 1, timeout_ms) };
    if result > 0 {
        return Ok(poll_fd.revents & (libc::POLLIN | libc::POLLHUP) != 0);
    }
    if result == 0 {
        return Ok(false);
    }
    let err = io::Error::last_os_error();
    if err.kind() == io::ErrorKind::Interrupted {
        return Ok(false);
    }
    Err(err)
}

fn signal_watcher() -> anyhow::Result<(Receiver<i32>, SignalHandle, thread::JoinHandle<()>)> {
    let mut signals =
        Signals::new([SIGHUP, SIGINT, SIGQUIT, SIGTERM]).context("installing signal handlers")?;
    let handle = signals.handle();
    let (tx, rx) = mpsc::channel();
    let thread = thread::spawn(move || {
        for signal in signals.forever() {
            if tx.send(signal).is_err() {
                break;
            }
        }
    });
    Ok((rx, handle, thread))
}

const TAKEOVER_CLEANUP: &[u8] = b"\x18\x1b\\\x1b[?2026l\x1b[?9l\x1b[?1000l\x1b[?1002l\x1b[?1003l\
    \x1b[?1004l\x1b[?1005l\x1b[?1006l\x1b[?2004l\x1b[?1l\x1b>\
    \x1b[?1049l\x1b[?47l\x1b[0m\x1b[?25h";

struct TerminalState {
    raw: bool,
    managed: bool,
    baseline: baseline::Baseline,
    final_screen: Option<vt100::Screen>,
    initial_input: Vec<u8>,
    palette_open: bool,
    borrowed: Option<borrowed::BorrowedMenu>,
    /// Remote sequence state (including hidden overlay output), so toasts
    /// paint between sequences and menu close can resume an unfinished one.
    output: toast::OutputTracker,
    menu_prefix: Vec<u8>,
    menu_snapshot: Option<vt100::Screen>,
    covered_alternate: bool,
    /// The open menu overlays the remote screen (true) or took it over.
    overlay_menu: bool,
    /// Entry probes ran, so cleanup restores observed settings. Otherwise
    /// cleanup resets the ordinary shell input modes, as takeover always has.
    probed: bool,
    late_replies: Option<LateReplies>,
}

/// Entry probes the terminal had not finished answering when the probe
/// window closed.
struct LateReplies {
    capture: baseline::Capture,
    until: Instant,
    last_input: Instant,
}

impl TerminalState {
    fn enter_raw() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        Ok(Self {
            raw: true,
            managed: true,
            baseline: baseline::Baseline::default(),
            final_screen: None,
            initial_input: Vec::new(),
            palette_open: false,
            borrowed: None,
            output: toast::OutputTracker::default(),
            menu_prefix: Vec::new(),
            menu_snapshot: None,
            covered_alternate: false,
            overlay_menu: false,
            probed: false,
            late_replies: None,
        })
    }

    fn capture_baseline(&mut self) -> anyhow::Result<()> {
        let mut stdout = io::stdout().lock();
        stdout.write_all(&baseline::Baseline::probes()).context("querying terminal baseline")?;
        stdout.flush().context("flushing terminal baseline probes")?;
        drop(stdout);
        let started = Instant::now();
        let deadline = started + PROBE_WINDOW;
        let mut capture = baseline::Capture::default();
        while !capture.finished() && Instant::now() < deadline {
            if stdin_ready(deadline.saturating_duration_since(Instant::now()).min(LOOP_TICK))? {
                let mut bytes = [0u8; 4096];
                let read = io::stdin().read(&mut bytes)?;
                if read == 0 { break; }
                capture.feed(&bytes[..read]);
                if capture.input.len() > 8192 {
                    bail!("terminal input exceeded the 8192-byte baseline capture budget");
                }
            }
        }
        self.probed = true;
        if capture.finished() {
            let (baseline, input) = capture.finish();
            self.baseline = baseline;
            if input.len() > 8192 { bail!("terminal input exceeded the 8192-byte baseline capture budget"); }
            self.initial_input = input;
        } else {
            // A slow terminal: keep claiming replies to our own probes, so
            // they never reach the remote as typed input. Entry evidence
            // uses only what arrived in time.
            self.initial_input = capture.take_input(false);
            self.baseline = baseline::Baseline::default();
            self.late_replies = Some(LateReplies {
                capture,
                until: started + LATE_REPLY_WINDOW,
                last_input: Instant::now(),
            });
        }
        Ok(())
    }

    /// The open menu leaves remote output flowing to the model (an overlay
    /// or a borrowed buffer), rather than deferring it as takeover does.
    fn menu_over_live_output(&self) -> bool {
        self.overlay_menu || self.borrowed.is_some()
    }

    /// Remove late replies to the entry probes from terminal input.
    fn filter_late_replies(&mut self, bytes: &[u8], now: Instant) -> Vec<u8> {
        let Some(late) = self.late_replies.as_mut() else {
            return bytes.to_vec();
        };
        late.capture.feed(bytes);
        late.last_input = now;
        let input = late.capture.take_input(false);
        if late.capture.finished() {
            self.end_late_replies();
        }
        input
    }

    /// Release a partial sequence that stopped arriving (a lone Escape
    /// key), and stop claiming replies once the window ends.
    fn expire_late_replies(&mut self, now: Instant) -> Vec<u8> {
        let Some(late) = self.late_replies.as_mut() else {
            return Vec::new();
        };
        let stalled = late.capture.has_partial()
            && now.saturating_duration_since(late.last_input) >= INPUT_SEQUENCE_TIMEOUT;
        let mut input = late.capture.take_input(stalled);
        if now >= late.until {
            input.extend(late.capture.take_input(true));
            self.end_late_replies();
        }
        input
    }

    fn end_late_replies(&mut self) {
        if let Some(late) = self.late_replies.take() {
            // Late values still improve cleanup; evidence was already fixed.
            self.baseline = late.capture.finish().0;
        }
    }

    fn open_palette(&mut self, screen: &vt100::Screen, overlay: bool, borrow: bool) -> io::Result<()> {
        let mut stdout = io::stdout();
        self.menu_prefix = self.output.pending().to_vec();
        self.covered_alternate = screen.alternate_screen();
        self.menu_snapshot = Some(screen.clone());
        self.palette_open = true;
        self.overlay_menu = overlay;
        if borrow {
            self.borrowed = Some(borrowed::BorrowedMenu::new(screen));
            // No implicit cursor saving; ordinary overlay/takeover stay in
            // the remote buffer. Ownership was proved before this call.
            stdout.write_all(b"\x1b[?47h")?;
        }
        // A menu can open between chunks of a CSI, OSC/DCS or UTF-8
        // character. Cancel only the physical parser; the model and tracker
        // retain the remote prefix, which close replays before passthrough.
        stdout.write_all(crate::terminal_paint::BEGIN)?;
        stdout.write_all(b"\x1b[?2004h")?;
        execute!(stdout, Hide, Clear(ClearType::All), MoveTo(0, 0))?;
        Ok(())
    }

    fn close_palette(&mut self) -> io::Result<()> {
        let mut stdout = io::stdout();
        if self.overlay_menu {
            execute!(stdout, Show)?;
        } else {
            execute!(stdout, Clear(ClearType::All), MoveTo(0, 0), Show)?;
        }
        self.palette_open = false;
        self.overlay_menu = false;
        Ok(())
    }

    fn restore(&mut self) {
        if !self.raw && !self.palette_open {
            return;
        }
        self.end_late_replies();
        if self.managed {
            // Cancel an unfinished parser and restore only settings observed
            // before the first child output. Never clear an inherited buffer,
            // pop an unowned keyboard stack or invent unknown shell defaults.
            if let Some(mut borrowed) = self.borrowed.take() {
                let _ = borrowed.close(&mut io::stdout().lock());
                self.palette_open = false;
            } else if self.palette_open {
                if let Some(snapshot) = &self.menu_snapshot {
                    let mut restore = crate::terminal_paint::BEGIN.to_vec();
                    crate::terminal_paint::restore(snapshot, false, &mut restore);
                    restore.extend_from_slice(crate::terminal_paint::END);
                    let _ = io::stdout().write_all(&restore);
                }
                self.palette_open = false;
            }
            let mut stdout = io::stdout().lock();
            if self.probed {
                let _ = stdout.write_all(&self.baseline.restore(self.final_screen.as_ref()));
            } else {
                // Nothing was observed: a killed SSH child cannot be relied
                // on for its application's cleanup, so end any partial
                // string/update and reset the ordinary shell input modes.
                let _ = stdout.write_all(TAKEOVER_CLEANUP);
            }
            let _ = stdout.flush();
        }
        if self.raw {
            let _ = terminal::disable_raw_mode();
            self.raw = false;
        }
    }
}

impl Drop for TerminalState {
    fn drop(&mut self) {
        self.restore();
    }
}

fn draw_palette(
    terminal: &mut PaletteTerminal,
    info: &ConnectionInfo<'_>,
    escape_key: EscapeKey,
    daemon: &DaemonView,
    terminal_model: &TerminalModel,
    overlay_enabled: bool,
) -> anyhow::Result<()> {
    io::stdout()
        .write_all(crate::terminal_paint::BEGIN)
        .context("establishing menu drawing state")?;
    terminal
        .draw(|frame| {
            ui::draw(
                frame,
                info,
                escape_key,
                daemon,
                overlay_enabled.then_some(terminal_model),
            )
        })
        .context("drawing local menu")?;
    io::stdout()
        .write_all(crate::terminal_paint::END)
        .context("ending menu drawing transaction")?;
    io::stdout().flush().context("flushing menu transaction")?;
    Ok(())
}

/// Borrowing a scratch alternate buffer for a menu closes on the first
/// query, non-ASCII byte or buffer switch, so under a live redraw it can
/// close before it is ever seen. Takeover covers the same cases; keep
/// borrowing opt-in until it is reliable.
fn borrow_enabled() -> bool {
    std::env::var_os("BEAMS_OVERLAY_BORROW").is_some_and(|value| value == "1")
}

fn new_palette_terminal() -> anyhow::Result<PaletteTerminal> {
    Terminal::new(CrosstermBackend::new(io::stdout())).context("creating local menu terminal")
}

#[cfg(test)]
mod tests {
    #[test]
    fn prefix_always_opens_a_menu_before_later_keys() {
        // Keys typed after the prefix belong to the menu the user asked
        // for, never to the remote session behind it.
        let mut router = super::InputRouter::default();
        let key = super::EscapeKey::default();
        let mut keys = Vec::new();
        let mut on_key = |key| {
            keys.push(key);
            false
        };
        assert_eq!(
            router.route(key, b"ab\x1dy", std::time::Instant::now(), &mut on_key),
            vec![super::InputAction::Remote(b"ab".to_vec()), super::InputAction::OpenPalette],
        );
        assert_eq!(router.mode, super::InputMode::Palette);
        assert_eq!(keys.len(), 1, "the key after the prefix goes to the menu");
    }

    use super::*;
    use std::io::{BufRead, BufReader};

    #[test]
    fn parses_control_hex_literal_and_disabled_escape_keys() {
        assert_eq!("ctrl-]".parse(), Ok(EscapeKey::Byte(0x1d)));
        assert_eq!("CTRL-A".parse(), Ok(EscapeKey::Byte(0x01)));
        assert_eq!("ctrl-?".parse(), Ok(EscapeKey::Byte(0x7f)));
        assert_eq!("0x1d".parse(), Ok(EscapeKey::Byte(0x1d)));
        assert_eq!("x".parse(), Ok(EscapeKey::Byte(b'x')));
        assert_eq!("none".parse(), Ok(EscapeKey::Disabled));
        assert!("ctrl-ab".parse::<EscapeKey>().is_err());
    }

    #[test]
    fn normal_input_is_forwarded_without_reencoding() {
        let mut mode = InputMode::Passthrough;
        let input = b"\x1b[<0;12;9Mhello\x00\xff";
        assert_eq!(
            route_input(&mut mode, EscapeKey::default(), input),
            vec![InputAction::Remote(input.to_vec())]
        );
        assert_eq!(mode, InputMode::Passthrough);
    }

    #[test]
    fn prefix_opens_palette_and_second_prefix_sends_literal() {
        let mut mode = InputMode::Passthrough;
        assert_eq!(
            route_input(&mut mode, EscapeKey::default(), b"\x1d"),
            vec![InputAction::OpenPalette]
        );
        assert_eq!(mode, InputMode::Palette);

        assert_eq!(
            route_input(&mut mode, EscapeKey::default(), b"\x1d"),
            vec![InputAction::ClosePalette, InputAction::Remote(vec![0x1d])]
        );
        assert_eq!(mode, InputMode::Passthrough);
    }

    #[test]
    fn enhanced_keyboard_prefix_opens_palette_and_preserves_literal_sequence() {
        let kitty_ctrl_bracket = b"\x1b[93;5u";
        let mut mode = InputMode::Passthrough;
        assert_eq!(
            route_input(&mut mode, EscapeKey::default(), kitty_ctrl_bracket),
            vec![InputAction::OpenPalette]
        );
        assert_eq!(mode, InputMode::Palette);

        assert_eq!(
            route_input(&mut mode, EscapeKey::default(), kitty_ctrl_bracket),
            vec![
                InputAction::ClosePalette,
                InputAction::Remote(kitty_ctrl_bracket.to_vec()),
            ]
        );
        assert_eq!(mode, InputMode::Passthrough);
    }

    #[test]
    fn kitty_prefix_release_and_repeat_do_not_toggle_the_menu() {
        let mut router = InputRouter::default();
        let actions = router.route(
            EscapeKey::default(),
            b"\x1b[93;5:1u\x1b[93;5:2u\x1b[93;5:3u",
            Instant::now(),
            &mut |_| false,
        );
        assert_eq!(actions, vec![InputAction::OpenPalette]);
        assert_eq!(router.mode, InputMode::Palette);
    }

    #[test]
    fn kitty_and_modify_other_keys_sequences_decode_control_keys() {
        let now = Instant::now();
        let mut router = InputRouter::default();
        assert_eq!(
            router.route(EscapeKey::Byte(b'\n'), b"\x1b[106;5u", now, &mut |_| false),
            vec![InputAction::OpenPalette]
        );

        router.mode = InputMode::Passthrough;
        assert_eq!(
            router.route(EscapeKey::Byte(b'\n'), b"\x1b[27;5;106~", now, &mut |_| {
                false
            }),
            vec![InputAction::OpenPalette]
        );
    }

    #[test]
    fn mouse_and_bracketed_paste_input_remain_byte_exact() {
        let mut mode = InputMode::Passthrough;
        let input = b"\x1b[<0;12;9M\x1b[200~pasted\x1b[201~";
        assert_eq!(
            route_input(&mut mode, EscapeKey::default(), input),
            vec![InputAction::Remote(input.to_vec())]
        );
    }

    #[test]
    fn pasted_prefixes_remain_remote_text_across_reads() {
        let mut router = InputRouter::default();
        let now = Instant::now();
        let input = b"\x1b[200~a\x1db\x1b[93;5u\x1b]text\x1b[201~";
        let mut received = Vec::new();
        for &byte in input {
            for action in router.route(EscapeKey::default(), &[byte], now, &mut |_| {
                panic!("pasted input reached the local menu")
            }) {
                match action {
                    InputAction::Remote(bytes) => received.extend(bytes),
                    other => panic!("paste opened the menu: {other:?}"),
                }
            }
        }
        assert_eq!(received, input);
        assert_eq!(router.mode, InputMode::Passthrough);
        assert_eq!(
            router.route(EscapeKey::default(), b"\x1d", now, &mut |_| false),
            vec![InputAction::OpenPalette],
            "prefix works again after paste ends"
        );
    }

    #[test]
    fn a_slow_split_paste_end_does_not_leave_the_router_in_paste_mode() {
        for mode in [InputMode::Passthrough, InputMode::Palette] {
            let mut router = InputRouter {
                mode,
                ..InputRouter::default()
            };
            let now = Instant::now();
            router.route(
                EscapeKey::default(),
                b"\x1b[200~text\x1b[20",
                now,
                &mut |_| false,
            );
            assert!(router
                .flush_expired(
                    EscapeKey::default(),
                    now + Duration::from_secs(1),
                    &mut |_| { panic!("a paste delimiter became a key") }
                )
                .is_empty());
            router.route(
                EscapeKey::default(),
                b"1~",
                now + Duration::from_secs(1),
                &mut |key| {
                    assert_eq!(key, Key::Paste("text".to_string()));
                    false
                },
            );
            assert!(!router.remote_paste && router.paste.is_none());
            assert_eq!(router.poll_timeout(now), LOOP_TICK);
        }
    }

    #[test]
    fn all_mouse_encodings_are_opaque_to_the_menu() {
        for (mouse_utf8, input) in [
            (false, b"\x1b[M qy".as_slice()),
            (false, b"\x1b[<0;12;9M".as_slice()),
            (false, b"\x1b[32;12;9M".as_slice()),
            (true, "\x1b[M \u{171}\u{179}".as_bytes()),
        ] {
            let mut router = InputRouter {
                mode: InputMode::Palette,
                mouse_utf8,
                ..InputRouter::default()
            };
            for byte in input {
                assert!(router
                    .route(EscapeKey::default(), &[*byte], Instant::now(), &mut |key| {
                        panic!("mouse became a menu key: {key:?}")
                    })
                    .is_empty());
            }
            assert!(router.pending.is_empty());
            router.mode = InputMode::Passthrough;
            assert_eq!(
                router.route(EscapeKey::default(), input, Instant::now(), &mut |_| false),
                vec![InputAction::Remote(input.to_vec())]
            );
        }
    }

    #[test]
    fn terminal_replies_bypass_menu_keys_across_reads() {
        for reply in [
            b"\x1b[12;34R".as_slice(),
            b"\x1b[?1;2c".as_slice(),
            b"\x1b[8;24;100t".as_slice(),
            b"\x1b[?2004;1$y".as_slice(),
            b"\x1b]52;c;cXk=\x1b\\".as_slice(),
            b"\x1bP1$r0m\x1b\\".as_slice(),
            // Focus reports, the theme report and a Kitty graphics reply,
            // all requested by zellij, belong to the remote.
            b"\x1b[I".as_slice(),
            b"\x1b[O".as_slice(),
            b"\x1b[?997;1n".as_slice(),
            b"\x1b_Gi=31;OK\x1b\\".as_slice(),
        ] {
            let mut router = InputRouter {
                mode: InputMode::Palette,
                ..InputRouter::default()
            };
            let now = Instant::now();
            for byte in &reply[..reply.len() - 1] {
                assert!(router
                    .route(EscapeKey::default(), &[*byte], now, &mut |key| {
                        panic!("terminal reply became a menu key: {key:?}")
                    })
                    .is_empty());
            }
            assert_eq!(
                router.route(
                    EscapeKey::default(),
                    &reply[reply.len() - 1..],
                    now,
                    &mut |_| { panic!("terminal reply reached menu") }
                ),
                vec![InputAction::Remote(reply.to_vec())]
            );
        }
    }

    #[test]
    fn partial_mouse_or_terminal_reply_does_not_become_menu_input_on_timeout() {
        let now = Instant::now();
        for input in [b"\x1b[M q".as_slice(), b"\x1b]52;c;qyn".as_slice()] {
            let mut router = InputRouter {
                mode: InputMode::Palette,
                ..InputRouter::default()
            };
            assert!(router
                .route(EscapeKey::default(), input, now, &mut |_| false)
                .is_empty());
            assert!(router
                .flush_expired(
                    EscapeKey::default(),
                    now + INPUT_SEQUENCE_TIMEOUT,
                    &mut |key| { panic!("partial report became a key: {key:?}") }
                )
                .is_empty());
        }
    }

    #[test]
    fn slow_and_oversized_clipboard_replies_stay_opaque_and_bounded() {
        let now = Instant::now();
        let mut router = InputRouter {
            mode: InputMode::Palette,
            ..InputRouter::default()
        };
        assert!(router
            .route(EscapeKey::default(), b"\x1b]52;c;", now, &mut |_| false)
            .is_empty());
        assert!(router
            .flush_expired(
                EscapeKey::default(),
                now + Duration::from_secs(1),
                &mut |_| { panic!("slow clipboard reply became a key") }
            )
            .is_empty());
        assert_eq!(router.poll_timeout(now + Duration::from_secs(1)), LOOP_TICK);
        let mut payload = vec![b'y'; MAX_PASTE_LEN];
        let first = router.route(EscapeKey::default(), &payload, now, &mut |_| {
            panic!("oversized clipboard reply became a key")
        });
        assert!(router.pending.is_empty(), "no unbounded pending reply");
        assert_eq!(first.len(), 1);
        payload.splice(0..0, b"\x1b]52;c;".iter().copied());
        assert_eq!(first, vec![InputAction::Remote(payload)]);
        assert_eq!(
            router.route(EscapeKey::default(), b"qyn\x1b\\", now, &mut |_| {
                panic!("streaming reply tail became a key")
            }),
            vec![InputAction::Remote(b"qyn\x1b\\".to_vec())]
        );
        assert!(router.string_tail.is_none());
        assert_eq!(
            router.route(EscapeKey::default(), b"q", now, &mut |key| {
                assert_eq!(key, Key::Char('q'));
                true
            }),
            vec![InputAction::ClosePalette]
        );
    }

    #[test]
    fn enhanced_sequences_can_span_reads_and_unmatched_input_is_unchanged() {
        let now = Instant::now();
        let mut router = InputRouter::default();
        assert_eq!(
            router.route(EscapeKey::default(), b"\x1b[93;", now, &mut |_| false),
            Vec::<InputAction>::new()
        );
        assert_eq!(
            router.route(
                EscapeKey::default(),
                b"5u",
                now + Duration::from_millis(1),
                &mut |_| false,
            ),
            vec![InputAction::OpenPalette]
        );

        router.mode = InputMode::Passthrough;
        let mouse = b"\x1b[<0;12;9M";
        assert_eq!(
            router.route(EscapeKey::default(), mouse, now, &mut |_| false),
            vec![InputAction::Remote(mouse.to_vec())]
        );
    }

    #[test]
    fn incomplete_escape_is_forwarded_after_the_sequence_timeout() {
        let now = Instant::now();
        let mut router = InputRouter::default();
        assert_eq!(
            router.route(EscapeKey::default(), b"\x1b", now, &mut |_| false),
            Vec::<InputAction>::new()
        );
        assert_eq!(
            router.flush_expired(
                EscapeKey::default(),
                now + INPUT_SEQUENCE_TIMEOUT,
                &mut |_| false
            ),
            vec![InputAction::Remote(vec![b'\x1b'])]
        );
    }

    #[test]
    fn palette_routes_local_approval_keys_without_sending_them_remotely() {
        let mut mode = InputMode::Palette;
        assert_eq!(
            route_input_keys(&mut mode, EscapeKey::default(), b"yn"),
            (Vec::new(), vec![Key::Char('y'), Key::Char('n')])
        );
        assert_eq!(mode, InputMode::Palette);
    }

    #[test]
    fn palette_decodes_arrows_utf8_and_bracketed_paste_into_keys() {
        let mut mode = InputMode::Palette;
        let (actions, keys) = route_input_keys(
            &mut mode,
            EscapeKey::default(),
            "\x1b[A\x1bOB\x1b[Zé\x1b[200~a\x1db\x1b[201~".as_bytes(),
        );
        assert!(actions.is_empty(), "{actions:?}");
        assert_eq!(
            keys,
            vec![
                Key::Up,
                Key::Down,
                Key::BackTab,
                Key::Char('é'),
                // The prefix inside a paste is text, not a menu close.
                Key::Paste("a\x1db".to_string()),
            ]
        );
        assert_eq!(mode, InputMode::Palette);
    }

    #[test]
    fn passthrough_keeps_utf8_and_ss3_bytes_exact_across_reads() {
        let now = Instant::now();
        let mut router = InputRouter::default();
        let bytes = "é\x1bOA".as_bytes();
        assert_eq!(
            router.route(EscapeKey::default(), &bytes[..1], now, &mut |_| false),
            Vec::<InputAction>::new()
        );
        assert_eq!(
            router.route(EscapeKey::default(), &bytes[1..], now, &mut |_| false),
            vec![InputAction::Remote(bytes.to_vec())],
            "the held lead byte is emitted with the rest of its character"
        );
    }

    #[test]
    fn approval_attention_is_once_per_request_and_scoped_to_this_beam() {
        let local_daemon = LocalDaemon {
            frontend: None,
            unavailable: None,
            managed_bridge: Some(ManagedBridge {
                target: crate::bridge::Target {
                    beam_id: "beam-a".to_string(),
                    beam_uuid: "beam-a-uuid".to_string(),
                    login: "beams".to_string(),
                },
                session_token: "token".to_string(),
            }),
            reconnect_after: Instant::now(),
        };
        let mut view = DaemonView::new(&local_daemon);
        for (request_id, beam_uuid) in [
            ("other-beam-request", "beam-b-uuid"),
            ("this-beam-request", "beam-a-uuid"),
        ] {
            view.request_records.insert(
                request_id.to_string(),
                RequestRecord {
                    request_id: request_id.to_string(),
                    beam_uuid: beam_uuid.to_string(),
                    kind: "local_port_forward".to_string(),
                    status: RequestStatus::PendingApproval,
                    request: None,
                    action: None,
                    message: None,
                    local_path: None,
                    agent: None,
                },
            );
        }

        assert_eq!(
            view.pending_request()
                .map(|request| request.request_id.as_str()),
            Some("this-beam-request")
        );
        assert!(view.take_approval_attention());
        assert!(!view.take_approval_attention());

        view.request_records.insert(
            "second-this-beam-request".to_string(),
            RequestRecord {
                request_id: "second-this-beam-request".to_string(),
                beam_uuid: "beam-a-uuid".to_string(),
                kind: "local_port_forward".to_string(),
                status: RequestStatus::PendingApproval,
                request: None,
                action: None,
                message: None,
                local_path: None,
                agent: None,
            },
        );
        assert!(view.take_approval_attention());
    }

    #[test]
    fn input_before_and_after_a_prefix_keeps_its_order() {
        let mut mode = InputMode::Passthrough;
        assert_eq!(
            route_input(&mut mode, EscapeKey::default(), b"before\x1dqafter"),
            vec![
                InputAction::Remote(b"before".to_vec()),
                InputAction::OpenPalette,
                InputAction::ClosePalette,
                InputAction::Remote(b"after".to_vec()),
            ]
        );
        assert_eq!(mode, InputMode::Passthrough);
    }

    #[test]
    fn disabled_prefix_is_pure_passthrough() {
        let mut mode = InputMode::Passthrough;
        assert_eq!(
            route_input(&mut mode, EscapeKey::Disabled, b"a\x1dz"),
            vec![InputAction::Remote(b"a\x1dz".to_vec())]
        );
        assert_eq!(mode, InputMode::Passthrough);
    }

    #[test]
    fn deferred_output_keeps_the_newest_bounded_bytes() {
        let mut output = DeferredOutput::default();
        output.push(&vec![b'a'; DEFERRED_OUTPUT_LIMIT - 2]);
        output.push(b"bcde");
        assert_eq!(output.bytes.len(), DEFERRED_OUTPUT_LIMIT);
        assert_eq!(
            output
                .bytes
                .iter()
                .rev()
                .take(4)
                .copied()
                .collect::<Vec<_>>(),
            b"edcb"
        );
        assert!(output.truncated);
    }

    #[test]
    fn overlay_relays_complete_osc_sequences_across_output_chunks() {
        let mut model = TerminalModel::new(PtySize::default());
        model.process(b"prefix\x1b]2;title");
        assert!(model.take_events().is_empty());
        model.process(b" continued\x07suffix");
        assert_eq!(
            model
                .take_events()
                .iter()
                .filter_map(TerminalEvent::terminal_bytes)
                .collect::<Vec<_>>(),
            vec![b"\x1b]2;title continued\x1b\\".to_vec()]
        );

        model.process(b"\x1b]52;c;SGV");
        assert!(model.take_events().is_empty());
        model.process(b"sbG8=\x1b\\");
        assert_eq!(
            model
                .take_events()
                .iter()
                .filter_map(TerminalEvent::terminal_bytes)
                .collect::<Vec<_>>(),
            vec![b"\x1b]52;c;SGVsbG8=\x1b\\".to_vec()]
        );

        model.process(b"\x1b");
        assert!(model.take_events().is_empty());
        model.process(b"]2;split");
        assert!(model.take_events().is_empty());
        model.process(b"\x07");
        assert_eq!(
            model
                .take_events()
                .iter()
                .filter_map(TerminalEvent::terminal_bytes)
                .collect::<Vec<_>>(),
            vec![b"\x1b]2;split\x1b\\".to_vec()]
        );
    }

    #[test]
    fn pty_child_reports_exit_and_binary_input_without_terminal_reencoding() {
        let PtyPair { master, slave } = native_pty_system().openpty(PtySize::default()).unwrap();
        let mut command = CommandBuilder::new("sh");
        command.args([
            "-c",
            "stty raw -echo; printf R; head -c 4 | od -An -tu1; exit 7",
        ]);
        let mut child = slave.spawn_command(command).unwrap();
        drop(slave);
        let mut reader = master.try_clone_reader().unwrap();
        let mut writer = master.take_writer().unwrap();

        // Wait until the child has disabled echo and canonical processing;
        // otherwise the deliberately non-UTF-8 0xff test byte can be echoed
        // back by the PTY before `stty raw -echo` takes effect.
        let mut ready = [0u8; 1];
        reader.read_exact(&mut ready).unwrap();
        assert_eq!(ready, [b'R']);
        writer.write_all(&[0, 3, 29, 255]).unwrap();
        writer.flush().unwrap();
        drop(writer);

        let mut output = Vec::new();
        match reader.read_to_end(&mut output) {
            Ok(_) => {}
            Err(err) if err.raw_os_error() == Some(libc::EIO) => {}
            Err(err) => panic!("reading PTY output: {err}"),
        }
        let status = child.wait().unwrap();
        let output = String::from_utf8(output).unwrap();

        assert_eq!(status.exit_code(), 7);
        assert!(
            output.split_whitespace().eq(["0", "3", "29", "255"]),
            "unexpected od output: {output:?}"
        );
    }

    #[test]
    fn pty_resize_is_visible_to_the_child() {
        let initial = PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        };
        let resized = PtySize {
            rows: 40,
            cols: 100,
            ..initial
        };
        let PtyPair { master, slave } = native_pty_system().openpty(initial).unwrap();
        let mut command = CommandBuilder::new("sh");
        command.args(["-c", "stty size; read ready; stty size"]);
        let mut child = slave.spawn_command(command).unwrap();
        drop(slave);
        let reader = master.try_clone_reader().unwrap();
        let mut reader = BufReader::new(reader);
        let mut writer = master.take_writer().unwrap();

        let mut first = String::new();
        reader.read_line(&mut first).unwrap();
        assert!(first.contains("24 80"), "initial size: {first:?}");

        master.resize(resized).unwrap();
        writer.write_all(b"ready\n").unwrap();
        writer.flush().unwrap();

        let mut rest = String::new();
        match reader.read_to_string(&mut rest) {
            Ok(_) => {}
            Err(err) if err.raw_os_error() == Some(libc::EIO) => {}
            Err(err) => panic!("reading PTY output: {err}"),
        }
        let status = child.wait().unwrap();
        assert!(status.success());
        assert!(rest.contains("40 100"), "resized output: {rest:?}");
    }

    // ---- local menu: screens, forms, and rendering ----

    fn managed_daemon() -> LocalDaemon {
        LocalDaemon {
            frontend: None,
            unavailable: None,
            managed_bridge: Some(ManagedBridge {
                target: crate::bridge::Target {
                    beam_id: "beam-a".to_string(),
                    beam_uuid: "beam-a-uuid".to_string(),
                    login: "beams".to_string(),
                },
                session_token: "token".to_string(),
            }),
            reconnect_after: Instant::now() + Duration::from_secs(3600),
        }
    }

    fn service(id: &str, beam_uuid: &str, spec: serde_json::Value, status: &str) -> ServiceRecord {
        serde_json::from_value(serde_json::json!({
            "service_id": id,
            "beam_uuid": beam_uuid,
            "kind": spec["kind"],
            "spec": spec,
            "status": status,
        }))
        .unwrap()
    }

    fn forward_spec(remote_port: u16, local_port: u16) -> serde_json::Value {
        serde_json::json!({
            "kind": "local_port_forward",
            "remote_host": "127.0.0.1",
            "remote_port": remote_port,
            "local_port": local_port,
        })
    }

    fn type_keys(view: &mut DaemonView, daemon: &mut LocalDaemon, text: &str) {
        for ch in text.chars() {
            view.handle_key(Key::Char(ch), daemon);
        }
    }

    fn info() -> ConnectionInfo<'static> {
        ConnectionInfo {
            beam_id: "beam-a",
            login: "beams",
        }
    }

    fn render(
        view: &DaemonView,
        model: Option<&TerminalModel>,
    ) -> ratatui::Terminal<ratatui::backend::TestBackend> {
        render_sized(view, model, 100, 30)
    }

    fn render_sized(
        view: &DaemonView,
        model: Option<&TerminalModel>,
        width: u16,
        height: u16,
    ) -> ratatui::Terminal<ratatui::backend::TestBackend> {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| ui::draw(frame, &info(), EscapeKey::default(), view, model))
            .unwrap();
        terminal
    }

    fn screen_text(terminal: &ratatui::Terminal<ratatui::backend::TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer.cell((x, y)).unwrap().symbol());
            }
            text.push('\n');
        }
        text
    }

    #[test]
    fn home_keys_open_forms_and_close_the_menu() {
        let mut daemon = managed_daemon();
        let mut view = DaemonView::new(&daemon);
        assert!(!view.handle_key(Key::Char('f'), &mut daemon));
        assert!(matches!(
            view.screen,
            Screen::Form {
                kind: FormKind::Forward,
                ..
            }
        ));
        // `q` is text inside a form, not a close.
        assert!(!view.handle_key(Key::Char('q'), &mut daemon));
        assert!(!view.handle_key(Key::Esc, &mut daemon));
        assert!(matches!(view.screen, Screen::Home));
        assert!(view.handle_key(Key::Char('q'), &mut daemon));
        assert!(view.handle_key(Key::Esc, &mut daemon));
    }

    #[test]
    fn forward_form_reports_field_errors_then_daemon_errors_in_place() {
        let mut daemon = managed_daemon();
        let mut view = DaemonView::new(&daemon);
        view.handle_key(Key::Char('f'), &mut daemon);
        view.handle_key(Key::Enter, &mut daemon);
        let Screen::Form { form, .. } = &view.screen else {
            panic!("form closed on a validation error");
        };
        assert_eq!(form.fields[0].error.as_deref(), Some("a port is required"));

        type_keys(&mut view, &mut daemon, "99999");
        view.handle_key(Key::Enter, &mut daemon);
        let Screen::Form { form, .. } = &view.screen else {
            panic!("form closed on a validation error");
        };
        assert_eq!(form.fields[0].error.as_deref(), Some("ports are 1-65535"));

        for _ in 0..5 {
            view.handle_key(Key::Backspace, &mut daemon);
        }
        type_keys(&mut view, &mut daemon, "8080");
        view.handle_key(Key::Enter, &mut daemon);
        let Screen::Form { form, .. } = &view.screen else {
            panic!("form closed although the daemon is unavailable");
        };
        assert!(!form.has_errors());
        assert_eq!(
            form.error.as_deref(),
            Some("the local daemon connection is unavailable")
        );
        assert_eq!(form.text("remote_port"), "8080", "input survives the error");
    }

    #[test]
    fn service_selection_is_clamped_for_both_display_and_actions() {
        let mut daemon = managed_daemon();
        let mut view = DaemonView::new(&daemon);
        for (id, uuid) in [
            ("svc-1", "beam-a-uuid"),
            ("svc-2", "beam-a-uuid"),
            ("other", "beam-b-uuid"),
        ] {
            view.service_records.insert(
                id.to_string(),
                service(id, uuid, forward_spec(80, 8080), "running"),
            );
        }
        for _ in 0..5 {
            view.handle_key(Key::Down, &mut daemon);
        }
        assert_eq!(view.selected_index(), 1);
        assert_eq!(view.selected_service().unwrap().service_id, "svc-2");
        view.handle_key(Key::Up, &mut daemon);
        assert_eq!(view.selected_index(), 0);

        view.select(1);
        view.service_records.remove("svc-2");
        assert_eq!(view.selected_service().unwrap().service_id, "svc-1");
        let text = screen_text(&render(&view, None));
        assert!(text.contains("› forward"), "{text}");
    }

    #[test]
    fn home_screen_renders_requests_and_services_in_plain_language() {
        let daemon = managed_daemon();
        let mut view = DaemonView::new(&daemon);
        view.client_id = Some("client-123".to_string());
        view.service_records.insert(
            "svc-1".to_string(),
            service("svc-1", "beam-a-uuid", forward_spec(3000, 13000), "running"),
        );
        view.request_records.insert(
            "req-1".to_string(),
            serde_json::from_value(serde_json::json!({
                "request_id": "req-1",
                "beam_uuid": "beam-a-uuid",
                "kind": "local_port_forward",
                "status": "pending_approval",
                "request": {"request_id": "req-1", "kind": forward_spec(5432, 15432)},
            }))
            .unwrap(),
        );
        let text = screen_text(&render(&view, None));
        for expected in [
            "Beams local menu",
            "client-123",
            "Awaiting approval",
            "pending",
            "Forward workstation port 15432 to 127.0.0.1:5432 in the Beam.",
            "TYPE",
            "localhost:13000 → 127.0.0.1:3000",
            "running",
            "Enter review",
            "y approve",
            "beams@beam-a",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in\n{text}");
        }
        assert!(
            !text.contains("LocalPortForward"),
            "debug output leaked:\n{text}"
        );
    }

    #[test]
    fn path_dropdown_draws_under_the_field_and_recents_are_recorded() {
        let dir = std::env::temp_dir().join(format!("beams-dropdown-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for name in ["Downloads", "Documents", "deploy-logs", "src"] {
            std::fs::create_dir_all(dir.join(name)).unwrap();
        }
        let mut daemon = managed_daemon();
        let mut view = DaemonView::new(&daemon);
        view.handle_key(Key::Char('m'), &mut daemon);
        type_keys(&mut view, &mut daemon, &format!("{}/dl", dir.display()));
        let terminal = render(&view, None);
        let text = screen_text(&terminal);
        if std::env::var_os("BEAMS_PRINT_RENDER").is_some() {
            println!("{text}");
        }
        let lines: Vec<&str> = text.lines().collect();
        let field = lines
            .iter()
            .position(|line| line.contains("Workstation dir"))
            .unwrap();
        assert!(
            lines[field + 1].contains('╭'),
            "popup under the field:\n{text}"
        );
        // "d…-l" hits a word boundary, so it outranks "D…l".
        assert!(lines[field + 2].contains("deploy-logs/"), "{text}");
        assert!(lines[field + 3].contains("Downloads/"), "{text}");
        assert!(
            !lines[field + 1].ends_with("╮╮"),
            "stays inside the dialog:\n{text}"
        );
        assert!(
            text.contains("Tab complete") && text.contains("Esc hide list"),
            "{text}"
        );

        view.handle_key(Key::Down, &mut daemon);
        view.handle_key(Key::Down, &mut daemon);
        view.handle_key(Key::Enter, &mut daemon);
        let Screen::Form { form, .. } = &view.screen else {
            panic!("Enter on a highlighted entry stays in the form");
        };
        assert_eq!(
            form.text("local_path"),
            format!("{}/Downloads/", dir.display())
        );
        // The managed test daemon has no live frontend, so submitting fails
        // at the daemon; nothing is recorded for a failed submit.
        type_keys(&mut view, &mut daemon, "\r");
        assert!(view.recents.directories().is_empty());
        let form = Form::new(
            "t",
            "go",
            vec![form::Field::text("local_path", "P").value(&format!("{}/src", dir.display()))],
        );
        let used = used_local_path(&FormKind::Sync, &form).unwrap();
        view.recents.record(&used.0, used.1);
        assert_eq!(
            view.recents.directories(),
            vec![format!("{}/src/", dir.display())]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn path_dropdown_fits_a_short_terminal_and_follows_the_highlight() {
        let dir = std::env::temp_dir().join(format!("beams-dropdown-short-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for index in 0..12 {
            std::fs::create_dir_all(dir.join(format!("dir-{index:02}"))).unwrap();
        }
        let mut daemon = managed_daemon();
        let mut view = DaemonView::new(&daemon);
        view.handle_key(Key::Char('d'), &mut daemon);
        type_keys(&mut view, &mut daemon, "/remote/file");
        view.handle_key(Key::Tab, &mut daemon);
        type_keys(&mut view, &mut daemon, &format!("{}/", dir.display()));
        for _ in 0..10 {
            view.handle_key(Key::Down, &mut daemon);
        }
        let terminal = render_sized(&view, None, 90, 16);
        let text = screen_text(&terminal);
        if std::env::var_os("BEAMS_PRINT_RENDER").is_some() {
            println!("{text}");
        }
        let lines: Vec<&str> = text.lines().collect();
        let bottom = lines
            .iter()
            .rposition(|line| line.contains('╰') && line.contains('╯'))
            .unwrap();
        assert!(bottom < lines.len(), "{text}");
        assert!(
            text.contains("› dir-09/"),
            "highlight scrolled into view:\n{text}"
        );
        assert!(text.contains(" of 12"), "footer kept:\n{text}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sync_form_shows_every_field_with_the_cursor_in_the_focused_one() {
        let mut daemon = managed_daemon();
        let mut view = DaemonView::new(&daemon);
        view.handle_key(Key::Char('m'), &mut daemon);
        type_keys(&mut view, &mut daemon, "/nonexistent-beams-dir");
        let mut terminal = render(&view, None);
        let text = screen_text(&terminal);
        for expected in [
            "Sync a folder with the Beam",
            "Workstation dir",
            "/nonexistent-beams-dir",
            "Beam dir",
            "two-way, safe",
            "Ignore VCS",
            "[x] skip .git",
            "Ignore rules",
            "comma-separated",
            "Both directories must already exist.",
            "Enter create",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in\n{text}");
        }
        let cursor = terminal.get_cursor_position().unwrap();
        let row: String = (0..100)
            .map(|x| {
                terminal
                    .backend()
                    .buffer()
                    .cell((x, cursor.y))
                    .unwrap()
                    .symbol()
                    .to_string()
            })
            .collect();
        assert!(
            row.contains("Workstation dir"),
            "cursor on wrong row: {row}"
        );
        assert_eq!(
            terminal
                .backend()
                .buffer()
                .cell((cursor.x - 1, cursor.y))
                .unwrap()
                .symbol(),
            "r",
            "cursor sits just after the typed text"
        );

        // Moving to the mode choice shows its warning for non-default modes.
        view.handle_key(Key::Tab, &mut daemon);
        view.handle_key(Key::Tab, &mut daemon);
        view.handle_key(Key::Right, &mut daemon);
        let text = screen_text(&render(&view, None));
        assert!(text.contains("‹ two-way, workstation wins ›"), "{text}");
        assert!(
            text.contains("Workstation content wins conflicts"),
            "{text}"
        );
    }

    #[test]
    fn overlay_mode_draws_the_dimmed_remote_screen_behind_the_same_dialog() {
        let daemon = managed_daemon();
        let view = DaemonView::new(&daemon);
        let mut model = TerminalModel::new(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        });
        model.process(b"remote zellij content");
        let terminal = render(&view, Some(&model));
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer.cell((0, 0)).unwrap().symbol(), "r");
        assert!(buffer
            .cell((0, 0))
            .unwrap()
            .modifier
            .contains(ratatui::style::Modifier::DIM));
        assert!(screen_text(&terminal).contains("Beams local menu"));
    }

    #[test]
    fn bridge_status_for_other_beams_does_not_take_over_the_status_line() {
        let daemon = managed_daemon();
        let mut view = DaemonView::new(&daemon);
        let bridge = |beam_id: &str, beam_uuid: &str, status: &str| BridgeRecord {
            beam_id: beam_id.to_string(),
            beam_uuid: beam_uuid.to_string(),
            login: "beams".to_string(),
            status: status.to_string(),
            session_count: 1,
            bridge_id: None,
            message: Some("bridge exited".to_string()),
        };
        view.apply_bridge_status(bridge("beam-a", "beam-a-uuid", "connected"));
        assert_eq!(view.message, "bridge to beam-a: connected (bridge exited)");
        view.apply_bridge_status(bridge("old-beam", "old-uuid", "reconnecting"));
        assert_eq!(view.message, "bridge to beam-a: connected (bridge exited)");
        assert_eq!(view.bridge_statuses.len(), 2);
        view.apply_bridge_status(bridge("old-beam", "old-uuid", "retired"));
        assert_eq!(
            view.bridge_statuses.len(),
            1,
            "retired bridges are forgotten"
        );
    }

    fn pending_forward(request_id: &str, remote_port: u16) -> RequestRecord {
        serde_json::from_value(serde_json::json!({
            "request_id": request_id,
            "beam_uuid": "beam-a-uuid",
            "kind": "local_port_forward",
            "status": "pending_approval",
            "request": {"request_id": request_id, "kind": forward_spec(remote_port, 0)},
        }))
        .unwrap()
    }

    fn pending_fetch(request_id: &str, hint: Option<&str>) -> RequestRecord {
        serde_json::from_value(serde_json::json!({
            "request_id": request_id,
            "beam_uuid": "beam-a-uuid",
            "kind": "file_fetch",
            "status": "pending_approval",
            "request": {"request_id": request_id, "kind": {
                "kind": "file_fetch",
                "sources": [
                    {"path": "/home/beams/out/a.log", "size": 1_200_000, "is_dir": false},
                    {"path": "/home/beams/out/logs", "size": 300, "is_dir": true},
                ],
                "local_path_hint": hint,
                "overwrite": "deny",
            }},
        }))
        .unwrap()
    }

    #[test]
    fn fetch_approval_prefills_the_destination_and_closes_when_withdrawn() {
        let mut daemon = managed_daemon();
        let mut view = DaemonView::new(&daemon);
        view.recents.record(std::path::Path::new("/srv/drop"), true);

        // No suggestion from the Beam: the most recent directory.
        add_request(&mut view, pending_fetch("fetch-1", None));
        view.handle_key(Key::Char('y'), &mut daemon);
        let Screen::Form { kind, form } = &view.screen else {
            panic!("approving a fetch asks where it goes");
        };
        assert!(matches!(
            kind,
            FormKind::FetchApproval { into_dir: true, .. }
        ));
        assert_eq!(form.text("local_path"), "/srv/drop/");
        let text = screen_text(&render(&view, None));
        if std::env::var_os("BEAMS_PRINT_RENDER").is_some() {
            println!("{text}");
        }
        for expected in [
            "Fetch from the Beam",
            "Copy 2 items (1.2 MB) from the Beam",
            "• /home/beams/out/a.log  1.2 MB",
            "• /home/beams/out/logs/",
            "Save to",
            "inside the destination directory",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in\n{text}");
        }

        // The Beam withdraws it while the form is open.
        let mut withdrawn = view.request_records["fetch-1"].clone();
        withdrawn.status = RequestStatus::Expired;
        withdrawn.message = Some("withdrawn on the Beam".to_string());
        view.request_records
            .insert("fetch-1".to_string(), withdrawn);
        view.close_stale_approval();
        assert!(matches!(view.screen, Screen::Home));
        assert!(view
            .notice
            .as_ref()
            .unwrap()
            .text
            .contains("withdrawn on the Beam"));

        // A Beam suggestion wins over recents, and is flagged as such.
        add_request(&mut view, pending_fetch("fetch-2", Some("~/Downloads/")));
        view.approve("fetch-2", &mut daemon);
        let Screen::Form { form, .. } = &view.screen else {
            panic!("form expected");
        };
        assert_eq!(form.text("local_path"), "~/Downloads/");
        assert!(form
            .notes
            .iter()
            .any(|note| note.contains("Beam's suggestion")));
    }

    fn toast_text(toast: &toast::Toast) -> String {
        let mut text = toast.title.clone();
        for line in &toast.lines {
            text.push('\n');
            for span in &line.spans {
                text.push_str(&span.content);
            }
        }
        text
    }

    #[test]
    fn request_toasts_show_until_reviewed_or_ignored_for_a_while() {
        let daemon = managed_daemon();
        let mut view = DaemonView::new(&daemon);
        let mut policy = notices::ToastPolicy::default();
        let start = Instant::now();
        assert!(policy.current(&view, EscapeKey::default(), start).is_none());

        add_request(&mut view, pending_fetch("fetch-1", None));
        let (toast, subject) = policy.current(&view, EscapeKey::default(), start).unwrap();
        assert_eq!(subject, notices::Subject::Request("fetch-1".to_string()));
        assert_eq!(
            toast_text(&toast),
            "Beam request\nFetch 2 items (1.2 MB)\nCtrl-] to review"
        );
        assert!(
            policy.current(&view, EscapeKey::Disabled, start).is_none(),
            "no toast pointing at a menu key that doesn't exist"
        );

        add_request(&mut view, pending_forward("forward-1", 8080));
        let later = start + Duration::from_secs(5);
        let (toast, subject) = policy.current(&view, EscapeKey::default(), later).unwrap();
        assert_eq!(
            subject,
            notices::Subject::Request("forward-1".to_string()),
            "newest first"
        );
        assert_eq!(
            toast_text(&toast),
            "2 Beam requests\nForward Beam port 8080\n+1 more · Ctrl-] to review"
        );

        // Ignored: fetch-1 times out first, then forward-1.
        let (_, subject) = policy
            .current(&view, EscapeKey::default(), start + Duration::from_secs(21))
            .unwrap();
        assert_eq!(subject, notices::Subject::Request("forward-1".to_string()));
        assert!(policy
            .current(&view, EscapeKey::default(), start + Duration::from_secs(26))
            .is_none());

        // Reviewed: opening the menu retires the toast at once.
        add_request(&mut view, pending_forward("forward-2", 9090));
        let now = start + Duration::from_secs(30);
        assert!(policy.current(&view, EscapeKey::default(), now).is_some());
        policy.mark_reviewed(&view);
        assert!(policy.current(&view, EscapeKey::default(), now).is_none());
    }

    #[test]
    fn transfer_toasts_follow_progress_then_report_the_outcome() {
        let daemon = managed_daemon();
        let mut view = DaemonView::new(&daemon);
        let mut policy = notices::ToastPolicy::default();
        let fetch_spec = pending_fetch("x", None).request.unwrap().kind;
        let spec = serde_json::to_value(&fetch_spec).unwrap();
        let mut old = service("old", "beam-a-uuid", spec.clone(), "stopped");
        old.request_id = "old-request".to_string();
        view.service_records.insert("old".to_string(), old);
        let start = Instant::now();
        assert!(
            policy.current(&view, EscapeKey::default(), start).is_none(),
            "transfers that finished before this session stay quiet"
        );

        let mut running = service("svc", "beam-a-uuid", spec, "running");
        running.local_path = Some("/srv/drop".to_string());
        running.progress = Some(beams_protocol::TransferProgress {
            bytes_done: 600_000,
            bytes_total: 1_200_300,
            files_done: 0,
            files_total: 2,
        });
        view.service_records
            .insert("svc".to_string(), running.clone());
        let (toast, subject) = policy.current(&view, EscapeKey::default(), start).unwrap();
        assert_eq!(subject, notices::Subject::Service("svc".to_string()));
        let text = toast_text(&toast);
        assert!(text.starts_with("Fetching\n2 items\n"), "{text}");
        assert!(text.ends_with(" 49% · 0/2"), "{text}");

        running.status = beams_protocol::ServiceStatus::Stopped;
        running.progress = Some(beams_protocol::TransferProgress {
            bytes_done: 1_200_300,
            bytes_total: 1_200_300,
            files_done: 2,
            files_total: 2,
        });
        view.service_records
            .insert("svc".to_string(), running.clone());
        let (toast, _) = policy.current(&view, EscapeKey::default(), start).unwrap();
        assert_eq!(toast.tone, toast::Tone::Ok);
        assert_eq!(toast_text(&toast), "Fetched\n✓ 2 items\ninto /srv/drop");
        assert!(policy
            .current(&view, EscapeKey::default(), start + Duration::from_secs(6))
            .is_none());
    }

    #[test]
    fn reviewing_from_a_toast_opens_the_request_itself() {
        let mut daemon = managed_daemon();
        let mut view = DaemonView::new(&daemon);
        add_request(&mut view, pending_fetch("fetch-1", Some("~/x/")));
        add_request(&mut view, pending_forward("forward-1", 8080));
        view.review_request("fetch-1", &mut daemon);
        assert!(matches!(
            &view.screen,
            Screen::Form { kind: FormKind::FetchApproval { request_id, .. }, .. } if request_id == "fetch-1"
        ));
        assert!(
            !view.handle_key(Key::Char('x'), &mut daemon),
            "typing stays in the form"
        );
        assert!(
            view.handle_key(Key::Esc, &mut daemon),
            "backing out of a toast's form returns to the session"
        );
        view.review_request("forward-1", &mut daemon);
        let Screen::Menu(menu) = &view.screen else {
            panic!("a forward gets the approve/reject menu");
        };
        assert!(menu.items.iter().any(|item| item.label.contains("Approve")));
        assert!(
            view.handle_key(Key::Char('n'), &mut daemon),
            "deciding returns too"
        );

        // Opened normally, the menu stays open after a decision.
        add_request(&mut view, pending_forward("forward-2", 9090));
        view.handle_key(Key::Enter, &mut daemon);
        assert!(matches!(view.screen, Screen::Menu(_)));
        assert!(!view.handle_key(Key::Char('n'), &mut daemon));
    }

    fn add_request(view: &mut DaemonView, request: RequestRecord) {
        view.note_request_arrival(&request.request_id);
        view.request_records
            .insert(request.request_id.clone(), request);
    }

    fn pending_agent(request_id: &str) -> RequestRecord {
        serde_json::from_value(serde_json::json!({
            "request_id": request_id,
            "beam_uuid": "beam-a-uuid",
            "kind": "ssh_agent",
            "status": "pending_approval",
            "agent": {
                "key_fingerprint": "SHA256:abc",
                "key_comment": "work key",
                "operation": {"kind": "login", "user": "git",
                              "host": {"status": "verified", "name": "github.com"}},
                "claimed": {"reason": "clone the repo", "command": "ssh git@github.com git-upload-pack 'o/r.git'"},
                "expires_at_ms": crate::agent::now_ms() + 60_000,
            },
        }))
        .unwrap()
    }

    /// Answers identity listings with one generated key.
    fn fake_agent_with_one_key(dir: &std::path::Path) -> (String, String) {
        use beams_protocol::agent as wire;
        use ssh_key::{rand_core::OsRng, Algorithm, PrivateKey};
        let key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap();
        let blob = key.public_key().to_bytes().unwrap();
        let fingerprint = beams_protocol::agent_crypto::fingerprint(&blob).unwrap();
        let path = dir.join("agent.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { return };
                let blob = blob.clone();
                std::thread::spawn(move || {
                    while let Ok(Some(_)) = wire::read_message(&mut conn) {
                        let reply = wire::encode_identities_answer(&[wire::Identity {
                            key_blob: blob.clone(),
                            comment: "laptop key".into(),
                        }]);
                        if wire::write_message(&mut conn, &reply).is_err() {
                            return;
                        }
                    }
                });
            }
        });
        (path.display().to_string(), fingerprint)
    }

    #[test]
    fn key_setup_is_offered_once_and_writes_the_choice_to_the_config() {
        let dir = std::env::temp_dir().join(format!("beams-key-setup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (sock, fingerprint) = fake_agent_with_one_key(&dir);
        // Only this test reads these (agent_keys_menu and Config::path).
        std::env::set_var("XDG_CONFIG_HOME", &dir);
        std::env::set_var("SSH_AUTH_SOCK", &sock);
        assert!(!crate::agent::Config::configured());

        let mut daemon = managed_daemon();
        let mut view = DaemonView::new(&daemon);
        view.offer_key_setup = true;
        view.key_setup_hint = true;
        let home_text = |view: &DaemonView| {
            ui::home_body(view, 80, 40)
                .iter()
                .map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert!(home_text(&view).contains("No SSH key is shared"), "{}", home_text(&view));
        let mut policy = notices::ToastPolicy::default();
        let (toast, subject) = policy.current(&view, EscapeKey::Byte(0x1d), Instant::now()).unwrap();
        assert_eq!(toast.title, "Share an SSH key with Beams?");
        assert_eq!(subject, notices::Subject::KeySetup);

        view.open_agent_keys_menu();
        assert!(!view.offer_key_setup, "offered once");
        assert!(view.key_setup_hint, "the hint stays until a choice is made");
        let Screen::Menu(menu) = &view.screen else { panic!("keys menu") };
        assert_eq!(menu.title, "SSH keys for Beams");
        assert_eq!(menu.items[0].action, MenuAction::AllowKey(fingerprint.clone()));
        assert!(menu.items[0].label.starts_with("Share laptop key"), "{}", menu.items[0].label);
        assert_eq!(menu.items[0].key, Some('1'));
        assert_eq!(menu.items[1].action, MenuAction::DeclineKeys);
        assert_eq!(menu.items[2].label, "Not now");

        view.handle_key(Key::Char('1'), &mut daemon);
        assert_eq!(crate::agent::Config::load().unwrap().allowed_keys, vec![fingerprint.clone()]);
        assert!(crate::agent::Config::configured());
        assert!(!home_text(&view).contains("No SSH key is shared"));

        // Now shared: the menu offers to stop, and no "don't ask" any more.
        view.handle_key(Key::Char('a'), &mut daemon);
        let Screen::Menu(menu) = &view.screen else { panic!("keys menu") };
        assert_eq!(menu.items[0].action, MenuAction::DenyKey(fingerprint.clone()));
        assert!(!menu.items.iter().any(|item| item.action == MenuAction::DeclineKeys));
        assert_eq!(menu.items[menu.selected].label, "Back");
        view.handle_key(Key::Char('1'), &mut daemon);
        assert!(crate::agent::Config::load().unwrap().allowed_keys.is_empty());
        assert!(crate::agent::Config::configured(), "an explicit empty list stays configured");

        std::env::remove_var("XDG_CONFIG_HOME");
        std::env::remove_var("SSH_AUTH_SOCK");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn startup_grants_are_listed_revocable_and_announce_each_use() {
        let mut daemon = managed_daemon();
        let mut view = DaemonView::new(&daemon);
        view.grants.insert(
            "g1".to_string(),
            crate::startup::GrantRecord {
                grant_id: "g1".into(),
                beam_uuid: "beam-a-uuid".into(),
                manifest_id: "manifest-1".into(),
                hosts: vec!["github.com".into()],
                user: "git".into(),
                uses: 1,
                max_uses: 5,
                expires_at_ms: crate::agent::now_ms() + 300_000,
                ended: None,
            },
        );
        let text = screen_text(&render(&view, None));
        assert!(text.contains("SSH key grants") && text.contains("Startup: github.com as git · 1 of 5 used"), "{text}");
        view.handle_key(Key::Enter, &mut daemon);
        let Screen::Menu(menu) = &view.screen else {
            panic!("Enter on a grant opens its menu");
        };
        assert_eq!(menu.items[0].action, MenuAction::RevokeGrant("g1".to_string()));
        assert_eq!(menu.items[menu.selected].label, "Keep", "keeping is the default");

        // A use shows a passive toast.
        let mut used = pending_agent("used-1");
        used.status = RequestStatus::Running;
        used.agent.as_mut().unwrap().granted = Some("startup grant, 2 of 5".into());
        add_request(&mut view, used);
        view.screen = Screen::Home;
        let mut policy = notices::ToastPolicy::default();
        let (toast, _) = policy.current(&view, EscapeKey::Byte(0x1d), Instant::now()).unwrap();
        assert_eq!(toast.title, "SSH key used");
        let detail: String = toast.lines[1].spans.iter().map(|span| span.content.as_ref()).collect();
        assert_eq!(detail, "startup grant, 2 of 5");
        let later = Instant::now() + Duration::from_secs(30);
        assert!(policy.current(&view, EscapeKey::Byte(0x1d), later).is_none(), "the notice goes away");
    }

    #[test]
    fn ssh_key_requests_are_only_approved_from_a_guarded_review_menu() {
        let mut daemon = managed_daemon();
        let mut view = DaemonView::new(&daemon);
        add_request(&mut view, pending_agent("key-1"));

        // `y` on the home screen opens the review instead of approving.
        view.handle_key(Key::Char('y'), &mut daemon);
        let Screen::Menu(menu) = &view.screen else {
            panic!("`y` should open the SSH key review");
        };
        assert_eq!(menu.title, "SSH key request");
        assert_eq!(menu.items[menu.selected].action, MenuAction::Reject("key-1".to_string()));
        assert!(view.input_guard_until.is_some());

        // Keys typed right away are dropped; the menu stays open.
        view.handle_key(Key::Char('y'), &mut daemon);
        view.handle_key(Key::Enter, &mut daemon);
        assert!(matches!(view.screen, Screen::Menu(_)), "guarded keys must not decide");

        // Typing that continues past the original window is still dropped:
        // each dropped key restarts the guard.
        let nearly = Instant::now() + Duration::from_millis(20);
        view.input_guard_until = Some(nearly);
        view.handle_key(Key::Up, &mut daemon);
        assert!(view.input_guard_until.is_some_and(|until| until > nearly + AGENT_INPUT_GUARD / 2));
        view.handle_key(Key::Char('y'), &mut daemon);
        assert!(matches!(view.screen, Screen::Menu(_)), "keys still being typed must not decide");

        // Once the guard passes, keys work again; Enter takes the default (Deny).
        view.input_guard_until = Some(Instant::now() - Duration::from_millis(1));
        view.handle_key(Key::Enter, &mut daemon);
        assert!(matches!(view.screen, Screen::Home));
        assert!(view.notice.as_ref().is_some_and(|notice| notice.text.contains("rejected") || notice.error));
    }

    #[test]
    fn ssh_key_review_keeps_the_beam_claims_indented_and_shows_a_countdown() {
        let mut daemon = managed_daemon();
        let mut view = DaemonView::new(&daemon);
        add_request(&mut view, pending_agent("key-1"));
        view.handle_key(Key::Enter, &mut daemon);
        let text = screen_text(&render(&view, None));
        assert!(text.contains("Sign in to github.com as git (host verified)."), "{text}");
        assert!(text.contains("Reported by the Beam (unverified):"), "{text}");
        assert!(text.contains("  Reason: clone the repo"), "claims stay indented:\n{text}");

        let mut policy = notices::ToastPolicy::default();
        view.screen = Screen::Home;
        let (toast, _) = policy.current(&view, EscapeKey::Byte(0x1d), Instant::now()).unwrap();
        assert_eq!(toast.title, "SSH key request");
        let hint: String = toast.lines[1].spans.iter().map(|span| span.content.as_ref()).collect();
        assert!(hint.contains("s left"), "{hint}");
    }

    #[test]
    fn every_pending_request_is_listed_oldest_first_and_the_new_one_is_selected() {
        let mut daemon = managed_daemon();
        let mut view = DaemonView::new(&daemon);
        // Ids sort the other way round from arrival.
        add_request(&mut view, pending_forward("zz-first", 1111));
        assert!(view.take_approval_attention());
        add_request(&mut view, pending_forward("aa-second", 2222));
        view.service_records.insert(
            "svc-1".to_string(),
            service("svc-1", "beam-a-uuid", forward_spec(80, 8080), "running"),
        );

        let ids: Vec<&str> = view
            .pending_requests()
            .iter()
            .map(|request| request.request_id.as_str())
            .collect();
        assert_eq!(ids, ["zz-first", "aa-second"]);

        assert!(view.take_approval_attention());
        assert_eq!(view.pending_request().unwrap().request_id, "aa-second");
        let text = screen_text(&render(&view, None));
        assert!(text.contains("Awaiting approval  2"), "{text}");
        assert!(text.contains(":2222"), "{text}");
        assert!(text.contains(":1111"), "{text}");

        // Selection follows the row: deciding the first request elsewhere
        // doesn't move it onto a different one.
        view.request_records.remove("zz-first");
        assert_eq!(view.pending_request().unwrap().request_id, "aa-second");
        view.handle_key(Key::Down, &mut daemon);
        assert_eq!(view.selected_service().unwrap().service_id, "svc-1");
        // With a service selected, y/n still act on the oldest request.
        assert_eq!(view.pending_request().unwrap().request_id, "aa-second");
    }

    #[test]
    fn enter_opens_a_request_menu_and_esc_returns_home() {
        let mut daemon = managed_daemon();
        let mut view = DaemonView::new(&daemon);
        add_request(&mut view, pending_forward("req", 5432));
        assert!(
            !view.handle_key(Key::Enter, &mut daemon),
            "Enter no longer closes"
        );
        let Screen::Menu(menu) = &view.screen else {
            panic!("Enter did not open the request menu");
        };
        assert_eq!(menu.items[0].action, MenuAction::Approve("req".to_string()));
        let text = screen_text(&render(&view, None));
        assert!(
            text.contains("Remote request") && text.contains("Reject"),
            "{text}"
        );
        view.handle_key(Key::Esc, &mut daemon);
        assert!(matches!(view.screen, Screen::Home));
        // `n` in the menu rejects; without a daemon that surfaces an error.
        view.handle_key(Key::Enter, &mut daemon);
        view.handle_key(Key::Char('n'), &mut daemon);
        assert!(matches!(view.screen, Screen::Home));
        assert!(view.notice.as_ref().is_some_and(|notice| notice.error));
    }

    #[test]
    fn destructive_service_actions_need_confirmation_defaulting_to_keep() {
        let mut daemon = managed_daemon();
        let mut view = DaemonView::new(&daemon);
        view.service_records.insert(
            "sync".to_string(),
            service(
                "sync",
                "beam-a-uuid",
                serde_json::json!({"kind": "folder_sync", "remote_path": "/r",
                    "local_path_hint": "/l", "mode": "two-way-safe",
                    "ignore_vcs": true, "ignores": []}),
                "running",
            ),
        );

        // The `t` shortcut asks first, and Enter alone keeps the sync.
        view.handle_key(Key::Char('t'), &mut daemon);
        let Screen::Menu(menu) = &view.screen else {
            panic!("terminate did not ask for confirmation");
        };
        assert_eq!(menu.title, "Terminate the sync?");
        assert_eq!(menu.items[menu.selected].action, MenuAction::Back);
        view.handle_key(Key::Enter, &mut daemon);
        assert!(matches!(view.screen, Screen::Home));
        assert!(view.notice.is_none(), "nothing was sent");

        // Through the actions menu: Enter on the service, pick terminate.
        view.handle_key(Key::Enter, &mut daemon);
        let Screen::Menu(menu) = &view.screen else {
            panic!("Enter did not open the service menu");
        };
        let labels: Vec<&str> = menu.items.iter().map(|item| item.label.as_str()).collect();
        assert_eq!(labels, ["Pause syncing", "Terminate the sync", "Back"]);
        let text = screen_text(&render(&view, None));
        assert!(
            text.contains("Service id: sync") && text.contains("Status: running"),
            "{text}"
        );
        view.handle_key(Key::Char('t'), &mut daemon);
        assert!(matches!(&view.screen, Screen::Menu(menu) if menu.title == "Terminate the sync?"));
        view.handle_key(Key::Char('y'), &mut daemon);
        assert!(matches!(view.screen, Screen::Home));
        assert_eq!(
            view.notice.as_ref().map(|notice| notice.text.as_str()),
            Some("the local daemon connection is unavailable"),
            "confirming sends the action"
        );
    }

    #[test]
    fn available_actions_follow_service_state() {
        let forward = |status| service("f", "beam-a-uuid", forward_spec(80, 8080), status);
        assert_eq!(
            ui::available_actions(&forward("running")),
            [ServiceAction::Stop, ServiceAction::Restart]
        );
        assert_eq!(
            ui::available_actions(&forward("stopped")),
            [ServiceAction::Restart]
        );
        let sync = |status| {
            service(
                "s",
                "beam-a-uuid",
                serde_json::json!({"kind": "folder_sync", "remote_path": "/r",
                    "mode": "two-way-safe", "ignore_vcs": true, "ignores": []}),
                status,
            )
        };
        assert_eq!(
            ui::available_actions(&sync("paused")),
            [ServiceAction::Resume, ServiceAction::Terminate]
        );
        let copy = |status| {
            service(
                "c",
                "beam-a-uuid",
                serde_json::json!({"kind": "file_copy", "direction": "upload",
                    "remote_path": "/r", "overwrite": "deny"}),
                status,
            )
        };
        assert_eq!(
            ui::available_actions(&copy("running")),
            [ServiceAction::Stop]
        );
        assert!(ui::needs_confirmation(
            &copy("running"),
            ServiceAction::Stop
        ));
        assert!(ui::available_actions(&copy("stopped")).is_empty());
    }
}
