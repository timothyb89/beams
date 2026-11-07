//! Conservative evidence and an explicit supported output inventory.
//!
//! A cursor report or buffer selection never proves inherited cells/modes.
//! RIS establishes shared state. A resized buffer can subsequently establish
//! its geometry explicitly; inherited/unsupported state still requires RIS.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unavailable {
    InheritedState,
    UnsupportedSequence,
    Resize,
    SequenceBudget,
}

#[derive(Clone, Copy, Default)]
struct BufferEvidence {
    cells: bool,
    cursor: bool,
    origin: bool,
    margins: bool,
    saved: bool,
}

impl BufferEvidence {
    fn complete(self) -> bool {
        self.cells && self.cursor && self.origin && self.margins && self.saved
    }
}

pub struct Evidence {
    // Resize remains the last loss even after a buffer recovers, so switches
    // cannot silently reuse geometry whose cross-emulator semantics differ.
    reason: Option<Unavailable>,
    buffers: [BufferEvidence; 2],
}

impl Default for Evidence {
    fn default() -> Self {
        Self {
            reason: Some(Unavailable::InheritedState),
            buffers: [BufferEvidence::default(); 2],
        }
    }
}

impl Evidence {
    pub fn observe(&mut self, frame: &[u8], before_alternate: bool, alternate: bool) {
        if frame == b"\x1bc" {
            self.reason = None;
            self.buffers = [BufferEvidence {
                cells: true,
                cursor: true,
                origin: true,
                margins: true,
                saved: true,
            }; 2];
            return;
        }
        // A geometry recipe cannot reset an unknown font, mode or protocol.
        if !matches!(self.reason, None | Some(Unavailable::Resize)) {
            return;
        }
        if self.reason == Some(Unavailable::Resize)
            && (before_alternate != alternate
                || private_modes(frame)
                    .is_some_and(|(_, modes)| mode_numbers(modes).any(|m| m == Some(1049))))
        {
            // Buffer switches can transfer cursors and clear the alternate
            // buffer differently across emulators. Do not carry recovered
            // geometry through a switch; require initialization in its target.
            self.buffers[usize::from(alternate)] = BufferEvidence::default();
            if before_alternate {
                self.buffers[1] = BufferEvidence::default();
            }
            return;
        }

        let buffer = &mut self.buffers[usize::from(before_alternate)];
        // 1049 saves the departing slot, but selecting/clearing a buffer is
        // deliberately not evidence for that buffer's other inherited state.
        if private_modes(frame)
            .is_some_and(|(set, modes)| set && mode_numbers(modes).any(|mode| mode == Some(1049)))
        {
            buffer.saved = buffer.cursor && buffer.origin && buffer.margins;
        }
        let buffer = &mut self.buffers[usize::from(alternate)];
        if frame == b"\x1b[2J" {
            buffer.cells = true;
        } else if mutates_cells(frame) && !(buffer.cursor && buffer.origin && buffer.margins) {
            // Clearing first is safe only if subsequent writes have known
            // coordinates. A later CUP cannot reconstruct an earlier write.
            buffer.cells = false;
        }
        if private_modes(frame)
            .is_some_and(|(set, modes)| !set && mode_numbers(modes).any(|mode| mode == Some(6)))
        {
            buffer.origin = true;
            // Require CUP after both origin and margins have been established.
            // Do not rely on emulator-specific implicit homing.
        }
        if matches!(frame, b"\x1b[r" | b"\x1b[0r" | b"\x1b[;r" | b"\x1b[0;0r") {
            buffer.margins = true;
        }
        if let Some(body) = frame.strip_prefix(b"\x1b[") {
            if matches!(body.last(), Some(b'H' | b'f')) && buffer.origin && buffer.margins {
                buffer.cursor = true;
            }
        }
        if matches!(frame, b"\x1b7" | b"\x1b[s" | b"\x1b[0s") {
            buffer.saved = buffer.cursor && buffer.origin && buffer.margins;
        } else if matches!(frame, b"\x1b8" | b"\x1b[u" | b"\x1b[0u") && !buffer.saved {
            buffer.cursor = false;
        }
    }

    pub fn invalidate(&mut self, reason: Unavailable) {
        self.reason = Some(reason);
        self.buffers = [BufferEvidence::default(); 2];
    }

    pub fn resize(&mut self) {
        // Never replace a stronger loss reason with recoverable geometry.
        if matches!(self.reason, None | Some(Unavailable::Resize)) {
            self.reason = Some(Unavailable::Resize);
        }
        self.buffers = [BufferEvidence::default(); 2];
    }

