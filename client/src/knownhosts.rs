//! Answers a Beam's host key lookups (`KnownHostsQuery`) from this
//! workstation's own known_hosts, for the Beam's ssh `KnownHostsCommand`
//! (see `bootstrap/src/knownhosts.rs`). Nothing is copied into the Beam
//! wholesale: each lookup returns only the entries for the one host ssh is
//! checking, so the workstation stays the trust root and nothing goes stale.
//!
//! Lookups use `ssh-keygen -F`, which matches hashed entries and
//! `[host]:port` names, against the known_hosts files `ssh -G` resolves
//! for that host (so `UserKnownHostsFile`/`GlobalKnownHostsFile` in the
//! workstation's ssh config count). Lines are returned as written,
//! including `@cert-authority` and `@revoked` markers.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context};
use beams_protocol::{valid_known_host_name, MAX_KNOWN_HOSTS_BYTES, MAX_KNOWN_HOSTS_LINES};

/// The workstation's known_hosts entries for `host` (`name` or
/// `[name]:port`); empty when it doesn't know the host.
pub fn lookup(host: &str) -> anyhow::Result<Vec<String>> {
    if !valid_known_host_name(host) {
        bail!("not a plain host name: {host:?}");
    }
    lookup_in(host, &known_hosts_files(host))
}

/// `lookup` against explicit files (missing ones are skipped).
pub fn lookup_in(host: &str, files: &[PathBuf]) -> anyhow::Result<Vec<String>> {
    if !valid_known_host_name(host) {
        bail!("not a plain host name: {host:?}");
    }
    let mut lines: Vec<String> = Vec::new();
    let mut bytes = 0;
    for file in files.iter().filter(|f| f.is_file()) {
        let output = Command::new("ssh-keygen")
            .arg("-F")
            .arg(host)
            .arg("-f")
            .arg(file)
            .output()
            .context("running ssh-keygen -F")?;
        // 1 is "not found" (as is an unreadable file).
        if !output.status.success() {
            continue;
        }
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || lines.iter().any(|l| l == line) {
                continue;
            }
            if lines.len() == MAX_KNOWN_HOSTS_LINES || bytes + line.len() + 1 > MAX_KNOWN_HOSTS_BYTES {
                return Ok(lines);
            }
            bytes += line.len() + 1;
            lines.push(line.to_string());
        }
    }
    Ok(lines)
}

/// The user and global known_hosts files ssh would read for `host`, from
/// `ssh -G`; OpenSSH's defaults if that fails. `BEAMS_KNOWN_HOSTS_FILES`
/// (colon-separated) replaces them, e.g. for tests (ssh resolves `~` from
/// the password database, not `$HOME`).
fn known_hosts_files(host: &str) -> Vec<PathBuf> {
    if let Some(files) = std::env::var_os("BEAMS_KNOWN_HOSTS_FILES").filter(|f| !f.is_empty()) {
        return std::env::split_paths(&files).collect();
    }
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    let (name, port) = match host.strip_prefix('[').and_then(|rest| rest.split_once("]:")) {
        Some((name, port)) => (name, Some(port)),
        None => (host, None),
    };
    let mut cmd = Command::new("ssh");
    cmd.arg("-G");
    if let Some(port) = port {
        cmd.args(["-p", port]);
    }
    let resolved = cmd
        .arg("--")
        .arg(name)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success());
    let files = resolved
        .map(|output| parse_ssh_g(&String::from_utf8_lossy(&output.stdout), &home))
        .unwrap_or_default();
    if !files.is_empty() {
        return files;
    }
    [".ssh/known_hosts", ".ssh/known_hosts2"]
        .iter()
        .map(|p| home.join(p))
        .chain(["/etc/ssh/ssh_known_hosts", "/etc/ssh/ssh_known_hosts2"].map(PathBuf::from))
        .collect()
}

fn parse_ssh_g(config: &str, home: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for line in config.lines() {
        let mut words = line.split_whitespace();
        if !matches!(words.next(), Some("userknownhostsfile" | "globalknownhostsfile")) {
            continue;
        }
        for word in words.filter(|w| *w != "none") {
            let path = match word.strip_prefix("~/") {
                Some(rest) => home.join(rest),
                None => PathBuf::from(word),
            };
            if !files.contains(&path) {
                files.push(path);
            }
        }
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl";
    const OTHER: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGVhB4m3vKqU1bAo7KB9PIg8EzPb3Jm0Dz1yU9L5s8nW";

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("beams-known-hosts-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn returns_only_the_queried_hosts_entries_with_markers_and_ports() {
        let dir = temp("plain");
        let file = dir.join("known_hosts");
        fs::write(
            &file,
            format!(
                "github.com {KEY}\nexample.org {OTHER}\n@revoked github.com {OTHER}\n[git.example.com]:2222 {OTHER}\n"
            ),
        )
        .unwrap();
        let files = [file.clone(), dir.join("missing")];
        let github = lookup_in("github.com", &files).unwrap();
        assert_eq!(github, [format!("github.com {KEY}"), format!("@revoked github.com {OTHER}")]);
        assert_eq!(lookup_in("[git.example.com]:2222", &files).unwrap(), [format!("[git.example.com]:2222 {OTHER}")]);
        assert!(lookup_in("git.example.com", &files).unwrap().is_empty(), "the port is part of the name");
        assert!(lookup_in("unknown.example", &files).unwrap().is_empty());
        assert!(lookup_in("*.org", &files).is_err());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn hashed_entries_match_by_name() {
        let dir = temp("hashed");
        let file = dir.join("known_hosts");
        fs::write(&file, format!("github.com {KEY}\nexample.org {OTHER}\n")).unwrap();
        let status = Command::new("ssh-keygen").arg("-H").arg("-f").arg(&file).output().unwrap();
        assert!(status.status.success());
        assert!(!fs::read_to_string(&file).unwrap().contains("github.com"), "hashed");
        let lines = lookup_in("github.com", &[file]).unwrap();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].starts_with("|1|") && lines[0].ends_with(KEY), "{lines:?}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn ssh_g_files_expand_home_and_skip_none() {
        let home = Path::new("/home/u");
        let files = parse_ssh_g(
            "user u\nuserknownhostsfile ~/.ssh/known_hosts ~/.ssh/known_hosts2\nglobalknownhostsfile /etc/ssh/ssh_known_hosts none\n",
            home,
        );
        assert_eq!(
            files,
            [
                PathBuf::from("/home/u/.ssh/known_hosts"),
                PathBuf::from("/home/u/.ssh/known_hosts2"),
                PathBuf::from("/etc/ssh/ssh_known_hosts"),
            ]
        );
    }
}
