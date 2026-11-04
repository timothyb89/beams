//! SSH agent wire protocol (draft-ietf-sshm-ssh-agent), just enough for the
//! Beam-side agent listener and the workstation policy engine described in
//! `SSH_AGENT_PROXY_DESIGN.md`.
//!
//! Everything here is pure parsing/encoding of untrusted bytes: no I/O policy
//! and no cryptography. Signature verification and `known_hosts` lookups live
//! in [`crate::agent_crypto`] behind the `agent-crypto` feature, so a crate
//! that only relays messages doesn't pull in RSA/ECDSA implementations.

use std::fmt;
use std::io::{self, Read, Write};

/// Message type numbers used here.
pub mod msg {
    pub const FAILURE: u8 = 5;
    pub const SUCCESS: u8 = 6;
    pub const REQUEST_IDENTITIES: u8 = 11;
    pub const IDENTITIES_ANSWER: u8 = 12;
    pub const SIGN_REQUEST: u8 = 13;
    pub const SIGN_RESPONSE: u8 = 14;
    pub const ADD_IDENTITY: u8 = 17;
    pub const REMOVE_IDENTITY: u8 = 18;
    pub const REMOVE_ALL_IDENTITIES: u8 = 19;
    pub const ADD_SMARTCARD_KEY: u8 = 20;
    pub const REMOVE_SMARTCARD_KEY: u8 = 21;
    pub const LOCK: u8 = 22;
    pub const UNLOCK: u8 = 23;
    pub const ADD_ID_CONSTRAINED: u8 = 25;
    pub const ADD_SMARTCARD_KEY_CONSTRAINED: u8 = 26;
    pub const EXTENSION: u8 = 27;
    pub const EXTENSION_FAILURE: u8 = 28;
}

/// `SSH_MSG_USERAUTH_REQUEST`, the first byte after the session id in
/// user-authentication signing data (RFC 4252 section 7).
const SSH_MSG_USERAUTH_REQUEST: u8 = 50;
pub const SESSION_BIND_EXTENSION: &str = "session-bind@openssh.com";
pub const METHOD_PUBLICKEY: &str = "publickey";
/// OpenSSH >= 8.9 against a server advertising it: same as `publickey` plus
/// the server host key appended to the signed data.
pub const METHOD_PUBLICKEY_HOSTBOUND: &str = "publickey-hostbound-v00@openssh.com";
const SSHSIG_MAGIC: &[u8] = b"SSHSIG";

/// Upper bound on one agent message body. OpenSSH's own agent uses 256 KiB.
pub const MAX_AGENT_MESSAGE: usize = 256 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WireError(pub String);

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for WireError {}

fn err<T>(message: impl Into<String>) -> Result<T, WireError> {
    Err(WireError(message.into()))
}

/// A cursor over SSH wire encoding (RFC 4251 section 5).
struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }

    fn u8(&mut self) -> Result<u8, WireError> {
        let (&first, rest) = self.buf.split_first().ok_or_else(|| WireError("truncated byte".into()))?;
        self.buf = rest;
        Ok(first)
    }

    fn bool(&mut self) -> Result<bool, WireError> {
        Ok(self.u8()? != 0)
    }

    fn u32(&mut self) -> Result<u32, WireError> {
        if self.buf.len() < 4 {
            return err("truncated uint32");
        }
        let (head, rest) = self.buf.split_at(4);
        self.buf = rest;
        Ok(u32::from_be_bytes([head[0], head[1], head[2], head[3]]))
    }

    fn bytes(&mut self) -> Result<&'a [u8], WireError> {
        let len = self.u32()? as usize;
        if self.buf.len() < len {
            return err("truncated string");
        }
        let (head, rest) = self.buf.split_at(len);
        self.buf = rest;
        Ok(head)
    }

    fn utf8(&mut self) -> Result<String, WireError> {
        String::from_utf8(self.bytes()?.to_vec()).map_err(|_| WireError("string is not UTF-8".into()))
    }

    fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    fn finish(&self) -> Result<(), WireError> {
        if self.buf.is_empty() {
            Ok(())
        } else {
            err(format!("{} trailing bytes", self.buf.len()))
        }
    }
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn put_bytes(out: &mut Vec<u8>, value: &[u8]) {
    put_u32(out, value.len() as u32);
    out.extend_from_slice(value);
}

