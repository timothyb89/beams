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
//! - terminal modes are restored on every normal/error/signal exit path.

use std::collections::{BTreeMap, VecDeque};
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
use ratatui::layout::{Alignment, Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Terminal;
use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
use signal_hook::iterator::{Handle as SignalHandle, Signals};

use beams_protocol::{RequestStatus, ServiceRequestKind};

use crate::clientd::{Frontend, RequestRecord, ServerEvent};

const LOOP_TICK: Duration = Duration::from_millis(25);
const EXIT_OUTPUT_GRACE: Duration = Duration::from_millis(250);
const DAEMON_RECONNECT_RETRY: Duration = Duration::from_secs(1);
const DEFERRED_OUTPUT_LIMIT: usize = 1024 * 1024;

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
    pub beam_uuid: &'a str,
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
) -> anyhow::Result<u8> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("interactive `beams ssh` requires a terminal on stdin and stdout");
    }

    let initial_size = current_size().context("reading local terminal size")?;
    let PtyPair { master, slave } = native_pty_system()
        .openpty(initial_size)
        .context("opening local PTY for tsh")?;

    let mut command = CommandBuilder::new(program);
    command.args(args);
    let mut child = slave
        .spawn_command(command)
        .with_context(|| format!("spawning {program} under local PTY"))?;
    drop(slave);

    let mut reader = master
        .try_clone_reader()
        .context("cloning local PTY reader")?;
    let mut writer = master.take_writer().context("opening local PTY writer")?;

    let (output_tx, output_rx) = mpsc::channel();
    let output_thread = thread::spawn(move || read_pty_output(&mut reader, output_tx));

    let (signal_rx, signal_handle, signal_thread) = signal_watcher()?;

    if let Some(key) = escape_key.byte() {
        eprintln!(
            "beams: {} opens the local menu; press it twice to send it to the remote session",
            EscapeKey::Byte(key).label()
        );
    } else {
        eprintln!("beams: local menu disabled (--escape-key none)");
    }

    let mut terminal_state = TerminalState::enter_raw().context("enabling raw terminal mode")?;
    let drive_result = drive(
        &*master,
        &mut *writer,
        &mut *child,
        &output_rx,
        &signal_rx,
        &info,
        escape_key,
        initial_size,
        &mut terminal_state,
        &mut local_daemon,
    );

    // Restore the local terminal before waiting/printing any error. The child
    // is explicitly killed on a local driver error; on success it has already
    // exited and `kill` is unnecessary.
    terminal_state.restore();
    if drive_result.is_err() {
        let _ = child.kill();
    }
    drop(writer); // sends EOF if the child is somehow still reading
    let _ = child.wait();

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
    writer: &mut dyn Write,
    child: &mut dyn portable_pty::Child,
    output_rx: &Receiver<OutputEvent>,
    signal_rx: &Receiver<i32>,
    info: &ConnectionInfo<'_>,
    escape_key: EscapeKey,
    mut last_size: PtySize,
    terminal_state: &mut TerminalState,
    local_daemon: &mut LocalDaemon,
) -> anyhow::Result<u8> {
    let mut input_mode = InputMode::Passthrough;
    let mut deferred = DeferredOutput::default();
    let mut output_finished = false;
    let mut daemon_view = DaemonView::new(local_daemon);

    loop {
        daemon_view.drain(local_daemon);

        if let Ok(signal) = signal_rx.try_recv() {
            let _ = child.kill();
            close_palette(
                master,
                terminal_state,
                &mut input_mode,
                &mut deferred,
                last_size,
            )?;
            return Ok((128 + signal).clamp(1, 255) as u8);
        }

        drain_output(output_rx, input_mode, &mut deferred, &mut output_finished)?;

        if let Some(status) = child.try_wait().context("polling tsh child")? {
            close_palette(
                master,
                terminal_state,
                &mut input_mode,
                &mut deferred,
                last_size,
            )?;
            finish_output(output_rx, &mut output_finished)?;
            return Ok(exit_code(&status));
        }

        if let Ok(size) = current_size() {
            if size != last_size {
                master.resize(size).context("resizing local PTY")?;
                last_size = size;
                if input_mode == InputMode::Palette {
                    draw_palette(info, escape_key, &daemon_view)?;
                }
            }
        }

        if stdin_ready(LOOP_TICK).context("polling terminal input")? {
            let mut bytes = [0u8; 4096];
            let read = io::stdin()
                .read(&mut bytes)
                .context("reading terminal input")?;
            if read == 0 {
                let _ = child.kill();
                close_palette(
                    master,
                    terminal_state,
                    &mut input_mode,
                    &mut deferred,
                    last_size,
                )?;
                return Ok(0);
            }

            for action in route_input(&mut input_mode, escape_key, &bytes[..read]) {
                match action {
                    InputAction::Remote(bytes) => {
                        writer.write_all(&bytes).context("writing to local PTY")?;
                        writer.flush().context("flushing local PTY")?;
                    }
                    InputAction::OpenPalette => {
                        terminal_state.open_palette()?;
                        draw_palette(info, escape_key, &daemon_view)?;
                    }
                    InputAction::ClosePalette => {
                        close_palette(
                            master,
                            terminal_state,
                            &mut input_mode,
                            &mut deferred,
                            last_size,
                        )?;
                    }
                    InputAction::ApprovePending => {
                        daemon_view.decide(local_daemon, true);
                        draw_palette(info, escape_key, &daemon_view)?;
                    }
                    InputAction::RejectPending => {
                        daemon_view.decide(local_daemon, false);
                        draw_palette(info, escape_key, &daemon_view)?;
                    }
                }
            }
        }
    }
}

