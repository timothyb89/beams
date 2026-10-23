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
//! deliberately not here yet - auth/multi-daemon concerns, resolving a
//! `run_task`/manifest against capabilities from a *previous* run rather
//! than just the instances named in the same request):
//!
//! ```json
//! {"request_id": "1", "op": "list_tasks"}
//! {"request_id": "2", "op": "run_task", "task": "clone-git-repo", "params": {"repo": "...", "dest": "..."}}
//! {"request_id": "3", "op": "run_manifest", "tasks": [
//!   {"template": "clone-git-repo", "params": {"repo": "...", "dest": "~/projects/core"}},
//!   {"template": "checkout-pr-worktree", "params": {"base": "~/projects/core", "pr": 1234, "dest": "..."}}
//! ]}
//! ```
//!
//! `list_tasks`'s reply carries each template's param schema too (`tasks:
//! [{"template": "clone-git-repo", "params": [{"name": "repo", "kind":
//! "string", "required": true}, ...]}, ...]`, see `tasks::TemplateInfo`)
//! - enough for a client to build an input field per param without
//! hardcoding per-template knowledge.
//!
//! `run_manifest` is `run_task` generalized to several task instances at
//! once (see DESIGN.md's "Seeding manifest") - the entries are resolved
//! against each other into dependency waves (`executor::plan`, the same
//! thing `executor::run` uses to actually run them - see DESIGN.md's
//! "Concurrent execution"), so listing them out of dependency order, as
//! above, is fine, and independent entries run concurrently rather than
//! waiting on each other. Its `accepted` reply's `waves` field is that
//! plan: a list of task-id lists, one per wave, in the order they'll
//! run. Stops at the first wave containing a failure (not the first
//! failing *task* - the rest of that wave still finishes), same
//! semantics as the fixed startup sequence (`steps::run`). A manifest
//! entry can only depend on a capability provided by *another entry in
//! the same manifest* - not on one left over from a previous, separate
//! run. Because every task here is idempotent, the fix is simply to
//! list the full chain every time (e.g. always include `clone-git-repo`
//! alongside a `checkout-pr-worktree` that needs it): the redundant
//! entry just skips if its work is already done.
//!
//! Direct replies (only to the requester) carry the same `request_id`
//! back; broadcast `task_status`/`manifest_status` events don't carry
//! one, since they aren't a reply to anything in particular. A
//! `task_status` broadcast carries a `manifest_id` and a `wave` index
//! (into that manifest's `accepted` reply's `waves`) when the task ran
//! as part of a manifest - both `null` when it ran via a standalone
//! `run_task`. Every task sharing a `wave` value ran (or at least
//! started) concurrently - that's what a client should group progress
//! rows by, rather than inferring concurrency from event timing.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::Context;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::executor;
use crate::tasks::{self, Task};

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
    manifest_counter: AtomicU64,
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

    /// A fresh id for a `run_manifest` call - just needs to be unique
    /// within this daemon's lifetime, not globally meaningful.
    fn next_manifest_id(&self) -> String {
        format!("manifest-{}", self.manifest_counter.fetch_add(1, Ordering::Relaxed))
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
                json!({"request_id": request_id, "type": "tasks", "tasks": tasks::templates()})
                    .to_string(),
            );
        }
        "run_task" => {
            let name = request.get("task").and_then(Value::as_str).unwrap_or_default();
            let params = request.get("params").cloned().unwrap_or(Value::Null);
            run_task(hub, request_id, name, params, reply);
        }
        "run_manifest" => {
            let tasks_value = request.get("tasks").cloned().unwrap_or(Value::Null);
            run_manifest(hub, request_id, tasks_value, reply);
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
    let _ = run_one_task(hub, task.as_ref(), None);
}

#[derive(Deserialize)]
struct ManifestEntry {
    template: String,
    #[serde(default)]
    params: Value,
}