/// One request from an agent client, as far as this proxy cares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentRequest {
    RequestIdentities,
    Sign {
        key_blob: Vec<u8>,
        data: Vec<u8>,
        flags: u32,
    },
    SessionBind(SessionBind),
    /// Any other `SSH_AGENTC_EXTENSION`.
    Extension { name: String },
    /// Add/remove/lock/unlock/smartcard or unknown: always refused.
    Other { msg_type: u8 },
}

impl AgentRequest {
    pub fn label(&self) -> String {
        match self {
            Self::RequestIdentities => "request_identities".into(),
            Self::Sign { .. } => "sign".into(),
            Self::SessionBind(_) => "session_bind".into(),
            Self::Extension { name } => format!("extension:{name}"),
            Self::Other { msg_type } => format!("type:{msg_type}"),
        }
    }
}

/// The `session-bind@openssh.com` extension: the client tells the agent which
/// server host key the session it is about to authenticate belongs to, with
/// the server's signature over the session identifier as proof.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionBind {
    pub host_key: Vec<u8>,
    pub session_id: Vec<u8>,
    pub signature: Vec<u8>,
    pub is_forwarding: bool,
}

/// Parses one message body (the bytes after the 4-byte length).
pub fn parse_request(body: &[u8]) -> Result<AgentRequest, WireError> {
    let mut r = Reader::new(body);
    let msg_type = r.u8()?;
    match msg_type {
        msg::REQUEST_IDENTITIES => Ok(AgentRequest::RequestIdentities),
        msg::SIGN_REQUEST => {
            let key_blob = r.bytes()?.to_vec();
            let data = r.bytes()?.to_vec();
            // Some clients omit flags entirely.
            let flags = if r.is_empty() { 0 } else { r.u32()? };
            r.finish()?;
            Ok(AgentRequest::Sign { key_blob, data, flags })
        }
        msg::EXTENSION => {
            let name = r.utf8()?;
            if name != SESSION_BIND_EXTENSION {
                return Ok(AgentRequest::Extension { name });
            }
            let host_key = r.bytes()?.to_vec();
            let session_id = r.bytes()?.to_vec();
            let signature = r.bytes()?.to_vec();
            let is_forwarding = r.bool()?;
            r.finish()?;
            Ok(AgentRequest::SessionBind(SessionBind {
                host_key,
                session_id,
                signature,
                is_forwarding,
            }))
        }
        other => Ok(AgentRequest::Other { msg_type: other }),
    }
}

