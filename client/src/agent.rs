//! Workstation half of the SSH agent proxy (see `SSH_AGENT_PROXY_DESIGN.md`).
//!
//! The Beam relays agent traffic over its bridge; everything that decides
//! anything lives here:
//!
//! - which keys a Beam may see (`[ssh_agent] allowed_keys` in
//!   `config.toml`; empty means the proxy is off);
//! - what a sign request actually is, parsed from the raw bytes the
//!   workstation agent would sign (never from the Beam's description);
//! - which host a login is for, from the session-bind the Beam forwarded,
//!   verified against the host key's signature and the user's own
//!   `known_hosts`.
//!
//! The Beam's process context (command lines, `SSH_KEY_REASON`, task) is
//! carried along for display only.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::ErrorKind;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context};
use beams_protocol::agent::{self, AgentRequest, Identity, SignPayload};
use beams_protocol::{agent_crypto, AgentContext, AgentKey, MAX_AGENT_TEXT};
use serde::{Deserialize, Serialize};

/// Default time to approve a request. Servers usually allow ~120s before
/// authentication completes, so much longer than this is pointless.
pub const DEFAULT_PROMPT_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_PROMPT_TIMEOUT: Duration = Duration::from_secs(110);
/// Bounds on Beam-driven state.
const MAX_BINDS_PER_CONN: usize = 8;
const MAX_CONNS_PER_BEAM: usize = 256;
pub const MAX_PENDING_PER_BEAM: usize = 8;
/// Talking to the local agent; a hardware key waiting for a touch can take
/// a while.
const LOCAL_AGENT_TIMEOUT: Duration = Duration::from_secs(60);

/// Wall-clock milliseconds since the Unix epoch (prompt expiry times).
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `[ssh_agent]` in `config.toml`.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// SHA256 fingerprints (`SHA256:...`, as printed by `ssh-add -l`) a Beam
    /// may use. Empty disables the proxy.
    #[serde(default)]
    pub allowed_keys: Vec<String>,
    /// Seconds to approve a request before it's denied.
    #[serde(default)]
    pub prompt_timeout: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    ssh_agent: Config,
}

impl Config {
    pub fn path() -> anyhow::Result<PathBuf> {
        Ok(crate::clientd::config_dir()?.join("beams-client/config.toml"))
    }

    /// Missing file: the default (disabled). A broken file is an error, so
    /// a typo can't silently leave the proxy in an unexpected state.
    pub fn load() -> anyhow::Result<Self> {
        Self::load_from(&Self::path()?)
    }

    pub fn load_from(path: &Path) -> anyhow::Result<Self> {
        match fs::read_to_string(path) {
            Ok(text) => Self::parse(&text).with_context(|| format!("parsing {}", path.display())),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
        }
    }

    fn parse(text: &str) -> anyhow::Result<Self> {
        // Other sections belong to other features.
        let file: ConfigFile = toml::from_str(text)?;
        Ok(file.ssh_agent)
    }

    pub fn enabled(&self) -> bool {
        !self.allowed_keys.is_empty()
    }

    pub fn prompt_timeout(&self) -> Duration {
        self.prompt_timeout
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_PROMPT_TIMEOUT)
            .clamp(Duration::from_secs(5), MAX_PROMPT_TIMEOUT)
    }

    pub fn allows(&self, key_blob: &[u8]) -> bool {
        agent_crypto::fingerprint(key_blob).is_some_and(|fp| self.allowed_keys.iter().any(|k| k == &fp))
    }
}

