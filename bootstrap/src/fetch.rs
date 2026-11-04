//! `beams fetch PATH... [DEST]`: copy files or directories from this Beam
//! to the workstation running `beams ssh`.
//!
//! The Beam only *proposes* the copy. The workstation's local menu shows the
//! request, and the workstation user confirms (or edits) where it lands
//! before anything is transferred. `DEST` is just the initial suggestion.
//! The command waits, printing progress, until the copy finishes or is
//! rejected; Ctrl-C withdraws the request (the daemon notices this
//! connection closing - see `daemon::ConnectionContext::tied_requests`).
//!
//! Installed as `~/beams/bin/beams`, which runs this binary under the name
//! `beams`; `main` dispatches on that.

use std::io::{IsTerminal, Write};
use std::path::{Component, Path, PathBuf};

use beams_protocol::{FetchSource, OverwritePolicy, ServiceRequest, ServiceRequestKind, TransferProgress};
use clap::{Args, Parser, Subcommand};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::pathutil::expand_tilde;

/// Beam-side helpers for the workstation running `beams ssh`.
#[derive(Parser)]
#[command(name = "beams")]
pub struct BeamsCli {
    #[command(subcommand)]
    command: BeamsCommand,
}

#[derive(Subcommand)]
enum BeamsCommand {
    /// Copy files or directories from this Beam to your workstation. Your
    /// workstation shows the request; you pick (or confirm) where it goes
    /// there before anything is copied.
    Fetch(FetchArgs),
}

#[derive(Args)]
pub struct FetchArgs {
    /// Files or directories in this Beam (your shell expands globs),
    /// optionally followed by a destination on the workstation. A last
    /// argument that doesn't exist here is taken as the destination; a
    /// relative destination is relative to where `beams ssh` was started.
    #[arg(required = true, value_name = "PATH")]
    paths: Vec<String>,
    /// The suggested workstation destination (instead of a trailing PATH).
    #[arg(short = 't', long = "to", value_name = "DEST")]
    to: Option<String>,
    /// Replace existing files at the destination.
    #[arg(long)]
    overwrite: bool,
    /// The daemon socket (for tests).
    #[arg(long, hide = true)]
    socket: Option<String>,
}

