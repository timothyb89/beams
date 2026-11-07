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
    let output = cmd.output().with_context(|| format!("running {}", describe(cmd)))?;
    if !output.status.success() {
        let mut tail = StderrTail::default();
        tail.feed(&output.stderr, &|_| {});
        tail.finish(&|_| {});
        return Err(command_failed(cmd, &output.status, &tail));
    }
    Ok(())
}

/// A command as `program arg...`, quoting only arguments that need it.
/// Unlike `Command`'s `Debug`, this leaves out the environment: a
/// relay-tagged command's `BEAMS_TASK_RUN=... SSH_AUTH_SOCK=...` prefix
/// otherwise fills the TUI's one-line detail before the program name.
pub fn describe(cmd: &Command) -> String {
    std::iter::once(cmd.get_program())
        .chain(cmd.get_args())
        .map(|part| {
            let part = part.to_string_lossy();
            let plain = !part.is_empty()
                && part.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:=@%+,~".contains(c));
            if plain {
                part.into_owned()
            } else {
                format!("'{}'", part.replace('\'', r"'\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// How many trailing stderr lines a failure keeps.
const TAIL_LINES: usize = 4;
/// Longest single stderr line a failure keeps, in characters.
const TAIL_LINE_CHARS: usize = 300;

/// Splits a child's stderr into updates and keeps the last few real
/// lines for a failure message. A segment ended by `\r` is a transient
/// progress tick (`git --progress`, curl): reported, never kept, so the
/// tail holds the actual diagnostics ("Host key verification failed.")
/// rather than the last percentage.
#[derive(Default)]
struct StderrTail {
    pending: Vec<u8>,
    /// A `\r` ended `pending`; whether it was a tick depends on the next
    /// byte, since ssh ends its messages with `\r\n`.
    after_cr: bool,
    lines: std::collections::VecDeque<String>,
}

impl StderrTail {
    fn feed(&mut self, bytes: &[u8], on_line: &dyn Fn(&str)) {
        for &byte in bytes {
            if std::mem::take(&mut self.after_cr) {
                // `\r\n` is a line ending; a lone `\r` ended a tick.
                self.flush(byte == b'\n', on_line);
                if byte == b'\n' {
                    continue;
                }
            }
            match byte {
                b'\r' => self.after_cr = true,
                b'\n' => self.flush(true, on_line),
                _ => self.pending.push(byte),
            }
        }
    }

    /// Flushes the final segment, which counts as a line however it ended.
    fn finish(&mut self, on_line: &dyn Fn(&str)) {
        self.after_cr = false;
        self.flush(true, on_line);
    }

    fn flush(&mut self, keep: bool, on_line: &dyn Fn(&str)) {
        if self.pending.is_empty() {
            return;
        }
        let text = String::from_utf8_lossy(&self.pending).trim().to_string();
        self.pending.clear();
        if text.is_empty() {
            return;
        }
        on_line(&text);
        if keep {
            if self.lines.len() == TAIL_LINES {
                self.lines.pop_front();
            }
            self.lines.push_back(match text.char_indices().nth(TAIL_LINE_CHARS) {
                Some((cut, _)) => format!("{}…", &text[..cut]),
                None => text,
            });
        }
    }

    fn summary(&self) -> String {
        if self.lines.is_empty() {
            return "no output on stderr".to_string();
        }
        Vec::from_iter(self.lines.iter().map(String::as_str)).join("; ")
    }
}

/// One-line failure, most useful part first: the TUI shows a task's
/// message as a single unwrapped line, so the short label (program and
/// subcommand) and stderr come before the full command.
fn command_failed(cmd: &Command, status: &std::process::ExitStatus, tail: &StderrTail) -> anyhow::Error {
    let program = Path::new(cmd.get_program())
        .file_name()
        .unwrap_or(cmd.get_program())
        .to_string_lossy()
        .into_owned();
    let label = match cmd.get_args().next().map(|a| a.to_string_lossy()) {
        Some(sub) if !sub.starts_with('-') => format!("{program} {sub}"),
        _ => program,
    };
    anyhow::anyhow!("{label} failed ({status}): {} [{}]", tail.summary(), describe(cmd))
}

/// Same contract as `run`, but for a command whose stderr is where
/// incremental progress shows up (e.g. `git clone --progress`) -
/// `on_line` is called once per update, in order, as the child produces
/// them, *before* the overall success/failure of the call is known. On
/// failure the error carries the last few newline-terminated stderr
/// lines (see `StderrTail`), same as `run`.
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
        .with_context(|| format!("spawning {}", describe(cmd)))?;

    let mut stderr = child.stderr.take().expect("stderr was piped above");
    let mut tail = StderrTail::default();
    let mut chunk = [0u8; 4096];
    loop {
        let n = stderr
            .read(&mut chunk)
            .with_context(|| format!("reading stderr of {}", describe(cmd)))?;
        if n == 0 {
            break;
        }
        tail.feed(&chunk[..n], on_line);
    }
    tail.finish(on_line);

    let status = child.wait().with_context(|| format!("waiting for {}", describe(cmd)))?;
    if !status.success() {
        return Err(command_failed(cmd, &status, &tail));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn sh(script: &str) -> Command {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", script]);
        cmd
    }

    #[test]
    fn describe_omits_the_environment_and_quotes_only_when_needed() {
        let mut cmd = Command::new("git");
        cmd.env("BEAMS_TASK_RUN", "manifest-0/clone")
            .args(["clone", "git@github.com:o/r.git", "/home/x/my repo", "it's"]);
        assert_eq!(describe(&cmd), r"git clone git@github.com:o/r.git '/home/x/my repo' 'it'\''s'");
    }

    #[test]
    fn run_streaming_failure_leads_with_label_and_real_stderr_not_progress_or_env() {
        // Mirrors a git clone failing host key verification (ssh ends its
        // line with `\r\n`).
        let mut cmd = sh("printf 'Cloning into x...\\n10%%\\r50%%\\rHost key verification failed.\\r\\nfatal: Could not read from remote repository.\\n\\nPlease make sure you have the correct access rights\\nand the repository exists.\\n' >&2; exit 128");
        cmd.env("BEAMS_TASK_RUN", "manifest-0/clone");
        let seen = RefCell::new(Vec::new());
        let err = run_streaming(&mut cmd, &|line| seen.borrow_mut().push(line.to_string())).unwrap_err();
        let message = err.to_string();

        assert!(message.starts_with("sh failed (exit status: 128): Host key verification failed.; fatal: Could not read"), "{message}");
        assert!(message.contains("and the repository exists. [sh -c "), "{message}");
        assert!(!message.contains("BEAMS_TASK_RUN"), "{message}");
        assert!(!message.split(" [").next().unwrap().contains("50%"), "{message}");
        // Progress ticks are still reported live.
        assert!(seen.borrow().iter().any(|l| l == "50%"));
    }

    #[test]
    fn run_failure_with_silent_stderr_says_so() {
        let err = run(&mut sh("exit 3")).unwrap_err().to_string();
        assert!(err.starts_with("sh failed (exit status: 3): no output on stderr [sh -c 'exit 3']"), "{err}");
    }

    #[test]
    fn unterminated_final_stderr_line_is_kept_and_long_lines_are_cut() {
        let long = "e".repeat(TAIL_LINE_CHARS + 50);
        let err = run(&mut sh(&format!("printf '{long}' >&2; exit 1"))).unwrap_err().to_string();
        // The summary only; the full command after it repeats `long`.
        let err = err.split(" [").next().unwrap();
        assert!(err.contains(&format!("{}…", "e".repeat(TAIL_LINE_CHARS))), "{err}");
        assert!(!err.contains(&"e".repeat(TAIL_LINE_CHARS + 1)), "{err}");
    }
}