/// `beams agent-keys`: the keys in this terminal's agent, marked by
/// whether Beams may use them, and how to allow one.
pub fn print_keys() -> anyhow::Result<u8> {
    let path = Config::path()?;
    let config = Config::load()?;
    let Some(sock) = std::env::var("SSH_AUTH_SOCK").ok().filter(|s| !s.is_empty()) else {
        bail!("SSH_AUTH_SOCK is not set: start an ssh-agent and add your key first");
    };
    let keys = local_identities(&sock)?;
    if keys.is_empty() {
        println!("The agent at {sock} has no keys; add one with `ssh-add`.");
    }
    for key in &keys {
        let fingerprint = agent_crypto::fingerprint(&key.key_blob).unwrap_or_else(|| "(unparsed key)".to_string());
        let mark = if config.allows(&key.key_blob) { "allowed" } else { "-" };
        println!("{mark:8} {fingerprint}  {}", clean(&key.comment));
    }
    for allowed in &config.allowed_keys {
        if !keys.iter().any(|k| agent_crypto::fingerprint(&k.key_blob).as_deref() == Some(allowed.as_str())) {
            println!("missing  {allowed}  (allowed, but not in this agent)");
        }
    }
    println!();
    if config.enabled() {
        println!("Beams may ask to use the allowed keys; each use needs your approval in `beams ssh`.");
    } else {
        println!("The SSH agent proxy is off. To let Beams ask to use a key, add to {}:", path.display());
        println!();
        println!("  [ssh_agent]");
        println!("  allowed_keys = [\"SHA256:...\"]");
    }
    Ok(0)
}

fn connect_local(sock: &str) -> anyhow::Result<UnixStream> {
    let stream = UnixStream::connect(sock).with_context(|| format!("connecting to the local agent at {sock}"))?;
    stream.set_read_timeout(Some(LOCAL_AGENT_TIMEOUT))?;
    stream.set_write_timeout(Some(LOCAL_AGENT_TIMEOUT))?;
    Ok(stream)
}

fn call_local(sock: &str, body: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut stream = connect_local(sock)?;
    agent::write_message(&mut stream, body).context("writing to the local agent")?;
    agent::read_message(&mut stream)
        .context("reading from the local agent")?
        .context("the local agent closed the connection")
}

/// Keys the local agent holds.
pub fn local_identities(sock: &str) -> anyhow::Result<Vec<Identity>> {
    let reply = call_local(sock, &[agent::msg::REQUEST_IDENTITIES])?;
    agent::parse_identities_answer(&reply).map_err(|e| anyhow::anyhow!("local agent: {e}"))
}

/// The allowlisted subset of the local agent's keys, as pushed to a Beam.
pub fn allowed_keys(config: &Config, sock: &str) -> anyhow::Result<Vec<AgentKey>> {
    Ok(local_identities(sock)?
        .into_iter()
        .filter(|id| config.allows(&id.key_blob))
        .map(|id| AgentKey { blob: agent::b64_encode(&id.key_blob), comment: id.comment })
        .collect())
}

/// Forwards an approved sign request to the local agent and returns its
/// reply, which must be a signature.
pub fn local_sign(sock: &str, request: &[u8]) -> anyhow::Result<Vec<u8>> {
    let reply = call_local(sock, request)?;
    match agent::parse_sign_response(&reply) {
        Ok(Some(_)) => Ok(reply),
        Ok(None) => bail!("the local agent refused to sign (is the key still loaded? try `ssh-add -l`)"),
        Err(e) => bail!("unexpected reply from the local agent: {e}"),
    }
}

/// A session-bind as received and checked here.
#[derive(Clone, Debug)]
pub struct VerifiedBind {
    pub host_key: Vec<u8>,
    pub session_id: Vec<u8>,
    pub is_forwarding: bool,
    pub verified: Result<(), String>,
}

/// Session binds per Beam-side agent connection. Cleared per Beam whenever
/// its bridge reconnects (the Beam's connection ids restart with it).
#[derive(Default)]
pub struct BindStore {
    by_beam: HashMap<String, BTreeMap<String, Vec<VerifiedBind>>>,
}

impl BindStore {
    /// Parses and verifies a forwarded session-bind message.
    pub fn add(&mut self, beam_uuid: &str, conn_id: &str, message: &str) -> Result<(), String> {
        let body = agent::b64_decode(message).map_err(|e| e.to_string())?;
        let AgentRequest::SessionBind(bind) = agent::parse_request(&body).map_err(|e| e.to_string())? else {
            return Err("not a session-bind message".to_string());
        };
        let conns = self.by_beam.entry(beam_uuid.to_string()).or_default();
        if !conns.contains_key(conn_id) && conns.len() >= MAX_CONNS_PER_BEAM {
            // Forget the oldest-named connection rather than growing forever.
            if let Some(oldest) = conns.keys().next().cloned() {
                conns.remove(&oldest);
            }
        }
        let binds = conns.entry(conn_id.to_string()).or_default();
        if binds.len() >= MAX_BINDS_PER_CONN {
            return Err("too many session binds on one agent connection".to_string());
        }
        binds.push(VerifiedBind {
            verified: agent_crypto::verify_session_bind(&bind),
            host_key: bind.host_key,
            session_id: bind.session_id,
            is_forwarding: bind.is_forwarding,
        });
        Ok(())
    }

