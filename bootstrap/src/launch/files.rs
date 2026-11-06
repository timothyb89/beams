//! Merge trees without deleting unrelated content; publish each leaf atomically.
use anyhow::{bail, Context};
use beams_protocol::launch::{EntryKind, FileMode, Overwrite, PackedArtifact};
use std::fs;
use std::path::{Path, PathBuf};

pub fn install(
    artifact: &PackedArtifact,
    staging: &Path,
    target: &Path,
    mode: FileMode,
    overwrite: Overwrite,
    progress: &dyn Fn(&str),
) -> anyhow::Result<Option<String>> {
    if !target.is_absolute() || target.parent().is_none() {
        bail!("file target must be an absolute non-root path");
    }
    ensure_parents(target.parent().unwrap())?;
    let source = staging.join(&artifact.root);
    if matches!(mode, FileMode::Link) {
        if fs::symlink_metadata(target).is_ok_and(|m| m.file_type().is_symlink())
            && fs::read_link(target)? == source
        {
            return Ok(None);
        }
        let temp = sibling(target)?;
        std::os::unix::fs::symlink(source, &temp)?;
        let result = publish(&temp, target, overwrite);
        let _ = fs::remove_file(&temp);
        result?;
        return Ok(Some(format!("linked {}", target.display())));
    }
    let mut changed = 0;
    let mut directories = Vec::new();
    for entry in &artifact.entries {
        let destination = if artifact.directory {
            let suffix = entry
                .path
                .strip_prefix(&artifact.root)
                .context("entry outside artifact root")?
                .trim_start_matches('/');
            target.join(suffix)
        } else {
            target.to_owned()
        };
        match &entry.entry {
            EntryKind::Directory => {
                ensure_parents(destination.parent().unwrap())?;
                match fs::symlink_metadata(&destination) {
                    Ok(metadata) if metadata.is_dir() => {}
                    Ok(_) => {
                        let temp = sibling(&destination)?;
                        fs::create_dir(&temp)?;
                        let result = publish(&temp, &destination, overwrite);
                        let _ = fs::remove_dir(&temp);
                        result?;
                        changed += 1;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        fs::create_dir(&destination)?;
                        changed += 1;
                    }
                    Err(error) => return Err(error.into()),
                }
                directories.push((destination, entry.mode));
            }
            EntryKind::File { sha256 } => {
                ensure_parents(destination.parent().unwrap())?;
                if fs::symlink_metadata(&destination).is_ok_and(|m| m.is_file())
                    && beams_artifacts::hash_file(&destination)? == (sha256.clone(), entry.size)
                    && file_mode(&destination)? == entry.mode
                {
                    continue;
                }
                let temp = sibling(&destination)?;
                let result = (|| {
                    let mut input = beams_artifacts::open_regular(&staging.join(&entry.path))?;
                    let mut output = fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&temp)?;
                    std::io::copy(&mut input, &mut output)?;
                    output.sync_all()?;
                    if beams_artifacts::hash_file(&temp)? != (sha256.clone(), entry.size) {
                        bail!("staged source changed during installation");
                    }
                    beams_artifacts::set_mode(&temp, entry.mode)?;
                    publish(&temp, &destination, overwrite)
                })();
                let _ = fs::remove_file(temp);
                result?;
                progress(&format!("Installed {}", destination.display()));
                changed += 1;
            }
            EntryKind::Symlink { target: link } => {
                ensure_parents(destination.parent().unwrap())?;
                if fs::symlink_metadata(&destination).is_ok_and(|m| m.file_type().is_symlink())
                    && fs::read_link(&destination)? == Path::new(link)
                {
                    continue;
                }
                let temp = sibling(&destination)?;
                std::os::unix::fs::symlink(link, &temp)?;
                let result = publish(&temp, &destination, overwrite);
                let _ = fs::remove_file(temp);
                result?;
                changed += 1;
            }
        }
    }
    for (directory, mode) in directories.into_iter().rev() {
        if file_mode(&directory)? != mode {
            beams_artifacts::set_mode(&directory, mode)?;
            changed += 1;
        }
    }
    Ok((changed > 0).then(|| format!("installed {changed} entries into {}", target.display())))
}

pub fn ensure_parents(path: &Path) -> anyhow::Result<()> {
    if !path.is_absolute() {
        bail!("directory path must be absolute");
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        if matches!(component, std::path::Component::ParentDir) {
            bail!("directory path must be normalized");
        }
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => bail!(
                "destination parent is not a real directory: {}",
                current.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::create_dir(&current)?,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}
fn sibling(target: &Path) -> anyhow::Result<PathBuf> {
    Ok(target
        .parent()
        .context("target needs a parent")?
        .join(format!(".beams-install-{}", uuid::Uuid::new_v4())))
}
fn file_mode(path: &Path) -> anyhow::Result<u32> {
    use std::os::unix::fs::PermissionsExt;
    Ok(fs::symlink_metadata(path)?.permissions().mode() & 0o777)
}
fn publish(temp: &Path, target: &Path, overwrite: Overwrite) -> anyhow::Result<()> {
    let existing = match fs::symlink_metadata(target) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let backup = if let Some(metadata) = existing {
        match overwrite {
            Overwrite::Deny => bail!("target differs and overwrite=deny: {}", target.display()),
            Overwrite::Backup => {
                let backup = target.with_file_name(format!(
                    ".{}.beams-backup-{}",
                    target.file_name().unwrap().to_string_lossy(),
                    uuid::Uuid::new_v4()
                ));
                fs::rename(target, &backup)?;
                Some(backup)
            }
            Overwrite::Replace => {
                if metadata.is_dir() {
                    bail!(
                        "cannot replace a directory with a leaf: {}",
                        target.display()
                    );
                }
                None
            }
        }
    } else {
        None
    };
    if let Err(error) = fs::rename(temp, target) {
        if let Some(backup) = backup {
            if fs::symlink_metadata(target).is_err() {
                let _ = fs::rename(backup, target);
            }
        }
        return Err(error.into());
    }
    Ok(())
}
