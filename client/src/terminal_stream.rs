//! Bounded framing shared by the shadow model and physical-output tracking.
//!
//! Complete control sequences reach the allocating emulator only after they
//! fit the budget. On overflow the caller can restore the screen and replay
//! the retained prefix before continuing raw passthrough.

pub const CONTROL_SEQUENCE_LIMIT: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    Ground,
    Escape,
    EscapeIntermediate,
    Csi,
    Osc,
    String,
    StringEscape,
}

#[derive(Debug, Default)]
pub struct TerminalStream {
    state: State,
    utf8_pending: u8,
    pending: Vec<u8>,
    /// ESC can finish one string and begin the next control sequence. Keep
    /// that byte in the replay prefix without feeding it to the model twice.
    prefix_emitted: usize,
    discarding: bool,
    sync_open: bool,
}

impl TerminalStream {
    /// Returns bytes consumed, stopping before a control sequence exceeds its
    /// budget. The unfinished prefix is still available through `pending()`.
    pub fn advance<E>(
        &mut self,
        bytes: &[u8],
        mut frame: impl FnMut(&[u8]) -> Result<(), E>,
    ) -> Result<usize, E> {
        self.advance_guarded(bytes, |bytes, _| {
            frame(bytes)?;
            Ok(true)
        })
        .map(|(consumed, _)| consumed)
    }

    /// Stop before applying a rejected frame. Return its complete original
    /// bytes (including an earlier read's prefix) for ordered physical replay.
    pub fn advance_guarded<E>(
        &mut self,
        bytes: &[u8],
        mut frame: impl FnMut(&[u8], &[u8]) -> Result<bool, E>,
    ) -> Result<(usize, Option<Vec<u8>>), E> {
        let mut i = 0;
        while i < bytes.len() {
            if self.state == State::Ground {
                let start = i;
                while i < bytes.len() && bytes[i] != 0x1b && bytes[i] >= 0x20 {
                    i += 1;
                }
                if i > start {
                    // Only the final UTF-8 character can be unfinished.
                    // Avoid per-byte bookkeeping for long text/log bursts.
                    let tail = if i - start >= 4 {
                        self.utf8_pending = 0;
                        self.pending.clear();
                        i - 4
                    } else {
                        start
                    };
                    for &byte in &bytes[tail..i] {
                        self.text_byte(byte);
                    }
                    if !frame(&bytes[start..i], &bytes[start..i])? {
                        self.cancel_pending();
                        return Ok((i, Some(bytes[start..i].to_vec())));
                    }
                    continue;
                }
                if bytes[i] != 0x1b {
                    self.text_byte(bytes[i]);
                    if !frame(&bytes[i..i + 1], &bytes[i..i + 1])? {
                        self.cancel_pending();
                        return Ok((i + 1, Some(bytes[i..i + 1].to_vec())));
                    }
                    i += 1;
                    continue;
                }
                // ESC cancels a pending UTF-8 character in the terminal.
                self.utf8_pending = 0;
                self.pending.clear();
                self.prefix_emitted = 0;
            }

            if !self.discarding && self.pending.len() == CONTROL_SEQUENCE_LIMIT {
                return Ok((i, None));
            }
            let byte = bytes[i];
            if self.state == State::StringEscape
                && !matches!(byte, b'\\' | 0x18 | 0x1a)
                && !self.discarding
            {
                // Keep ST together so consumers can distinguish a completed
                // DCS from vte's unhook on ESC/CAN/SUB. Any other ESC command
                // aborts the string; emit its prefix before the new command.
                if !frame(&self.pending[self.prefix_emitted..], &self.pending)? {
                    let replay = self.pending.clone();
                    self.cancel_pending();
                    return Ok((i, Some(replay)));
                }
                self.pending.clear();
                self.pending.push(0x1b);
                self.prefix_emitted = 1;
                self.state = State::Escape;
            }
            if !self.discarding {
                self.pending.push(byte);
            }
            let previous = self.state;
            self.step(byte);
            i += 1;
            if byte == 0x1b
                && matches!(previous, State::Csi | State::EscapeIntermediate)
                && !self.discarding
            {
                if !frame(&self.pending[self.prefix_emitted..], &self.pending)? {
                    let replay = self.pending.clone();
                    self.cancel_pending();
                    return Ok((i, Some(replay)));
                }
                self.pending.clear();
                self.pending.push(0x1b);
                self.prefix_emitted = 1;
                self.state = State::Escape;
                continue;
            }
            if self.state == State::Ground {
                if !self.discarding {
                    if !frame(&self.pending[self.prefix_emitted..], &self.pending)? {
                        let replay = self.pending.clone();
                        self.cancel_pending();
                        return Ok((i, Some(replay)));
                    }
                }
                self.pending.clear();
                self.prefix_emitted = 0;
                self.discarding = false;
            }
        }
        Ok((i, None))
    }