    pub fn close(&mut self, beam_uuid: &str, conn_id: &str) {
        if let Some(conns) = self.by_beam.get_mut(beam_uuid) {
            conns.remove(conn_id);
        }
    }

    pub fn clear_beam(&mut self, beam_uuid: &str) {
        self.by_beam.remove(beam_uuid);
    }

    pub fn binds(&self, beam_uuid: &str, conn_id: &str) -> Vec<VerifiedBind> {
        self.by_beam
            .get(beam_uuid)
            .and_then(|conns| conns.get(conn_id))
            .cloned()
            .unwrap_or_default()
    }
}

/// What the user is asked to approve. Built only from facts checked here,
/// plus the Beam's claims kept separately in `claimed`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct AgentPrompt {
    pub key_fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_comment: Option<String>,
    pub operation: AgentOperation,
    #[serde(default)]
    pub claimed: Claimed,
    /// When the request is denied unless approved (Unix ms).
    pub expires_at_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentOperation {
    /// Log in to an SSH server.
    Login { user: String, host: HostCheck },
    /// An `SSHSIG` signature (`git commit -S`, `ssh-keygen -Y sign`).
    Signature { namespace: String },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum HostCheck {
    /// Session bound to a host key that the user's `known_hosts` names.
    Verified { name: String },
    /// Session bound to a verified host key that `known_hosts` doesn't name.
    UnknownKey { fingerprint: String },
    /// No usable session-bind: the host isn't known at all.
    Unverified { why: String },
}

/// The Beam's (unverified) description of who asked.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Claimed {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_run: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
}

impl AgentPrompt {
    /// One line for toasts and list rows.
    pub fn summary(&self) -> String {
        match &self.operation {
            AgentOperation::Login { user, host } => match host {
                HostCheck::Verified { name } => format!("SSH key: {name} as {user}"),
                HostCheck::UnknownKey { .. } => format!("SSH key: unknown host as {user}"),
                HostCheck::Unverified { .. } => match &self.claimed.host {
                    Some(host) => format!("SSH key: {host}? (unverified) as {user}"),
                    None => format!("SSH key: unverified host as {user}"),
                },
            },
            AgentOperation::Signature { namespace } => format!("SSH key: sign ({namespace})"),
        }
    }

    /// Detail lines `(text, warn)`: verified facts first, then the Beam's
    /// claims, labeled as such.
    pub fn describe(&self) -> Vec<(String, bool)> {
        let key = match &self.key_comment {
            Some(comment) if !comment.is_empty() => format!("{comment} ({})", self.key_fingerprint),
            _ => self.key_fingerprint.clone(),
        };
        let mut lines = Vec::new();
        match &self.operation {
            AgentOperation::Login { user, host } => {
                let (text, warn) = match host {
                    HostCheck::Verified { name } => (format!("Sign in to {name} as {user} (host verified)."), false),
                    HostCheck::UnknownKey { fingerprint } => (
                        format!("Sign in as {user} to a host not in your known_hosts (host key {fingerprint})."),
                        true,
                    ),
                    HostCheck::Unverified { why } => {
                        (format!("Sign in as {user} to an UNVERIFIED host ({why})."), true)
                    }
                };
                lines.push((text, warn));
            }
            AgentOperation::Signature { namespace } => {
                lines.push((format!("Create a signature in namespace {namespace:?}."), namespace != "git"));
            }
        }
        lines.push((format!("Key: {key}."), false));
        let claimed = &self.claimed;
        let reported: Vec<String> = [
            claimed.reason.as_ref().map(|r| format!("Reason: {r}")),
            claimed.command.as_ref().map(|c| format!("Command: {c}")),
            claimed.parent.as_ref().map(|p| format!("Parent: {p}")),
            claimed.cwd.as_ref().map(|c| format!("In: {c}")),
            claimed.task_run.as_ref().map(|t| format!("Task: {t}")),
        ]
        .into_iter()
        .flatten()
        .collect();
        if reported.is_empty() {
            lines.push(("The Beam reported nothing about who asked.".to_string(), false));
        } else {
            lines.push(("Reported by the Beam (unverified):".to_string(), false));
            lines.extend(reported.into_iter().map(|line| (format!("  {line}"), false)));
        }
        lines
    }
}

