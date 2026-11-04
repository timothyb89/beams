//! Phase 0 spike for `SSH_AGENT_PROXY_DESIGN.md`: a filtering SSH agent
//! listener in front of a local upstream agent, logging what it can parse,
//! verify, and recover from `/proc`. No bridge, no prompts: approvals are
//! simulated with `--delay-ms`. Everything is logged as JSON lines on stdout.
//!
//! In the real design the verification half runs in `beamsd`, not here.
//! The spike does it Beam-side only to exercise the parsing and the
//! `known_hosts` matching against real OpenSSH traffic.

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Context;
use beams_protocol::agent::{self, AgentRequest, SignPayload};
use beams_protocol::agent_crypto;
use serde::Serialize;
use serde_json::json;

pub struct SpikeConfig {
    pub listen: PathBuf,
    pub upstream: PathBuf,
    pub delay: Duration,
    /// Delay before answering identity listings (probes server login grace).
    pub delay_identities: Duration,
    pub known_hosts: Option<PathBuf>,
    /// Fingerprints exposed to clients; empty = expose everything upstream has.
    pub allow: BTreeSet<String>,
}

const MAX_CHAIN: usize = 6;
const MAX_TEXT: usize = 300;

pub fn run(config: SpikeConfig) -> anyhow::Result<()> {
    if config.listen.exists() {
        fs::remove_file(&config.listen).with_context(|| format!("removing stale {}", config.listen.display()))?;
    }
    let listener = UnixListener::bind(&config.listen).with_context(|| format!("binding {}", config.listen.display()))?;
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&config.listen, fs::Permissions::from_mode(0o600))?;
    log("listening", json!({"socket": config.listen, "upstream": config.upstream, "delay_ms": config.delay.as_millis()}));

    let config = Arc::new(config);
    let ids = AtomicU64::new(0);
    for conn in listener.incoming() {
        let conn = match conn {
            Ok(conn) => conn,
            Err(e) => {
                log("accept_error", json!({"error": e.to_string()}));
                continue;
            }
        };
        let conn_id = ids.fetch_add(1, Ordering::Relaxed) + 1;
        let config = Arc::clone(&config);
        thread::spawn(move || {
            if let Err(e) = handle(conn_id, conn, &config) {
                log("conn_error", json!({"conn": conn_id, "error": format!("{e:#}")}));
            }
        });
    }
    Ok(())
}

fn log(event: &str, mut fields: serde_json::Value) {
    if let Some(map) = fields.as_object_mut() {
        map.insert("event".into(), event.into());
        map.insert(
            "ts_ms".into(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0)
                .into(),
        );
    }
    println!("{fields}");
}

#[derive(Clone, Debug, Serialize)]
struct ProcInfo {
    pid: u32,
    ppid: u32,
    start_time: u64,
    argv: Vec<String>,
    cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    task_run: Option<String>,
}

#[derive(Debug, Serialize)]
struct PeerContext {
    pid: Option<u32>,
    uid: Option<u32>,
    gather_us: u128,
    chain: Vec<ProcInfo>,
    /// Nearest `SSH_KEY_REASON` up the chain.
    reason: Option<String>,
    task_run: Option<String>,
    /// Hostnames the command lines mention, for hashed known_hosts lookups.
    host_candidates: Vec<String>,
}

struct BindRecord {
    host_key: Vec<u8>,
    session_id: Vec<u8>,
    is_forwarding: bool,
    verified: Result<(), String>,
    host_fingerprint: Option<String>,
    names: Vec<agent_crypto::KnownHostMatch>,
}

