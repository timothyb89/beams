//! Cryptographic checks for the SSH agent proxy (feature `agent-crypto`):
//! verifying `session-bind@openssh.com` host signatures, key fingerprints,
//! and mapping a verified host key back to a hostname via `known_hosts`.
//!
//! These are the facts the workstation treats as *verified* (see
//! `SSH_AGENT_PROXY_DESIGN.md`); everything else a Beam reports is advisory.

use hmac::{Hmac, Mac};
use sha1::Sha1;
use ssh_key::known_hosts::{HostPatterns, KnownHosts, Marker};
use ssh_key::{HashAlg, PublicKey, Signature};

use crate::agent::SessionBind;

/// `SHA256:...` fingerprint of a public key blob, or `None` if it doesn't
/// parse as a plain (non-certificate) public key.
pub fn fingerprint(key_blob: &[u8]) -> Option<String> {
    let key = PublicKey::from_bytes(key_blob).ok()?;
    Some(key.fingerprint(HashAlg::Sha256).to_string())
}

/// Checks that `bind.signature` is the server host key's signature over the
/// session identifier. On success, a user-auth signature over data with the
/// same session id can only be used in a session with that host key.
pub fn verify_session_bind(bind: &SessionBind) -> Result<(), String> {
    let key = PublicKey::from_bytes(&bind.host_key).map_err(|e| format!("host key: {e}"))?;
    let signature = Signature::try_from(bind.signature.as_slice()).map_err(|e| format!("signature: {e}"))?;
    use signature::Verifier;
    key.key_data()
        .verify(&bind.session_id, &signature)
        .map_err(|e| format!("bad host signature: {e}"))
}

/// A `known_hosts` entry naming the host key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KnownHostMatch {
    /// Hostname (or `[host]:port`) as written, or the candidate that matched
    /// a hashed entry.
    pub name: String,
    pub hashed: bool,
}

/// Finds names for `host_key` in `known_hosts` contents.
///
/// Plain entries yield their literal (non-wildcard, non-negated) names
/// directly. Hashed entries (`|1|salt|hash`, Debian's default via
/// `HashKnownHosts yes`) can't be reversed: they only confirm a *candidate*
/// name, so `candidates` should be the hostnames the Beam claims (from the
/// requesting command line). A candidate confirmed this way is still a
/// verified fact: the host key is cryptographically bound to the session and
/// the user's own `known_hosts` says that key belongs to that name.
///
/// `@revoked` and `@cert-authority` lines never match.
pub fn known_host_names(known_hosts: &str, host_key: &[u8], candidates: &[String]) -> Vec<KnownHostMatch> {
    let Ok(wanted) = PublicKey::from_bytes(host_key) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in KnownHosts::new(known_hosts).flatten() {
        if entry.marker().is_some_and(|m| matches!(m, Marker::Revoked | Marker::CertAuthority)) {
            continue;
        }
        if entry.public_key().key_data() != wanted.key_data() {
            continue;
        }
        match entry.host_patterns() {
            HostPatterns::Patterns(patterns) => {
                for pattern in patterns {
                    if pattern.starts_with('!') || pattern.contains(['*', '?']) {
                        continue;
                    }
                    push_unique(&mut out, KnownHostMatch { name: pattern.clone(), hashed: false });
                }
            }
            HostPatterns::HashedName { salt, hash } => {
                for candidate in candidates {
                    if hashed_name_matches(salt, hash, candidate) {
                        push_unique(&mut out, KnownHostMatch { name: candidate.clone(), hashed: true });
                    }
                }
            }
        }
    }
    out
}

fn push_unique(out: &mut Vec<KnownHostMatch>, m: KnownHostMatch) {
    if !out.contains(&m) {
        out.push(m);
    }
}

fn hashed_name_matches(salt: &[u8], hash: &[u8; 20], candidate: &str) -> bool {
    let Ok(mut mac) = Hmac::<Sha1>::new_from_slice(salt) else {
        return false;
    };
    mac.update(candidate.as_bytes());
    mac.verify_slice(hash).is_ok()
}

/// The `known_hosts` spelling of a destination: bare host for port 22,
/// `[host]:port` otherwise.
pub fn known_hosts_name(host: &str, port: Option<u16>) -> String {
    match port {
        None | Some(22) => host.to_string(),
        Some(port) => format!("[{host}]:{port}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::SessionBind;
    use ssh_key::{rand_core::OsRng, Algorithm, PrivateKey};

    fn host_key() -> PrivateKey {
        PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap()
    }

    fn bind_for(key: &PrivateKey, session_id: &[u8], sign_over: &[u8]) -> SessionBind {
        use signature::Signer;
        let sig: Signature = key.key_data().try_sign(sign_over).unwrap();
        SessionBind {
            host_key: key.public_key().to_bytes().unwrap(),
            session_id: session_id.to_vec(),
            signature: sig.try_into().map(|s: Vec<u8>| s).unwrap(),
            is_forwarding: false,
        }
    }

    #[test]
    fn valid_session_bind_verifies_and_tampered_does_not() {
        let key = host_key();
        let sid = [9u8; 32];
        assert!(verify_session_bind(&bind_for(&key, &sid, &sid)).is_ok());
        assert!(verify_session_bind(&bind_for(&key, &sid, &[8u8; 32])).is_err());

        let mut swapped = bind_for(&key, &sid, &sid);
        swapped.host_key = host_key().public_key().to_bytes().unwrap();
        assert!(verify_session_bind(&swapped).is_err());
    }

    #[test]
    fn known_hosts_plain_and_hashed_entries() {
        let key = host_key();
        let other = host_key();
        let openssh = key.public_key().to_openssh().unwrap();
        let other_openssh = other.public_key().to_openssh().unwrap();

        // Hash "example.com" the way `ssh-keygen -H` does.
        let salt = [3u8; 20];
        let mut mac = Hmac::<Sha1>::new_from_slice(&salt).unwrap();
        mac.update(b"example.com");
        let hash = mac.finalize().into_bytes();
        use base64ct::{Base64, Encoding};
        let hashed = format!("|1|{}|{}", Base64::encode_string(&salt), Base64::encode_string(&hash));

        let known_hosts = format!(
            "github.com,*.wild,!neg {openssh}\n\
             {hashed} {openssh}\n\
             @revoked revoked.example {openssh}\n\
             unrelated.example {other_openssh}\n"
        );
        let blob = key.public_key().to_bytes().unwrap();

        let found = known_host_names(&known_hosts, &blob, &["example.com".into(), "nope.example".into()]);
        assert_eq!(
            found,
            vec![
                KnownHostMatch { name: "github.com".into(), hashed: false },
                KnownHostMatch { name: "example.com".into(), hashed: true },
            ]
        );
        let other_blob = other.public_key().to_bytes().unwrap();
        assert_eq!(known_host_names(&known_hosts, &other_blob, &[])[0].name, "unrelated.example");
    }

    #[test]
    fn fingerprint_matches_ssh_keygen_format() {
        let key = host_key();
        let fp = fingerprint(&key.public_key().to_bytes().unwrap()).unwrap();
        assert!(fp.starts_with("SHA256:"), "{fp}");
        assert_eq!(fingerprint(b"garbage"), None);
    }

    #[test]
    fn known_hosts_name_formats_ports() {
        assert_eq!(known_hosts_name("h", None), "h");
        assert_eq!(known_hosts_name("h", Some(22)), "h");
        assert_eq!(known_hosts_name("h", Some(2200)), "[h]:2200");
    }
}
