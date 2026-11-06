use crate::*;
use anyhow::{bail, Context};
use beams_protocol::launch::*;
use flate2::{Compression as GzipLevel, GzBuilder};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use tar::{Builder, EntryType, Header};

pub enum Selection {
    Local(PathBuf),
    Embedded { name: String, bytes: &'static [u8] },
}

/// Snapshot only this phase. Each returned spool is replayable independently
/// of subsequent edits to local sources. Never walks deferred post roots.
pub fn pack(
    declaration: &Declaration,
    phase: Phase,
    sources: &BTreeMap<String, Selection>,
    output: &Path,
    compression: Compression,
) -> anyhow::Result<BundleDescriptor> {
    validate_declaration(declaration)?;
    if output.exists() {
        bail!("bundle output already exists: {}", output.display());
    }
    let parent = output
        .parent()
        .context("archive needs a parent directory")?;
    let temp = TemporaryDirectory::new(parent)?;
    let snapshot = temp.0.join("snapshot");
    fs::create_dir(&snapshot)?;
    let mut manifest = BundleManifest {
        schema_version: 1,
        launch_id: declaration.launch_id.clone(),
        plan_digest: declaration.plan_digest.clone(),
        phase,
        artifacts: Vec::new(),
    };
    let mut ids = BTreeSet::new();
    for artifact in declaration
        .plan
        .artifacts
        .iter()
        .filter(|a| a.phase == phase)
    {
        if !ids.insert(&artifact.id)
            || !artifact
                .id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_".contains(&c))
            || artifact.id.is_empty()
        {
            bail!("invalid or duplicate artifact ID {:?}", artifact.id);
        }
        let selection = sources
            .get(&artifact.id)
            .with_context(|| format!("missing source for {}", artifact.id))?;
        let prefix = format!("payload/{}", artifact.id);
        let mut entries = Vec::new();
        let root = match selection {
            Selection::Local(source) => {
                let metadata = fs::symlink_metadata(source)?;
                if metadata.is_dir() != artifact.directory {
                    bail!("source type changed for {}", artifact.id);
                }
                let root = if artifact.directory {
                    prefix
                } else {
                    format!(
                        "{prefix}/{}",
                        source
                            .file_name()
                            .context("source needs a filename")?
                            .to_str()
                            .context("source filename must be UTF-8")?
                    )
                };
                copy_selection(
                    source,
                    &root,
                    &root,
                    &snapshot,
                    &artifact.exclude,
                    &mut entries,
                )?;
                root
            }
            Selection::Embedded { name, bytes } => {
                if artifact.directory {
                    bail!("embedded asset cannot be a directory");
                }
                let filename = Path::new(name)
                    .file_name()
                    .context("asset needs a filename")?
                    .to_str()
                    .context("asset name must be UTF-8")?;
                let root = format!("{prefix}/{filename}");
                safe_path(&root)?;
                fs::create_dir_all(snapshot.join(&prefix))?;
                fs::write(snapshot.join(&root), bytes)?;
                entries.push(BundleEntry {
                    path: root.clone(),
                    mode: 0o644,
                    size: bytes.len() as u64,
                    entry: EntryKind::File {
                        sha256: format!("{:x}", Sha256::digest(bytes)),
                    },
                });
                root
            }
        };
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        manifest.artifacts.push(PackedArtifact {
            id: artifact.id.clone(),
            root,
            directory: artifact.directory,
            digest: json_digest(&entries)?,
            entries,
        });
    }
    crate::receive::validate_manifest(&manifest)?;
    let archive_path = temp.0.join("bundle");
    let file = File::create(&archive_path)?;
    match compression {
        Compression::None => {
            write_archive(file, &manifest, &snapshot)?.sync_all()?;
        }
        Compression::Gzip => {
            let encoder = GzBuilder::new()
                .mtime(0)
                .operating_system(255)
                .write(file, GzipLevel::fast());
            let encoder = write_archive(encoder, &manifest, &snapshot)?;
            encoder.finish()?.sync_all()?;
        }
    };
    let (sha256, bytes) = hash_file(&archive_path)?;
    fs::rename(archive_path, output)?;
    set_mode(output, 0o400)?;
    Ok(BundleDescriptor {
        schema_version: 1,
        launch_id: declaration.launch_id.clone(),
        plan_digest: declaration.plan_digest.clone(),
        phase,
        compression,
        sha256,
        bytes,
    })
}

fn copy_selection(
    source: &Path,
    name: &str,
    root: &str,
    snapshot: &Path,
    exclude: &[String],
    entries: &mut Vec<BundleEntry>,
) -> anyhow::Result<()> {
    safe_path(name)?;
    let relative = name.strip_prefix(root).unwrap().trim_start_matches('/');
    if exclude
        .iter()
        .any(|e| relative == e || relative.starts_with(&format!("{e}/")))
    {
        return Ok(());
    }
    let metadata = fs::symlink_metadata(source)
        .with_context(|| format!("snapshotting {}", source.display()))?;
    let target = snapshot.join(name);
    fs::create_dir_all(target.parent().unwrap())?;
    let entry = if metadata.is_dir() {
        fs::create_dir_all(&target)?;
        let mut children = fs::read_dir(source)?.collect::<Result<Vec<_>, _>>()?;
        children.sort_by_key(|e| e.file_name());
        for child in children {
            let filename = child
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("source names must be UTF-8"))?;
            copy_selection(
                &child.path(),
                &format!("{name}/{filename}"),
                root,
                snapshot,
                exclude,
                entries,
            )?;
        }
        EntryKind::Directory
    } else if metadata.is_file() {
        let mut input = open_regular(source)?;
        let before = input.metadata()?;
        if before.len() != metadata.len()
            || before.modified()? != metadata.modified()?
            || mode(&before) != mode(&metadata)
        {
            bail!("source changed before snapshotting {}", source.display());
        }
        let mut output = File::create(target)?;
        let mut hash = Sha256::new();
        let mut size = 0;
        let mut bytes = [0u8; 64 * 1024];
        loop {
            let n = input.read(&mut bytes)?;
            if n == 0 {
                break;
            }
            output.write_all(&bytes[..n])?;
            hash.update(&bytes[..n]);
            size += n as u64;
        }
        let after = input.metadata()?;
        if size != before.len()
            || before.len() != after.len()
            || before.modified()? != after.modified()?
        {
            bail!("source changed while snapshotting {}", source.display());
        }
        EntryKind::File {
            sha256: format!("{:x}", hash.finalize()),
        }
    } else if metadata.file_type().is_symlink() {
        let link = fs::read_link(source)?
            .to_str()
            .context("symlink targets must be UTF-8")?
            .to_owned();
        EntryKind::Symlink { target: link }
    } else {
        bail!("unsupported special file {}", source.display());
    };
    let size = if matches!(entry, EntryKind::File { .. }) {
        metadata.len()
    } else {
        0
    };
    entries.push(BundleEntry {
        path: name.to_owned(),
        mode: mode(&metadata),
        size,
        entry,
    });
    Ok(())
}

