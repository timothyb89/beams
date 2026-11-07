//! Host key lookups through the workstation, for ssh's `KnownHostsCommand`.
//!
//! The Beam keeps no copy of the workstation's known_hosts. Instead its ssh
//! config (a drop-in in `/etc/ssh/ssh_config.d/`, so `~/.ssh` is left
//! alone) runs `beam-bootstrap client known-hosts %H` for every host it
//! checks. That asks the daemon (`known_hosts_query`), which routes the
//! query to a workstation bridge like an agent request (session token,
//! then the owner of the running manifest, then the only client) and
//! prints whatever entries the workstation's own known_hosts has for that
//! one host (`client/src/knownhosts.rs`). ssh then verifies the server's
//! key against them as usual; an unknown host still fails, since no TTY
//! asks and nothing is ever accepted automatically.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context};
use beams_protocol::{
    valid_known_host_name, BeamToClient, CAPABILITY_KNOWN_HOSTS, MAX_KNOWN_HOSTS_BYTES, MAX_KNOWN_HOSTS_LINES,
};
use serde_json::{json, Value};

/// How long a query may wait for the workstation. ssh waits on the
/// command, so this bounds how long a lookup can stall a connection.
const QUERY_TIMEOUT: Duration = Duration::from_secs(10);

/// The ssh config drop-in (Debian's `ssh_config` includes `ssh_config.d/*.conf`).
pub const SSH_CONFIG_DROP_IN: &str = "/etc/ssh/ssh_config.d/beams-known-hosts.conf";

/// Queries waiting for a `KnownHostsAnswer`, by bridge request id.
#[derive(Default)]
pub struct Pending {
    queries: Mutex<HashMap<String, Waiting>>,
}

struct Waiting {
    bridge_id: String,
    socket_request_id: Option<String>,
    reply: Sender<String>,
}

fn error(request_id: Option<&str>, message: &str) -> String {
    json!({"request_id": request_id, "type": "error", "message": message}).to_string()
}

/// The daemon op: `{"op": "known_hosts_query", "host": "github.com",
/// "session_token": ..., "task_run": ...}`, answered (possibly after the
/// workstation replies) with `{"type": "known_hosts", "lines": [...]}`.
pub fn query(
    pending: &Arc<Pending>,
    route: impl FnOnce(&str, Option<&str>, Option<&str>) -> Result<String, String>,
    send: impl FnOnce(&str, &BeamToClient) -> bool,
    request_id: Option<&str>,
    request: &Value,
    reply: &Sender<String>,
) {
    let host = request.get("host").and_then(Value::as_str).unwrap_or_default();
    if !valid_known_host_name(host) {
        let _ = reply.send(error(request_id, &format!("not a plain host name: {host:?}")));
        return;
    }
    let field = |name| request.get(name).and_then(Value::as_str).filter(|v| !v.is_empty());
    let bridge_id = match route(CAPABILITY_KNOWN_HOSTS, field("session_token"), field("task_run")) {
        Ok(bridge_id) => bridge_id,
        Err(message) => {
            let _ = reply.send(error(request_id, &message));
            return;
        }
    };
    let query_id = uuid::Uuid::new_v4().to_string();
    pending.queries.lock().unwrap().insert(
        query_id.clone(),
        Waiting { bridge_id: bridge_id.clone(), socket_request_id: request_id.map(str::to_string), reply: reply.clone() },
    );
    if !send(&bridge_id, &BeamToClient::KnownHostsQuery { request_id: query_id.clone(), host: host.to_string() }) {
        pending.queries.lock().unwrap().remove(&query_id);
        let _ = reply.send(error(request_id, "the workstation bridge went away"));
        return;
    }
    let pending = Arc::clone(pending);
    std::thread::spawn(move || {
        std::thread::sleep(QUERY_TIMEOUT);
        if let Some(waiting) = pending.queries.lock().unwrap().remove(&query_id) {
            let _ = waiting.reply.send(error(waiting.socket_request_id.as_deref(), "the workstation did not answer in time"));
        }
    });
}