/// What a `SIGN_REQUEST`'s data claims to be. Parsed strictly: anything that
/// isn't exactly one of the known shapes is `Unknown` and should be refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignPayload {
    UserAuth(UserAuthRequest),
    SshSig { namespace: String, hash_alg: String },
    Unknown { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserAuthRequest {
    pub session_id: Vec<u8>,
    pub user: String,
    pub service: String,
    pub method: String,
    pub algorithm: String,
    pub key_blob: Vec<u8>,
    /// Present for `publickey-hostbound-v00@openssh.com`.
    pub host_key: Option<Vec<u8>>,
}

pub fn parse_sign_payload(data: &[u8]) -> SignPayload {
    if data.starts_with(SSHSIG_MAGIC) {
        return match parse_sshsig(&data[SSHSIG_MAGIC.len()..]) {
            Ok(payload) => payload,
            Err(e) => SignPayload::Unknown {
                reason: format!("malformed SSHSIG: {e}"),
            },
        };
    }
    match parse_userauth(data) {
        Ok(request) => SignPayload::UserAuth(request),
        Err(e) => SignPayload::Unknown {
            reason: format!("not a user-auth request: {e}"),
        },
    }
}

fn parse_sshsig(rest: &[u8]) -> Result<SignPayload, WireError> {
    let mut r = Reader::new(rest);
    let namespace = r.utf8()?;
    let _reserved = r.bytes()?;
    let hash_alg = r.utf8()?;
    let _hash = r.bytes()?;
    r.finish()?;
    Ok(SignPayload::SshSig { namespace, hash_alg })
}

fn parse_userauth(data: &[u8]) -> Result<UserAuthRequest, WireError> {
    let mut r = Reader::new(data);
    let session_id = r.bytes()?.to_vec();
    if session_id.is_empty() || session_id.len() > 64 {
        return err("implausible session id length");
    }
    if r.u8()? != SSH_MSG_USERAUTH_REQUEST {
        return err("missing SSH_MSG_USERAUTH_REQUEST");
    }
    let user = r.utf8()?;
    let service = r.utf8()?;
    let method = r.utf8()?;
    if method != METHOD_PUBLICKEY && method != METHOD_PUBLICKEY_HOSTBOUND {
        return err(format!("unexpected method {method:?}"));
    }
    if !r.bool()? {
        return err("has-signature flag is false");
    }
    let algorithm = r.utf8()?;
    let key_blob = r.bytes()?.to_vec();
    let host_key = if method == METHOD_PUBLICKEY_HOSTBOUND {
        Some(r.bytes()?.to_vec())
    } else {
        None
    };
    r.finish()?;
    Ok(UserAuthRequest {
        session_id,
        user,
        service,
        method,
        algorithm,
        key_blob,
        host_key,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub key_blob: Vec<u8>,
    pub comment: String,
}

pub fn parse_identities_answer(body: &[u8]) -> Result<Vec<Identity>, WireError> {
    let mut r = Reader::new(body);
    match r.u8()? {
        msg::IDENTITIES_ANSWER => {}
        msg::FAILURE => return err("agent returned failure"),
        other => return err(format!("unexpected reply type {other}")),
    }
    let count = r.u32()? as usize;
    let mut out = Vec::with_capacity(count.min(64));
    for _ in 0..count {
        let key_blob = r.bytes()?.to_vec();
        let comment = String::from_utf8_lossy(r.bytes()?).into_owned();
        out.push(Identity { key_blob, comment });
    }
    r.finish()?;
    Ok(out)
}

pub fn encode_identities_answer(identities: &[Identity]) -> Vec<u8> {
    let mut out = vec![msg::IDENTITIES_ANSWER];
    put_u32(&mut out, identities.len() as u32);
    for identity in identities {
        put_bytes(&mut out, &identity.key_blob);
        put_bytes(&mut out, identity.comment.as_bytes());
    }
    out
}

pub fn encode_sign_request(key_blob: &[u8], data: &[u8], flags: u32) -> Vec<u8> {
    let mut out = vec![msg::SIGN_REQUEST];
    put_bytes(&mut out, key_blob);
    put_bytes(&mut out, data);
    put_u32(&mut out, flags);
    out
}

/// Returns the signature blob from a `SIGN_RESPONSE`, or `None` for a failure.
pub fn parse_sign_response(body: &[u8]) -> Result<Option<Vec<u8>>, WireError> {
    let mut r = Reader::new(body);
    match r.u8()? {
        msg::SIGN_RESPONSE => {
            let signature = r.bytes()?.to_vec();
            r.finish()?;
            Ok(Some(signature))
        }
        msg::FAILURE => Ok(None),
        other => err(format!("unexpected reply type {other}")),
    }
}

pub fn failure() -> Vec<u8> {
    vec![msg::FAILURE]
}

pub fn success() -> Vec<u8> {
    vec![msg::SUCCESS]
}

/// Reads one length-prefixed message body. `Ok(None)` on a clean EOF between
/// messages.
pub fn read_message<R: Read>(reader: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match reader.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len) as usize;
    if len == 0 || len > MAX_AGENT_MESSAGE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("agent message length {len} out of range"),
        ));
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body)?;
    Ok(Some(body))
}

pub fn write_message<W: Write>(writer: &mut W, body: &[u8]) -> io::Result<()> {
    let mut framed = Vec::with_capacity(body.len() + 4);
    put_bytes(&mut framed, body);
    writer.write_all(&framed)?;
    writer.flush()
}

/// The key type name at the start of a public key blob (`ssh-ed25519`, ...).
pub fn key_type(blob: &[u8]) -> Option<String> {
    Reader::new(blob).utf8().ok()
}

/// Base64 (standard, padded) for agent bytes carried in JSON bridge messages.
pub fn b64_encode(bytes: &[u8]) -> String {
    use base64ct::Encoding;
    base64ct::Base64::encode_string(bytes)
}

pub fn b64_decode(text: &str) -> Result<Vec<u8>, WireError> {
    use base64ct::Encoding;
    base64ct::Base64::decode_vec(text).map_err(|_| WireError("invalid base64".into()))
}

/// Lowercase hex, for logging session ids.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Test/fixture helper: builds user-auth signing data the way OpenSSH does.
pub fn encode_userauth_data(request: &UserAuthRequest) -> Vec<u8> {
    let mut out = Vec::new();
    put_bytes(&mut out, &request.session_id);
    out.push(SSH_MSG_USERAUTH_REQUEST);
    put_bytes(&mut out, request.user.as_bytes());
    put_bytes(&mut out, request.service.as_bytes());
    put_bytes(&mut out, request.method.as_bytes());
    out.push(1);
    put_bytes(&mut out, request.algorithm.as_bytes());
    put_bytes(&mut out, &request.key_blob);
    if let Some(host_key) = &request.host_key {
        put_bytes(&mut out, host_key);
    }
    out
}