struct DaemonView {
    client_id: Option<String>,
    connected: bool,
    frontends: usize,
    requests: usize,
    services: usize,
    request_records: BTreeMap<String, RequestRecord>,
    bridge_statuses: BTreeMap<String, String>,
    message: String,
}

impl DaemonView {
    fn new(local_daemon: &LocalDaemon) -> Self {
        match &local_daemon.frontend {
            Some(frontend) => Self {
                client_id: Some(frontend.client_id.clone()),
                connected: true,
                frontends: 1,
                requests: 0,
                services: 0,
                request_records: BTreeMap::new(),
                bridge_statuses: BTreeMap::new(),
                message: "connected".to_string(),
            },
            None => Self {
                client_id: None,
                connected: false,
                frontends: 0,
                requests: 0,
                services: 0,
                request_records: BTreeMap::new(),
                bridge_statuses: BTreeMap::new(),
                message: local_daemon
                    .unavailable
                    .clone()
                    .unwrap_or_else(|| "unavailable".to_string()),
            },
        }
    }

    fn drain(&mut self, local_daemon: &mut LocalDaemon) {
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
                }
                Err(err) => {
                    self.connected = false;
                    self.message = format!("reconnecting to local daemon: {err}");
                    local_daemon.reconnect_after = Instant::now() + DAEMON_RECONNECT_RETRY;
                    return;
                }
            }
        }

        let Some(frontend) = &mut local_daemon.frontend else {
            return;
        };
        loop {
            match frontend.events.try_recv() {
                Ok(ServerEvent::Snapshot {
                    client_id,
                    frontends,
                    requests,
                    services,
                    bridges,
                }) => {
                    self.client_id = Some(client_id);
                    self.connected = true;
                    self.frontends = frontends;
                    self.requests = requests.len();
                    self.services = services.len();
                    self.request_records = requests
                        .into_iter()
                        .map(|request| (request.request_id.clone(), request))
                        .collect();
                    self.bridge_statuses = bridges
                        .into_iter()
                        .map(|bridge| (bridge.beam_uuid, bridge.status))
                        .collect();
                    self.message = "connected".to_string();
                }
                Ok(ServerEvent::DaemonStatus { frontends }) => {
                    self.frontends = frontends;
                    self.connected = true;
                }
                Ok(ServerEvent::Error { message }) => {
                    self.message = format!("daemon error: {message}");
                }
                Ok(ServerEvent::RequestChanged { request }) => {
                    self.request_records
                        .insert(request.request_id.clone(), request);
                    self.requests = self.request_records.len();
                }
                Ok(ServerEvent::BridgeStatus { bridge }) => {
                    self.bridge_statuses
                        .insert(bridge.beam_uuid.clone(), bridge.status.clone());
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
                Ok(ServerEvent::Hello { .. } | ServerEvent::Pong) => {}
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    self.connected = false;
                    self.message = "connection lost".to_string();
                    local_daemon.frontend = None;
                    local_daemon.reconnect_after = Instant::now() + DAEMON_RECONNECT_RETRY;
                    return;
                }
            }
        }
    }

    fn pending_request(&self) -> Option<&RequestRecord> {
        self.request_records
            .values()
            .find(|request| request.status == RequestStatus::PendingApproval)
    }

    fn connected_bridges(&self) -> usize {
        self.bridge_statuses
            .values()
            .filter(|status| status.as_str() == "connected")
            .count()
    }

    fn decide(&mut self, local_daemon: &mut LocalDaemon, approve: bool) {
        let Some(request_id) = self
            .pending_request()
            .map(|request| request.request_id.clone())
        else {
            self.message = "no pending request to decide".to_string();
            return;
        };
        let Some(frontend) = &mut local_daemon.frontend else {
            self.message = "local daemon connection is unavailable".to_string();
            return;
        };
        match frontend.decide_request(&request_id, approve) {
            Ok(()) => {
                self.message = format!(
                    "{} request {request_id}",
                    if approve { "approving" } else { "rejecting" }
                );
            }
            Err(err) => {
                self.message = format!("could not send decision: {err}");
            }
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

#[derive(Debug, PartialEq, Eq)]
enum InputAction {
    Remote(Vec<u8>),
    OpenPalette,
    ClosePalette,
    ApprovePending,
    RejectPending,
}

/// Routes bytes without decoding/re-encoding terminal escape sequences. This
/// is the property that keeps normal SSH/zellij/mouse/paste behavior
/// transparent. The palette's intentionally tiny phase-one keymap is also raw:
/// q/Esc/Enter close it, while a second prefix sends the literal prefix byte.
fn route_input(mode: &mut InputMode, escape_key: EscapeKey, input: &[u8]) -> Vec<InputAction> {
    let mut actions = Vec::new();
    let mut remote = Vec::new();

    let flush_remote = |actions: &mut Vec<InputAction>, remote: &mut Vec<u8>| {
        if !remote.is_empty() {
            actions.push(InputAction::Remote(std::mem::take(remote)));
        }
    };

    for &byte in input {
        match *mode {
            InputMode::Passthrough if escape_key.byte() == Some(byte) => {
                flush_remote(&mut actions, &mut remote);
                *mode = InputMode::Palette;
                actions.push(InputAction::OpenPalette);
            }
            InputMode::Passthrough => remote.push(byte),
            InputMode::Palette if escape_key.byte() == Some(byte) => {
                *mode = InputMode::Passthrough;
                actions.push(InputAction::ClosePalette);
                remote.push(byte);
            }
            InputMode::Palette if matches!(byte, b'q' | b'Q' | b'\x1b' | b'\r' | b'\n') => {
                *mode = InputMode::Passthrough;
                actions.push(InputAction::ClosePalette);
            }
            InputMode::Palette if matches!(byte, b'y' | b'Y') => {
                actions.push(InputAction::ApprovePending);
            }
            InputMode::Palette if matches!(byte, b'n' | b'N') => {
                actions.push(InputAction::RejectPending);
            }
            InputMode::Palette => {
                // Future service forms will extend this tiny state machine.
            }
        }
    }
    flush_remote(&mut actions, &mut remote);
    actions
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

    fn flush(&mut self) -> anyhow::Result<()> {
        if self.bytes.is_empty() && !self.truncated {
            return Ok(());
        }
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
}

enum OutputEvent {
    Data(Vec<u8>),
    Eof,
    Error(io::Error),
}

fn read_pty_output(reader: &mut dyn Read, tx: mpsc::Sender<OutputEvent>) {
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

fn drain_output(
    rx: &Receiver<OutputEvent>,
    mode: InputMode,
    deferred: &mut DeferredOutput,
    finished: &mut bool,
) -> anyhow::Result<()> {
    let mut stdout = io::stdout().lock();
    let mut wrote = false;
    loop {
        match rx.try_recv() {
            Ok(OutputEvent::Data(bytes)) => {
                if mode == InputMode::Palette {
                    deferred.push(&bytes);
                } else {
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
    Ok(())
}

fn finish_output(rx: &Receiver<OutputEvent>, finished: &mut bool) -> anyhow::Result<()> {
    let deadline = Instant::now() + EXIT_OUTPUT_GRACE;
    let mut deferred = DeferredOutput::default();
    while !*finished && Instant::now() < deadline {
        drain_output(rx, InputMode::Passthrough, &mut deferred, finished)?;
        if !*finished {
            thread::sleep(Duration::from_millis(5));
        }
    }
    Ok(())
}

fn close_palette(
    master: &dyn MasterPty,
    terminal_state: &mut TerminalState,
    mode: &mut InputMode,
    deferred: &mut DeferredOutput,
    size: PtySize,
) -> anyhow::Result<()> {
    if terminal_state.palette_open {
        terminal_state.close_palette()?;
        deferred.flush()?;
        force_remote_redraw(master, size)?;
    }
    *mode = InputMode::Passthrough;
    Ok(())
}

/// Force a SIGWINCH-driven redraw without injecting Ctrl-L (which could be
/// meaningful input to a non-shell remote application). The transient size is
/// changed in the least disruptive dimension, then immediately restored.
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

struct TerminalState {
    raw: bool,
    palette_open: bool,
}

impl TerminalState {
    fn enter_raw() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        Ok(Self {
            raw: true,
            palette_open: false,
        })
    }

    fn open_palette(&mut self) -> io::Result<()> {
        // Do not enter the terminal's alternate screen here. The remote zellij
        // application normally already owns it, and alternate-screen state is
        // not safely nestable. Clear the current screen and rely on the
        // SIGWINCH redraw in `close_palette` instead.
        let mut stdout = io::stdout();
        execute!(stdout, Hide, Clear(ClearType::All), MoveTo(0, 0))?;
        self.palette_open = true;
        Ok(())
    }

    fn close_palette(&mut self) -> io::Result<()> {
        let mut stdout = io::stdout();
        execute!(stdout, Clear(ClearType::All), MoveTo(0, 0), Show)?;
        self.palette_open = false;
        Ok(())
    }

    fn restore(&mut self) {
        if self.palette_open {
            let _ = self.close_palette();
        } else {
            let _ = execute!(io::stdout(), Show);
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
    info: &ConnectionInfo<'_>,
    escape_key: EscapeKey,
    daemon: &DaemonView,
) -> anyhow::Result<()> {
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).context("creating local menu terminal")?;
    terminal
        .draw(|frame| {
            let area = frame.area();
            let [top, body, footer] = Layout::vertical([
                Constraint::Length(3),
                Constraint::Min(6),
                Constraint::Length(3),
            ])
            .areas(area);

            let heading = Paragraph::new(Line::from(vec![
                Span::styled(
                    "Beams local menu",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!(
                    " — {}@{} ({})",
                    info.login, info.beam_id, info.beam_uuid
                )),
            ]))
            .alignment(Alignment::Center)
            .block(Block::default().borders(Borders::ALL));
            frame.render_widget(heading, top);

            let daemon_color = if daemon.connected {
                Color::Green
            } else {
                Color::Yellow
            };
            let client_id = daemon.client_id.as_deref().unwrap_or("-");
            let mut body_text = vec![
                Line::from(""),
                Line::from(Span::styled(
                    "The managed SSH session is active.",
                    Style::default().add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from(vec![
                    Span::raw("local daemon: "),
                    Span::styled(&daemon.message, Style::default().fg(daemon_color)),
                ]),
                Line::from(format!("client id: {client_id}")),
                Line::from(format!(
                    "{} frontend(s), {} request(s), {} service(s), {} connected bridge(s)",
                    daemon.frontends,
                    daemon.requests,
                    daemon.services,
                    daemon.connected_bridges()
                )),
            ];
            if let Some(request) = daemon.pending_request() {
                body_text.push(Line::from(""));
                body_text.push(Line::from(Span::styled(
                    "Remote request awaiting approval",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )));
                if let Some(service_request) = &request.request {
                    match &service_request.kind {
                        ServiceRequestKind::LocalPortForward {
                            remote_host,
                            remote_port,
                            local_port,
                        } => {
                            body_text.push(Line::from(format!(
                                "local port {} → {}:{} in the Beam",
                                local_port
                                    .map(|port| port.to_string())
                                    .unwrap_or_else(|| "automatic".to_string()),
                                remote_host,
                                remote_port
                            )));
                        }
                    }
                } else {
                    body_text.push(Line::from(format!("service: {}", request.kind)));
                }
                body_text.push(Line::from("Press y to approve or n to reject."));
            }
            body_text.push(Line::from(""));
            body_text.push(Line::from(
                "Remote output continues to be drained while this menu is open.",
            ));
            frame.render_widget(
                Paragraph::new(body_text)
                    .alignment(Alignment::Center)
                    .block(Block::default().borders(Borders::ALL).title("connection")),
                body,
            );

            let literal = match escape_key {
                EscapeKey::Byte(_) => {
                    format!("{} again: send it remotely  •  ", escape_key.label())
                }
                EscapeKey::Disabled => String::new(),
            };
            let decision_keys = if daemon.pending_request().is_some() {
                "y: approve  •  n: reject  •  "
            } else {
                ""
            };
            frame.render_widget(
                Paragraph::new(format!("{decision_keys}{literal}q / Esc / Enter: close"))
                    .alignment(Alignment::Center)
                    .block(Block::default().borders(Borders::ALL)),
                footer,
            );
        })
        .context("drawing local menu")?;
    Ok(())
}

#[cfg(test)]
mod tests {
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
    fn palette_routes_local_approval_keys_without_sending_them_remotely() {
        let mut mode = InputMode::Palette;
        assert_eq!(
            route_input(&mut mode, EscapeKey::default(), b"yn"),
            vec![InputAction::ApprovePending, InputAction::RejectPending]
        );
        assert_eq!(mode, InputMode::Palette);
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
}
