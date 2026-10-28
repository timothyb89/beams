//! Stdio-to-UDS adapter used by the workstation daemon's long-lived SSH
//! bridge. It intentionally contains no service implementation or approval
//! logic: it only translates typed shared-protocol messages to operations on
//! the in-VM daemon socket.

use std::io::{self, BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc;
use std::thread;

use anyhow::{bail, Context};
use beams_protocol::{BeamToClient, ClientToBeam, MAX_FRAME_BYTES};
use serde::Serialize;
use serde_json::{json, Value};

pub fn run(socket_path: &Path) -> anyhow::Result<()> {
    let stream =
        UnixStream::connect(socket_path).with_context(|| format!("connecting to {}", socket_path.display()))?;
    let read_half = stream.try_clone().context("cloning daemon socket")?;
    let (error_tx, error_rx) = mpsc::channel();

    let input_thread = thread::spawn(move || {
        let result = forward_stdin(stream);
        if let Err(err) = result {
            let _ = error_tx.send(err);
        }
    });

    let mut daemon = BufReader::new(read_half);
    let mut stdout = io::stdout().lock();
    while let Some(value) = read_json_value(&mut daemon).context("reading VM daemon")? {
        match value.get("type").and_then(Value::as_str) {
            Some("bridge_registered") | Some("client_bridge_event") => {
                let message = value.get("message").cloned().unwrap_or(Value::Null);
                let typed: BeamToClient = serde_json::from_value(message).context("parsing VM daemon bridge event")?;
                write_json(&mut stdout, &typed)?;
            }
            Some("error") => {
                let message = value.get("message").and_then(Value::as_str).unwrap_or("unknown VM daemon error");
                write_json(&mut stdout, &BeamToClient::Error { message: message.to_string() })?;
                if value.get("request_id").and_then(Value::as_str) == Some("bridge-register") {
                    bail!("VM daemon rejected bridge registration: {message}");
                }
            }
            _ => {}
        }
    }

    let _ = input_thread.join();
    match error_rx.try_recv() {
        Ok(err) => Err(err),
        Err(_) => Ok(()),
    }
}

fn forward_stdin(mut daemon: UnixStream) -> anyhow::Result<()> {
    let stdin = io::stdin();
    let mut input = BufReader::new(stdin.lock());
    let Some(first) = read_typed::<ClientToBeam, _>(&mut input)? else {
        return Ok(());
    };
    if !matches!(first, ClientToBeam::Hello { .. }) {
        bail!("first workstation bridge message must be hello");
    }
    write_json(
        &mut daemon,
        &json!({
            "request_id": "bridge-register",
            "op": "register_client_bridge",
            "message": first,
        }),
    )?;

    let mut counter = 0u64;
    while let Some(message) = read_typed::<ClientToBeam, _>(&mut input)? {
        counter += 1;
        write_json(
            &mut daemon,
            &json!({
                "request_id": format!("bridge-message-{counter}"),
                "op": "client_bridge_message",
                "message": message,
            }),
        )?;
    }
    let _ = daemon.shutdown(Shutdown::Write);
    Ok(())
}

fn read_typed<T: for<'de> serde::Deserialize<'de>, R: BufRead>(reader: &mut R) -> anyhow::Result<Option<T>> {
    let Some(frame) = read_frame(reader)? else {
        return Ok(None);
    };
    serde_json::from_slice(&frame).context("parsing workstation bridge message").map(Some)
}

fn read_json_value<R: BufRead>(reader: &mut R) -> anyhow::Result<Option<Value>> {
    let Some(frame) = read_frame(reader)? else {
        return Ok(None);
    };
    serde_json::from_slice(&frame).context("parsing VM daemon message").map(Some)
}

fn read_frame<R: BufRead>(reader: &mut R) -> anyhow::Result<Option<Vec<u8>>> {
    let mut frame = Vec::new();
    loop {
        let available = reader.fill_buf().context("reading bridge message")?;
        if available.is_empty() {
            return if frame.is_empty() { Ok(None) } else { Ok(Some(frame)) };
        }
        let end = available.iter().position(|byte| *byte == b'\n').map(|index| index + 1).unwrap_or(available.len());
        if frame.len() + end > MAX_FRAME_BYTES {
            bail!("bridge message exceeds {MAX_FRAME_BYTES} byte limit");
        }
        frame.extend_from_slice(&available[..end]);
        reader.consume(end);
        if frame.last() == Some(&b'\n') {
            frame.pop();
            return Ok(Some(frame));
        }
    }
}

fn write_json<W: Write, T: Serialize>(writer: &mut W, value: &T) -> anyhow::Result<()> {
    serde_json::to_writer(&mut *writer, value).context("serializing bridge message")?;
    writer.write_all(b"\n").context("writing bridge message")?;
    writer.flush().context("flushing bridge message")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn bridge_frames_are_bounded() {
        let mut valid = Cursor::new(vec![b'a'; MAX_FRAME_BYTES - 1]);
        assert_eq!(read_frame(&mut valid).unwrap().unwrap().len(), MAX_FRAME_BYTES - 1);

        let mut oversized = Cursor::new(vec![b'a'; MAX_FRAME_BYTES + 1]);
        assert!(read_frame(&mut oversized).is_err());
    }
}