/// Test/fixture helper: builds a `session-bind@openssh.com` request body.
pub fn encode_session_bind(bind: &SessionBind) -> Vec<u8> {
    let mut out = vec![msg::EXTENSION];
    put_bytes(&mut out, SESSION_BIND_EXTENSION.as_bytes());
    put_bytes(&mut out, &bind.host_key);
    put_bytes(&mut out, &bind.session_id);
    put_bytes(&mut out, &bind.signature);
    out.push(bind.is_forwarding as u8);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_userauth(method: &str) -> UserAuthRequest {
        UserAuthRequest {
            session_id: vec![7; 32],
            user: "git".into(),
            service: "ssh-connection".into(),
            method: method.into(),
            algorithm: "ssh-ed25519".into(),
            key_blob: b"\0\0\0\x0bssh-ed25519\0\0\0\x20AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".to_vec(),
            host_key: (method == METHOD_PUBLICKEY_HOSTBOUND).then(|| b"hostkey".to_vec()),
        }
    }

    #[test]
    fn userauth_round_trips_for_both_methods() {
        for method in [METHOD_PUBLICKEY, METHOD_PUBLICKEY_HOSTBOUND] {
            let request = sample_userauth(method);
            let data = encode_userauth_data(&request);
            assert_eq!(parse_sign_payload(&data), SignPayload::UserAuth(request));
        }
    }

    #[test]
    fn userauth_with_trailing_bytes_is_unknown() {
        let mut data = encode_userauth_data(&sample_userauth(METHOD_PUBLICKEY));
        data.push(0);
        assert!(matches!(parse_sign_payload(&data), SignPayload::Unknown { .. }));
    }

    #[test]
    fn arbitrary_data_is_unknown() {
        assert!(matches!(parse_sign_payload(b"hello"), SignPayload::Unknown { .. }));
        assert!(matches!(parse_sign_payload(&[]), SignPayload::Unknown { .. }));
    }

    #[test]
    fn sshsig_namespace_is_extracted() {
        let mut data = SSHSIG_MAGIC.to_vec();
        put_bytes(&mut data, b"git");
        put_bytes(&mut data, b"");
        put_bytes(&mut data, b"sha512");
        put_bytes(&mut data, &[1; 64]);
        assert_eq!(
            parse_sign_payload(&data),
            SignPayload::SshSig {
                namespace: "git".into(),
                hash_alg: "sha512".into()
            }
        );
    }

    #[test]
    fn session_bind_round_trips() {
        let bind = SessionBind {
            host_key: b"hk".to_vec(),
            session_id: vec![1; 32],
            signature: b"sig".to_vec(),
            is_forwarding: true,
        };
        assert_eq!(
            parse_request(&encode_session_bind(&bind)).unwrap(),
            AgentRequest::SessionBind(bind)
        );
    }

    #[test]
    fn sign_request_and_identities_round_trip() {
        let body = encode_sign_request(b"key", b"data", 4);
        assert_eq!(
            parse_request(&body).unwrap(),
            AgentRequest::Sign {
                key_blob: b"key".to_vec(),
                data: b"data".to_vec(),
                flags: 4
            }
        );
        let ids = vec![Identity {
            key_blob: b"key".to_vec(),
            comment: "c".into(),
        }];
        assert_eq!(parse_identities_answer(&encode_identities_answer(&ids)).unwrap(), ids);
    }

    #[test]
    fn key_management_requests_are_other() {
        for t in [msg::ADD_IDENTITY, msg::REMOVE_ALL_IDENTITIES, msg::LOCK, msg::ADD_ID_CONSTRAINED] {
            assert_eq!(parse_request(&[t, 0, 0]).unwrap(), AgentRequest::Other { msg_type: t });
        }
    }

    #[test]
    fn oversized_and_empty_frames_are_rejected() {
        let mut big = Vec::new();
        put_u32(&mut big, (MAX_AGENT_MESSAGE + 1) as u32);
        assert!(read_message(&mut big.as_slice()).is_err());
        let zero = 0u32.to_be_bytes();
        assert!(read_message(&mut zero.as_slice()).is_err());
        assert!(read_message(&mut [].as_slice()).unwrap().is_none());
    }
}
