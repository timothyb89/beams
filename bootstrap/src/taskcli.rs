//! A thin, synchronous daemon client for the one-shot `task
//! list`/`task run`/`task run-manifest` CLI commands (`main.rs`) -
//! deliberately not `tui::client::DaemonClient`, which spawns a
//! background reader thread so a render loop never blocks on a slow
//! daemon. A CLI invocation here *is* one blocking round trip by
//! design: send a request, print whatever broadcasts come back, exit
//! once the thing it asked for reaches a terminal state. See
//! `DESIGN.md`'s architecture diagram (`beams task run ...`) and
//! `TODO.md`'s "CLI/TUI sugar for run_manifest" item - this is both of
//! those at once, since they turned out to be the same piece of work.

use std::io::{BufRead, BufReader, Lines, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use anyhow::Context;
use serde_json::Value;

pub struct Connection {
    write: UnixStream,
    lines: Lines<BufReader<UnixStream>>,
}

impl Connection {
    pub fn connect(socket_path: &Path) -> anyhow::Result<Self> {
        let write = UnixStream::connect(socket_path)
            .with_context(|| format!("connecting to {}", socket_path.display()))?;
        let read = write.try_clone().context("cloning connection for reading")?;
        Ok(Connection { write, lines: BufReader::new(read).lines() })
    }

    pub fn send(&mut self, request: &Value) -> anyhow::Result<()> {
        writeln!(self.write, "{request}").context("writing to daemon socket")?;
        self.write.flush().context("flushing daemon socket")
    }

    /// Blocks for the next parsed JSON line from the daemon - a
    /// malformed line is skipped rather than treated as an error, same
    /// tolerance `tui::client` has (a daemon that's otherwise healthy
    /// shouldn't be unusable over one garbled broadcast). `Ok(None)`
    /// means the daemon closed the connection.
    pub fn recv(&mut self) -> anyhow::Result<Option<Value>> {
        loop {
            match self.lines.next() {
                None => return Ok(None),
                Some(Err(err)) => return Err(err).context("reading from daemon socket"),
                Some(Ok(line)) => {
                    if line.trim().is_empty() {
                        continue;
                    }
                    if let Ok(value) = serde_json::from_str(&line) {
                        return Ok(Some(value));
                    }
                    // else: malformed line, keep reading
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::thread;

    fn socket_path(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("{name}-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    /// Exercises `Connection` against a real daemon process (well, a
    /// real `daemon::serve` listener - not a mock), rather than just a
    /// bare echo server, so a real `request_id` round trip through the
    /// actual wire protocol is what's under test here, not just the
    /// socket plumbing.
    #[test]
    fn round_trips_a_real_list_tasks_request_against_the_daemon() {
        let path = socket_path("taskcli-list-tasks");
        let serve_path = path.clone();
        thread::spawn(move || {
            let _ = crate::daemon::serve(&serve_path);
        });

        // `serve` binds the socket asynchronously on its own thread -
        // poll for it rather than guessing a fixed sleep.
        let mut conn = connect_with_retry(&path);

        conn.send(&serde_json::json!({"request_id": "1", "op": "list_tasks"})).unwrap();
        let reply = conn.recv().unwrap().expect("daemon should reply, not hang up");
        assert_eq!(reply["type"], "tasks");
        assert_eq!(reply["request_id"], "1");
        assert!(reply["tasks"].as_array().unwrap().iter().any(|t| t["template"] == "clone-git-repo"));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn recv_returns_none_once_the_daemon_closes_the_connection() {
        let path = socket_path("taskcli-closed");
        let listener = UnixListener::bind(&path).unwrap();
        thread::spawn(move || {
            // Accept and immediately drop - simulates the daemon process
            // exiting mid-request.
            let _ = listener.accept();
        });

        let mut conn = connect_with_retry(&path);
        assert_eq!(conn.recv().unwrap(), None);

        let _ = std::fs::remove_file(&path);
    }

    fn connect_with_retry(path: &std::path::Path) -> Connection {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if let Ok(conn) = Connection::connect(path) {
                return conn;
            }
            if std::time::Instant::now() >= deadline {
                panic!("daemon never started listening on {}", path.display());
            }
            thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}
