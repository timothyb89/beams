//! Beam-side SSH agent listener (see `docs/designs/SSH_AGENT_PROXY_DESIGN.md`).
//!
//! `beam-bootstrapd` listens on `ssh-agent.sock` next to its own socket
//! (`~/.beam-bootstrap/ssh-agent.sock` by default) and relays agent use
//! to the workstation over the existing client bridge. No key material ever
//! exists here, and this side makes no security decisions:
//!
//! - `REQUEST_IDENTITIES` is answered locally from the keys the workstation
//!   last pushed for the bridge this connection routes to (none if no bridge
//!   is connected).
//! - `session-bind@openssh.com` is acknowledged at once and forwarded to the
//!   workstation, which verifies it and keeps it for the connection's sign
//!   requests.
//! - `SIGN_REQUEST`s are relayed with advisory process context gathered from
//!   `/proc`, and block until the workstation answers (it prompts the user)
//!   or [`SIGN_WAIT`] passes.
//! - Everything else (add/remove/lock keys, other extensions) is refused.
//!
//! Routing follows the managed-session token in the requester's (or a
//! parent's) environment when it names exactly one ready bridge, and
//! otherwise the only ready bridge that supports the agent, if there is
//! exactly one.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::Context;
use beams_protocol::agent::{self, AgentRequest, Identity};
use beams_protocol::{
    AgentContext, AgentKey, AgentProcess, BeamToClient, MAX_AGENT_ARGV, MAX_AGENT_CHAIN, MAX_AGENT_TEXT,
};
use serde::Serialize;

/// How long a relayed sign request may wait for the workstation. A bit more
/// than its prompt timeout, so its answer (even "expired") normally arrives
/// first; servers drop unauthenticated connections after ~120s anyway.
pub const SIGN_WAIT: Duration = Duration::from_secs(120);
/// Most keys a workstation may push for one bridge.
const MAX_KEYS: usize = 32;
/// Total characters of argv/cwd text in one request's context. The
/// per-field limits alone would allow ~60 KiB, and a frame over
/// `MAX_FRAME_BYTES` would cost the whole bridge.
const MAX_CONTEXT_TEXT: usize = 16 * 1024;

/// What the listener needs from the daemon's bridge registry.
pub trait AgentBridges: Send + Sync + 'static {
    fn agent_state(&self) -> &AgentState;
    /// The bridge to use for a requester carrying `session_token` and/or
    /// claiming `task_run` (`BEAMS_TASK_RUN`), or why there is none.
    fn route_agent(&self, session_token: Option<&str>, task_run: Option<&str>) -> Result<String, String>;
    /// Sends `message` over `bridge_id`; false if it's gone.
    fn send_agent(&self, bridge_id: &str, message: &BeamToClient) -> bool;
}

/// Per-daemon agent relay state: pushed keys per bridge and sign requests
/// waiting for an answer.
#[derive(Default)]
pub struct AgentState {
    identities: Mutex<HashMap<String, Vec<Identity>>>,
    pending: Mutex<HashMap<String, PendingSign>>,
    connections: AtomicU64,
}

struct PendingSign {
    bridge_id: String,
    reply: mpsc::Sender<Vec<u8>>,
}

