//! Reconnecting workstation-to-Beam bridge supervisor.
//!
//! One worker is owned by `beamsd` for each Beam UUID. Interactive SSH
//! frontends only add managed-session tokens and submit decisions; closing a
//! frontend does not stop this process.

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Write};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::thread;
use std::time::Duration;

use anyhow::Context;
use beams_protocol::{BeamToClient, ClientToBeam, MAX_FRAME_BYTES, PROTOCOL, PROTOCOL_VERSION};
use serde::Serialize;
use uuid::Uuid;

const COMMAND_QUEUE_CAPACITY: usize = 256;
const RETRY_MIN: Duration = Duration::from_millis(250);
const RETRY_MAX: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub beam_id: String,
    pub beam_uuid: String,
    pub login: String,
}

#[derive(Debug)]
pub enum Event {
    Connected {
        beam_uuid: String,
        bridge_id: String,
    },
    Disconnected {
        beam_uuid: String,
        message: String,
    },
    Message {
        beam_uuid: String,
        message: BeamToClient,
    },
}

pub fn spawn(
    target: Target,
    client_id: String,
    initial_token: String,
    events: SyncSender<Event>,
) -> SyncSender<ClientToBeam> {
    spawn_with_program(target, client_id, initial_token, events, "tsh".to_string())
}

fn spawn_with_program(
    target: Target,
    client_id: String,
    initial_token: String,
    events: SyncSender<Event>,
    program: String,
) -> SyncSender<ClientToBeam> {
    let (commands, command_rx) = mpsc::sync_channel(COMMAND_QUEUE_CAPACITY);
    thread::spawn(move || {
        supervise(
            program,
            target,
            client_id,
            initial_token,
            events,
            command_rx,
        )
    });
    commands
}

