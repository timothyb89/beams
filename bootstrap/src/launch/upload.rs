//! Launch blobs uploaded over the workstation bridge (`LaunchUpload*`): a
//! declaration, or one phase's bundle archive, written chunk by chunk to a
//! private spool and published only once its size and SHA-256 match what
//! the upload began with.
//!
//! Layout, per launch, under `<root>/<launch id>/` (0700):
//!
//! ```text
//! <blob>.json   what the upload began with (size, digest, descriptor)
//! <blob>.part   bytes received so far (0600); a later begin resumes here
//! <blob>        the complete, verified blob
//! ```
//!
//! What may be uploaded, and when a complete bundle is staged, is decided
//! by `launch::daemon`; this module only stores bytes.

use anyhow::{bail, Context as _};
use beams_protocol::launch::{Blob, BundleDescriptor};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// What an upload began with; a resumed begin must match it.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Meta {
    pub bytes: u64,
    pub sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub descriptor: Option<BundleDescriptor>,
}

/// The outcome of a chunk.
#[derive(Debug, PartialEq, Eq)]
pub enum Chunk {
    /// Bytes now held: an acknowledgement, or (if lower than what was sent)
    /// where the sender must resume from.
    Offset(u64),
    /// The blob is complete and verified.
    Complete,
}

struct Session {
    bridge_id: String,
    meta: Meta,
    file: File,
    offset: u64,
}

pub struct Uploads {
    root: PathBuf,
    sessions: Mutex<BTreeMap<(String, Blob), Session>>,
}

impl Uploads {
    pub fn new(root: PathBuf) -> Self {
        Self { root, sessions: Mutex::default() }
    }

    fn dir(&self, launch_id: &str) -> PathBuf {
        self.root.join(launch_id)
    }

    fn path(&self, launch_id: &str, blob: Blob, suffix: &str) -> PathBuf {
        self.dir(launch_id).join(format!("{}{suffix}", blob.name()))
    }

    /// The complete blob, if one is held.
    pub fn complete(&self, launch_id: &str, blob: Blob) -> Option<(PathBuf, Meta)> {
        let path = self.path(launch_id, blob, "");
        let meta = beams_artifacts::read_json(&self.path(launch_id, blob, ".json")).ok()?;
        path.is_file().then_some((path, meta))
    }

    /// Starts or resumes an upload for `bridge_id`, returning the bytes
    /// already held (`meta.bytes` when it is already complete). A begin
    /// with different size/digest than a held one starts over.
    pub fn begin(&self, bridge_id: &str, launch_id: &str, blob: Blob, meta: Meta) -> anyhow::Result<u64> {
        check_id(launch_id)?;
        check_digest(&meta.sha256)?;
        let mut sessions = self.sessions.lock().unwrap();
        let key = (launch_id.to_string(), blob);
        sessions.remove(&key);
        let dir = self.dir(launch_id);
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        beams_artifacts::set_mode(&self.root, 0o700)?;
        beams_artifacts::set_mode(&dir, 0o700)?;
        let meta_path = self.path(launch_id, blob, ".json");
        let part = self.path(launch_id, blob, ".part");
        let complete = self.path(launch_id, blob, "");
        let held: Option<Meta> = beams_artifacts::read_json(&meta_path).ok();
        if held.as_ref() == Some(&meta) && complete.is_file() {
            return Ok(meta.bytes);
        }
        if held.as_ref() != Some(&meta) {
            let _ = fs::remove_file(&complete);
            let _ = fs::remove_file(&part);
            beams_artifacts::write_json(&meta_path, &meta)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&part)
            .with_context(|| format!("opening {}", part.display()))?;
        let mut offset = file.metadata()?.len();
        if offset > meta.bytes {
            file.set_len(0)?;
            offset = 0;
        }
        sessions.insert(key, Session { bridge_id: bridge_id.to_string(), meta, file, offset });
        Ok(offset)
    }

    /// Appends `data` at `offset`. Anything but the next expected offset
    /// (a duplicate, a gap) is not written; the reply says where to resume.
    pub fn chunk(&self, bridge_id: &str, launch_id: &str, blob: Blob, offset: u64, data: &[u8]) -> anyhow::Result<Chunk> {
        let mut sessions = self.sessions.lock().unwrap();
        let key = (launch_id.to_string(), blob);
        let Some(session) = sessions.get_mut(&key).filter(|s| s.bridge_id == bridge_id) else {
            bail!("no {} upload in progress for launch {launch_id} on this connection", blob.name());
        };
        if offset != session.offset {
            return Ok(Chunk::Offset(session.offset));
        }
        if data.len() > beams_protocol::launch::UPLOAD_CHUNK_BYTES {
            bail!("chunk larger than {} bytes", beams_protocol::launch::UPLOAD_CHUNK_BYTES);
        }
        let end = offset + data.len() as u64;
        if end > session.meta.bytes {
            bail!("upload is longer than the {} bytes it began with", session.meta.bytes);
        }
        session.file.write_all(data).context("writing upload")?;
        session.offset = end;
        if end < session.meta.bytes {
            return Ok(Chunk::Offset(end));
        }
        let session = sessions.remove(&key).expect("present");
        drop(sessions);
        self.finish(launch_id, blob, session)
    }

