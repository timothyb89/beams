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

use crate::agentproxy::{host_candidates, peer_cred, process_chain, ProcInfo};

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
                        "raw": agent::b64_encode(&body),
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
