use std::fs;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{bail, Context};

/// Shared helpers for steps/tasks that install a pinned release binary -
/// `steps::zellij`/`steps::starship`/`steps::teleport_bin`, and
/// `tasks::install_toolchain_go`. Lives at the crate root (not nested
/// under `steps`) since both `steps` and `tasks` need it.
pub fn arch_target(project: &str) -> anyhow::Result<&'static str> {
    let output = Command::new("uname")
        .arg("-m")
        .output()
        .context("running uname -m")?;
    match String::from_utf8_lossy(&output.stdout).trim() {
        "x86_64" => Ok("x86_64-unknown-linux-musl"),
        "aarch64" => Ok("aarch64-unknown-linux-musl"),
        other => bail!("no known {project} release asset for architecture {other}"),
    }
}

pub fn download(url: &str, dest: &Path) -> anyhow::Result<()> {
    run(Command::new("curl").args(["-fsSL", "-o", dest.to_str().unwrap(), url]))
}

/// Compares `sha256sum(file)` against the hash in `checksum_file` (its
/// first whitespace-separated field - some projects publish bare hashes,
/// others `<hash>  <path>` à la `sha256sum -c`).
pub fn verify_checksum(file: &Path, checksum_file: &Path, label: &str) -> anyhow::Result<()> {
    let expected = fs::read_to_string(checksum_file)
        .context("reading downloaded checksum file")?
        .split_whitespace()
        .next()
        .context("checksum file was empty")?
        .to_string();
    verify_checksum_str(file, &expected, label)
}

/// Same as `verify_checksum`, but against a checksum already known as a
/// string (e.g. pinned as a source constant, or pulled from a release
/// manifest JSON) rather than a downloaded sidecar file - not every
/// project publishes one of those (go.dev doesn't; it only lists
/// checksums in its own JSON release manifest).
pub fn verify_checksum_str(file: &Path, expected: &str, label: &str) -> anyhow::Result<()> {
    let output = Command::new("sha256sum")
        .arg(file)
        .output()
        .context("running sha256sum")?;
    let actual = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string();

    if actual != expected {
        bail!("{label} checksum mismatch: expected {expected}, got {actual}");
    }
    Ok(())
}

pub fn run(cmd: &mut Command) -> anyhow::Result<()> {
    let output = cmd.output().with_context(|| format!("running {cmd:?}"))?;
    if !output.status.success() {
        bail!(
            "{:?} failed ({}): {}",
            cmd,
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Same contract as `run`, but for a command whose stderr is where
/// incremental progress shows up (e.g. `git clone --progress`) -
/// `on_line` is called once per update, in order, as the child produces
/// them, *before* the overall success/failure of the call is known. The
/// last update (if any) is also what ends up in the error message on
/// failure, same as `run`'s `output.stderr` does.
///
/// Deliberately reads raw bytes and splits on *either* `\r` or `\n`,
/// not `std::io::BufRead::lines()` (which only splits on `\n`): tools
/// like `git --progress` report each percent-complete tick by writing
/// `\r` and reusing the same terminal line, with a real `\n` only at the
/// very end of a whole phase ("Receiving objects: 100% (23/23), done.")
/// - `lines()` would buffer every `\r`-separated update from one phase
/// into a single giant concatenated "line" instead of calling `on_line`
/// once per update, which is exactly the bug this replaced (confirmed
/// live: a real clone's `on_line` calls were one-per-*phase*, each a
/// huge string of every tick glued together, not one-per-tick).
pub fn run_streaming(cmd: &mut Command, on_line: &dyn Fn(&str)) -> anyhow::Result<()> {
    let mut child = cmd
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning {cmd:?}"))?;

    let mut stderr = child.stderr.take().expect("stderr was piped above");
    let mut pending = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut last_line = String::new();

    let mut flush = |pending: &mut Vec<u8>| {
        if pending.is_empty() {
            return;
        }
        let text = String::from_utf8_lossy(pending).trim().to_string();
        pending.clear();
        if !text.is_empty() {
            on_line(&text);
            last_line = text;
        }
    };

    loop {
        let n = stderr.read(&mut chunk).with_context(|| format!("reading stderr of {cmd:?}"))?;
        if n == 0 {
            break;
        }
        for &byte in &chunk[..n] {
            if byte == b'\r' || byte == b'\n' {
                flush(&mut pending);
            } else {
                pending.push(byte);
            }
        }
    }
    flush(&mut pending);

    let status = child.wait().with_context(|| format!("waiting for {cmd:?}"))?;
    if !status.success() {
        bail!("{:?} failed ({status}): {last_line}", cmd);
    }
    Ok(())
}