    fn finish(&self, launch_id: &str, blob: Blob, session: Session) -> anyhow::Result<Chunk> {
        session.file.sync_all().context("syncing upload")?;
        drop(session.file);
        let part = self.path(launch_id, blob, ".part");
        let (digest, bytes) = beams_artifacts::hash_file(&part)?;
        if digest != session.meta.sha256 || bytes != session.meta.bytes {
            let _ = fs::remove_file(&part);
            bail!("{} upload doesn't match its SHA-256", blob.name());
        }
        let complete = self.path(launch_id, blob, "");
        fs::rename(&part, &complete).with_context(|| format!("publishing {}", complete.display()))?;
        Ok(Chunk::Complete)
    }

    /// Forgets a blob (consumed, or no longer wanted).
    pub fn remove(&self, launch_id: &str, blob: Blob) {
        self.sessions.lock().unwrap().remove(&(launch_id.to_string(), blob));
        for suffix in ["", ".part", ".json"] {
            let _ = fs::remove_file(self.path(launch_id, blob, suffix));
        }
        let _ = fs::remove_dir(self.dir(launch_id));
    }
}

/// Launch IDs name directories: a UUID's alphabet only.
pub fn check_id(launch_id: &str) -> anyhow::Result<()> {
    if launch_id.is_empty()
        || launch_id.len() > 64
        || !launch_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        bail!("invalid launch ID {launch_id:?}");
    }
    Ok(())
}

fn check_digest(digest: &str) -> anyhow::Result<()> {
    if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        bail!("digest must be a lowercase hex SHA-256");
    }
    Ok(())
}

/// Where uploads for the daemon whose launch state is `state_root` live.
pub fn root_for(state_root: &Path) -> PathBuf {
    state_root.with_file_name("launch-uploads")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> (PathBuf, Uploads) {
        let dir = std::env::temp_dir().join(format!("launch-uploads-test-{}-{}", std::process::id(), rand_suffix()));
        let _ = fs::remove_dir_all(&dir);
        (dir.clone(), Uploads::new(dir))
    }

    fn rand_suffix() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        N.fetch_add(1, Ordering::Relaxed)
    }

    fn meta(data: &[u8]) -> Meta {
        let (sha256, bytes) = beams_artifacts::hash_reader(data).unwrap();
        Meta { bytes, sha256, descriptor: None }
    }

    #[test]
    fn chunks_resume_and_complete_only_when_the_digest_matches() {
        let (dir, uploads) = scratch();
        let data = b"hello, launch".to_vec();
        assert_eq!(uploads.begin("b1", "L1", Blob::Declaration, meta(&data)).unwrap(), 0);
        assert_eq!(uploads.chunk("b1", "L1", Blob::Declaration, 0, &data[..5]).unwrap(), Chunk::Offset(5));
        // A duplicate or a gap isn't written; the reply says where to resume.
        assert_eq!(uploads.chunk("b1", "L1", Blob::Declaration, 0, &data[..5]).unwrap(), Chunk::Offset(5));
        assert_eq!(uploads.chunk("b1", "L1", Blob::Declaration, 9, &data[9..]).unwrap(), Chunk::Offset(5));
        // Another connection can't write into this one's upload...
        assert!(uploads.chunk("b2", "L1", Blob::Declaration, 5, &data[5..]).is_err());
        // ...but after a reconnect it resumes where the bytes stopped.
        assert_eq!(uploads.begin("b2", "L1", Blob::Declaration, meta(&data)).unwrap(), 5);
        assert!(uploads.chunk("b1", "L1", Blob::Declaration, 5, &data[5..]).is_err(), "the old session is gone");
        assert_eq!(uploads.chunk("b2", "L1", Blob::Declaration, 5, &data[5..]).unwrap(), Chunk::Complete);
        let (path, held) = uploads.complete("L1", Blob::Declaration).unwrap();
        assert_eq!(fs::read(path).unwrap(), data);
        assert_eq!(held, meta(&data));
        // Beginning a complete upload again needs nothing more.
        assert_eq!(uploads.begin("b3", "L1", Blob::Declaration, meta(&data)).unwrap(), data.len() as u64);

        // Different content under the same blob starts over.
        let other = b"something else".to_vec();
        assert_eq!(uploads.begin("b3", "L1", Blob::Declaration, meta(&other)).unwrap(), 0);
        assert!(uploads.complete("L1", Blob::Declaration).is_none());
        uploads.remove("L1", Blob::Declaration);
        assert!(!dir.join("L1").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_wrong_digest_or_oversized_upload_is_refused() {
        let (dir, uploads) = scratch();
        let data = b"payload".to_vec();
        let mut lying = meta(b"PAYLOAD");
        lying.bytes = data.len() as u64;
        uploads.begin("b", "L2", Blob::Pre, lying.clone()).unwrap();
        let err = uploads.chunk("b", "L2", Blob::Pre, 0, &data).unwrap_err();
        assert!(err.to_string().contains("SHA-256"), "{err}");
        assert!(uploads.complete("L2", Blob::Pre).is_none());
        // The spool was discarded: a new attempt starts from zero.
        assert_eq!(uploads.begin("b", "L2", Blob::Pre, lying).unwrap(), 0);

        uploads.begin("b", "L2", Blob::Post, meta(&data)).unwrap();
        assert!(uploads.chunk("b", "L2", Blob::Post, 0, b"payload and more").is_err());
        assert!(uploads.begin("b", "../x", Blob::Pre, meta(&data)).is_err());
        assert!(uploads.begin("b", "L3", Blob::Pre, Meta { bytes: 1, sha256: "md5:00".into(), descriptor: None }).is_err());
        let _ = fs::remove_dir_all(&dir);
    }
}