    pub fn can_paint(&self, alternate: bool) -> bool {
        matches!(self.reason, None | Some(Unavailable::Resize))
            && self.buffers[usize::from(alternate)].complete()
    }

    pub fn unavailable_reason(&self, alternate: bool) -> Option<Unavailable> {
        if self.can_paint(alternate) {
            None
        } else {
            self.reason
        }
    }

    /// Check every selection in a multi-mode frame, including intermediate
    /// switches. Unknown buffers and post-resize transitions need raw output.
    pub fn enters_unknown_buffer(&self, frame: &[u8], alternate: bool) -> bool {
        private_modes(frame).is_some_and(|(set, modes)| {
            mode_numbers(modes).any(|mode| {
                matches!(mode, Some(47 | 1049))
                    && (!self.can_paint(set)
                        || self.reason == Some(Unavailable::Resize)
                            && (set != alternate || mode == Some(1049)))
            })
        })
    }
}

fn mode_numbers(modes: &[u8]) -> impl Iterator<Item = Option<u16>> + '_ {
    modes.split(|b| *b == b';').map(|mode| {
        std::str::from_utf8(mode)
            .ok()
            .and_then(|mode| mode.parse().ok())
    })
}

fn private_modes(frame: &[u8]) -> Option<(bool, &[u8])> {
    let (&action, modes) = frame.strip_prefix(b"\x1b[?")?.split_last()?;
    match action {
        b'h' => Some((true, modes)),
        b'l' => Some((false, modes)),
        _ => None,
    }
}

fn mutates_cells(frame: &[u8]) -> bool {
    if !frame.starts_with(b"\x1b") {
        return frame.iter().any(|b| *b >= 0x20 || matches!(*b, 10..=12));
    }
    frame == b"\x1bM"
        || frame.strip_prefix(b"\x1b[").is_some_and(|body| {
            matches!(
                body.last(),
                Some(b'@' | b'J' | b'K' | b'L' | b'M' | b'P' | b'S' | b'T' | b'X' | b'b')
            )
        })
}