/// Instantiates every entry, failing (with entry-indexed context) on the
/// first one that doesn't name a known template or match its params -
/// before anything has run, same as an unresolvable manifest graph below.
fn build_manifest(entries: Vec<ManifestEntry>) -> Result<Vec<Box<dyn Task>>, String> {
    entries
        .into_iter()
        .enumerate()
        .map(|(i, entry)| {
            tasks::instantiate(&entry.template, entry.params)
                .map_err(|err| format!("manifest entry {i} ({:?}): {err}", entry.template))
        })
        .collect()
}

fn run_manifest(hub: &Hub, request_id: Option<&str>, tasks_value: Value, reply: &Sender<String>) {
    let entries: Vec<ManifestEntry> = match serde_json::from_value(tasks_value) {
        Ok(entries) => entries,
        Err(err) => {
            let _ = reply.send(error_reply(request_id, &format!("invalid manifest: {err}")));
            return;
        }
    };

    let instances = match build_manifest(entries) {
        Ok(instances) => instances,
        Err(message) => {
            let _ = reply.send(error_reply(request_id, &message));
            return;
        }
    };

    // Resolved against *this manifest's own* instances only - see the
    // module doc comment on why a dependency on an earlier, separate
    // run's capability doesn't work (and what to do instead). Just for
    // the `accepted` reply's `waves` list below - `executor::run` (which
    // actually runs these) resolves the same graph again itself, per
    // `executor::plan`'s own doc comment.
    let waves: Vec<Vec<String>> = match executor::plan(&instances) {
        Ok(waves) => waves,
        Err(err) => {
            let _ = reply.send(error_reply(request_id, &err.to_string()));
            return;
        }
    };

    let manifest_id = hub.next_manifest_id();
    let _ = reply.send(
        json!({
            "request_id": request_id,
            "type": "accepted",
            "manifest_id": manifest_id,
            "waves": waves,
        })
        .to_string(),
    );

    // Held for the *whole* manifest's duration, not just one task at a
    // time within it - a manifest runs as one atomic unit relative to
    // other connections' `run_task`/`run_manifest` calls, which queue on
    // this same lock (see `Hub::run_lock`'s doc comment). Tasks *within*
    // the manifest still run concurrently where the graph allows it -
    // this lock is about manifests/standalone tasks not interleaving
    // with each other, not about serializing within one.
    let _guard = hub.run_lock.lock().unwrap();
    hub.broadcast(&json!({"type": "manifest_status", "manifest_id": manifest_id, "status": "start"}).to_string());

    let on_event = |event: executor::Event| {
        let line = match event {
            executor::Event::Start { task_id, wave } => {
                task_status(&task_id, "start", None, Some(&manifest_id), Some(wave))
            }
            executor::Event::Progress { task_id, wave, message } => {
                task_status(&task_id, "progress", Some(&message), Some(&manifest_id), Some(wave))
            }
            executor::Event::Ok { task_id, wave, message } => {
                task_status(&task_id, "ok", Some(&message), Some(&manifest_id), Some(wave))
            }
            executor::Event::Skip { task_id, wave } => {
                task_status(&task_id, "skip", None, Some(&manifest_id), Some(wave))
            }
            executor::Event::Error { task_id, wave, message } => {
                task_status(&task_id, "error", Some(&message), Some(&manifest_id), Some(wave))
            }
        };
        hub.broadcast(&line);
    };

    // Already validated above (same instances) - the only way this
    // fails is a graph error, which computing `waves` already ruled out.
    let ok = executor::run(instances, &on_event).expect("graph already validated above");
    let status = if ok { "ok" } else { "error" };
    hub.broadcast(&json!({"type": "manifest_status", "manifest_id": manifest_id, "status": status}).to_string());
}

