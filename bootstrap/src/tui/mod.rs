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
use ratatui::crossterm::event::{self, Event};
use ratatui::DefaultTerminal;

use app::App;
use client::DaemonClient;

/// How often the event loop wakes up even with no input/daemon activity
/// - just enough to animate the running-task spinner.
const TICK: Duration = Duration::from_millis(120);

pub fn run(socket_path: &Path) -> anyhow::Result<()> {
    let mut client = DaemonClient::connect(socket_path)
        .with_context(|| format!("connecting to daemon at {}", socket_path.display()))?;

    let mut terminal = ratatui::try_init().context("initializing terminal")?;
    // Runs regardless of how `run_loop` exits, so a crash or an early
    // `?` return doesn't strand the shell that started this in raw
    // mode/an alternate screen.
    let result = run_loop(&mut terminal, &mut client);
    let _ = ratatui::try_restore();
    result
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
            if let Event::Key(key) = event::read()? {
                app.handle_key(key);
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
