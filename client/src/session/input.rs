//! One ordered, byte-bounded path for user input and terminal replies.
//!
//! The OS write can block indefinitely when the child stops reading. Only the
//! worker blocks; the session loop keeps handling output, local UI and signals.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

const INPUT_LIMIT: usize = 256 * 1024;
const WRITE_CHUNK: usize = 16 * 1024;

#[derive(Default)]
struct State {
    queue: VecDeque<Vec<u8>>,
    /// Includes bytes already taken by the worker but not yet written.
    outstanding: usize,
    closed: bool,
    failure: Option<io::Error>,
}

pub(super) struct ChildInput {
    shared: Arc<(Mutex<State>, Condvar)>,
}

impl ChildInput {
    pub fn new(mut writer: Box<dyn Write + Send>) -> io::Result<Self> {
        let shared = Arc::new((Mutex::new(State::default()), Condvar::new()));
        let worker = Arc::clone(&shared);
        // Never join this thread: a blocked OS write must not delay terminal
        // restoration or signal handling. Killing/exiting the child releases
        // its PTY; process teardown also closes all remaining descriptors.
        thread::Builder::new()
            .name("beams-pty-input".into())
            .spawn(move || loop {
                let (lock, ready) = &*worker;
                let bytes = {
                    let mut state = lock.lock().unwrap();
                    while state.queue.is_empty() && !state.closed {
                        state = ready.wait(state).unwrap();
                    }
                    if state.closed {
                        return;
                    }
                    state.queue.pop_front().unwrap()
                };
                let result = writer.write_all(&bytes).and_then(|()| writer.flush());
                let mut state = lock.lock().unwrap();
                state.outstanding -= bytes.len();
                if let Err(error) = result {
                    state.failure = Some(error);
                    state.closed = true;
                    state.queue.clear();
                    return;
                }
            })?;
        Ok(Self { shared })
    }

    pub fn enqueue(&self, bytes: &[u8]) -> io::Result<()> {
        let (lock, ready) = &*self.shared;
        let mut state = lock.lock().unwrap();
        check_state(&state)?;
        if bytes.len() > INPUT_LIMIT - state.outstanding {
            // An explicit session error is preferable to silently losing a
            // query reply or user input, or blocking the driver on a full queue.
            return Err(io::Error::other(
                "child PTY input exceeded 256 KiB; the child is not reading fast enough",
            ));
        }
        for chunk in bytes.chunks(WRITE_CHUNK) {
            if let Some(tail) = state
                .queue
                .back_mut()
                .filter(|tail| tail.len() + chunk.len() <= WRITE_CHUNK)
            {
                tail.extend_from_slice(chunk);
            } else {
                state.queue.push_back(chunk.to_vec());
            }
        }
        state.outstanding += bytes.len();
        ready.notify_one();
        Ok(())
    }

    pub fn check(&self) -> io::Result<()> {
        check_state(&self.shared.0.lock().unwrap())
    }
}

fn check_state(state: &State) -> io::Result<()> {
    if let Some(error) = &state.failure {
        return Err(io::Error::new(error.kind(), error.to_string()));
    }
    if state.closed {
        return Err(io::ErrorKind::BrokenPipe.into());
    }
    Ok(())
}

impl Drop for ChildInput {
    fn drop(&mut self) {
        let (lock, ready) = &*self.shared;
        let mut state = lock.lock().unwrap();
        state.closed = true;
        state.queue.clear();
        ready.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    struct GatedWriter {
        started: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
        written: mpsc::Sender<Vec<u8>>,
    }

    impl Write for GatedWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.started.send(()).unwrap();
            self.release.recv().unwrap();
            // Short writes must neither reorder nor repeat input.
            let count = bytes.len().min(3);
            self.written.send(bytes[..count].to_vec()).unwrap();
            Ok(count)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn stalled_writes_are_bounded_including_inflight_bytes_and_keep_order() {
        let (started_tx, started) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let (written_tx, written) = mpsc::channel();
        let input = ChildInput::new(Box::new(GatedWriter {
            started: started_tx,
            release: release_rx,
            written: written_tx,
        }))
        .unwrap();
        input.enqueue(b"abc").unwrap();
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        input.enqueue(b"\x1b[0n").unwrap();
        input.enqueue(b"typed").unwrap();
        let padding = vec![b'x'; INPUT_LIMIT - 12];
        input.enqueue(&padding).unwrap();
        assert!(input.enqueue(b"overflow").is_err());
        input.check().unwrap();
        let expected = [b"abc\x1b[0ntyped".as_slice(), &padding].concat();
        let mut actual = Vec::new();
        while actual.len() < expected.len() {
            release.send(()).unwrap();
            actual.extend(written.recv_timeout(Duration::from_secs(2)).unwrap());
        }
        assert_eq!(actual, expected);
    }

    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "fixture failure"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn asynchronous_write_errors_reach_the_driver() {
        let input = ChildInput::new(Box::new(FailingWriter)).unwrap();
        input.enqueue(b"input").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if let Err(error) = input.check() {
                assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
                assert!(error.to_string().contains("fixture failure"));
                assert!(input.enqueue(b"later").is_err());
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            thread::yield_now();
        }
    }

    #[test]
    fn teardown_does_not_join_a_blocked_writer() {
        let (started_tx, started) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let (written_tx, written) = mpsc::channel();
        let input = ChildInput::new(Box::new(GatedWriter {
            started: started_tx,
            release: release_rx,
            written: written_tx,
        }))
        .unwrap();
        input.enqueue(b"abc").unwrap();
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        drop(input); // Would deadlock here if teardown joined the worker.
        release.send(()).unwrap();
        assert_eq!(
            written.recv_timeout(Duration::from_secs(2)).unwrap(),
            b"abc"
        );
    }
}