/// A `KnownHostsAnswer` from `bridge_id`. Only the bridge that was asked
/// may answer; oversized answers are refused rather than trimmed.
pub fn answer(pending: &Pending, bridge_id: &str, request_id: &str, lines: Vec<String>) {
    let mut queries = pending.queries.lock().unwrap();
    if queries.get(request_id).is_none_or(|w| w.bridge_id != bridge_id) {
        return;
    }
    let waiting = queries.remove(request_id).expect("checked above");
    drop(queries);
    let bytes: usize = lines.iter().map(|l| l.len() + 1).sum();
    let message = if lines.len() > MAX_KNOWN_HOSTS_LINES || bytes > MAX_KNOWN_HOSTS_BYTES {
        error(waiting.socket_request_id.as_deref(), "the workstation's answer was too large")
    } else if lines.iter().any(|l| l.contains(['\n', '\r'])) {
        error(waiting.socket_request_id.as_deref(), "the workstation's answer was malformed")
    } else {
        json!({"request_id": waiting.socket_request_id, "type": "known_hosts", "lines": lines}).to_string()
    };
    let _ = waiting.reply.send(message);
}

/// The bridge went away: fail its queries now rather than at the timeout.
pub fn bridge_gone(pending: &Pending, bridge_id: &str) {
    let gone: Vec<Waiting> = {
        let mut queries = pending.queries.lock().unwrap();
        let ids: Vec<String> = queries.iter().filter(|(_, w)| w.bridge_id == bridge_id).map(|(id, _)| id.clone()).collect();
        ids.into_iter().filter_map(|id| queries.remove(&id)).collect()
    };
    for waiting in gone {
        let _ = waiting.reply.send(error(waiting.socket_request_id.as_deref(), "the workstation bridge disconnected"));
    }
}

/// `beam-bootstrap client known-hosts HOST`: print the workstation's
/// entries for `HOST` on stdout. Always exits 0 (ssh treats a failing
/// command as a warning anyway); a failure is one line on stderr.
pub fn run_cli(host: &str, socket: Option<String>) -> anyhow::Result<()> {
    match lookup_via_daemon(host, socket) {
        Ok(lines) => {
            let mut out = std::io::stdout().lock();
            for line in lines {
                writeln!(out, "{line}")?;
            }
        }
        Err(err) => eprintln!("beams known-hosts: {host}: {err:#}"),
    }
    Ok(())
}

fn lookup_via_daemon(host: &str, socket: Option<String>) -> anyhow::Result<Vec<String>> {
    let path = crate::pathutil::expand_tilde(&socket.unwrap_or_else(crate::daemon::default_socket_path));
    let mut conn = crate::taskcli::Connection::connect(&path)?;
    let env = |name| std::env::var(name).ok().filter(|v| !v.is_empty());
    conn.send(&json!({
        "request_id": "known-hosts",
        "op": "known_hosts_query",
        "host": host,
        "session_token": env("BEAMS_SESSION_TOKEN"),
        "task_run": env("BEAMS_TASK_RUN"),
    }))?;
    while let Some(reply) = conn.recv()? {
        if reply["request_id"] != "known-hosts" {
            continue;
        }
        if reply["type"] == "error" {
            bail!("{}", reply["message"].as_str().unwrap_or("unknown error"));
        }
        return Ok(serde_json::from_value(reply["lines"].clone()).context("reading the daemon's answer")?);
    }
    bail!("the daemon closed the connection without answering")
}

/// The drop-in's contents for `exe` (this binary).
pub fn ssh_config(exe: &Path) -> String {
    format!(
        "# Written by beam-bootstrap (bootstrap/src/knownhosts.rs): ask the beams\n\
         # workstation's own known_hosts for hosts this Beam doesn't know.\n\
         Host *\n    KnownHostsCommand {} client known-hosts %H\n",
        exe.display()
    )
}