/// Runs one task, broadcasting its `task_status` lifecycle (`start` then
/// exactly one of `ok`/`skip`/`error`) - shared by `run_task` and
/// `run_manifest` so the broadcast shape can't drift between the two.
/// `manifest_id` tags the events when running as part of a manifest,
/// `None` for a standalone `run_task`. Returns `Err` (with a message
/// naming the failed task) on failure, so `run_manifest` knows to stop.
fn run_one_task(hub: &Hub, task: &dyn Task, manifest_id: Option<&str>) -> Result<(), String> {
    let task_id = task.id();
    hub.broadcast(&task_status(&task_id, "start", None, manifest_id, None));
    let report = |message: &str| {
        hub.broadcast(&task_status(&task_id, "progress", Some(message), manifest_id, None));
    };
    match task.run_with_progress(&report) {
        Ok(Some(message)) => {
            hub.broadcast(&task_status(&task_id, "ok", Some(&message), manifest_id, None));
            Ok(())
        }
        Ok(None) => {
            hub.broadcast(&task_status(&task_id, "skip", None, manifest_id, None));
            Ok(())
        }
        Err(err) => {
            let message = err.to_string();
            hub.broadcast(&task_status(&task_id, "error", Some(&message), manifest_id, None));
            Err(format!("task {task_id} failed: {message}"))
        }
    }
}