fn header(entry_type: EntryType, size: u64, mode: u32) -> Header {
    let mut header = Header::new_gnu();
    header.set_entry_type(entry_type);
    header.set_size(size);
    header.set_mode(mode);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header
}

fn write_archive<W: Write>(
    writer: W,
    manifest: &BundleManifest,
    snapshot: &Path,
) -> anyhow::Result<W> {
    let mut tar = Builder::new(writer);
    tar.follow_symlinks(false);
    let json = serde_json::to_vec(manifest)?;
    tar.append_data(
        &mut header(EntryType::Regular, json.len() as u64, 0o600),
        "bundle.json",
        json.as_slice(),
    )?;
    for artifact in &manifest.artifacts {
        for entry in &artifact.entries {
            match &entry.entry {
                EntryKind::File { .. } => tar.append_data(
                    &mut header(EntryType::Regular, entry.size, entry.mode),
                    &entry.path,
                    open_regular(&snapshot.join(&entry.path))?,
                )?,
                EntryKind::Directory => tar.append_data(
                    &mut header(EntryType::Directory, 0, entry.mode),
                    &entry.path,
                    std::io::empty(),
                )?,
                EntryKind::Symlink { target } => tar.append_link(
                    &mut header(EntryType::Symlink, 0, entry.mode),
                    &entry.path,
                    target,
                )?,
            }
        }
    }
    Ok(tar.into_inner()?)
}
