//! Drain both pipes with bounded lines/queues. Callbacks run on the task thread.
use anyhow::{bail, Context};
use std::io::Read;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;

pub fn status(command: &mut Command, progress: &dyn Fn(&str)) -> anyhow::Result<ExitStatus> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning {}", command.get_program().to_string_lossy()))?;
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = mpsc::sync_channel(32);
    let mut read_error = None;
    std::thread::scope(|scope| {
        for (name, mut pipe) in [
            ("stdout", Box::new(stdout) as Box<dyn Read + Send>),
            ("stderr", Box::new(stderr) as Box<dyn Read + Send>),
        ] {
            let tx = tx.clone();
            scope.spawn(move || {
                let mut pending = Vec::with_capacity(4096);
                let mut buffer = [0u8; 4096];
                let flush = |pending: &mut Vec<u8>| {
                    if pending.is_empty() {
                        return;
                    }
                    let text: String = String::from_utf8_lossy(pending)
                        .chars()
                        .map(|c| if c.is_control() { ' ' } else { c })
                        .collect();
                    pending.clear();
                    let _ = tx.send(Ok(format!("{name}: {text}")));
                };
                loop {
                    match pipe.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(n) => {
                            for &byte in &buffer[..n] {
                                if byte == b'\n' || byte == b'\r' {
                                    flush(&mut pending);
                                } else {
                                    pending.push(byte);
                                    if pending.len() == 4096 {
                                        flush(&mut pending);
                                    }
                                }
                            }
                        }
                        Err(err) => {
                            let _ = tx.send(Err(err));
                            break;
                        }
                    }
                }
                flush(&mut pending);
            });
        }
        drop(tx);
        for message in rx {
            match message {
                Ok(line) => progress(&line),
                Err(err) => read_error = Some(err),
            }
        }
    });
    let status = child.wait().context("waiting for subprocess")?;
    if let Some(err) = read_error {
        return Err(err).context("reading subprocess output");
    }
    Ok(status)
}
pub fn run(command: &mut Command, progress: &dyn Fn(&str)) -> anyhow::Result<()> {
    let last = std::cell::RefCell::new(String::new());
    let status = status(command, &|line| {
        *last.borrow_mut() = line.to_owned();
        progress(line);
    })?;
    if !status.success() {
        bail!("command exited {status}: {}", last.borrow());
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn drains_both_pipes_and_bounds_long_lines_and_failure_diagnostics() {
        let lines = std::cell::RefCell::new(Vec::new());
        let mut command = Command::new("sh");
        command.args([
            "-c",
            "printf 'out\\n'; printf 'err\\n' >&2; head -c 25000 /dev/zero | tr '\\000' x; exit 7",
        ]);
        let error = run(&mut command, &|line| {
            lines.borrow_mut().push(line.to_owned())
        })
        .unwrap_err();
        assert!(error.to_string().contains("7") && error.to_string().len() < 4200);
        let lines = lines.borrow();
        assert!(lines.iter().any(|s| s == "stdout: out"));
        assert!(lines.iter().any(|s| s == "stderr: err"));
        assert!(lines.iter().all(|s| s.len() <= 4104));
    }
}