/// Decides what a forwarded sign request is, or why it must be refused
/// without asking. `known_hosts` is the concatenated text of the user's
/// known-hosts files.
pub fn evaluate(
    config: &Config,
    message: &str,
    binds: &[VerifiedBind],
    context: &AgentContext,
    known_hosts: &str,
    local_keys: &[Identity],
    expires_at_ms: u64,
) -> Result<(Vec<u8>, AgentPrompt), String> {
    let body = agent::b64_decode(message).map_err(|e| format!("malformed request: {e}"))?;
    let AgentRequest::Sign { key_blob, data, .. } =
        agent::parse_request(&body).map_err(|e| format!("malformed request: {e}"))?
    else {
        return Err("not a sign request".to_string());
    };
    if !config.allows(&key_blob) {
        return Err("key is not in ssh_agent.allowed_keys".to_string());
    }
    let key_fingerprint = agent_crypto::fingerprint(&key_blob).unwrap_or_default();
    let key_comment = local_keys.iter().find(|id| id.key_blob == key_blob).map(|id| clean(&id.comment));

    let claimed = claimed_context(context);
    let operation = match agent::parse_sign_payload(&data) {
        SignPayload::UserAuth(auth) => {
            if auth.key_blob != key_blob {
                return Err("signed data names a different key than the request".to_string());
            }
            if auth.service != "ssh-connection" {
                return Err(format!("unexpected service {:?}", auth.service));
            }
            let host = match binds.iter().rev().find(|b| b.session_id == auth.session_id) {
                None => HostCheck::Unverified { why: "no session-bind for this session".to_string() },
                Some(bind) => match &bind.verified {
                    Err(e) => HostCheck::Unverified { why: format!("session-bind failed verification: {e}") },
                    Ok(()) if auth.host_key.as_ref().is_some_and(|hk| hk != &bind.host_key) => {
                        return Err("host-bound request names a different host key than its session-bind".into());
                    }
                    Ok(()) if bind.is_forwarding => HostCheck::Unverified {
                        why: "session-bind is for a forwarded agent".to_string(),
                    },
                    Ok(()) => {
                        let candidates: Vec<String> =
                            context.host_candidates.iter().map(|c| clean(c)).take(8).collect();
                        match agent_crypto::known_host_names(known_hosts, &bind.host_key, &candidates).first() {
                            Some(found) => HostCheck::Verified { name: found.name.clone() },
                            None => HostCheck::UnknownKey {
                                fingerprint: agent_crypto::fingerprint(&bind.host_key).unwrap_or_default(),
                            },
                        }
                    }
                },
            };
            AgentOperation::Login { user: clean(&auth.user), host }
        }
        SignPayload::SshSig { namespace, .. } => AgentOperation::Signature { namespace: clean(&namespace) },
        SignPayload::Unknown { reason } => return Err(format!("unrecognized signature payload ({reason})")),
    };
    Ok((body, AgentPrompt { key_fingerprint, key_comment, operation, claimed, expires_at_ms }))
}

/// The user's known-hosts files, concatenated (missing ones skipped).
pub fn read_known_hosts() -> String {
    let mut paths = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        paths.push(home.join(".ssh/known_hosts"));
        paths.push(home.join(".ssh/known_hosts2"));
    }
    paths.push(PathBuf::from("/etc/ssh/ssh_known_hosts"));
    paths.push(PathBuf::from("/etc/ssh/ssh_known_hosts2"));
    let mut text = String::new();
    for path in paths {
        if let Ok(contents) = fs::read_to_string(path) {
            text.push_str(&contents);
            text.push('\n');
        }
    }
    text
}