    /// Observe the raw stream without retaining oversized sequence tails.
    pub fn observe(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            let consumed = self
                .advance(bytes, |_| Ok::<(), std::convert::Infallible>(()))
                .unwrap();
            bytes = &bytes[consumed..];
            if !bytes.is_empty() {
                self.discard_sequence();
            }
        }
    }

    /// Continue tracking framing after the caller has handled budget exhaustion.
    /// Suppress this sequence's model frame until its actual end/cancellation.
    pub fn discard_sequence(&mut self) {
        self.discarding = true;
    }

    pub fn pending(&self) -> &[u8] {
        &self.pending
    }

    pub fn cancel_pending(&mut self) {
        self.state = State::Ground;
        self.utf8_pending = 0;
        self.pending.clear();
        self.prefix_emitted = 0;
        self.discarding = false;
    }

    pub fn synchronized_update(&self) -> bool {
        self.sync_open
    }

    pub fn at_boundary(&self) -> bool {
        self.state == State::Ground && self.utf8_pending == 0 && !self.sync_open
    }

    fn text_byte(&mut self, byte: u8) {
        if (0x80..=0xbf).contains(&byte) && self.utf8_pending != 0 {
            self.pending.push(byte);
            self.utf8_pending -= 1;
        } else {
            self.pending.clear();
            self.utf8_pending = match byte {
                0xc2..=0xdf => 1,
                0xe0..=0xef => 2,
                0xf0..=0xf4 => 3,
                _ => 0,
            };
            if self.utf8_pending != 0 {
                self.pending.push(byte);
            }
        }
        if self.utf8_pending == 0 {
            self.pending.clear();
        }
    }

    fn step(&mut self, byte: u8) {
        if matches!(byte, 0x18 | 0x1a) {
            self.state = State::Ground;
            return;
        }
        if self.discarding
            && byte == 0x1b
            && matches!(
                self.state,
                State::Csi | State::Escape | State::EscapeIntermediate
            )
        {
            self.pending.clear();
            self.pending.push(0x1b);
            self.prefix_emitted = 0;
            self.discarding = false;
            self.state = State::Escape;
            return;
        }
        match self.state {
            State::Ground => self.state = State::Escape,
            State::Escape => self.escape(byte),
            State::EscapeIntermediate => match byte {
                0x1b => self.state = State::Escape,
                0x00..=0x1f | 0x20..=0x2f => {}
                0x30..=0x7e => self.state = State::Ground,
                _ => {}
            },
            State::Csi => match byte {
                0x1b => self.state = State::Escape,
                0x40..=0x7e => {
                    if !self.discarding && matches!(byte, b'h' | b'l') {
                        if let Some(params) = self.pending.strip_prefix(b"\x1b[?") {
                            let params = &params[..params.len() - 1];
                            if params.split(|b| *b == b';').any(|param| {
                                std::str::from_utf8(param)
                                    .ok()
                                    .and_then(|p| p.parse::<u16>().ok())
                                    == Some(2026)
                            }) {
                                self.sync_open = byte == b'h';
                            }
                        }
                    }
                    self.state = State::Ground;
                }
                _ => {}
            },
            State::Osc => match byte {
                0x07 => self.state = State::Ground,
                0x1b => self.state = State::StringEscape,
                _ => {}
            },
            State::String => {
                if byte == 0x1b {
                    self.state = State::StringEscape;
                }
            }
            State::StringEscape => {
                if byte == b'\\' {
                    self.state = State::Ground;
                } else {
                    if self.discarding {
                        // An ESC followed by another command aborts the old
                        // string and starts a new escape sequence.
                        self.pending.clear();
                        self.pending.push(0x1b);
                        self.prefix_emitted = 0;
                        if byte != 0x1b {
                            self.pending.push(byte);
                        }
                        self.discarding = false;
                    }
                    self.escape(byte);
                }
            }
        }
    }

    fn escape(&mut self, byte: u8) {
        if byte == b'c' {
            self.sync_open = false;
        }
        self.state = match byte {
            b'[' => State::Csi,
            b']' => State::Osc,
            b'P' | b'_' | b'^' | b'X' => State::String,
            0x1b => State::Escape,
            0x00..=0x1f | 0x7f..=0xff => State::Escape,
            0x20..=0x2f => State::EscapeIntermediate,
            _ => State::Ground,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing_preserves_bytes_at_every_read_split() {
        let trace = "a界\x1b[31mred\x1b]2;title\x1b\\\x1bP$qm\x1b\\\x1b(B\x18Z".as_bytes();
        for split in 0..=trace.len() {
            let mut stream = TerminalStream::default();
            let mut output = Vec::new();
            for chunk in [&trace[..split], &trace[split..]] {
                assert_eq!(
                    stream.advance(chunk, |frame| {
                        output.extend_from_slice(frame);
                        Ok::<(), ()>(())
                    }),
                    Ok(chunk.len()),
                );
            }
            assert_eq!(output, trace, "split at {split}");
            assert!(stream.at_boundary());
        }
    }

    #[test]
    fn oversized_strings_stop_before_allocating_and_do_not_print_their_tails() {
        for start in [b"\x1b]2;".as_slice(), b"\x1bP$q", b"\x1b_"] {
            let mut stream = TerminalStream::default();
            let mut bytes = start.to_vec();
            bytes.extend(std::iter::repeat_n(b'x', CONTROL_SEQUENCE_LIMIT * 4));
            bytes.extend_from_slice(b"\x1b\\after");
            let consumed = stream.advance(&bytes, |_| Ok::<(), ()>(())).unwrap();
            assert_eq!(consumed, CONTROL_SEQUENCE_LIMIT);
            assert_eq!(stream.pending(), &bytes[..consumed]);
            assert!(!stream.at_boundary());
            stream.discard_sequence();
            let mut rendered = Vec::new();
            assert_eq!(
                stream.advance(&bytes[consumed..], |frame| {
                    rendered.extend_from_slice(frame);
                    Ok::<(), ()>(())
                }),
                Ok(bytes.len() - consumed),
            );
            assert_eq!(rendered, b"after");
            assert!(stream.at_boundary());
            assert!(stream.pending().is_empty());
        }
    }

    #[test]
    fn cancellation_and_combined_sync_modes_obey_real_boundaries() {
        let mut stream = TerminalStream::default();
        stream.observe(b"\x1b]2;unfinished");
        assert!(!stream.at_boundary());
        stream.observe(b"\x18\x1b[?25;2026h");
        assert!(stream.synchronized_update());
        assert!(!stream.at_boundary());
        stream.observe(b"\x1b[?2026;25l");
        assert!(stream.at_boundary());
        stream.observe(b"\x1b[?2026h\x1bc");
        assert!(stream.at_boundary());
    }

    #[test]
    fn oversized_sequences_can_be_aborted_by_a_new_escape() {
        for start in [b"\x1b]2;".as_slice(), b"\x1bP$q", b"\x1b["] {
            let mut stream = TerminalStream::default();
            let mut bytes = start.to_vec();
            bytes.extend(std::iter::repeat_n(b'0', CONTROL_SEQUENCE_LIMIT));
            let consumed = stream.advance(&bytes, |_| Ok::<(), ()>(())).unwrap();
            stream.discard_sequence();
            stream.observe(&bytes[consumed..]);
            let mut commands = Vec::new();
            assert_eq!(
                stream.advance(b"\x1b[5nafter", |frame| {
                    commands.extend_from_slice(frame);
                    Ok::<(), ()>(())
                }),
                Ok(9),
            );
            assert_eq!(commands, b"\x1b[5nafter");
            assert!(stream.at_boundary());
        }
    }

    #[test]
    fn an_escape_ends_a_string_before_the_next_command_completes() {
        let mut stream = TerminalStream::default();
        let mut delivered = Vec::new();
        stream
            .advance(b"\x1b]52;c;?\x1b[", |frame| {
                delivered.extend_from_slice(frame);
                Ok::<(), ()>(())
            })
            .unwrap();
        assert_eq!(delivered, b"\x1b]52;c;?\x1b");
        assert_eq!(stream.pending(), b"\x1b[");
        assert!(!stream.at_boundary());
        stream
            .advance(b"5n", |frame| {
                delivered.extend_from_slice(frame);
                Ok::<(), ()>(())
            })
            .unwrap();
        assert_eq!(delivered, b"\x1b]52;c;?\x1b[5n");
        assert!(stream.at_boundary());
    }

    #[test]
    fn long_text_runs_retain_only_an_unfinished_utf8_suffix() {
        for character in ["é", "界", "🎉"] {
            let encoded = character.as_bytes();
            for split in 1..encoded.len() {
                let mut stream = TerminalStream::default();
                let mut bytes = vec![b'x'; 2048];
                bytes.extend_from_slice(&encoded[..split]);
                stream.observe(&bytes);
                assert_eq!(stream.pending(), &encoded[..split]);
                assert!(!stream.at_boundary());
                stream.observe(&encoded[split..]);
                assert!(stream.pending().is_empty());
                assert!(stream.at_boundary());
            }
        }
    }
}