pub fn run_beams_cli() -> i32 {
    match BeamsCli::parse().command {
        BeamsCommand::Fetch(args) => run_fetch(args),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Plan {
    pub sources: Vec<FetchSource>,
    pub destination: Option<String>,
}

/// Works out what to fetch: which arguments are sources (made absolute,
/// sized) and which, if any, is the suggested destination.
pub fn plan(paths: &[String], to: Option<&str>, cwd: &Path, home: Option<&Path>) -> Result<Plan, String> {
    let mut sources: Vec<&String> = paths.iter().collect();
    let mut destination = to.map(str::to_string);
    if destination.is_none() && sources.len() >= 2 {
        let last = sources[sources.len() - 1];
        if std::fs::symlink_metadata(absolute(cwd, last)).is_err() {
            destination = sources.pop().cloned();
        }
    }
    let mut planned = Vec::new();
    for source in sources {
        let path = absolute(cwd, source);
        let meta = std::fs::metadata(&path).map_err(|err| format!("{source}: {err}"))?;
        let size = if meta.is_dir() { tree_size(&path) } else { meta.len() };
        planned.push(FetchSource { path: path.to_string_lossy().into_owned(), size, is_dir: meta.is_dir() });
    }
    let mut names = std::collections::HashSet::new();
    for source in &planned {
        let name = Path::new(&source.path).file_name().map(|name| name.to_string_lossy().into_owned());
        match name {
            Some(name) if names.insert(name.clone()) => {}
            Some(name) => return Err(format!("two sources are both named {name:?}; fetch them separately")),
            None => return Err(format!("{}: has no file name", source.path)),
        }
    }
    Ok(Plan { sources: planned, destination: destination.map(|dest| workstation_hint(&dest, home)) })
}

/// `path` against `cwd`, with `.` and `..` resolved lexically (the remote
/// side needs a stable absolute path, not a canonicalized symlink target).
fn absolute(cwd: &Path, path: &str) -> PathBuf {
    let joined = cwd.join(expand_tilde(path));
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// Total bytes of regular files under `dir`, not following symlinks.
fn tree_size(dir: &Path) -> u64 {
    let mut total = 0;
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let Ok(meta) = entry.path().symlink_metadata() else { continue };
            if meta.is_dir() {
                pending.push(entry.path());
            } else if meta.is_file() {
                total += meta.len();
            }
        }
    }
    total
}

/// The destination as a workstation path. The shell here expanded `~` to
/// the Beam's home, which means nothing on the workstation, so put it back.
fn workstation_hint(dest: &str, home: Option<&Path>) -> String {
    if let Some(home) = home.filter(|home| !home.as_os_str().is_empty()) {
        if let Ok(rest) = Path::new(dest).strip_prefix(home) {
            let mut hint = if rest.as_os_str().is_empty() {
                "~".to_string()
            } else {
                format!("~/{}", rest.display())
            };
            if dest.ends_with('/') && !hint.ends_with('/') {
                hint.push('/');
            }
            return hint;
        }
    }
    dest.to_string()
}

pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else if value < 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

fn describe(sources: &[FetchSource]) -> String {
    let total: u64 = sources.iter().map(|source| source.size).sum();
    match sources {
        [source] => format!(
            "{}{} ({})",
            Path::new(&source.path).file_name().unwrap_or_default().to_string_lossy(),
            if source.is_dir { "/" } else { "" },
            human_bytes(total)
        ),
        _ => format!("{} items ({})", sources.len(), human_bytes(total)),
    }
}

fn progress_line(progress: &TransferProgress) -> String {
    let percent = if progress.bytes_total == 0 {
        100
    } else {
        (progress.bytes_done.min(progress.bytes_total) * 100 / progress.bytes_total) as u32
    };
    let mut line = format!(
        "{percent:>3}%  {} / {}",
        human_bytes(progress.bytes_done.min(progress.bytes_total)),
        human_bytes(progress.bytes_total)
    );
    if progress.files_total > 1 {
        line.push_str(&format!("  ·  {}/{} items", progress.files_done, progress.files_total));
    }
    line
}

pub fn run_fetch(args: FetchArgs) -> i32 {
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(err) => {
            eprintln!("beams: can't read the current directory: {err}");
            return 1;
        }
    };
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let plan = match plan(&args.paths, args.to.as_deref(), &cwd, home.as_deref()) {
        Ok(plan) => plan,
        Err(err) => {
            eprintln!("beams: {err}");
            return 1;
        }
    };
    let session_token = match std::env::var("BEAMS_SESSION_TOKEN") {
        Ok(token) if !token.is_empty() => token,
        _ => {
            eprintln!(
                "beams: this shell isn't connected to a workstation; reconnect with `beams ssh`, then retry"
            );
            return 1;
        }
    };
    let summary = describe(&plan.sources);
    let request = ServiceRequest {
        request_id: Uuid::new_v4().to_string(),
        kind: ServiceRequestKind::FileFetch {
            sources: plan.sources,
            local_path_hint: plan.destination,
            overwrite: if args.overwrite { OverwritePolicy::Replace } else { OverwritePolicy::Deny },
        },
    };
    let socket = expand_tilde(&args.socket.unwrap_or_else(crate::daemon::default_socket_path));
    let mut conn = match crate::taskcli::Connection::connect(&socket) {
        Ok(conn) => conn,
        Err(err) => {
            eprintln!("beams: {err:#}");
            return 1;
        }
    };
    if let Err(err) = conn.send(&json!({
        "request_id": "fetch",
        "op": "request_service",
        "session_token": session_token,
        "client_id": std::env::var("BEAMS_CLIENT_ID").ok(),
        "request": request,
    })) {
        eprintln!("beams: {err:#}");
        return 1;
    }

    // On a terminal, transient lines (waiting, progress) overwrite each
    // other in place; otherwise only lasting ones are printed.
    let interactive = std::io::stderr().is_terminal();
    let show = |line: &str, transient: bool| {
        let mut stderr = std::io::stderr().lock();
        if interactive {
            let _ = write!(stderr, "\r\x1b[K{line}");
            if !transient {
                let _ = writeln!(stderr);
            }
        } else if !transient {
            let _ = writeln!(stderr, "{line}");
        }
        let _ = stderr.flush();
    };
    let mut last_files_done = None;
    loop {
        let value = match conn.recv() {
            Ok(Some(value)) => value,
            Ok(None) => {
                show("beams: the Beam daemon closed the connection", false);
                return 1;
            }
            Err(err) => {
                show(&format!("beams: {err:#}"), false);
                return 1;
            }
        };
        match value.get("type").and_then(Value::as_str) {
            Some("error") if value.get("request_id").and_then(Value::as_str) == Some("fetch") => {
                let message = value.get("message").and_then(Value::as_str).unwrap_or("unknown error");
                if message.contains("unknown variant `file_fetch`") {
                    show(
                        "beams: this Beam's daemon (beam-bootstrapd) predates `beams fetch`; it works once the daemon restarts on the updated build",
                        false,
                    );
                } else {
                    show(&format!("beams: {message}"), false);
                }
                return 1;
            }
            Some("service_request_accepted") => {
                show(
                    &format!("beams: fetch {summary}: waiting for approval on your workstation (Ctrl-C to withdraw)"),
                    interactive,
                );
            }
            Some("service_request_status") => {
                let Some(projection) = value.get("request") else { continue };
                if projection.pointer("/request/request_id").and_then(Value::as_str) != Some(&request.request_id) {
                    continue;
                }
                let message = projection.get("message").and_then(Value::as_str);
                match projection.get("status").and_then(Value::as_str) {
                    Some("rejected") => {
                        show("beams: rejected on your workstation", false);
                        return 1;
                    }
                    Some("expired") => {
                        show("beams: nobody approved the fetch in time", false);
                        return 1;
                    }
                    Some("failed") => {
                        show(&format!("beams: fetch failed: {}", message.unwrap_or("unknown error")), false);
                        return 1;
                    }
                    Some("approved") => show("beams: approved; copying…", true),
                    _ => {}
                }
            }
            Some("client_service_status" | "client_services_snapshot") => {
                let Some(service) = crate::projected_service_for_request(&value, &request.request_id) else {
                    continue;
                };
                let progress: Option<TransferProgress> =
                    service.get("progress").and_then(|progress| serde_json::from_value(progress.clone()).ok());
                let message = service.get("message").and_then(Value::as_str);
                match service.get("status").and_then(Value::as_str) {
                    Some("running" | "starting") => {
                        if let Some(progress) = &progress {
                            if interactive {
                                show(&progress_line(progress), true);
                            } else if last_files_done != Some(progress.files_done) {
                                last_files_done = Some(progress.files_done);
                                show(&progress_line(progress), false);
                            }
                        }
                    }
                    Some("stopped") => {
                        let complete = progress.is_some_and(|p| p.files_done == p.files_total);
                        if complete {
                            show(&format!("beams: fetched {summary}"), false);
                            return 0;
                        }
                        show(&format!("beams: {}", message.unwrap_or("the fetch was cancelled on your workstation")), false);
                        return 1;
                    }
                    Some("error") => {
                        show(&format!("beams: fetch failed: {}", message.unwrap_or("unknown error")), false);
                        return 1;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("beams-fetch-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("logs/nested")).unwrap();
        std::fs::write(dir.join("report.csv"), "a,b\n").unwrap();
        std::fs::write(dir.join("logs/one.log"), "12345").unwrap();
        std::fs::write(dir.join("logs/nested/two.log"), "123").unwrap();
        dir
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn plan_makes_sources_absolute_and_sizes_directories() {
        let dir = scratch("sizes");
        let planned = plan(&strings(&["report.csv", "./logs/../logs"]), None, &dir, None).unwrap();
        assert_eq!(planned.destination, None, "both arguments exist, so both are sources");
        assert_eq!(
            planned.sources,
            vec![
                FetchSource { path: format!("{}/report.csv", dir.display()), size: 4, is_dir: false },
                FetchSource { path: format!("{}/logs", dir.display()), size: 8, is_dir: true },
            ]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_missing_last_argument_is_the_destination() {
        let dir = scratch("dest");
        let home = dir.clone();
        let planned = plan(&strings(&["report.csv", &format!("{}/Downloads/", dir.display())]), None, &dir, Some(&home))
            .unwrap();
        assert_eq!(planned.sources.len(), 1);
        assert_eq!(planned.destination.as_deref(), Some("~/Downloads/"), "the Beam's $HOME becomes ~");

        let planned = plan(&strings(&["report.csv"]), Some("out"), &dir, Some(&home)).unwrap();
        assert_eq!(planned.destination.as_deref(), Some("out"));

        let err = plan(&strings(&["missing.txt"]), None, &dir, None).unwrap_err();
        assert!(err.starts_with("missing.txt:"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn duplicate_names_are_refused() {
        let dir = scratch("dupes");
        std::fs::write(dir.join("logs/report.csv"), "").unwrap();
        let err = plan(&strings(&["report.csv", "logs/report.csv"]), Some("x"), &dir, None).unwrap_err();
        assert!(err.contains("report.csv"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sizes_and_progress_read_naturally() {
        assert_eq!(human_bytes(999), "999 B");
        assert_eq!(human_bytes(2_100_000), "2.1 MB");
        assert_eq!(human_bytes(14_200_000), "14 MB");
        let progress = TransferProgress { bytes_done: 41, bytes_total: 100, files_done: 1, files_total: 3 };
        assert_eq!(progress_line(&progress), " 41%  41 B / 100 B  ·  1/3 items");
    }
}
