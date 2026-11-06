//! Immutable tar spools and verified, atomic staging. Destinations never come
//! from an archive: installation is a separate launch task.

mod receive;
mod snapshot;
#[cfg(test)]
mod tests;

use anyhow::{bail, Context};
use beams_protocol::launch::*;
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub use receive::{receive, verify_staging};
pub use snapshot::{pack, Selection};

pub fn json_digest(value: &impl serde::Serialize) -> anyhow::Result<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(value)?)))
}

pub fn hash_reader(mut reader: impl Read) -> anyhow::Result<(String, u64)> {
    let mut hash = Sha256::new();
    let mut bytes = 0;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
        bytes += n as u64;
    }
    Ok((format!("{:x}", hash.finalize()), bytes))
}

pub fn hash_file(path: &Path) -> anyhow::Result<(String, u64)> {
    hash_reader(open_regular(path)?).with_context(|| format!("hashing {}", path.display()))
}

pub fn declaration(plan: LaunchPlan) -> anyhow::Result<Declaration> {
    Ok(Declaration {
        schema_version: 1,
        launch_id: uuid::Uuid::new_v4().to_string(),
        plan_digest: json_digest(&plan)?,
        plan,
    })
}
pub fn validate_declaration(declaration: &Declaration) -> anyhow::Result<()> {
    validate_binding(
        declaration.schema_version,
        &declaration.launch_id,
        &declaration.plan_digest,
    )?;
    if declaration.plan.schema_version != 1
        || json_digest(&declaration.plan)? != declaration.plan_digest
    {
        bail!("launch plan digest/version mismatch");
    }
    Ok(())
}

fn validate_binding(version: u32, launch: &str, digest: &str) -> anyhow::Result<()> {
    if version != 1 {
        bail!("unsupported artifact schema version {version}");
    }
    if uuid::Uuid::parse_str(launch)?.to_string() != launch {
        bail!("launch ID must be a canonical UUID");
    }
    validate_hash(digest)
}
fn validate_hash(hash: &str) -> anyhow::Result<()> {
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    {
        bail!("expected lowercase SHA-256 digest");
    }
    Ok(())
}

/// Archive names are portable POSIX relative paths, without normalization.
pub fn safe_path(path: &str) -> anyhow::Result<()> {
    if path.is_empty()
        || path.contains(['\\', '\0'])
        || path
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == "..")
    {
        bail!("unsafe archive path {path:?}");
    }
    Ok(())
}

pub fn open_regular(path: &Path) -> anyhow::Result<File> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    if !file.metadata()?.is_file() {
        bail!("{} is not a regular file", path.display());
    }
    Ok(file)
}

pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> anyhow::Result<T> {
    let file = open_regular(path)?;
    if file.metadata()?.len() > 64 * 1024 * 1024 {
        bail!("JSON manifest exceeds 64 MiB");
    }
    serde_json::from_reader(file).with_context(|| format!("reading {}", path.display()))
}

/// Small metadata updates use a private temporary sibling and atomic rename.
pub fn write_json(path: &Path, value: &impl serde::Serialize) -> anyhow::Result<()> {
    let parent = path.parent().context("metadata path needs a parent")?;
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!(".metadata-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        serde_json::to_writer_pretty(&mut file, value)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        Ok(())
    })();
    let _ = fs::remove_file(temp);
    result
}

pub(crate) fn mode(metadata: &fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o777
    }
    #[cfg(not(unix))]
    {
        if metadata.is_dir() {
            0o755
        } else {
            0o644
        }
    }
}
pub fn set_mode(path: &Path, mode: u32) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}

pub struct TemporaryDirectory(pub PathBuf);
impl TemporaryDirectory {
    pub fn new(parent: &Path) -> anyhow::Result<Self> {
        fs::create_dir_all(parent)?;
        let path = parent.join(format!(".beams-staging-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path)?;
        set_mode(&path, 0o700)?;
        Ok(Self(path))
    }
    pub fn publish(mut self, destination: &Path) -> anyhow::Result<()> {
        if destination.exists() || fs::symlink_metadata(destination).is_ok() {
            bail!("destination already exists: {}", destination.display());
        }
        fs::rename(&self.0, destination)?;
        self.0 = PathBuf::new();
        Ok(())
    }
}
impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        if self.0.as_os_str().is_empty() {
            return;
        }
        fn unlock(path: &Path) {
            if fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()) {
                let _ = set_mode(path, 0o700);
                if let Ok(entries) = fs::read_dir(path) {
                    for entry in entries.flatten() {
                        unlock(&entry.path());
                    }
                }
            }
        }
        unlock(&self.0);
        let _ = fs::remove_dir_all(&self.0);
    }
}