fn handle(conn_id: u64, mut conn: UnixStream, config: &SpikeConfig) -> anyhow::Result<()> {
    let started = Instant::now();
    let (pid, uid) = peer_cred(&conn).map(|(p, u)| (Some(p), Some(u))).unwrap_or((None, None));
    let chain = pid.map(process_chain).unwrap_or_default();
    let ctx = PeerContext {
        pid,
        uid,
        gather_us: started.elapsed().as_micros(),
        reason: chain.iter().find_map(|p| p.reason.clone()),
        task_run: chain.iter().find_map(|p| p.task_run.clone()),
        host_candidates: host_candidates(&chain),
        chain,
    };
    log("accept", json!({"conn": conn_id, "context": ctx}));

    let known_hosts = config
        .known_hosts
        .as_ref()
        .and_then(|p| fs::read_to_string(p).ok())
        .unwrap_or_default();
    let mut binds: Vec<BindRecord> = Vec::new();
    let mut upstream: Option<UnixStream> = None;

    while let Some(body) = agent::read_message(&mut conn).context("reading client message")? {
        let request = match agent::parse_request(&body) {
            Ok(request) => request,
            Err(e) => {
                log("malformed", json!({"conn": conn_id, "error": e.to_string()}));
                agent::write_message(&mut conn, &agent::failure())?;
                continue;
            }
        };
        let reply = match request {
            AgentRequest::RequestIdentities => {
                thread::sleep(config.delay_identities);
                let reply = call_upstream(&mut upstream, &config.upstream, &body)?;
                match agent::parse_identities_answer(&reply) {
                    Ok(ids) => {
                        let total = ids.len();
                        let shown: Vec<_> = ids
                            .into_iter()
                            .filter(|id| {
                                config.allow.is_empty()
                                    || agent_crypto::fingerprint(&id.key_blob).is_some_and(|fp| config.allow.contains(&fp))
                            })
                            .collect();
                        log(
                            "identities",
                            json!({"conn": conn_id, "upstream_count": total, "exposed": shown.iter().map(|i| agent_crypto::fingerprint(&i.key_blob)).collect::<Vec<_>>()}),
                        );
                        agent::encode_identities_answer(&shown)
                    }
                    Err(e) => {
                        log("identities_error", json!({"conn": conn_id, "error": e.to_string()}));
                        agent::failure()
                    }
                }
            }
            AgentRequest::SessionBind(bind) => {
                let verified = agent_crypto::verify_session_bind(&bind);
                let names = if verified.is_ok() {
                    agent_crypto::known_host_names(&known_hosts, &bind.host_key, &ctx.host_candidates)
                } else {
                    Vec::new()
                };
                let record = BindRecord {
                    host_fingerprint: agent_crypto::fingerprint(&bind.host_key),
                    host_key: bind.host_key,
                    session_id: bind.session_id,
                    is_forwarding: bind.is_forwarding,
                    verified,
                    names,
                };
                log(
                    "session_bind",
                    json!({
                        "conn": conn_id,
                        "session_id": agent::hex(&record.session_id),
                        "host_key_type": agent::key_type(&record.host_key),
                        "host_fingerprint": record.host_fingerprint,
                        "is_forwarding": record.is_forwarding,
                        "verified": record.verified.as_ref().err().map_or(json!(true), |e| json!(e)),
                        "known_hosts": record.names.iter().map(|m| json!({"name": m.name, "hashed": m.hashed})).collect::<Vec<_>>(),
                        "bind_index": binds.len(),
                    }),
                );
                binds.push(record);
                agent::success()
            }
            AgentRequest::Sign { key_blob, data, flags } => {
                let key_fp = agent_crypto::fingerprint(&key_blob);
                let allowed = config.allow.is_empty() || key_fp.as_ref().is_some_and(|fp| config.allow.contains(fp));
                let payload = agent::parse_sign_payload(&data);
                let (summary, decision) = describe_sign(&payload, &key_blob, &binds);
                let permit = allowed && decision.is_ok();
                log(
                    "sign_request",
                    json!({
                        "conn": conn_id,
                        "key": key_fp,
                        "key_allowed": allowed,
                        "flags": flags,
                        "payload": summary,
                        "decision": decision.as_ref().map_or_else(|e| json!({"deny": e}), |v| json!({"ok": v})),
                        "reason_claimed": ctx.reason,
                        "task_run_claimed": ctx.task_run,
                    }),
                );
                if !permit {
                    agent::failure()
                } else {
                    // Stand-in for the human approval round trip.
                    thread::sleep(config.delay);
                    let t = Instant::now();
                    let reply = call_upstream(&mut upstream, &config.upstream, &body)?;
                    let ok = matches!(agent::parse_sign_response(&reply), Ok(Some(_)));
                    log(
                        "sign_result",
                        json!({"conn": conn_id, "signed": ok, "upstream_ms": t.elapsed().as_millis(), "delay_ms": config.delay.as_millis()}),
                    );
                    reply
                }
            }
            AgentRequest::Extension { ref name } => {
                log("refused", json!({"conn": conn_id, "request": request.label(), "name": name}));
                agent::failure()
            }
            AgentRequest::Other { .. } => {
                log("refused", json!({"conn": conn_id, "request": request.label()}));
                agent::failure()
            }
        };
        agent::write_message(&mut conn, &reply).context("writing reply")?;
    }
    log("close", json!({"conn": conn_id, "binds": binds.len(), "lifetime_ms": started.elapsed().as_millis()}));
    Ok(())
}

