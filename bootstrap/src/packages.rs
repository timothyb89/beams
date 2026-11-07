//! Probe, refresh and install while holding a cross-process lock
//! (`flock(2)` on `<state>/apt.lock`), so concurrent launches and tasks
//! never run apt/dpkg at once.
use crate::{commandlog, pathutil::expand_tilde};
use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct PackageManager {
    state: PathBuf,
    sources: Vec<PathBuf>,
    lists: PathBuf,
    query: PathBuf,
    dpkg: PathBuf,
    sudo: PathBuf,
    apt: PathBuf,
    env: BTreeMap<String, String>,
}
impl Default for PackageManager {
    fn default() -> Self {
        Self {
            state: expand_tilde("~/.beam-bootstrap"),
            sources: [
                "/etc/apt/sources.list",
                "/etc/apt/sources.list.d",
                "/etc/apt/keyrings",
                "/usr/share/keyrings",
            ]
            .map(PathBuf::from)
            .to_vec(),
            lists: "/var/lib/apt/lists".into(),
            query: "dpkg-query".into(),
            dpkg: "dpkg".into(),
            sudo: "sudo".into(),
            apt: "apt-get".into(),
            env: BTreeMap::new(),
        }
    }
}
#[derive(Serialize, Deserialize)]
struct IndexRecord {
    fingerprint: String,
}

