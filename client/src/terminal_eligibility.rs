//! Conservative evidence and an explicit supported output inventory.
//!
//! A cursor report or buffer selection never proves inherited cells/modes.
//! For now only RIS establishes the complete drawing baseline; unsupported
//! state and resize require another observed RIS before local painting.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unavailable {
    InheritedState,
    UnsupportedSequence,
    Resize,
    SequenceBudget,
}

pub struct Evidence {
    pub reason: Option<Unavailable>,
    pub cells: [bool; 2],
}

impl Default for Evidence {
    fn default() -> Self {
        Self {
            reason: Some(Unavailable::InheritedState),
            cells: [false; 2],
        }
    }
}

impl Evidence {
    pub fn observe(&mut self, frame: &[u8], alternate: bool) {
        if frame == b"\x1bc" {
            self.reason = None;
            self.cells = [true; 2];
        } else if matches!(frame, b"\x1b[2J" | b"\x1b[J") {
            // This establishes cells only when cursor/state already agrees.
            // CSI J at an unknown cursor is not a full clear.
            if frame == b"\x1b[2J" {
                self.cells[usize::from(alternate)] = true;
            }
        }
    }

    pub fn invalidate(&mut self, reason: Unavailable) {
        self.reason = Some(reason);
        self.cells = [false; 2];
    }

    pub fn can_paint(&self, alternate: bool) -> bool {
        self.reason.is_none() && self.cells[usize::from(alternate)]
    }
}

/// This is a support policy, not a second cursor/mode emulator. The backend
/// remains authoritative for represented state; rejected output stays raw.
pub fn supported(frame: &[u8], screen: &vt100::Screen) -> bool {
    if !frame.starts_with(b"\x1b") {
        return true;
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
        return !buffer.saved_cursor.origin_mode && buffer.saved_cursor.col < screen.size().1.get();
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
            return matches!(action, b'c' | b'n' | b'u' | b'm')
                || (action == b'p' && args.ends_with(b"$"));
        }
        if matches!(action, b'p') && args.ends_with(b"$") {
            return true;
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