/// `wave` is `None` for a standalone `run_task` (no concurrency grouping
/// concept there) and `Some(i)` for a `run_manifest` task, matching the
/// index into that manifest's `accepted` reply's `waves` list - see
/// `executor::Event`'s doc comment on why a client would want this.
fn task_status(
    task_id: &str,
    status: &str,
    message: Option<&str>,
    manifest_id: Option<&str>,
    wave: Option<usize>,
) -> String {
    json!({
        "type": "task_status",
        "task_id": task_id,
        "status": status,
        "message": message,
        "manifest_id": manifest_id,
        "wave": wave,
    })
    .to_string()
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
    /// `handle_connection`'s doc comment. The returned `BufReader` must
    /// be reused across every `recv` call on this connection, not
    /// rebuilt per call - rebuilding it would silently drop whatever
    /// extra bytes (quite possibly a whole extra line or two, once
    /// several broadcasts land in one read()) its predecessor had
    /// already buffered but not yet handed back via `read_line`.
    fn connect() -> (UnixStream, BufReader<UnixStream>, thread::JoinHandle<()>) {
        let (server_end, client_end) = UnixStream::pair().expect("creating socket pair");
        client_end.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let read_half = client_end.try_clone().expect("cloning client end for reading");
        let hub = Arc::new(Hub::default());
        let handle = thread::spawn(move || {
            let _ = handle_connection(&hub, server_end);
        });
        (client_end, BufReader::new(read_half), handle)
    }

    fn send(stream: &mut UnixStream, line: &str) {
        writeln!(stream, "{line}").unwrap();
    }

    fn recv(reader: &mut BufReader<UnixStream>) -> Value {
        let mut line = String::new();
        reader.read_line(&mut line).expect("reading reply");
        serde_json::from_str(line.trim()).expect("reply should be valid JSON")
    }

    /// Like `recv`, but discards any number of `task_status` events with
    /// `status: "progress"` first - `clone-git-repo` (via `git clone
    /// --progress`) now emits zero or more of these between its `start`
    /// and its terminal event, and these tests only care about the
    /// terminal one, same as before progress reporting existed.
    fn recv_skipping_progress(reader: &mut BufReader<UnixStream>) -> Value {
        loop {
            let value = recv(reader);
            if value["type"] == "task_status" && value["status"] == "progress" {
                continue;
            }
            return value;
        }
    }

    #[test]
    fn list_tasks_returns_every_template_name_and_its_param_schema() {
        let (mut client, mut reader, _handle) = connect();
        send(&mut client, r#"{"request_id":"1","op":"list_tasks"}"#);
        let reply = recv(&mut reader);
        assert_eq!(reply["request_id"], "1");
        assert_eq!(reply["type"], "tasks");
        assert_eq!(reply["tasks"], serde_json::to_value(tasks::templates()).unwrap());
    }

    fn git(dir: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("running git {args:?}: {e}"));
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// A minimal local repo with one commit - usable as a `clone-git-repo`
    /// source with no network access.
    fn make_source_repo(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        git(dir, &["init", "-q", "-b", "main"]);
        git(dir, &["config", "user.email", "t@example.com"]);
        git(dir, &["config", "user.name", "t"]);
        std::fs::write(dir.join("f"), "x").unwrap();
        git(dir, &["add", "f"]);
        git(dir, &["commit", "-q", "-m", "x"]);
    }

    /// Same as `make_source_repo`, plus a second commit stashed under
    /// `refs/pull/<pr>/head` - the shape GitHub exposes a PR's head as,
    /// so `checkout-pr-worktree` has something to fetch without network
    /// access (same trick as `tasks::checkout_pr_worktree`'s own tests).
    fn make_source_repo_with_pr(dir: &Path, pr: u32) {
        make_source_repo(dir);
        git(dir, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(dir.join("f"), "pr change").unwrap();
        git(dir, &["commit", "-q", "-am", "pr change"]);
        git(dir, &["update-ref", &format!("refs/pull/{pr}/head"), "feature"]);
        git(dir, &["checkout", "-q", "main"]);
        git(dir, &["branch", "-D", "feature"]);
    }

    fn scratch_dir(label: &str) -> std::path::PathBuf {
        let tmp = std::env::temp_dir().join(format!("daemon-test-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        tmp
    }

    #[test]
    fn run_task_accepts_then_broadcasts_terminal_status_to_the_requester() {
        let tmp = scratch_dir("run-task");
        let source = tmp.join("source");
        let dest = tmp.join("dest");
        make_source_repo(&source);

        let (mut client, mut reader, _handle) = connect();
        let request = serde_json::json!({
            "request_id": "7",
            "op": "run_task",
            "task": "clone-git-repo",
            "params": {"repo": source.display().to_string(), "dest": dest.display().to_string()},
        });
        send(&mut client, &request.to_string());

        let accepted = recv(&mut reader);
        assert_eq!(accepted["type"], "accepted");
        assert_eq!(accepted["request_id"], "7");

        let start = recv(&mut reader);
        assert_eq!(start["type"], "task_status");
        assert_eq!(start["status"], "start");

        let done = recv_skipping_progress(&mut reader);
        assert_eq!(done["type"], "task_status");
        assert_eq!(done["status"], "ok");
        assert!(dest.join(".git").exists());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn run_manifest_resolves_dependency_order_and_runs_both_tasks() {
        let tmp = scratch_dir("manifest-order");
        let source = tmp.join("source");
        let base = tmp.join("base");
        let worktree = tmp.join("worktree");
        make_source_repo_with_pr(&source, 42);

        let (mut client, mut reader, _handle) = connect();
        // Listed out of dependency order on purpose - checkout-pr-worktree
        // requires clone-git-repo's capability, so the resolver must still
        // run clone-git-repo first.
        let request = serde_json::json!({
            "request_id": "1",
            "op": "run_manifest",
            "tasks": [
                {"template": "checkout-pr-worktree", "params": {
                    "base": base.display().to_string(), "pr": 42, "dest": worktree.display().to_string(),
                }},
                {"template": "clone-git-repo", "params": {
                    "repo": source.display().to_string(), "dest": base.display().to_string(),
                }},
            ],
        });
        send(&mut client, &request.to_string());

        let accepted = recv(&mut reader);
        assert_eq!(accepted["type"], "accepted");
        let waves = accepted["waves"].as_array().unwrap();
        assert_eq!(waves.len(), 2, "checkout-pr-worktree depends on clone-git-repo, so they're 2 waves: {waves:?}");
        let wave_task = |wave: &serde_json::Value, i: usize| wave[i].as_str().unwrap().to_string();
        assert!(
            wave_task(&waves[0], 0).starts_with("clone-git-repo:"),
            "clone-git-repo should be reordered into the earlier wave, got {waves:?}"
        );
        assert!(wave_task(&waves[1], 0).starts_with("checkout-pr-worktree:"));
        let manifest_id = accepted["manifest_id"].as_str().unwrap().to_string();

        let manifest_start = recv(&mut reader);
        assert_eq!(manifest_start["type"], "manifest_status");
        assert_eq!(manifest_start["status"], "start");
        assert_eq!(manifest_start["manifest_id"], manifest_id);

        for (wave, expected_task) in [(0, "clone-git-repo"), (1, "checkout-pr-worktree")] {
            let start = recv(&mut reader);
            assert_eq!(start["type"], "task_status");
            assert_eq!(start["status"], "start");
            assert_eq!(start["manifest_id"], manifest_id);
            assert_eq!(start["wave"], wave);
            assert!(start["task_id"].as_str().unwrap().starts_with(expected_task));

            let done = recv_skipping_progress(&mut reader);
            assert_eq!(done["type"], "task_status");
            assert_eq!(done["status"], "ok");
            assert_eq!(done["manifest_id"], manifest_id);
            assert_eq!(done["wave"], wave);
        }

        let manifest_done = recv(&mut reader);
        assert_eq!(manifest_done["type"], "manifest_status");
        assert_eq!(manifest_done["status"], "ok");

        assert_eq!(std::fs::read_to_string(worktree.join("f")).unwrap(), "pr change");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn run_manifest_rejects_a_dependency_not_satisfied_within_the_same_manifest() {
        let tmp = scratch_dir("manifest-missing-dep");
        let base = tmp.join("base"); // deliberately never cloned

        let (mut client, mut reader, _handle) = connect();
        let request = serde_json::json!({
            "request_id": "2",
            "op": "run_manifest",
            "tasks": [
                {"template": "checkout-pr-worktree", "params": {
                    "base": base.display().to_string(), "pr": 1, "dest": tmp.join("worktree").display().to_string(),
                }},
            ],
        });
        send(&mut client, &request.to_string());

        let reply = recv(&mut reader);
        assert_eq!(reply["type"], "error");
        assert_eq!(reply["request_id"], "2");
        assert!(reply["message"].as_str().unwrap().contains("repo-cloned"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn run_manifest_stops_at_the_first_failing_task() {
        let tmp = scratch_dir("manifest-failure");
        let source = tmp.join("source");
        let base = tmp.join("base");
        make_source_repo(&source); // no PR ref - checkout-pr-worktree will fail to fetch it

        let (mut client, mut reader, _handle) = connect();
        let request = serde_json::json!({
            "request_id": "3",
            "op": "run_manifest",
            "tasks": [
                {"template": "clone-git-repo", "params": {
                    "repo": source.display().to_string(), "dest": base.display().to_string(),
                }},
                {"template": "checkout-pr-worktree", "params": {
                    "base": base.display().to_string(), "pr": 999, "dest": tmp.join("worktree").display().to_string(),
                }},
            ],
        });
        send(&mut client, &request.to_string());

        let accepted = recv(&mut reader);
        assert_eq!(accepted["type"], "accepted");

        let manifest_start = recv(&mut reader);
        assert_eq!(manifest_start["status"], "start");

        let clone_start = recv(&mut reader);
        assert_eq!(clone_start["status"], "start");
        let clone_done = recv_skipping_progress(&mut reader);
        assert_eq!(clone_done["status"], "ok");

        let checkout_start = recv(&mut reader);
        assert_eq!(checkout_start["status"], "start");
        let checkout_failed = recv(&mut reader);
        assert_eq!(checkout_failed["type"], "task_status");
        assert_eq!(checkout_failed["status"], "error");

        let manifest_failed = recv(&mut reader);
        assert_eq!(manifest_failed["type"], "manifest_status");
        assert_eq!(manifest_failed["status"], "error");

        assert!(!tmp.join("worktree").join(".git").exists(), "failed checkout shouldn't leave a worktree behind");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn unknown_op_is_a_direct_error_reply() {
        let (mut client, mut reader, _handle) = connect();
        send(&mut client, r#"{"request_id":"9","op":"not_a_real_op"}"#);
        let reply = recv(&mut reader);
        assert_eq!(reply["type"], "error");
        assert_eq!(reply["request_id"], "9");
    }
}
