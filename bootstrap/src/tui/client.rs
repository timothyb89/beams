//! A thin client for `daemon.rs`'s UDS socket, used by the TUI. Writes
//! requests directly on the caller's thread; reads happen on a dedicated
//! background thread that forwards every parsed line into an
//! `mpsc::Receiver` the UI polls each tick (`try_recv`) - keeps the
//! render loop itself non-blocking regardless of how fast or slow the
//! daemon is producing lines.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::thread;

use anyhow::Context;
use serde_json::Value;

pub struct DaemonClient {
    socket_path: PathBuf,
    write_half: UnixStream,
    pub events: Receiver<Value>,
}

impl DaemonClient {
    pub fn connect(socket_path: &Path) -> anyhow::Result<Self> {
        let (write_half, events) = open(socket_path)?;
        Ok(DaemonClient { socket_path: socket_path.to_path_buf(), write_half, events })
    }

    pub fn send(&mut self, request: &Value) -> anyhow::Result<()> {
        writeln!(self.write_half, "{request}").context("writing to daemon socket")?;
        self.write_half.flush().context("flushing daemon socket")
    }

    /// Drops the current connection (if any) and opens a fresh one to the
    /// same socket path, replacing both halves in place - same effect as
    /// a brand new `connect`, just reusable on an existing `DaemonClient`
    /// so callers (`tui::run_loop`'s reconnect handling) don't have to
    /// juggle swapping the whole value in and out of whatever's holding
    /// it. Errors (daemon not back up yet) leave the existing - already
    /// dead - halves in place; callers are expected to retry.
    pub fn reconnect(&mut self) -> anyhow::Result<()> {
        let (write_half, events) = open(&self.socket_path)?;
        self.write_half = write_half;
        self.events = events;
        Ok(())
    }
}

/// The actual connect-and-spawn-reader-thread logic, shared by `connect`
/// and `reconnect` - a fresh `UnixStream` plus a fresh reader thread and
/// channel every time, since the old reader thread (if any) already
/// exited on its own the moment the prior connection's read half hit EOF
/// or an error (see `run_loop`'s `RecvError::Disconnected` handling for
/// how that's detected).
fn open(socket_path: &Path) -> anyhow::Result<(UnixStream, Receiver<Value>)> {
    let read_half = UnixStream::connect(socket_path)
        .with_context(|| format!("connecting to {}", socket_path.display()))?;
    let write_half = read_half.try_clone().context("cloning connection for writing")?;

    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let reader = BufReader::new(read_half);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str(&line) {
                Ok(value) => {
                    if tx.send(value).is_err() {
                        break; // UI side hung up
                    }
                }
                Err(_) => continue, // ignore a malformed line rather than killing the reader
            }
        }
        // Falling off the end here (EOF, a read error, or a malformed
        // line loop that never breaks but the stream closes) drops `tx`,
        // which is exactly the signal `run_loop` watches for
        // (`TryRecvError::Disconnected`) to know the daemon connection
        // died and a reconnect is needed.
    });

    Ok((write_half, rx))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::sync::mpsc::TryRecvError;
    use std::time::{Duration, Instant};

    /// Blocks until `f` returns `Some`, or panics after `timeout` -
    /// standing in for the real daemon's broadcast/reconnect latency
    /// (not instantaneous: a background thread has to notice the
    /// socket event and react) without the test itself picking an
    /// arbitrary fixed sleep.
    fn wait_for<T>(timeout: Duration, mut f: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(v) = f() {
                return v;
            }
            if Instant::now() >= deadline {
                panic!("timed out after {timeout:?} waiting for condition");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn socket_path(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("{name}-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    /// Simulates exactly what a real `beam-bootstrapd` restart does to
    /// an existing `DaemonClient` connection: the daemon process exits
    /// (closing its end of the socket, which is what drives the client's
    /// reader thread to EOF - see `open`'s doc comment), then a fresh
    /// daemon binds the *same* socket path and starts broadcasting
    /// again. `reconnect` is what's supposed to pick that back up - this
    /// is the regression test for the TODO.md gap where the TUI
    /// previously just went quiet forever instead.
    #[test]
    fn reconnect_recovers_after_the_daemon_restarts() {
        let path = socket_path("tui-client-reconnect");

        // "Daemon" instance 1: accept one connection, send one line,
        // then drop everything (listener included) - closing the
        // socket file out from under the client, same as the process
        // exiting.
        let listener = UnixListener::bind(&path).unwrap();
        let mut client = DaemonClient::connect(&path).unwrap();
        let (mut stream, _) = listener.accept().unwrap();
        writeln!(stream, r#"{{"type":"tasks","tasks":[]}}"#).unwrap();
        stream.flush().unwrap();

        let first = wait_for(Duration::from_secs(2), || client.events.try_recv().ok());
        assert_eq!(first["type"], "tasks");

        drop(stream);
        drop(listener);
        let _ = std::fs::remove_file(&path);

        // The reader thread won't notice the close instantly - keep
        // draining until it reports `Disconnected` rather than racing it.
        wait_for(Duration::from_secs(2), || match client.events.try_recv() {
            Err(TryRecvError::Disconnected) => Some(()),
            _ => None,
        });

        // Reconnecting before a new daemon is listening should fail
        // cleanly (no panic, no stuck state) - same "caller retries"
        // contract `reconnect`'s doc comment promises.
        assert!(client.reconnect().is_err());

        // "Daemon" instance 2, same socket path - what `beamctl restart
        // beam-bootstrapd` actually produces.
        let listener2 = UnixListener::bind(&path).unwrap();
        client.reconnect().expect("should reconnect once the daemon is back");
        let (mut stream2, _) = listener2.accept().unwrap();
        writeln!(stream2, r#"{{"type":"tasks","tasks":[]}}"#).unwrap();
        stream2.flush().unwrap();

        let second = wait_for(Duration::from_secs(2), || client.events.try_recv().ok());
        assert_eq!(second["type"], "tasks");

        let _ = std::fs::remove_file(&path);
    }
}