/// This is a support policy, not a second cursor/mode emulator. The backend
/// remains authoritative for represented state; rejected output stays raw.
pub fn supported(frame: &[u8], screen: &vt100::Screen) -> bool {
    if !frame.starts_with(b"\x1b") {
        return std::str::from_utf8(frame).is_ok_and(|text| {
            text.chars().all(|c| {
                if c.is_control() {
                    matches!(
                        c,
                        '\0' | '\x07'
                            | '\x08'
                            | '\t'
                            | '\n'
                            | '\x0b'
                            | '\x0c'
                            | '\r'
                            | '\x0e'
                            | '\x0f'
                            | '\x18'
                            | '\x1a'
                    )
                } else {
                    c != '\u{fffd}'
                }
            })
        });
    }
    if matches!(frame, b"\x1b7" | b"\x1b[s" | b"\x1b[0s")
        && screen.drawing_state().hyperlink.is_some()
    {
        return false;
    }
    if matches!(frame, b"\x1b8" | b"\x1b[u" | b"\x1b[0u") {
        let state = screen.drawing_state();
        if state.hyperlink.is_some() {
            return false;
        }
        let buffer = if state.alternate_screen {
            state.alternate
        } else {
            state.primary
        };
        return !buffer.saved_cursor.origin_mode
            && buffer.saved_cursor.col < screen.size().1.get()
            && buffer.saved_character_sets == state.character_sets;
    }
    if let Some(body) = frame.strip_prefix(b"\x1b[") {
        let Some((&action, args)) = body.split_last() else {
            return false;
        };
        if let Some(modes) = args.strip_prefix(b"?") {
            if matches!(action, b'h' | b'l') {
                return modes.split(|b| *b == b';').all(|mode| {
                    std::str::from_utf8(mode)
                        .ok()
                        .and_then(|m| m.parse::<u16>().ok())
                        .is_some_and(|m| {
                            matches!(
                                m,
                                1 | 6
                                    | 9
                                    | 25
                                    | 47
                                    | 1000
                                    | 1002
                                    | 1003
                                    | 1005
                                    | 1006
                                    | 1049
                                    | 2004
                                    | 2026
                            )
                        })
                });
            }
            return action == b'c'
                || (action == b'n' && modes == b"6")
                || (action == b'u' && matches!(modes, b"" | b"0"))
                || (action == b'm' && matches!(modes, b"" | b"0" | b"1" | b"2" | b"3" | b"4"))
                || (action == b'p'
                    && modes
                        .strip_suffix(b"$")
                        .and_then(|s| std::str::from_utf8(s).ok())
                        .and_then(|s| s.parse::<u16>().ok())
                        .is_some());
        }
        if matches!(action, b'p') && args.ends_with(b"$") {
            return args
                .strip_suffix(b"$")
                .and_then(|s| std::str::from_utf8(s).ok())
                .and_then(|s| s.parse::<u16>().ok())
                .is_some();
        }
        if args.starts_with(b">") || args.starts_with(b"=") {
            return action == b'c' || (action == b'q' && args == b">0");
        }
        if action == b't' {
            return args
                .split(|b| *b == b';')
                .next()
                .and_then(|s| std::str::from_utf8(s).ok())
                .and_then(|s| s.parse::<u16>().ok())
                .is_some_and(|op| (11..=21).contains(&op));
        }
        if action == b'm' {
            // Preview SGR using the same backend to catch unrepresented
            // attributes before they reach a covered physical screen.
            #[derive(Default)]
            struct Check(bool);
            impl vt100::Callbacks for Check {
                fn unhandled_csi(
                    &mut self,
                    _: &mut vt100::Screen,
                    _: Option<u8>,
                    _: Option<u8>,
                    _: &[&[u16]],
                    _: char,
                ) {
                    self.0 = true;
                }
            }
            let mut parser = vt100::Parser::new_with_callbacks(
                std::num::NonZeroU16::MIN,
                std::num::NonZeroU16::MIN,
                0,
                Check::default(),
            );
            parser.process(frame);
            return !parser.callbacks().0;
        }
        if action == b'q' && args.ends_with(b" ") {
            return args.strip_suffix(b" ").is_some_and(|n| {
                n.is_empty()
                    || std::str::from_utf8(n)
                        .ok()
                        .and_then(|n| n.parse::<u16>().ok())
                        .is_some_and(|n| n <= 6)
            });
        }
        if args.iter().any(|b| !b.is_ascii_digit() && *b != b';') {
            return false;
        }
        if args.split(|b| *b == b';').any(|arg| {
            !arg.is_empty()
                && std::str::from_utf8(arg)
                    .ok()
                    .and_then(|s| s.parse::<u16>().ok())
                    .is_none()
        }) {
            return false;
        }
        if matches!(action, b'J' | b'K') {
            return matches!(args, b"" | b"0" | b"1" | b"2");
        }
        if action == b'n' {
            return matches!(args, b"5" | b"6");
        }
        return matches!(
            action,
            b'@' | b'A'
                | b'B'
                | b'C'
                | b'D'
                | b'E'
                | b'F'
                | b'G'
                | b'H'
                | b'J'
                | b'K'
                | b'L'
                | b'M'
                | b'P'
                | b'S'
                | b'T'
                | b'X'
                | b'`'
                | b'b'
                | b'd'
                | b'f'
                | b'r'
                | b'c'
                | b'n'
        ) || (matches!(action, b's' | b'u') && matches!(args, b"" | b"0"))
            || (matches!(action, b'h' | b'l') && args.split(|b| *b == b';').all(|p| p == b"4"));
    }
    if let Some(osc) = frame.strip_prefix(b"\x1b]") {
        let command = osc.split(|b| *b == b';').next().unwrap_or_default();
        if command == b"8" {
            let osc = osc
                .strip_suffix(b"\x1b\\")
                .or_else(|| osc.strip_suffix(b"\x07"));
            return osc.is_some_and(|osc| {
                let parts: Vec<_> = osc.splitn(3, |b| *b == b';').collect();
                parts.len() == 3
                    && parts[1].len() <= 256
                    && parts[2].len() <= 2048
                    && parts[1..].iter().all(|s| {
                        std::str::from_utf8(s).is_ok() && !s.iter().any(|b| *b < 0x20 || *b == 0x7f)
                    })
                    && (parts[2].is_empty() && parts[1].is_empty()
                        || !parts[2].is_empty() && !parts[2].contains(&b';'))
            });
        }
        return matches!(
            command,
            b"0" | b"1"
                | b"2"
                | b"4"
                | b"7"
                | b"10"
                | b"11"
                | b"12"
                | b"52"
                | b"104"
                | b"110"
                | b"111"
                | b"112"
                | b"133"
        );
    }
    if frame.starts_with(b"\x1bP") {
        return frame.starts_with(b"\x1bP$q")
            || frame.starts_with(b"\x1bP+q")
            || frame.starts_with(b"\x1bP+Q");
    }
    if matches!(frame, b"\x1b(B" | b"\x1b(0" | b"\x1b)B" | b"\x1b)0") {
        return true;
    }
    matches!(
        frame,
        b"\x1bc" | b"\x1b7" | b"\x1b=" | b"\x1b>" | b"\x1bM" | b"\x1bg" | b"\x1b\\"
    )
}
