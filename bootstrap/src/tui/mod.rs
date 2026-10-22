//! `beam-bootstrap tui`: a ratatui client for the daemon (`daemon.rs`),
//! per `DESIGN.md`'s "UX" section - deferred/templated tasks with param
//! input on top, concurrent per-task progress rows plus an aggregate
//! gauge on the bottom. See `app.rs` for the actual state/rendering, and
//! `client.rs` for the socket transport; this module is just the
//! terminal setup and the event loop tying the two together.

mod app;
mod client;

use std::path::Path;
use std::time::Duration;

use anyhow::Context;
use ratatui::crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event};
use ratatui::crossterm::execute;
use ratatui::DefaultTerminal;

use app::App;
use client::DaemonClient;

/// How often the event loop wakes up even with no input/daemon activity
/// - just enough to animate the running-task spinner.
const TICK: Duration = Duration::from_millis(120);

/// How long to keep retrying the initial connection before giving up.
/// Matters when this is baked into a zellij pane that starts around the
/// same time as the daemon itself (see `bin/beam-shell`/
/// `shell/zellij/beam-shell.kdl`) - `start-bootstrap-daemon` runs
/// synchronously before zellij launches specifically to avoid this race,
/// but retrying here is cheap insurance against the daemon just being
/// slow to bind its socket, rather than a hard requirement on exact
/// ordering elsewhere.
const CONNECT_RETRY_FOR: Duration = Duration::from_secs(5);
const CONNECT_RETRY_INTERVAL: Duration = Duration::from_millis(200);

pub fn run(socket_path: &Path) -> anyhow::Result<()> {
    let mut client = connect_with_retry(socket_path)
        .with_context(|| format!("connecting to daemon at {}", socket_path.display()))?;

    let mut terminal = ratatui::try_init().context("initializing terminal")?;
    // `try_init` only sets up raw mode + the alternate screen - mouse
    // capture is a separate opt-in, enabled/disabled here so the scroll
    // handling in `app::handle_mouse` actually gets events instead of
    // the terminal emulator just scrolling its own history buffer.
    execute!(std::io::stdout(), EnableMouseCapture).context("enabling mouse capture")?;
    // Runs regardless of how `run_loop` exits, so a crash or an early
    // `?` return doesn't strand the shell that started this in raw
    // mode/an alternate screen/mouse capture.
    let result = run_loop(&mut terminal, &mut client);
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    let _ = ratatui::try_restore();
    result
}

fn connect_with_retry(socket_path: &Path) -> anyhow::Result<DaemonClient> {
    let deadline = std::time::Instant::now() + CONNECT_RETRY_FOR;
    loop {
        match DaemonClient::connect(socket_path) {
            Ok(client) => return Ok(client),
            Err(err) if std::time::Instant::now() < deadline => {
                std::thread::sleep(CONNECT_RETRY_INTERVAL);
                let _ = err; // superseded by the next attempt, or the final one below
            }
            Err(err) => return Err(err),
        }
    }
}

fn run_loop(terminal: &mut DefaultTerminal, client: &mut DaemonClient) -> anyhow::Result<()> {
    let mut app = App::new();
    app.request_task_list();

    loop {
        if let Some(request) = app.pending_request.take() {
            client.send(&request)?;
        }

        while let Ok(value) = client.events.try_recv() {
            app.handle_daemon_event(value);
        }

        terminal.draw(|frame| app.draw(frame))?;

        if event::poll(TICK)? {
            match event::read()? {
                Event::Key(key) => app.handle_key(key),
                Event::Mouse(mouse) => app.handle_mouse(mouse),
                _ => {}
            }
        } else {
            app.tick();
        }

        if app.should_quit {
            // Flush anything the last keypress (e.g. a submit) queued -
            // otherwise quitting right after submitting would drop the
            // request on the floor.
            if let Some(request) = app.pending_request.take() {
                client.send(&request)?;
            }
            return Ok(());
        }
    }
}
