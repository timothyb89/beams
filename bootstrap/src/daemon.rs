//! The bootstrap daemon: a long-lived process reachable over a UDS
//! socket, driving on-demand task execution for whoever's connected (a
//! future TUI, the CLI, eventually a Mac app - see `DESIGN.md`'s
//! "Architecture" and "Wire protocol" sections).
//!
//! Protocol: newline-delimited JSON, one request per line in, any number
//! of lines back out. Every connection is both a requester and a
//! broadcast subscriber - a `run_task` from *any* connection emits
//! `task_status` events to *every* connected client, not just the one
//! that asked, so e.g. a TUI can watch progress for a task someone else
//! kicked off from the CLI. There's no `subscribe` op: connecting is
//! enough.
//!
//! Supported requests today (see the module doc comment for what's
//! deliberately not here yet - resolving a `run_task` against other
//! tasks' capabilities, a manifest/seeding op, auth/multi-daemon
//! concerns):
//!
//! ```json
//! {"request_id": "1", "op": "list_tasks"}
//! {"request_id": "2", "op": "run_task", "task": "clone-git-repo", "params": {"repo": "...", "dest": "..."}}
//! ```
//!
//! Direct replies (only to the requester) carry the same `request_id`
//! back; broadcast `task_status` events don't carry one, since they
//! aren't a reply to anything in particular.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::Context;
use serde_json::{json, Value};

use crate::tasks;

pub fn default_socket_path() -> String {
    "~/.beam-bootstrap/daemon.sock".to_string()
}

/// State shared by every connection: who's listening for broadcast
/// events, and a lock serializing task execution so only one task runs
/// at a time system-wide - the same one-thing-at-a-time semantics the
/// fixed startup sequence already has (see `steps::run`).
#[derive(Default)]
struct Hub {
    subscribers: Mutex<Vec<Sender<String>>>,
    run_lock: Mutex<()>,
}

impl Hub {
    /// Registers a new connection, returning the sender it should use
    /// for *both* its own direct replies and this hub's broadcasts (both
    /// end up as lines on the same socket - see the module doc comment)
    /// and the receiver its dedicated writer thread should drain.
    fn register(&self) -> (Sender<String>, Receiver<String>) {
        let (tx, rx) = mpsc::channel();
        self.subscribers.lock().unwrap().push(tx.clone());
        (tx, rx)
    }

    /// Sends `line` to every still-live subscriber, dropping any whose
    /// receiver has gone away (its connection closed).
    fn broadcast(&self, line: &str) {
        let mut subs = self.subscribers.lock().unwrap();
        subs.retain(|tx| tx.send(line.to_string()).is_ok());
    }
}

/// Binds `socket_path` and serves forever, one thread per connection.
/// Removes a stale socket file left behind by an unclean previous exit
/// (but only after confirming nothing is actually listening on it, so a
/// second daemon invocation doesn't steal the socket out from under a
/// live one).
pub fn serve(socket_path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = socket_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    if socket_path.exists() {
        if UnixStream::connect(socket_path).is_ok() {
            anyhow::bail!(
                "a daemon is already listening on {} - not starting a second one",
                socket_path.display()
            );
        }
        fs::remove_file(socket_path)
            .with_context(|| format!("removing stale socket {}", socket_path.display()))?;
    }

    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("binding {}", socket_path.display()))?;
    let hub = Arc::new(Hub::default());

    for stream in listener.incoming() {
        let stream = stream.context("accepting connection")?;
        let hub = Arc::clone(&hub);
        thread::spawn(move || {
            if let Err(err) = handle_connection(&hub, stream) {
                eprintln!("daemon: connection error: {err}");
            }
        });
    }
    Ok(())
}

/// Services one connection until it disconnects (EOF) or errors. Split
/// out from `serve` so tests can exercise the protocol directly over an
/// in-memory `UnixStream::pair()`, without a real socket file or an
/// infinite `accept()` loop to manage.
fn handle_connection(hub: &Arc<Hub>, stream: UnixStream) -> anyhow::Result<()> {
    let write_half = stream.try_clone().context("cloning connection for writing")?;
    let (tx, rx) = hub.register();

    // Dedicated writer thread: broadcasts (from *any* connection's
    // run_task) and this connection's own direct replies both arrive as
    // `String`s on `rx` and get written out here, independent of
    // whatever `handle_connection`'s own read loop below is doing (which
    // blocks for the duration of a `run_task` it issued) - this is what
    // lets a requester see live progress for its own task rather than
    // only after it finishes. Ends once every `Sender` for this
    // connection is gone (pruned by a future failed `broadcast`, since
    // the direct-reply `tx` passed to `handle_line` is this same sender).
    let writer = thread::spawn(move || {
        let mut write_half = write_half;
        for line in rx {
            if writeln!(write_half, "{line}").is_err() {
                break;
            }
            let _ = write_half.flush();
        }
    });

    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let line = line.context("reading request")?;
        if line.trim().is_empty() {
            continue;
        }
        handle_line(hub, &line, &tx);
    }

    drop(tx);
    let _ = writer.join();
    Ok(())
}