/// Summarizes a sign payload and decides what the policy engine could prove.
fn describe_sign(
    payload: &SignPayload,
    key_blob: &[u8],
    binds: &[BindRecord],
) -> (serde_json::Value, Result<serde_json::Value, String>) {
    match payload {
        SignPayload::UserAuth(auth) => {
            let summary = json!({
                "kind": "auth",
                "user": auth.user,
                "service": auth.service,
                "method": auth.method,
                "algorithm": auth.algorithm,
                "session_id": agent::hex(&auth.session_id),
                "hostbound": auth.host_key.is_some(),
            });
            if auth.key_blob != key_blob {
                return (summary, Err("signed data names a different key than the request".into()));
            }
            // The session id must match a verified bind on this connection;
            // the last matching one wins (ProxyJump chains rebind).
            let Some(bind) = binds.iter().rev().find(|b| b.session_id == auth.session_id) else {
                return (summary, Ok(json!({"host": "unverified", "why": "no session-bind for this session id"})));
            };
            if let Err(e) = &bind.verified {
                return (summary, Ok(json!({"host": "unverified", "why": e})));
            }
            if let Some(hk) = &auth.host_key {
                if hk != &bind.host_key {
                    return (summary, Err("hostbound host key differs from session-bind host key".into()));
                }
            }
            let host = match bind.names.first() {
                Some(m) => json!({"verified": m.name, "via": if m.hashed {"hashed known_hosts"} else {"known_hosts"}}),
                None => json!({"verified_key_unknown_host": bind.host_fingerprint}),
            };
            (summary, Ok(json!({"host": host, "user": auth.user, "forwarding": bind.is_forwarding})))
        }
        SignPayload::SshSig { namespace, hash_alg } => (
            json!({"kind": "sshsig", "namespace": namespace, "hash_alg": hash_alg}),
            Ok(json!({"sshsig_namespace": namespace})),
        ),
        SignPayload::Unknown { reason } => (json!({"kind": "unknown", "why": reason}), Err("unrecognized signature payload".into())),
    }
}

fn call_upstream(upstream: &mut Option<UnixStream>, path: &Path, body: &[u8]) -> anyhow::Result<Vec<u8>> {
    if upstream.is_none() {
        *upstream = Some(UnixStream::connect(path).with_context(|| format!("connecting upstream {}", path.display()))?);
    }
    let stream = upstream.as_mut().expect("just set");
    agent::write_message(stream, body).context("writing upstream")?;
    agent::read_message(stream)
        .context("reading upstream")?
        .ok_or_else(|| anyhow::anyhow!("upstream closed"))
}

fn peer_cred(conn: &UnixStream) -> Option<(u32, u32)> {
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

fn process_chain(pid: u32) -> Vec<ProcInfo> {
    let mut out = Vec::new();
    let mut next = pid;
    let me = std::process::id();
    while out.len() < MAX_CHAIN && next > 1 && next != me {
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
        .map(|raw| raw.split(|b| *b == 0).filter(|s| !s.is_empty()).map(|s| sanitize(&String::from_utf8_lossy(s))).collect())
        .unwrap_or_default();
    let cwd = fs::read_link(base.join("cwd")).ok().map(|p| sanitize(&p.display().to_string()));
    let (mut reason, mut task_run) = (None, None);
    if let Ok(environ) = fs::read(base.join("environ")) {
        for var in environ.split(|b| *b == 0) {
            let var = String::from_utf8_lossy(var);
            if let Some(v) = var.strip_prefix("SSH_KEY_REASON=") {
                reason = Some(sanitize(v));
            } else if let Some(v) = var.strip_prefix("BEAMS_TASK_RUN=") {
                task_run = Some(sanitize(v));
            }
        }
    }
    Some(ProcInfo { pid, ppid, start_time, argv, cwd, reason, task_run })
}

/// Drops control characters (incl. ESC, so no ANSI/OSC) and bidi overrides,
/// and bounds the length.
fn sanitize(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
        .take(MAX_TEXT)
        .collect();
    cleaned
}

/// Destination hosts mentioned by `ssh` invocations in the chain, spelled the
/// way `known_hosts` would. Untrusted but harmless: a candidate only matters
/// if the user's own known_hosts confirms it for the verified host key.
fn host_candidates(chain: &[ProcInfo]) -> Vec<String> {
    let mut out = Vec::new();
    for info in chain {
        let Some(argv0) = info.argv.first() else { continue };
        if Path::new(argv0).file_name().and_then(|n| n.to_str()) != Some("ssh") {
            continue;
        }
        if let Some((host, port)) = ssh_destination(&info.argv[1..]) {
            let name = agent_crypto::known_hosts_name(&host, port);
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
    fn sanitize_strips_escapes_and_bidi() {
        assert_eq!(sanitize("ok\x1b[31mred\u{202e}x\n"), "ok[31mredx");
        assert_eq!(sanitize(&"a".repeat(1000)).len(), MAX_TEXT);
    }
}