fn claimed_context(context: &AgentContext) -> Claimed {
    let command = context.chain.first().map(|p| summarize_command(&p.argv));
    // The first parent that isn't a plain `sh -c` wrapper (GIT_SSH_COMMAND).
    let parent = context
        .chain
        .iter()
        .skip(1)
        .find(|p| !is_shell_wrapper(&p.argv))
        .map(|p| summarize_command(&p.argv));
    Claimed {
        reason: context.reason.as_deref().map(clean).filter(|s| !s.is_empty()),
        command: command.filter(|s| !s.is_empty()),
        parent: parent.filter(|s| !s.is_empty()),
        cwd: context.chain.first().and_then(|p| p.cwd.as_deref()).map(clean),
        task_run: context.task_run.as_deref().map(clean),
        host: context.host_candidates.first().map(|c| clean(c)),
    }
}

/// Requests that should be decided together: same key, same operation
/// (including the host verdict), from the same reported parent process -
/// e.g. the several connections of one `git fetch --recurse-submodules`.
/// The parent comes from the Beam's (forgeable) context, but folding can
/// only merge requests whose verified prompt is identical anyway.
pub fn fold_key(prompt: &AgentPrompt, context: &AgentContext) -> Option<String> {
    let parent = context.chain.iter().skip(1).find(|p| !is_shell_wrapper(&p.argv))?;
    let operation = serde_json::to_string(&prompt.operation).ok()?;
    Some(format!("{}|{}|{}", prompt.key_fingerprint, operation, parent.pid))
}

fn is_shell_wrapper(argv: &[String]) -> bool {
    matches!(argv, [sh, flag, ..] if Path::new(sh).file_name().is_some_and(|n| n == "sh" || n == "bash") && flag == "-c")
}

/// A short, readable command line: the program's base name, and for `ssh`
/// only the destination and remote command (the options are noise).
fn summarize_command(argv: &[String]) -> String {
    let Some((program, args)) = argv.split_first() else {
        return String::new();
    };
    let name = Path::new(program)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| program.clone());
    let kept: Vec<&String> = if name == "ssh" { ssh_operands(args) } else { args.iter().collect() };
    let mut out = clean(&name);
    for arg in kept {
        out.push(' ');
        out.push_str(&clean(arg));
        if out.chars().count() >= MAX_AGENT_TEXT {
            break;
        }
    }
    out.chars().take(MAX_AGENT_TEXT).collect()
}

fn ssh_operands(args: &[String]) -> Vec<&String> {
    const WITH_ARG: &str = "BbcDEeFIiJLlmOoPpQRSWw";
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if !out.is_empty() {
            out.push(arg);
        } else if let Some(flags) = arg.strip_prefix('-').filter(|f| !f.is_empty()) {
            if let Some(pos) = flags.find(|c| WITH_ARG.contains(c)) {
                if pos + 1 == flags.len() {
                    i += 1;
                }
            }
        } else {
            out.push(arg);
        }
        i += 1;
    }
    out
}