/// Parses and dispatches one request line, sending its direct reply (and
/// any `run_task` broadcasts) through `reply`.
fn handle_line(hub: &Hub, line: &str, reply: &Sender<String>) {
    let request: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(err) => {
            let _ = reply.send(error_reply(None, &format!("invalid JSON: {err}")));
            return;
        }
    };
    let request_id = request.get("request_id").and_then(Value::as_str);
    let op = request.get("op").and_then(Value::as_str).unwrap_or_default();

    match op {
        "list_tasks" => {
            let _ = reply.send(
                json!({"request_id": request_id, "type": "tasks", "tasks": tasks::names()})
                    .to_string(),
            );
        }
        "run_task" => {
            let name = request.get("task").and_then(Value::as_str).unwrap_or_default();
            let params = request.get("params").cloned().unwrap_or(Value::Null);
            run_task(hub, request_id, name, params, reply);
        }
        other => {
            let _ = reply.send(error_reply(request_id, &format!("unknown op {other:?}")));
        }
    }
}

fn run_task(hub: &Hub, request_id: Option<&str>, name: &str, params: Value, reply: &Sender<String>) {
    let task = match tasks::instantiate(name, params) {
        Ok(task) => task,
        Err(err) => {
            let _ = reply.send(error_reply(request_id, &err.to_string()));
            return;
        }
    };
    let task_id = task.id();
    let _ = reply.send(json!({"request_id": request_id, "type": "accepted", "task_id": task_id}).to_string());

    // Held for the run's duration: only one task executes at a time
    // system-wide (see `Hub::run_lock`'s doc comment). Other connections
    // can still issue `list_tasks`/further `run_task` requests meanwhile
    // (they just queue on this lock) and keep receiving broadcasts.
    let _guard = hub.run_lock.lock().unwrap();

    hub.broadcast(&json!({"type": "task_status", "task_id": task_id, "status": "start"}).to_string());
    match task.run() {
        Ok(Some(message)) => hub.broadcast(
            &json!({"type": "task_status", "task_id": task_id, "status": "ok", "message": message})
                .to_string(),
        ),
        Ok(None) => hub.broadcast(
            &json!({"type": "task_status", "task_id": task_id, "status": "skip"}).to_string(),
        ),
        Err(err) => hub.broadcast(
            &json!({"type": "task_status", "task_id": task_id, "status": "error", "message": err.to_string()})
                .to_string(),
        ),
    }
}

fn error_reply(request_id: Option<&str>, message: &str) -> String {
    json!({"request_id": request_id, "type": "error", "message": message}).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;
    use std::time::Duration;

    /// Connects an in-memory pair, serves one end on a background
    /// thread, and returns the other end for the test to drive - no
    /// socket file, no infinite `accept()` loop to manage, per
    /// `handle_connection`'s doc comment.
    fn connect() -> (UnixStream, thread::JoinHandle<()>) {
        let (server_end, client_end) = UnixStream::pair().expect("creating socket pair");
        let hub = Arc::new(Hub::default());
        let handle = thread::spawn(move || {
            let _ = handle_connection(&hub, server_end);
        });
        client_end.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        (client_end, handle)
    }

    fn send(stream: &mut UnixStream, line: &str) {
        writeln!(stream, "{line}").unwrap();
    }

    fn recv(stream: &mut UnixStream) -> Value {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).expect("reading reply");
        serde_json::from_str(line.trim()).expect("reply should be valid JSON")
    }

    #[test]
    fn list_tasks_returns_every_template_name() {
        let (mut client, _handle) = connect();
        send(&mut client, r#"{"request_id":"1","op":"list_tasks"}"#);
        let reply = recv(&mut client);
        assert_eq!(reply["request_id"], "1");
        assert_eq!(reply["type"], "tasks");
        assert_eq!(reply["tasks"], serde_json::json!(tasks::names()));
    }

    #[test]
    fn run_task_accepts_then_broadcasts_terminal_status_to_the_requester() {
        let tmp = std::env::temp_dir().join(format!("daemon-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let source = tmp.join("source");
        let dest = tmp.join("dest");
        std::fs::create_dir_all(&source).unwrap();
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.email", "t@example.com"],
            &["config", "user.name", "t"],
        ] {
            assert!(std::process::Command::new("git")
                .current_dir(&source)
                .args(args)
                .status()
                .unwrap()
                .success());
        }
        std::fs::write(source.join("f"), "x").unwrap();
        for args in [&["add", "f"][..], &["commit", "-q", "-m", "x"]] {
            assert!(std::process::Command::new("git")
                .current_dir(&source)
                .args(args)
                .status()
                .unwrap()
                .success());
        }

        let (mut client, _handle) = connect();
        let request = serde_json::json!({
            "request_id": "7",
            "op": "run_task",
            "task": "clone-git-repo",
            "params": {"repo": source.display().to_string(), "dest": dest.display().to_string()},
        });
        send(&mut client, &request.to_string());

        let accepted = recv(&mut client);
        assert_eq!(accepted["type"], "accepted");
        assert_eq!(accepted["request_id"], "7");

        let start = recv(&mut client);
        assert_eq!(start["type"], "task_status");
        assert_eq!(start["status"], "start");

        let done = recv(&mut client);
        assert_eq!(done["type"], "task_status");
        assert_eq!(done["status"], "ok");
        assert!(dest.join(".git").exists());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn unknown_op_is_a_direct_error_reply() {
        let (mut client, _handle) = connect();
        send(&mut client, r#"{"request_id":"9","op":"not_a_real_op"}"#);
        let reply = recv(&mut client);
        assert_eq!(reply["type"], "error");
        assert_eq!(reply["request_id"], "9");
    }
}
