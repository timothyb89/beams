use crate::*;
use anyhow::{bail, Context};
use beams_protocol::launch::*;
use flate2::read::MultiGzDecoder;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use tar::Archive;

pub(crate) fn validate_manifest(manifest: &BundleManifest) -> anyhow::Result<()> {
    validate_binding(
        manifest.schema_version,
        &manifest.launch_id,
        &manifest.plan_digest,
    )?;
    let mut ids = BTreeSet::new();
    let mut paths = BTreeSet::new();
    for artifact in &manifest.artifacts {
        if artifact.id.is_empty()
            || !artifact
                .id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_".contains(&c))
            || !ids.insert(&artifact.id)
        {
            bail!("invalid or duplicate artifact ID");
        }
        safe_path(&artifact.root)?;
        let prefix = format!("payload/{}", artifact.id);
        if (artifact.directory && artifact.root != prefix)
            || (!artifact.directory && !artifact.root.starts_with(&format!("{prefix}/")))
        {
            bail!("artifact root outside its payload namespace");
        }
        if !artifact.directory && artifact.entries.len() != 1 {
            bail!("file artifact must contain exactly one entry");
        }
        if artifact.digest != json_digest(&artifact.entries)? {
            bail!("artifact digest mismatch");
        }
        let entries: BTreeMap<_, _> = artifact
            .entries
            .iter()
            .map(|e| (e.path.as_str(), e))
            .collect();
        if entries.len() != artifact.entries.len() {
            bail!("duplicate artifact path");
        }
        let root = entries
            .get(artifact.root.as_str())
            .context("artifact root is missing")?;
        if artifact.directory != matches!(root.entry, EntryKind::Directory) {
            bail!("artifact root type mismatch");
        }
        for entry in &artifact.entries {
            safe_path(&entry.path)?;
            if entry.path != artifact.root
                && !entry.path.starts_with(&format!("{}/", artifact.root))
            {
                bail!("entry outside artifact root");
            }
            if !paths.insert(&entry.path) || entry.mode & !0o777 != 0 {
                bail!("duplicate path or unsupported file mode");
            }
            if entry.path != artifact.root {
                let parent = entry.path.rsplit_once('/').unwrap().0;
                if !entries
                    .get(parent)
                    .is_some_and(|p| matches!(p.entry, EntryKind::Directory))
                {
                    bail!("entry parent must be a declared directory");
                }
            }
            match &entry.entry {
                EntryKind::File { sha256 } => validate_hash(sha256)?,
                EntryKind::Directory => {
                    if entry.size != 0 {
                        bail!("directory size must be zero");
                    }
                }
                EntryKind::Symlink { target } => {
                    if entry.size != 0 {
                        bail!("symlink size must be zero");
                    }
                    let mut current = entry.path.clone();
                    let mut link = target.clone();
                    let mut seen = BTreeSet::new();
                    loop {
                        if !seen.insert(current.clone()) {
                            bail!("symlink cycle at {current}");
                        }
                        current = resolve_link(&current, &link, &artifact.root)?;
                        let target_entry = entries.get(current.as_str()).with_context(|| {
                            format!("symlink target is missing or excluded: {current}")
                        })?;
                        if let EntryKind::Symlink { target } = &target_entry.entry {
                            link = target.clone();
                        } else {
                            break;
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

fn resolve_link(path: &str, target: &str, root: &str) -> anyhow::Result<String> {
    if target.is_empty() || target.starts_with('/') || target.contains(['\\', '\0']) {
        bail!("unsafe symlink target {target:?}");
    }
    let mut parts: Vec<&str> = path.rsplit_once('/').unwrap().0.split('/').collect();
    let root_depth = root.split('/').count();
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.len() <= root_depth {
                    bail!("symlink escapes its artifact");
                }
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    let resolved = parts.join("/");
    if resolved != root && !resolved.starts_with(&format!("{root}/")) {
        bail!("symlink escapes its artifact");
    }
    Ok(resolved)
}

/// The incoming bytes are copied and hashed before decoding; extraction then
/// reads that same private spool. No archive entry is unpacked over a live target.
pub fn receive(
    archive: &Path,
    descriptor: &BundleDescriptor,
    output: &Path,
) -> anyhow::Result<BundleManifest> {
    validate_binding(
        descriptor.schema_version,
        &descriptor.launch_id,
        &descriptor.plan_digest,
    )?;
    validate_hash(&descriptor.sha256)?;
    if fs::symlink_metadata(output).is_ok() {
        return verify_staging(output, descriptor);
    }
    let temp = TemporaryDirectory::new(output.parent().context("staging needs a parent")?)?;
    let input = open_regular(archive)?;
    let mut spool = File::create(temp.0.join("incoming"))?;
    let copied = std::io::copy(
        &mut input.take(descriptor.bytes.saturating_add(1)),
        &mut spool,
    )?;
    if copied != descriptor.bytes {
        bail!("bundle byte count mismatch");
    }
    spool.sync_all()?;
    let mut spool = open_regular(&temp.0.join("incoming"))?;
    let (digest, _) = hash_reader(&mut spool)?;
    if digest != descriptor.sha256 {
        bail!("bundle SHA-256 mismatch");
    }
    spool.seek(SeekFrom::Start(0))?;
    let reader: Box<dyn Read> = match descriptor.compression {
        Compression::None => Box::new(spool),
        Compression::Gzip => Box::new(MultiGzDecoder::new(spool)),
    };
    let manifest = extract(reader, descriptor, &temp.0)?;
    fs::remove_file(temp.0.join("incoming"))?;
    write_json(&temp.0.join("bundle.json"), &manifest)?;
    write_json(&temp.0.join("receipt.json"), descriptor)?;
    for artifact in &manifest.artifacts {
        for entry in artifact.entries.iter().rev() {
            match entry.entry {
                EntryKind::File { .. } => {
                    set_mode(&temp.0.join(&entry.path), 0o444 | (entry.mode & 0o111))?
                }
                EntryKind::Directory => set_mode(&temp.0.join(&entry.path), 0o555)?,
                EntryKind::Symlink { .. } => {}
            }
        }
    }
    set_mode(&temp.0.join("bundle.json"), 0o400)?;
    set_mode(&temp.0.join("receipt.json"), 0o400)?;
    temp.publish(output)?;
    Ok(manifest)
}

fn extract(
    reader: Box<dyn Read>,
    descriptor: &BundleDescriptor,
    output: &Path,
) -> anyhow::Result<BundleManifest> {
    let mut archive = Archive::new(reader);
    // Read through tar padding, including the gzip footer; reject undeclared
    // entries in concatenated streams instead of silently ignoring them.
    archive.set_ignore_zeros(true);
    let mut entries = archive.entries()?;
    let mut first = entries.next().context("bundle is empty")??;
    if first.path()?.as_ref() != Path::new("bundle.json")
        || !first.header().entry_type().is_file()
        || first.size() > 64 * 1024 * 1024
    {
        bail!("bundle.json must be the first regular entry (at most 64 MiB)");
    }
    let manifest: BundleManifest = serde_json::from_reader(&mut first)?;
    drop(first);
    validate_manifest(&manifest)?;
    if manifest.launch_id != descriptor.launch_id
        || manifest.plan_digest != descriptor.plan_digest
        || manifest.phase != descriptor.phase
    {
        bail!("bundle launch/plan/phase binding mismatch");
    }
    let expected: BTreeMap<_, _> = manifest
        .artifacts
        .iter()
        .flat_map(|a| &a.entries)
        .map(|e| (e.path.clone(), e))
        .collect();
    let mut seen = BTreeSet::new();
    let mut links = Vec::new();
    for incoming in entries {
        let mut incoming = incoming?;
        let path = incoming
            .path()?
            .to_str()
            .context("archive paths must be UTF-8")?
            .trim_end_matches('/')
            .to_owned();
        safe_path(&path)?;
        let entry = expected
            .get(&path)
            .with_context(|| format!("undeclared archive entry {path:?}"))?;
        if !seen.insert(path.clone()) {
            bail!("duplicate archive entry {path}");
        }
        if incoming.size() != entry.size || incoming.header().mode()? != entry.mode {
            bail!("entry metadata mismatch for {path}");
        }
        let destination = output.join(&path);
        fs::create_dir_all(destination.parent().unwrap())?;
        match &entry.entry {
            EntryKind::File { sha256 } => {
                if !incoming.header().entry_type().is_file() {
                    bail!("entry type mismatch");
                }
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&destination)?;
                let n = std::io::copy(&mut incoming, &mut file)?;
                if n != entry.size || hash_file(&destination)?.0 != *sha256 {
                    bail!("file content mismatch for {path}");
                }
            }
            EntryKind::Directory => {
                if !incoming.header().entry_type().is_dir() {
                    bail!("entry type mismatch");
                }
                fs::create_dir_all(&destination)?;
            }
            EntryKind::Symlink { target } => {
                if !incoming.header().entry_type().is_symlink()
                    || incoming.link_name()?.as_deref() != Some(Path::new(target))
                {
                    bail!("symlink metadata mismatch");
                }
                links.push((destination, target));
            }
        }
    }
    if seen.len() != expected.len() {
        bail!("bundle is missing declared payload entries");
    }
    // All parents have been checked as directories; links are created last,
    // so no extraction write can traverse an archive-provided symlink.
    for (path, target) in links {
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, path)?;
        #[cfg(not(unix))]
        {
            let _ = (path, target);
            bail!("symlink extraction requires Unix");
        }
    }
    Ok(manifest)
}

pub fn verify_staging(
    output: &Path,
    descriptor: &BundleDescriptor,
) -> anyhow::Result<BundleManifest> {
    if fs::symlink_metadata(output)?.file_type().is_symlink() {
        bail!("staging root must not be a symlink");
    }
    let receipt: BundleDescriptor = read_json(&output.join("receipt.json"))?;
    if json_digest(&receipt)? != json_digest(descriptor)? {
        bail!("existing staging belongs to a different bundle");
    }
    let manifest: BundleManifest = read_json(&output.join("bundle.json"))?;
    validate_manifest(&manifest)?;
    if manifest.launch_id != descriptor.launch_id
        || manifest.plan_digest != descriptor.plan_digest
        || manifest.phase != descriptor.phase
    {
        bail!("existing staging binding mismatch");
    }
    for entry in manifest.artifacts.iter().flat_map(|a| &a.entries) {
        let path = output.join(&entry.path);
        let metadata = fs::symlink_metadata(&path)?;
        match &entry.entry {
            EntryKind::File { sha256 } => {
                if !metadata.is_file() || hash_file(&path)? != (sha256.clone(), entry.size) {
                    bail!("staged content changed: {}", entry.path);
                }
            }
            EntryKind::Directory => {
                if !metadata.is_dir() {
                    bail!("staged directory changed");
                }
            }
            EntryKind::Symlink { target } => {
                if !metadata.file_type().is_symlink() || fs::read_link(path)? != Path::new(target) {
                    bail!("staged symlink changed");
                }
            }
        }
    }
    Ok(manifest)
}