/// Beam text is re-sanitized here regardless of what the Beam did: no
/// control characters (so no terminal escapes), no bidi overrides, bounded.
pub fn clean(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() && !matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
        .take(MAX_AGENT_TEXT)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use beams_protocol::agent::{SessionBind, UserAuthRequest, METHOD_PUBLICKEY_HOSTBOUND};
    use beams_protocol::AgentProcess;
    use ssh_key::{rand_core::OsRng, Algorithm, PrivateKey};

    fn key() -> PrivateKey {
        PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap()
    }

    fn blob(key: &PrivateKey) -> Vec<u8> {
        key.public_key().to_bytes().unwrap()
    }

    fn config_for(key: &PrivateKey) -> Config {
        Config { allowed_keys: vec![agent_crypto::fingerprint(&blob(key)).unwrap()], prompt_timeout: None }
    }

    fn signed_bind(host: &PrivateKey, session_id: &[u8]) -> String {
        use signature::Signer;
        let sig: ssh_key::Signature = host.key_data().try_sign(session_id).unwrap();
        let bind = SessionBind {
            host_key: blob(host),
            session_id: session_id.to_vec(),
            signature: Vec::<u8>::try_from(sig).unwrap(),
            is_forwarding: false,
        };
        agent::b64_encode(&agent::encode_session_bind(&bind))
    }

    fn login_request(user_key: &PrivateKey, session_id: &[u8], host: Option<&PrivateKey>) -> String {
        let data = agent::encode_userauth_data(&UserAuthRequest {
            session_id: session_id.to_vec(),
            user: "git".into(),
            service: "ssh-connection".into(),
            method: METHOD_PUBLICKEY_HOSTBOUND.into(),
            algorithm: "ssh-ed25519".into(),
            key_blob: blob(user_key),
            host_key: Some(blob(host.unwrap_or(user_key))),
        });
        agent::b64_encode(&agent::encode_sign_request(&blob(user_key), &data, 0))
    }

    fn context(argv: &[&str], candidates: &[&str]) -> AgentContext {
        AgentContext {
            pid: Some(10),
            chain: vec![
                AgentProcess { pid: 10, argv: argv.iter().map(|s| s.to_string()).collect(), cwd: Some("/w".into()) },
                AgentProcess { pid: 9, argv: vec!["/bin/sh".into(), "-c".into(), "ssh ...".into()], cwd: None },
                AgentProcess { pid: 8, argv: vec!["git".into(), "clone".into(), "x".into()], cwd: None },
            ],
            reason: Some("Clone \x1b[2Jthe repo".into()),
            task_run: None,
            host_candidates: candidates.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn verified_login_names_the_host_and_keeps_claims_separate() {
        let (user, host) = (key(), key());
        let config = config_for(&user);
        let sid = [5u8; 32];
        let mut store = BindStore::default();
        store.add("beam", "c1", &signed_bind(&host, &sid)).unwrap();
        let known_hosts = format!("github.com {}\n", host.public_key().to_openssh().unwrap());
        let ctx = context(&["/usr/bin/ssh", "-o", "SendEnv=GIT_PROTOCOL", "git@github.com", "git-upload-pack", "'o/r.git'"], &["github.com"]);

        let (_, prompt) = evaluate(&config, &login_request(&user, &sid, Some(&host)), &store.binds("beam", "c1"), &ctx, &known_hosts, &[], 0).unwrap();
        assert_eq!(
            prompt.operation,
            AgentOperation::Login { user: "git".into(), host: HostCheck::Verified { name: "github.com".into() } }
        );
        assert_eq!(prompt.claimed.command.as_deref(), Some("ssh git@github.com git-upload-pack 'o/r.git'"));
        assert_eq!(prompt.claimed.parent.as_deref(), Some("git clone x"));
        assert_eq!(prompt.claimed.reason.as_deref(), Some("Clone [2Jthe repo"));
        assert_eq!(prompt.summary(), "SSH key: github.com as git");
    }

    #[test]
    fn missing_or_forged_binds_never_verify() {
        let (user, host, impostor) = (key(), key(), key());
        let config = config_for(&user);
        let sid = [5u8; 32];
        let known_hosts = format!("github.com {}\n", host.public_key().to_openssh().unwrap());
        let ctx = context(&["ssh", "git@github.com"], &["github.com"]);

        // No bind at all.
        let (_, p) = evaluate(&config, &login_request(&user, &sid, Some(&host)), &[], &ctx, &known_hosts, &[], 0).unwrap();
        assert!(matches!(p.operation, AgentOperation::Login { host: HostCheck::Unverified { .. }, .. }));

        // A bind claiming github's key but signed by someone else.
        let mut store = BindStore::default();
        let forged = {
            use signature::Signer;
            let sig: ssh_key::Signature = impostor.key_data().try_sign(&sid).unwrap();
            agent::b64_encode(&agent::encode_session_bind(&SessionBind {
                host_key: blob(&host),
                session_id: sid.to_vec(),
                signature: Vec::<u8>::try_from(sig).unwrap(),
                is_forwarding: false,
            }))
        };
        store.add("beam", "c1", &forged).unwrap();
        let (_, p) = evaluate(&config, &login_request(&user, &sid, Some(&host)), &store.binds("beam", "c1"), &ctx, &known_hosts, &[], 0).unwrap();
        assert!(matches!(p.operation, AgentOperation::Login { host: HostCheck::Unverified { .. }, .. }));

        // A real bind for a host that isn't in known_hosts.
        let mut store = BindStore::default();
        store.add("beam", "c1", &signed_bind(&impostor, &sid)).unwrap();
        let (_, p) = evaluate(&config, &login_request(&user, &sid, Some(&impostor)), &store.binds("beam", "c1"), &ctx, &known_hosts, &[], 0).unwrap();
        assert!(matches!(p.operation, AgentOperation::Login { host: HostCheck::UnknownKey { .. }, .. }));

        // A hostbound request naming a different host key than its bind.
        let mut store = BindStore::default();
        store.add("beam", "c1", &signed_bind(&host, &sid)).unwrap();
        assert!(evaluate(&config, &login_request(&user, &sid, Some(&impostor)), &store.binds("beam", "c1"), &ctx, &known_hosts, &[], 0).is_err());
    }

    #[test]
    fn disallowed_keys_and_opaque_payloads_are_refused() {
        let (user, other) = (key(), key());
        let config = config_for(&user);
        let ctx = AgentContext::default();
        let other_req = login_request(&other, &[1; 32], None);
        assert!(evaluate(&config, &other_req, &[], &ctx, "", &[], 0).unwrap_err().contains("allowed_keys"));
        let opaque = agent::b64_encode(&agent::encode_sign_request(&blob(&user), b"just some bytes", 0));
        assert!(evaluate(&config, &opaque, &[], &ctx, "", &[], 0).unwrap_err().contains("unrecognized"));
        assert!(evaluate(&Config::default(), &other_req, &[], &ctx, "", &[], 0).is_err());
    }

    #[test]
    fn sshsig_requests_name_their_namespace() {
        let user = key();
        let mut data = b"SSHSIG".to_vec();
        for part in [&b"git"[..], b"", b"sha512", &[7; 64]] {
            data.extend_from_slice(&(part.len() as u32).to_be_bytes());
            data.extend_from_slice(part);
        }
        let request = agent::b64_encode(&agent::encode_sign_request(&blob(&user), &data, 0));
        let (_, p) = evaluate(&config_for(&user), &request, &[], &AgentContext::default(), "", &[], 0).unwrap();
        assert_eq!(p.operation, AgentOperation::Signature { namespace: "git".into() });
        assert_eq!(p.summary(), "SSH key: sign (git)");
    }

    #[test]
    fn config_parses_and_rejects_typos() {
        let config = Config::parse("[ssh_agent]\nallowed_keys = [\"SHA256:x\"]\nprompt_timeout = 30\n\n[other]\nx = 1\n").unwrap();
        assert_eq!(config.allowed_keys, vec!["SHA256:x".to_string()]);
        assert_eq!(config.prompt_timeout(), Duration::from_secs(30));
        assert!(Config::parse("[ssh_agent]\nallowed_key = []\n").is_err());
        assert!(!Config::parse("").unwrap().enabled());
        assert_eq!(Config::parse("[ssh_agent]\nprompt_timeout = 9999\n").unwrap().prompt_timeout(), MAX_PROMPT_TIMEOUT);
    }

    #[test]
    fn bind_store_is_bounded_and_scoped() {
        let host = key();
        let mut store = BindStore::default();
        for i in 0..MAX_BINDS_PER_CONN {
            store.add("beam", "c1", &signed_bind(&host, &[i as u8 + 1; 32])).unwrap();
        }
        assert!(store.add("beam", "c1", &signed_bind(&host, &[99; 32])).is_err());
        assert!(store.binds("other", "c1").is_empty());
        store.close("beam", "c1");
        assert!(store.binds("beam", "c1").is_empty());
        assert!(store.add("beam", "c2", &agent::b64_encode(&[agent::msg::REQUEST_IDENTITIES])).is_err());
    }
}