impl AgentState {
    pub fn set_identities(&self, bridge_id: &str, keys: &[AgentKey]) -> Result<(), String> {
        if keys.len() > MAX_KEYS {
            return Err(format!("at most {MAX_KEYS} agent keys may be pushed"));
        }
        let identities = keys
            .iter()
            .map(|key| {
                Ok(Identity {
                    key_blob: agent::b64_decode(&key.blob).map_err(|e| format!("agent key: {e}"))?,
                    comment: key.comment.chars().take(MAX_AGENT_TEXT).collect(),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        self.identities.lock().unwrap().insert(bridge_id.to_string(), identities);
        Ok(())
    }

    fn identities(&self, bridge_id: &str) -> Vec<Identity> {
        self.identities.lock().unwrap().get(bridge_id).cloned().unwrap_or_default()
    }

    /// Delivers the workstation's answer to a waiting sign request. Only the
    /// bridge the request went to may answer it.
    pub fn complete(&self, bridge_id: &str, request_id: &str, message: &str) -> Result<(), String> {
        let body = agent::b64_decode(message).map_err(|e| format!("agent response: {e}"))?;
        if !matches!(body.first(), Some(&agent::msg::SIGN_RESPONSE | &agent::msg::FAILURE)) {
            return Err("agent response must be a sign response or a failure".to_string());
        }
        let mut pending = self.pending.lock().unwrap();
        match pending.get(request_id) {
            Some(waiting) if waiting.bridge_id == bridge_id => {}
            Some(_) => return Err(format!("agent request {request_id:?} belongs to another bridge")),
            // Already timed out here, or never existed.
            None => return Ok(()),
        }
        let waiting = pending.remove(request_id).expect("checked above");
        let _ = waiting.reply.send(body);
        Ok(())
    }

    /// The bridge went away: its keys are no longer listed, and its waiting
    /// sign requests fail now rather than at their timeout.
    pub fn forget_bridge(&self, bridge_id: &str) {
        self.identities.lock().unwrap().remove(bridge_id);
        self.pending.lock().unwrap().retain(|_, waiting| waiting.bridge_id != bridge_id);
    }
}

/// Binds `socket_path` (replacing a stale socket) and serves agent
/// connections forever, one thread each.
pub fn serve<B: AgentBridges>(socket_path: &Path, bridges: Arc<B>) -> anyhow::Result<()> {
    if let Some(parent) = socket_path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    if socket_path.exists() {
        if UnixStream::connect(socket_path).is_ok() {
            anyhow::bail!("an agent is already listening on {}", socket_path.display());
        }
        fs::remove_file(socket_path).with_context(|| format!("removing stale {}", socket_path.display()))?;
    }
    let listener =
        UnixListener::bind(socket_path).with_context(|| format!("binding {}", socket_path.display()))?;
    fs::set_permissions(socket_path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restricting {}", socket_path.display()))?;
    for conn in listener.incoming() {
        let conn = match conn {
            Ok(conn) => conn,
            Err(err) => {
                eprintln!("ssh-agent: accept failed: {err}");
                continue;
            }
        };
        let bridges = Arc::clone(&bridges);
        thread::spawn(move || {
            let n = bridges.agent_state().connections.fetch_add(1, Ordering::Relaxed) + 1;
            if let Err(err) = handle_connection(&*bridges, format!("agent-{n}"), conn) {
                eprintln!("ssh-agent: agent-{n}: {err:#}");
            }
        });
    }
    Ok(())
}

fn handle_connection<B: AgentBridges>(bridges: &B, conn_id: String, mut conn: UnixStream) -> anyhow::Result<()> {
    let pid = peer_cred(&conn).map(|(pid, _)| pid);
    let chain = pid.map(process_chain).unwrap_or_default();
    let session_token = chain.iter().find_map(|p| p.session_token.clone());
    let context = build_context(pid, &chain);
    let route = || bridges.route_agent(session_token.as_deref(), context.task_run.as_deref());
    // Bridges this connection sent binds or requests to, told when it closes.
    let mut announced: HashSet<String> = HashSet::new();
    let state = bridges.agent_state();

    let result = (|| -> anyhow::Result<()> {
        while let Some(body) = agent::read_message(&mut conn).context("reading agent request")? {
            let reply = match agent::parse_request(&body) {
                Ok(AgentRequest::RequestIdentities) => match route() {
                    Ok(bridge_id) => agent::encode_identities_answer(&state.identities(&bridge_id)),
                    Err(_) => agent::encode_identities_answer(&[]),
                },
                Ok(AgentRequest::SessionBind(_)) => {
                    if let Ok(bridge_id) = route() {
                        let message = BeamToClient::AgentSessionBind {
                            conn_id: conn_id.clone(),
                            message: agent::b64_encode(&body),
                        };
                        if bridges.send_agent(&bridge_id, &message) {
                            announced.insert(bridge_id);
                        }
                    }
                    agent::success()
                }
                Ok(AgentRequest::Sign { .. }) => {
                    relay_sign(bridges, &conn_id, &body, &context, route(), &mut announced)
                }
                Ok(other) => {
                    eprintln!("ssh-agent: {conn_id}: refused {}", other.label());
                    agent::failure()
                }
                Err(err) => {
                    eprintln!("ssh-agent: {conn_id}: malformed request: {err}");
                    agent::failure()
                }
            };
            agent::write_message(&mut conn, &reply).context("writing agent reply")?;
        }
        Ok(())
    })();

    for bridge_id in announced {
        bridges.send_agent(&bridge_id, &BeamToClient::AgentConnClosed { conn_id: conn_id.clone() });
    }
    result
}

fn relay_sign<B: AgentBridges>(
    bridges: &B,
    conn_id: &str,
    body: &[u8],
    context: &AgentContext,
    route: Result<String, String>,
    announced: &mut HashSet<String>,
) -> Vec<u8> {
    let bridge_id = match route {
        Ok(bridge_id) => bridge_id,
        Err(why) => {
            eprintln!("ssh-agent: {conn_id}: sign refused: {why}");
            return agent::failure();
        }
    };
    let state = bridges.agent_state();
    let request_id = uuid::Uuid::new_v4().to_string();
    let (tx, rx) = mpsc::channel();
    state
        .pending
        .lock()
        .unwrap()
        .insert(request_id.clone(), PendingSign { bridge_id: bridge_id.clone(), reply: tx });
    let message = BeamToClient::AgentSignRequest {
        request_id: request_id.clone(),
        conn_id: conn_id.to_string(),
        message: agent::b64_encode(body),
        context: context.clone(),
    };
    if !bridges.send_agent(&bridge_id, &message) {
        state.pending.lock().unwrap().remove(&request_id);
        eprintln!("ssh-agent: {conn_id}: sign refused: workstation bridge went away");
        return agent::failure();
    }
    announced.insert(bridge_id);
    match rx.recv_timeout(SIGN_WAIT) {
        Ok(reply) => reply,
        Err(_) => {
            state.pending.lock().unwrap().remove(&request_id);
            eprintln!("ssh-agent: {conn_id}: sign request {request_id} got no answer");
            agent::failure()
        }
    }
}

fn build_context(pid: Option<u32>, chain: &[ProcInfo]) -> AgentContext {
    // Nearest processes matter most, so the budget is spent in chain order.
    let mut budget = MAX_CONTEXT_TEXT;
    let mut take = |text: &String| -> Option<String> {
        let cost = text.len().max(1);
        (cost <= budget).then(|| {
            budget -= cost;
            text.clone()
        })
    };
    let processes = chain
        .iter()
        .take(MAX_AGENT_CHAIN)
        .map(|p| AgentProcess {
            pid: p.pid,
            argv: p.argv.iter().take(MAX_AGENT_ARGV).map_while(&mut take).collect(),
            cwd: p.cwd.as_ref().and_then(&mut take),
        })
        .collect();
    AgentContext {
        pid,
        chain: processes,
        reason: chain.iter().find_map(|p| p.reason.clone()),
        task_run: chain.iter().find_map(|p| p.task_run.clone()),
        host_candidates: host_candidates(chain).into_iter().take(8).collect(),
    }
}

/// One process in the requester's ancestry, as read from `/proc`.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct ProcInfo {
    pub pid: u32,
    pub ppid: u32,
    pub start_time: u64,
    pub argv: Vec<String>,
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_run: Option<String>,
    /// `BEAMS_SESSION_TOKEN`, for routing only.
    #[serde(skip)]
    pub session_token: Option<String>,
}

pub(crate) fn peer_cred(conn: &UnixStream) -> Option<(u32, u32)> {
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // Safety: `cred`/`len` are valid for writes of the sizes passed.
    let rc = unsafe {
        libc::getsockopt(
            conn.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    (rc == 0 && cred.pid > 0).then_some((cred.pid as u32, cred.uid))
}

/// The process and its parents, stopping at init or at this process (the
/// daemon itself, for tasks it runs).
pub(crate) fn process_chain(pid: u32) -> Vec<ProcInfo> {
    let mut out = Vec::new();
    let mut next = pid;
    let me = std::process::id();
    while out.len() < MAX_AGENT_CHAIN && next > 1 && next != me {
        let Some(info) = proc_info(next) else { break };
        next = info.ppid;
        out.push(info);
    }
    out
}

fn proc_info(pid: u32) -> Option<ProcInfo> {
    let base = PathBuf::from(format!("/proc/{pid}"));
    let stat = fs::read_to_string(base.join("stat")).ok()?;
    // Fields after the last ')' (the comm may itself contain ')').
    let rest = &stat[stat.rfind(')')? + 2..];
    let fields: Vec<&str> = rest.split(' ').collect();
    let ppid = fields.get(1)?.parse().ok()?;
    let start_time = fields.get(19)?.parse().ok()?;
    let argv = fs::read(base.join("cmdline"))
        .ok()
        .map(|raw| {
            raw.split(|b| *b == 0)
                .filter(|s| !s.is_empty())
                .map(|s| sanitize(&String::from_utf8_lossy(s)))
                .collect()
        })
        .unwrap_or_default();
    let cwd = fs::read_link(base.join("cwd")).ok().map(|p| sanitize(&p.display().to_string()));
    let (mut reason, mut task_run, mut session_token) = (None, None, None);
    if let Ok(environ) = fs::read(base.join("environ")) {
        for var in environ.split(|b| *b == 0) {
            let var = String::from_utf8_lossy(var);
            if let Some(v) = var.strip_prefix("SSH_KEY_REASON=") {
                reason = Some(sanitize(v));
            } else if let Some(v) = var.strip_prefix("BEAMS_TASK_RUN=") {
                task_run = Some(sanitize(v));
            } else if let Some(v) = var.strip_prefix("BEAMS_SESSION_TOKEN=") {
                session_token = Some(sanitize(v)).filter(|v| !v.is_empty());
            }
        }
    }
    Some(ProcInfo { pid, ppid, start_time, argv, cwd, reason, task_run, session_token })
}

/// Drops control characters (incl. ESC, so no ANSI/OSC) and bidi overrides,
/// and bounds the length.
pub(crate) fn sanitize(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control() && !matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
        .take(MAX_AGENT_TEXT)
        .collect()
}

/// Destination hosts mentioned by `ssh` invocations in the chain, spelled the
/// way `known_hosts` would. Untrusted but harmless: a candidate only matters
/// if the user's own known_hosts confirms it for the verified host key.
pub(crate) fn host_candidates(chain: &[ProcInfo]) -> Vec<String> {
    let mut out = Vec::new();
    for info in chain {
        let Some(argv0) = info.argv.first() else { continue };
        if Path::new(argv0).file_name().and_then(|n| n.to_str()) != Some("ssh") {
            continue;
        }
        if let Some((host, port)) = ssh_destination(&info.argv[1..]) {
            let name = match port {
                None | Some(22) => host,
                Some(port) => format!("[{host}]:{port}"),
            };
            if !out.contains(&name) {
                out.push(name);
            }
        }
    }
    out
}

fn ssh_destination(args: &[String]) -> Option<(String, Option<u16>)> {
    const WITH_ARG: &str = "BbcDEeFIiJLlmOoPpQRSWw";
    let mut port = None;
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if let Some(flags) = arg.strip_prefix('-').filter(|f| !f.is_empty()) {
            // Clustered flags; the first flag that takes an argument consumes
            // the rest of the token or the next token.
            for (pos, flag) in flags.char_indices() {
                if WITH_ARG.contains(flag) {
                    let inline = &flags[pos + flag.len_utf8()..];
                    let value = if inline.is_empty() {
                        i += 1;
                        args.get(i).cloned().unwrap_or_default()
                    } else {
                        inline.to_string()
                    };
                    if flag == 'p' {
                        port = value.parse().ok();
                    } else if flag == 'o' {
                        if let Some(v) = value.strip_prefix("Port=").or_else(|| value.strip_prefix("Port ")) {
                            port = v.trim().parse().ok();
                        }
                    }
                    break;
                }
            }
            i += 1;
            continue;
        }
        let is_url = arg.starts_with("ssh://");
        let dest = arg.strip_prefix("ssh://").unwrap_or(arg);
        let dest = if is_url { dest.split('/').next().unwrap_or(dest) } else { dest };
        let dest = dest.rsplit('@').next().unwrap_or(dest);
        let (host, url_port) = match dest.split_once(':') {
            Some((h, p)) if is_url => (h.to_string(), p.parse().ok()),
            _ => (dest.to_string(), None),
        };
        return Some((host, url_port.or(port)));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn ssh_destination_parsing() {
        assert_eq!(
            ssh_destination(&args("-o SendEnv=GIT_PROTOCOL git@github.com git-upload-pack 'x.git'")),
            Some(("github.com".into(), None))
        );
        assert_eq!(ssh_destination(&args("-p 2200 -T me@localhost true")), Some(("localhost".into(), Some(2200))));
        assert_eq!(ssh_destination(&args("-oPort=2201 localhost")), Some(("localhost".into(), Some(2201))));
        assert_eq!(ssh_destination(&args("ssh://git@example.com:2222/repo")), Some(("example.com".into(), Some(2222))));
        assert_eq!(ssh_destination(&args("-tt -J jump host")), Some(("host".into(), None)));
    }

    #[test]
    fn context_always_fits_in_one_bridge_frame() {
        let huge = ProcInfo {
            pid: 2,
            ppid: 1,
            start_time: 0,
            argv: vec!["\"".repeat(MAX_AGENT_TEXT); 64],
            cwd: Some("\\".repeat(MAX_AGENT_TEXT)),
            reason: Some("r".repeat(MAX_AGENT_TEXT)),
            task_run: Some("t".repeat(MAX_AGENT_TEXT)),
            session_token: None,
        };
        let context = build_context(Some(2), &vec![huge; MAX_AGENT_CHAIN]);
        let message = BeamToClient::AgentSignRequest {
            request_id: "r".repeat(36),
            conn_id: "agent-1".into(),
            message: agent::b64_encode(&[0u8; 4096]),
            context,
        };
        // Worst case: every character needs JSON escaping.
        let framed = serde_json::to_vec(&serde_json::json!({"type": "client_bridge_event", "message": message})).unwrap();
        assert!(framed.len() < beams_protocol::MAX_FRAME_BYTES, "{} bytes", framed.len());
    }

    #[test]
    fn sanitize_strips_escapes_and_bidi() {
        assert_eq!(sanitize("ok\x1b[31mred\u{202e}x\n"), "ok[31mredx");
        assert_eq!(sanitize(&"a".repeat(1000)).len(), MAX_AGENT_TEXT);
    }

    /// A fake bridge registry: one bridge, `answer` decides how the
    /// "workstation" replies to each relayed sign request.
    struct FakeBridges {
        state: AgentState,
        routable: bool,
        sent: Mutex<Vec<BeamToClient>>,
        answer: Box<dyn Fn(&AgentState, &BeamToClient) + Send + Sync>,
    }

    impl AgentBridges for FakeBridges {
        fn agent_state(&self) -> &AgentState {
            &self.state
        }
        fn route_agent(&self, _token: Option<&str>, _task_run: Option<&str>) -> Result<String, String> {
            if self.routable {
                Ok("bridge-1".into())
            } else {
                Err("no managed beams client is connected".into())
            }
        }
        fn send_agent(&self, _bridge_id: &str, message: &BeamToClient) -> bool {
            self.sent.lock().unwrap().push(message.clone());
            (self.answer)(&self.state, message);
            true
        }
    }

    fn roundtrip(conn: &mut UnixStream, body: &[u8]) -> Vec<u8> {
        agent::write_message(conn, body).unwrap();
        agent::read_message(conn).unwrap().unwrap()
    }

    fn fake(routable: bool, answer: impl Fn(&AgentState, &BeamToClient) + Send + Sync + 'static) -> Arc<FakeBridges> {
        Arc::new(FakeBridges {
            state: AgentState::default(),
            routable,
            sent: Mutex::new(Vec::new()),
            answer: Box::new(answer),
        })
    }

    fn spawn_conn(bridges: &Arc<FakeBridges>) -> (UnixStream, thread::JoinHandle<()>) {
        let (client, server) = UnixStream::pair().unwrap();
        let bridges = Arc::clone(bridges);
        let handle = thread::spawn(move || {
            handle_connection(&*bridges, "agent-test".into(), server).unwrap();
        });
        (client, handle)
    }

    #[test]
    fn lists_pushed_keys_relays_signs_and_refuses_key_management() {
        let bridges = fake(true, |state, message| {
            if let BeamToClient::AgentSignRequest { request_id, .. } = message {
                let mut reply = vec![agent::msg::SIGN_RESPONSE];
                reply.extend_from_slice(&[0, 0, 0, 3]);
                reply.extend_from_slice(b"sig");
                state.complete("bridge-1", request_id, &agent::b64_encode(&reply)).unwrap();
            }
        });
        bridges
            .state
            .set_identities("bridge-1", &[AgentKey { blob: agent::b64_encode(b"key"), comment: "k".into() }])
            .unwrap();
        let (mut conn, handle) = spawn_conn(&bridges);

        let ids = agent::parse_identities_answer(&roundtrip(&mut conn, &[agent::msg::REQUEST_IDENTITIES])).unwrap();
        assert_eq!(ids, vec![Identity { key_blob: b"key".to_vec(), comment: "k".into() }]);

        let bind = agent::encode_session_bind(&agent::SessionBind {
            host_key: b"hk".to_vec(),
            session_id: vec![1; 32],
            signature: b"s".to_vec(),
            is_forwarding: false,
        });
        assert_eq!(roundtrip(&mut conn, &bind), agent::success());

        let sign = agent::encode_sign_request(b"key", b"data", 0);
        assert_eq!(agent::parse_sign_response(&roundtrip(&mut conn, &sign)).unwrap(), Some(b"sig".to_vec()));

        assert_eq!(roundtrip(&mut conn, &[agent::msg::REMOVE_ALL_IDENTITIES]), agent::failure());
        assert_eq!(roundtrip(&mut conn, &[agent::msg::LOCK, 0, 0, 0, 0]), agent::failure());

        drop(conn);
        handle.join().unwrap();
        let sent = bridges.sent.lock().unwrap();
        assert!(matches!(&sent[0], BeamToClient::AgentSessionBind { conn_id, .. } if conn_id == "agent-test"));
        match &sent[1] {
            BeamToClient::AgentSignRequest { message, context, .. } => {
                assert_eq!(agent::b64_decode(message).unwrap(), sign);
                // The test process itself is the peer.
                assert_eq!(context.pid, Some(std::process::id()));
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(&sent[2], BeamToClient::AgentConnClosed { .. }));
        assert!(bridges.state.pending.lock().unwrap().is_empty());
    }

    #[test]
    fn without_a_bridge_lists_nothing_and_fails_signs_without_relaying() {
        let bridges = fake(false, |_, _| {});
        let (mut conn, handle) = spawn_conn(&bridges);
        let ids = agent::parse_identities_answer(&roundtrip(&mut conn, &[agent::msg::REQUEST_IDENTITIES])).unwrap();
        assert!(ids.is_empty());
        assert_eq!(roundtrip(&mut conn, &agent::encode_sign_request(b"k", b"d", 0)), agent::failure());
        drop(conn);
        handle.join().unwrap();
        assert!(bridges.sent.lock().unwrap().is_empty());
    }

    #[test]
    fn forgetting_the_bridge_fails_a_waiting_sign_promptly() {
        let bridges = fake(true, |state, message| {
            if matches!(message, BeamToClient::AgentSignRequest { .. }) {
                state.forget_bridge("bridge-1");
            }
        });
        let (mut conn, handle) = spawn_conn(&bridges);
        let started = std::time::Instant::now();
        assert_eq!(roundtrip(&mut conn, &agent::encode_sign_request(b"k", b"d", 0)), agent::failure());
        assert!(started.elapsed() < Duration::from_secs(5));
        drop(conn);
        handle.join().unwrap();
    }

    #[test]
    fn answers_are_only_accepted_from_the_bridge_that_was_asked() {
        let state = AgentState::default();
        let (tx, rx) = mpsc::channel();
        state.pending.lock().unwrap().insert("r1".into(), PendingSign { bridge_id: "b1".into(), reply: tx });
        let failure = agent::b64_encode(&agent::failure());
        assert!(state.complete("b2", "r1", &failure).is_err());
        assert!(state.complete("b1", "r1", &agent::b64_encode(&[agent::msg::SUCCESS])).is_err());
        state.complete("b1", "r1", &failure).unwrap();
        assert_eq!(rx.recv().unwrap(), agent::failure());
        // Late or unknown answers are ignored.
        state.complete("b1", "r1", &failure).unwrap();
    }
}