pub struct FileLock(File);
impl FileLock {
    pub fn acquire(path: &Path) -> anyhow::Result<Self> {
        fs::create_dir_all(path.parent().context("lock needs a parent")?)?;
        use std::os::unix::{fs::OpenOptionsExt, io::AsRawFd};
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        if !file.metadata()?.is_file() {
            bail!("lock must be a regular file");
        }
        loop {
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
                break;
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error.into());
            }
        }
        Ok(Self(file))
    }
}
impl Drop for FileLock {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

pub fn package_name(package: &str) -> anyhow::Result<&str> {
    let (name, version) = package
        .split_once('=')
        .map_or((package, None), |(n, v)| (n, Some(v)));
    let (base, arch) = name
        .split_once(':')
        .map_or((name, None), |(n, a)| (n, Some(a)));
    if base.len() < 2
        || !base.as_bytes()[0].is_ascii_alphanumeric()
        || !base
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"+.-".contains(&c))
        || arch.is_some_and(|a| {
            a.is_empty()
                || !a.as_bytes()[0].is_ascii_alphanumeric()
                || !a
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        })
        || version.is_some_and(|v| {
            v.is_empty()
                || !v
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b".+~:-".contains(&c))
        })
    {
        bail!("invalid Debian package {package:?}");
    }
    Ok(name)
}
impl PackageManager {
    pub fn install(
        &self,
        requested: &[String],
        progress: &dyn Fn(&str),
    ) -> anyhow::Result<Option<String>> {
        let mut packages: Vec<String> = Vec::new();
        for package in requested {
            let name = package_name(package)?;
            if let Some(prior) = packages
                .iter_mut()
                .find(|p| package_name(p).ok() == Some(name))
            {
                if prior.contains('=') && package.contains('=') && prior != package {
                    bail!("conflicting versions of {name}");
                }
                if package.contains('=') {
                    *prior = package.clone();
                }
            } else {
                packages.push(package.clone());
            }
        }
        if packages.is_empty() || self.satisfied(&packages)? {
            return Ok(None);
        }
        progress("Waiting for the apt transaction lock");
        let _lock = FileLock::acquire(&self.state.join("apt.lock"))?;
        if self.satisfied(&packages)? {
            return Ok(None);
        }
        let fingerprint = self.fingerprint()?;
        let cached: Option<IndexRecord> =
            beams_artifacts::read_json(&self.state.join("apt-index.json")).ok();
        let has_lists = fs::read_dir(&self.lists).is_ok_and(|entries| {
            entries
                .flatten()
                .any(|e| e.file_name() != "lock" && e.file_type().is_ok_and(|t| t.is_file()))
        });
        if cached.is_none_or(|c| c.fingerprint != fingerprint) || !has_lists {
            progress("Refreshing apt indexes");
            self.apt(&["update".into()], progress)?;
            beams_artifacts::write_json(
                &self.state.join("apt-index.json"),
                &IndexRecord { fingerprint },
            )?;
        }
        progress(&format!(
            "Installing package batch: {}",
            packages.join(", ")
        ));
        let mut args = vec![
            "-o".into(),
            "Dpkg::Use-Pty=0".into(),
            "install".into(),
            "-y".into(),
            "--".into(),
        ];
        args.extend(packages.clone());
        self.apt(&args, progress)?;
        if !self.satisfied(&packages)? {
            bail!("apt finished but requested packages/versions are not installed");
        }
        Ok(Some(format!("installed {}", packages.join(", "))))
    }
    fn apt(&self, args: &[String], progress: &dyn Fn(&str)) -> anyhow::Result<()> {
        commandlog::run(
            Command::new(&self.sudo)
                .arg("env")
                .arg("DEBIAN_FRONTEND=noninteractive")
                .arg(&self.apt)
                .args(args)
                .envs(&self.env),
            progress,
        )
    }
    fn satisfied(&self, packages: &[String]) -> anyhow::Result<bool> {
        let architecture = Command::new(&self.dpkg)
            .arg("--print-architecture")
            .envs(&self.env)
            .output()?;
        if !architecture.status.success() {
            bail!("dpkg could not report the native architecture");
        }
        let architecture = String::from_utf8(architecture.stdout)?.trim().to_owned();
        let names: Vec<_> = packages.iter().map(|p| package_name(p).unwrap()).collect();
        let output = Command::new(&self.query)
            .args([
                "-W",
                "-f=${Package}\t${Architecture}\t${db:Status-Status}\t${Version}\n",
                "--",
            ])
            .args(names)
            .envs(&self.env)
            .output()?;
        if !output.status.success() && output.status.code() != Some(1) {
            bail!(
                "dpkg-query failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let installed: Vec<_> = String::from_utf8(output.stdout)?
            .lines()
            .map(str::to_owned)
            .collect();
        Ok(packages.iter().all(|package| {
            let (name, version) = package
                .split_once('=')
                .map_or((package.as_str(), None), |(n, v)| (n, Some(v)));
            let (name, arch) = name
                .split_once(':')
                .map_or((name, None), |(n, a)| (n, Some(a)));
            installed.iter().any(|line| {
                let fields: Vec<_> = line.split('\t').collect();
                fields.len() == 4
                    && fields[0] == name
                    && arch.map_or(fields[1] == architecture || fields[1] == "all", |a| {
                        fields[1] == a
                    })
                    && fields[2] == "installed"
                    && version.is_none_or(|v| v == fields[3])
            })
        }))
    }
    fn fingerprint(&self) -> anyhow::Result<String> {
        fn collect(
            path: &Path,
            files: &mut Vec<(String, String)>,
            ancestors: &mut BTreeSet<PathBuf>,
        ) -> anyhow::Result<()> {
            if !path.exists() {
                files.push((path.display().to_string(), "missing".into()));
            } else if path.is_dir() {
                let canonical = path.canonicalize()?;
                if !ancestors.insert(canonical.clone()) {
                    bail!("apt input directory symlink cycle at {}", path.display());
                }
                let mut entries = fs::read_dir(path)?.collect::<Result<Vec<_>, _>>()?;
                entries.sort_by_key(|e| e.file_name());
                for entry in entries {
                    collect(&entry.path(), files, ancestors)?;
                }
                ancestors.remove(&canonical);
            } else if path.is_file() {
                files.push((
                    path.display().to_string(),
                    beams_artifacts::hash_file(&path.canonicalize()?)?.0,
                ));
            }
            Ok(())
        }
        let mut files = Vec::new();
        for path in &self.sources {
            collect(path, &mut files, &mut BTreeSet::new())?;
        }
        beams_artifacts::json_digest(&files)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use beams_artifacts::{set_mode, TemporaryDirectory};
    fn manager(root: &Path) -> PackageManager {
        fs::create_dir_all(root.join("lists")).unwrap();
        fs::write(root.join("source"), "repository").unwrap();
        fs::write(root.join("installed"), "").unwrap();
        for (name, body) in [
            ("dpkg", "printf 'amd64\\n'"),
            ("query", "cat \"$INSTALLED\""),
            ("sudo", "exec \"$@\""),
            (
                "apt",
                r#"printf '%s\n' "$*" >> "$APT_LOG"
if [ "$1" = update ]; then printf index > "$APT_LISTS/Packages"; exit 0; fi
if [ "${FAIL_INSTALL:-}" = yes ]; then exit 9; fi
while [ "$1" != -- ]; do shift; done
shift
for p in "$@"; do
    name=${p%%=*}; version=1
    if [ "$name" != "$p" ]; then version=${p#*=}; fi
    printf '%s\tamd64\tinstalled\t%s\n' "$name" "$version" >> "$INSTALLED"
done"#,
            ),
        ] {
            let path = root.join(name);
            fs::write(&path, format!("#!/bin/sh\nset -eu\n{body}\n")).unwrap();
            set_mode(&path, 0o755).unwrap();
        }
        PackageManager {
            state: root.join("state"),
            sources: vec![root.join("source")],
            lists: root.join("lists"),
            query: root.join("query"),
            dpkg: root.join("dpkg"),
            sudo: root.join("sudo"),
            apt: root.join("apt"),
            env: BTreeMap::from([
                (
                    "INSTALLED".into(),
                    root.join("installed").display().to_string(),
                ),
                ("APT_LOG".into(), root.join("calls").display().to_string()),
                ("APT_LISTS".into(), root.join("lists").display().to_string()),
            ]),
        }
    }
    #[test]
    fn batches_skip_installed_packages_refresh_once_and_refresh_after_source_changes() {
        let root = TemporaryDirectory::new(&std::env::temp_dir()).unwrap();
        let manager = manager(&root.0);
        manager
            .install(&["jq=1".into(), "gcc".into(), "jq".into()], &|_| {})
            .unwrap();
        assert!(manager
            .install(&["jq=1".into(), "gcc".into()], &|_| {})
            .unwrap()
            .is_none());
        manager.install(&["curl".into()], &|_| {}).unwrap();
        let calls = fs::read_to_string(root.0.join("calls")).unwrap();
        assert_eq!(calls.lines().filter(|l| *l == "update").count(), 1);
        assert_eq!(calls.lines().filter(|l| l.contains(" install ")).count(), 2);
        assert!(calls.contains("-- jq=1 gcc"));
        fs::write(root.0.join("source"), "new repository").unwrap();
        manager.install(&["less".into()], &|_| {}).unwrap();
        assert_eq!(
            fs::read_to_string(root.0.join("calls"))
                .unwrap()
                .lines()
                .filter(|l| *l == "update")
                .count(),
            2
        );
    }
    #[test]
    fn failures_are_reported_and_invalid_options_never_reach_apt() {
        let root = TemporaryDirectory::new(&std::env::temp_dir()).unwrap();
        let mut manager = manager(&root.0);
        assert!(manager.install(&["--bad".into()], &|_| {}).is_err());
        assert!(!root.0.join("calls").exists());
        manager.env.insert("FAIL_INSTALL".into(), "yes".into());
        assert!(manager.install(&["jq".into()], &|_| {}).is_err());
        assert!(!manager.satisfied(&["jq".into()]).unwrap());
    }
    #[test]
    fn architecture_independent_packages_and_explicit_pins_are_probed() {
        let root = TemporaryDirectory::new(&std::env::temp_dir()).unwrap();
        let manager = manager(&root.0);
        fs::write(
            root.0.join("installed"),
            "data\tall\tinstalled\t1\njq\tarm64\tinstalled\t2\n",
        )
        .unwrap();
        assert!(manager
            .install(&["data=1".into(), "jq:arm64=2".into()], &|_| {})
            .unwrap()
            .is_none());
        assert!(!root.0.join("calls").exists());
        assert!(!manager.satisfied(&["data=2".into()]).unwrap());
        assert!(!manager.satisfied(&["jq".into()]).unwrap());
    }
    #[test]
    fn source_symlinks_are_hashed_but_directory_cycles_are_rejected() {
        let root = TemporaryDirectory::new(&std::env::temp_dir()).unwrap();
        let mut manager = manager(&root.0);
        std::os::unix::fs::symlink("source", root.0.join("source-link")).unwrap();
        manager.sources = vec![root.0.join("source-link")];
        let original = manager.fingerprint().unwrap();
        fs::write(root.0.join("source"), "updated").unwrap();
        assert_ne!(original, manager.fingerprint().unwrap());
        fs::create_dir(root.0.join("sources")).unwrap();
        std::os::unix::fs::symlink(".", root.0.join("sources/cycle")).unwrap();
        manager.sources = vec![root.0.join("sources")];
        assert!(manager
            .fingerprint()
            .unwrap_err()
            .to_string()
            .contains("cycle"));
    }
    #[test]
    fn concurrent_batches_share_the_lock_and_the_index_refresh() {
        let root = TemporaryDirectory::new(&std::env::temp_dir()).unwrap();
        let manager = manager(&root.0);
        std::thread::scope(|scope| {
            let a = scope.spawn(|| manager.install(&["jq".into()], &|_| {}));
            let b = scope.spawn(|| manager.install(&["jq".into()], &|_| {}));
            let a = a.join().unwrap().unwrap();
            let b = b.join().unwrap().unwrap();
            assert!(a.is_some() != b.is_some());
        });
        let calls = fs::read_to_string(root.0.join("calls")).unwrap();
        assert_eq!(calls.lines().count(), 2);
    }
}