fn supervise(
    program: String,
    target: Target,
    client_id: String,
    initial_token: String,
    events: SyncSender<Event>,
    commands: Receiver<ClientToBeam>,
) {
    let mut tokens = BTreeSet::from([initial_token]);
    let mut retry = RETRY_MIN;

    loop {
        drain_disconnected_commands(&commands, &mut tokens);
        let bridge_id = Uuid::new_v4().to_string();
        match start_child(&program, &target) {
            Ok((mut child, mut input)) => {
                let hello = ClientToBeam::hello(
                    client_id.clone(),
                    bridge_id,
                    tokens.iter().cloned().collect(),
                );
                if write_message(&mut input, &hello).is_err() {
                    let _ = child.kill();
                    let _ = child.wait();
                } else {
                    retry = RETRY_MIN;
                    if drive_child(
                        &target,
                        &commands,
                        &mut tokens,
                        &events,
                        &mut child,
                        &mut input,
                    ) {
                        return;
                    }
                }
            }
            Err(err) => {
                let _ = events.try_send(Event::Disconnected {
                    beam_uuid: target.beam_uuid.clone(),
                    message: format!("failed to start bridge: {err:#}"),
                });
            }
        }

        match commands.recv_timeout(retry) {
            Ok(message) => retain_disconnected_command(message, &mut tokens),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
        retry = (retry * 2).min(RETRY_MAX);
    }
}

fn start_child(
    program: &str,
    target: &Target,
) -> anyhow::Result<(std::process::Child, ChildStdin)> {
    let ssh_target = format!("{}@{}", target.login, target.beam_uuid);
    let mut child = Command::new(program)
        .args(["ssh", &ssh_target, "--", "~/beams/bin/beam-client-bridge"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| {
            format!(
                "starting bridge to {} ({})",
                target.beam_id, target.beam_uuid
            )
        })?;
    let input = child.stdin.take().context("opening bridge stdin")?;
    Ok((child, input))
}

fn drive_child(
    target: &Target,
    commands: &Receiver<ClientToBeam>,
    tokens: &mut BTreeSet<String>,
    events: &SyncSender<Event>,
    child: &mut std::process::Child,
    input: &mut ChildStdin,
) -> bool {
    let Some(output) = child.stdout.take() else {
        return false;
    };
    let (reader_done_tx, reader_done_rx) = mpsc::channel();
    let beam_uuid = target.beam_uuid.clone();
    let event_tx = events.clone();
    thread::spawn(move || {
        let mut reader = BufReader::new(output);
        while let Ok(Some(message)) = read_message(&mut reader) {
            let event = match &message {
                BeamToClient::Hello {
                    protocol,
                    version,
                    bridge_id,
                } if protocol == PROTOCOL && *version == PROTOCOL_VERSION => Event::Connected {
                    beam_uuid: beam_uuid.clone(),
                    bridge_id: bridge_id.clone(),
                },
                BeamToClient::Hello {
                    protocol, version, ..
                } => {
                    let _ = event_tx.try_send(Event::Disconnected {
                        beam_uuid: beam_uuid.clone(),
                        message: format!(
                            "bridge protocol mismatch: {protocol:?} version {version}"
                        ),
                    });
                    break;
                }
                _ => Event::Message {
                    beam_uuid: beam_uuid.clone(),
                    message,
                },
            };
            if event_tx.try_send(event).is_err() {
                break;
            }
        }
        let _ = reader_done_tx.send(());
    });

    loop {
        match commands.try_recv() {
            Ok(message) => {
                if let ClientToBeam::AddSessionToken { session_token } = &message {
                    tokens.insert(session_token.clone());
                }
                if write_message(input, &message).is_err() {
                    retain_disconnected_command(message, tokens);
                    break;
                }
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                let _ = child.kill();
                let _ = child.wait();
                return true;
            }
        }
        if reader_done_rx.try_recv().is_ok() {
            break;
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                let _ = events.try_send(Event::Disconnected {
                    beam_uuid: target.beam_uuid.clone(),
                    message: format!("bridge exited with {status}"),
                });
                return false;
            }
            Ok(None) => {}
            Err(err) => {
                let _ = events.try_send(Event::Disconnected {
                    beam_uuid: target.beam_uuid.clone(),
                    message: format!("checking bridge process: {err}"),
                });
                break;
            }
        }
        thread::sleep(POLL_INTERVAL);
    }

    let _ = child.kill();
    let _ = child.wait();
    let _ = events.try_send(Event::Disconnected {
        beam_uuid: target.beam_uuid.clone(),
        message: "bridge connection closed".to_string(),
    });
    false
}

fn drain_disconnected_commands(commands: &Receiver<ClientToBeam>, tokens: &mut BTreeSet<String>) {
    while let Ok(message) = commands.try_recv() {
        retain_disconnected_command(message, tokens);
    }
}

fn retain_disconnected_command(message: ClientToBeam, tokens: &mut BTreeSet<String>) {
    if let ClientToBeam::AddSessionToken { session_token } = message {
        tokens.insert(session_token);
    }
    // Request/service state is resent as an authoritative snapshot after
    // the next hello. Replaying stale incremental updates before that
    // snapshot could target the previous bridge incarnation.
}

fn write_message<T: Serialize>(writer: &mut ChildStdin, message: &T) -> anyhow::Result<()> {
    serde_json::to_writer(&mut *writer, message).context("serializing bridge message")?;
    writer.write_all(b"\n").context("writing bridge message")?;
    writer.flush().context("flushing bridge message")
}

fn read_message<R: BufRead>(reader: &mut R) -> anyhow::Result<Option<BeamToClient>> {
    let mut frame = Vec::new();
    loop {
        let available = reader.fill_buf().context("reading bridge message")?;
        if available.is_empty() {
            return if frame.is_empty() {
                Ok(None)
            } else {
                serde_json::from_slice(&frame)
                    .context("parsing bridge message")
                    .map(Some)
            };
        }
        let end = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|index| index + 1)
            .unwrap_or(available.len());
        if frame.len() + end > MAX_FRAME_BYTES {
            anyhow::bail!("bridge message exceeds {MAX_FRAME_BYTES} byte limit");
        }
        frame.extend_from_slice(&available[..end]);
        reader.consume(end);
        if frame.last() == Some(&b'\n') {
            frame.pop();
            return serde_json::from_slice(&frame)
                .context("parsing bridge message")
                .map(Some);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Instant;
    use uuid::Uuid;

    #[test]
    fn disconnected_session_tokens_are_folded_into_the_next_hello() {
        let mut tokens = BTreeSet::from(["first".to_string()]);
        retain_disconnected_command(
            ClientToBeam::AddSessionToken {
                session_token: "second".to_string(),
            },
            &mut tokens,
        );
        assert_eq!(
            tokens.into_iter().collect::<Vec<_>>(),
            vec!["first".to_string(), "second".to_string()]
        );
    }

    #[test]
    fn bridge_frames_are_bounded() {
        let valid = serde_json::to_vec(&BeamToClient::Pong {
            nonce: "ok".to_string(),
        })
        .unwrap();
        let mut reader = BufReader::new(valid.as_slice());
        assert_eq!(
            read_message(&mut reader).unwrap(),
            Some(BeamToClient::Pong {
                nonce: "ok".to_string()
            })
        );

        let oversized = vec![b'a'; MAX_FRAME_BYTES + 1];
        let mut reader = BufReader::new(oversized.as_slice());
        assert!(read_message(&mut reader).is_err());
    }

    #[test]
    fn supervisor_reconnects_with_a_new_bridge_incarnation() {
        let dir = std::env::temp_dir().join(format!(
            "beams-bridge-test-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let fake_tsh = dir.join("fake-tsh");
        std::fs::write(
            &fake_tsh,
            r#"#!/bin/sh
IFS= read -r hello
bridge_id=$(printf '%s\n' "$hello" | sed -n 's/.*"bridge_id":"\([^"]*\)".*/\1/p')
printf '{"type":"hello","protocol":"beams-client-services","version":1,"bridge_id":"%s"}\n' "$bridge_id"
exit 1
"#,
        )
        .unwrap();
        std::fs::set_permissions(&fake_tsh, std::fs::Permissions::from_mode(0o700)).unwrap();

        let (event_tx, event_rx) = mpsc::sync_channel(32);
        let commands = spawn_with_program(
            Target {
                beam_id: "test-beam".to_string(),
                beam_uuid: "test-uuid".to_string(),
                login: "beams".to_string(),
            },
            "client-id".to_string(),
            "session-token".to_string(),
            event_tx,
            fake_tsh.display().to_string(),
        );

        let deadline = Instant::now() + Duration::from_secs(4);
        let mut connected = Vec::new();
        while connected.len() < 2 && Instant::now() < deadline {
            if let Ok(Event::Connected { bridge_id, .. }) =
                event_rx.recv_timeout(Duration::from_millis(500))
            {
                connected.push(bridge_id);
            }
        }
        assert_eq!(connected.len(), 2, "supervisor did not reconnect twice");
        assert_ne!(
            connected[0], connected[1],
            "each reconnect must have a new bridge_id"
        );

        drop(commands);
        let _ = std::fs::remove_dir_all(dir);
    }
}
