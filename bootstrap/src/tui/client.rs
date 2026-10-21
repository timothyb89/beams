//! A thin client for `daemon.rs`'s UDS socket, used by the TUI. Writes
//! requests directly on the caller's thread; reads happen on a dedicated
//! background thread that forwards every parsed line into an
//! `mpsc::Receiver` the UI polls each tick (`try_recv`) - keeps the
//! render loop itself non-blocking regardless of how fast or slow the
//! daemon is producing lines.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc::{self, Receiver};
use std::thread;

use anyhow::Context;
use serde_json::Value;

pub struct DaemonClient {
    write_half: UnixStream,
    pub events: Receiver<Value>,
}

impl DaemonClient {
    pub fn connect(socket_path: &Path) -> anyhow::Result<Self> {
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
        });

        Ok(DaemonClient { write_half, events: rx })
    }

    pub fn send(&mut self, request: &Value) -> anyhow::Result<()> {
        writeln!(self.write_half, "{request}").context("writing to daemon socket")?;
        self.write_half.flush().context("flushing daemon socket")
    }
}