/// Installs the ssh config drop-in (with `sudo -n`) unless it already says
/// the same. `Ok(false)`: nothing to do.
pub fn install_ssh_config(exe: &Path) -> anyhow::Result<bool> {
    let exe = exe.to_str().context("this binary's path is not UTF-8")?;
    if exe.contains(char::is_whitespace) {
        bail!("this binary's path contains whitespace: {exe}");
    }
    let wanted = ssh_config(Path::new(exe));
    if std::fs::read_to_string(SSH_CONFIG_DROP_IN).is_ok_and(|current| current == wanted) {
        return Ok(false);
    }
    let dir = Path::new(SSH_CONFIG_DROP_IN).parent().expect("has a parent");
    let mut child = Command::new("sudo")
        .args(["-n", "sh", "-c", "mkdir -p \"$1\" && cat > \"$2.tmp\" && chmod 644 \"$2.tmp\" && mv \"$2.tmp\" \"$2\"", "sh"])
        .arg(dir)
        .arg(SSH_CONFIG_DROP_IN)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("running sudo")?;
    child.stdin.take().expect("piped").write_all(wanted.as_bytes())?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!("writing {SSH_CONFIG_DROP_IN} failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;

    fn reply_of(rx: &std::sync::mpsc::Receiver<String>) -> Value {
        serde_json::from_str(&rx.recv_timeout(Duration::from_secs(1)).unwrap()).unwrap()
    }

    #[test]
    fn a_query_is_routed_sent_and_answered_only_by_its_bridge() {
        let pending = Arc::new(Pending::default());
        let (tx, rx) = channel();
        let sent = Mutex::new(None);
        query(
            &pending,
            |cap, token, run| {
                assert_eq!((cap, token, run), (CAPABILITY_KNOWN_HOSTS, Some("tok"), None));
                Ok("bridge-1".into())
            },
            |bridge, message| {
                *sent.lock().unwrap() = Some((bridge.to_string(), message.clone()));
                true
            },
            Some("r1"),
            &json!({"host": "github.com", "session_token": "tok", "task_run": ""}),
            &tx,
        );
        let (bridge, message) = sent.lock().unwrap().clone().unwrap();
        assert_eq!(bridge, "bridge-1");
        let BeamToClient::KnownHostsQuery { request_id, host } = message else { panic!() };
        assert_eq!(host, "github.com");
        // Another bridge can't answer it.
        answer(&pending, "bridge-2", &request_id, vec!["github.com ssh-ed25519 FAKE".into()]);
        assert!(rx.try_recv().is_err());
        answer(&pending, "bridge-1", &request_id, vec!["github.com ssh-ed25519 KEY".into()]);
        let reply = reply_of(&rx);
        assert_eq!(reply["request_id"], "r1");
        assert_eq!(reply["lines"], json!(["github.com ssh-ed25519 KEY"]));
        // A repeat answer is ignored.
        answer(&pending, "bridge-1", &request_id, vec![]);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn bad_names_routing_failures_and_disconnects_reply_with_errors() {
        let pending = Arc::new(Pending::default());
        let (tx, rx) = channel();
        query(&pending, |_, _, _| unreachable!(), |_, _| unreachable!(), Some("r"), &json!({"host": "-oProxyCommand=x"}), &tx);
        assert!(reply_of(&rx)["message"].as_str().unwrap().contains("plain host name"));
        query(&pending, |_, _, _| Err("no managed beams client is connected".into()), |_, _| unreachable!(), Some("r"), &json!({"host": "github.com"}), &tx);
        assert_eq!(reply_of(&rx)["message"], "no managed beams client is connected");
        query(&pending, |_, _, _| Ok("b".into()), |_, _| true, Some("r"), &json!({"host": "github.com"}), &tx);
        bridge_gone(&pending, "b");
        assert!(reply_of(&rx)["message"].as_str().unwrap().contains("disconnected"));
        assert!(pending.queries.lock().unwrap().is_empty());
    }

    #[test]
    fn oversized_or_multiline_answers_are_refused() {
        let pending = Arc::new(Pending::default());
        let (tx, rx) = channel();
        for lines in [vec!["x".to_string(); MAX_KNOWN_HOSTS_LINES + 1], vec!["a\nb".to_string()]] {
            let sent = Mutex::new(String::new());
            query(
                &pending,
                |_, _, _| Ok("b".into()),
                |_, m| {
                    let BeamToClient::KnownHostsQuery { request_id, .. } = m else { panic!() };
                    *sent.lock().unwrap() = request_id.clone();
                    true
                },
                None,
                &json!({"host": "github.com"}),
                &tx,
            );
            answer(&pending, "b", &sent.lock().unwrap(), lines);
            assert_eq!(reply_of(&rx)["type"], "error");
        }
    }

    #[test]
    fn the_drop_in_names_this_binary() {
        let config = ssh_config(Path::new("/home/beams/.beams/runtime/bin/beam-bootstrap"));
        assert!(config.contains("KnownHostsCommand /home/beams/.beams/runtime/bin/beam-bootstrap client known-hosts %H\n"));
        assert!(config.contains("Host *\n"));
    }
}
