//! Conservative evidence and an explicit supported output inventory.
//!
//! A cursor report or buffer selection never proves inherited cells/modes.
//! RIS or explicit initialization establishes shared state. Unsupported state
//! still requires RIS; geometry and ownership are established per buffer.

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
    selected: bool,
    tabs: Tabs,
    normal_rows: NormalRows,
}

impl BufferEvidence {
    fn complete(self) -> bool {
        self.cells
            && self.cursor
            && self.origin
            && self.margins
            && self.saved
            && self.selected
            && self.tabs != Tabs::Unknown
            && self.normal_rows == NormalRows::Known
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Tabs {
    #[default]
    Unknown,
    Default,
    Cleared,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NormalRows {
    Next(u16),
    Known,
}

impl Default for NormalRows {
    fn default() -> Self {
        Self::Next(0)
    }
}

// Each control proves one part of state. This inventory records confidence,
// not values: the backend remains authoritative for current represented modes.
const INITIALIZERS: &[&[u8]] = &[
    b"\x18",      // cancel an inherited partial control
    b"\x1b\\",    // terminate an inherited string before state initialization
    b"\x1b<",     // ANSI, including an inherited VT52 terminal
    b"\x1b%G",    // UTF-8
    b"\x1b[?7h",  // represented autowrap behavior
    b"\x1b[?5l",  // normal global video
    b"\x1b[?45l", // no reverse wrap
    b"\x1b[?69l", // no horizontal margins
    b"\x1b[20l",  // LF does not imply CR
    b"\x1b[0\"q", // unprotected characters
    b"\x1b(B",
    b"\x1b)B",
    b"\x0f",           // G0, G1 and GL
    b"\x1b[0m",        // complete SGR
    b"\x1b]8;;\x1b\\", // current hyperlink (independent of SGR)
    b"\x1b[4l",        // insert/replace
    b"\x1b[?1l",
    b"\x1b>", // cursor keys, keypad
    b"\x1b[?25h",
    b"\x1b[?2004l", // visibility, paste
    b"\x1b[?2026l", // synchronized output ends
    b"\x1b[?9l",
    b"\x1b[?1000l",
    b"\x1b[?1001l",
    b"\x1b[?1002l",
    b"\x1b[?1003l",
    b"\x1b[?1005l",
    b"\x1b[?1006l",
    b"\x1b[?1015l",
    b"\x1b[?1016l",
];
const ALL_SHARED: u64 = (1 << INITIALIZERS.len()) - 1;
const PARSER_CANCELLED: u64 = 1;
const PARSER_BOUNDARY: u64 = 3;
const MOUSE_TRACKING: &[u16] = &[9, 1000, 1001, 1002, 1003];
const MOUSE_ENCODING: &[u16] = &[1005, 1006, 1015, 1016];

pub struct Evidence {
    // Resize remains the last loss even after a buffer recovers, so switches
    // cannot silently reuse geometry whose cross-emulator semantics differ.
    reason: Option<Unavailable>,
    shared: u64,
    buffers: [BufferEvidence; 2],
}

impl Default for Evidence {
    fn default() -> Self {
        Self {
            reason: Some(Unavailable::InheritedState),
            shared: 0,
            buffers: [BufferEvidence::default(); 2],
        }
    }
}

impl Evidence {
    pub fn observe(&mut self, frame: &[u8], before_alternate: bool, screen: &vt100::Screen) {
        let alternate = screen.alternate_screen();
        if frame == b"\x1bc" {
            self.reason = None;
            self.shared = ALL_SHARED;
            self.buffers = [BufferEvidence {
                cells: true,
                cursor: true,
                origin: true,
                margins: true,
                saved: true,
                selected: true,
                tabs: Tabs::Default,
                normal_rows: NormalRows::Known,
            }; 2];
            return;
        }
        // An explicit baseline cannot recover loss from an arbitrary protocol.
        if matches!(
            self.reason,
            Some(Unavailable::UnsupportedSequence | Unavailable::SequenceBudget)
        ) {
            return;
        }
        if self.reason == Some(Unavailable::InheritedState)
            && self.shared & PARSER_BOUNDARY != PARSER_BOUNDARY
        {
            // Earlier controls may have been swallowed by an inherited string.
            // Establish a parser boundary before accepting any state evidence.
            if frame == b"\x18" {
                self.shared = PARSER_CANCELLED;
                self.buffers = [BufferEvidence::default(); 2];
            } else if frame == b"\x1b\\" && self.shared & PARSER_CANCELLED != 0 {
                self.shared = PARSER_BOUNDARY;
            }
            return;
        }
        // Ordinary text cannot initialize shared state. Keep the high-volume
        // passthrough path cheap both before and after a baseline is known.
        if self.shared != ALL_SHARED
            && (frame.starts_with(b"\x1b") || matches!(frame, b"\x0e" | b"\x0f" | b"\x18"))
        {
            if let Some((true, modes)) = private_modes(frame) {
                for group in [MOUSE_TRACKING, MOUSE_ENCODING] {
                    let mask = group_mask(group);
                    if self.shared & mask != mask
                        && mode_numbers(modes).any(|m| m.is_some_and(|m| group.contains(&m)))
                    {
                        // Enabling before the entire coupled group is initialized
                        // makes earlier individual resets insufficient.
                        self.shared &= !mask;
                    }
                }
            }
            for (index, initializer) in INITIALIZERS.iter().enumerate() {
                if establishes(frame, initializer) {
                    self.shared |= 1 << index;
                }
            }
        }
        if self.reason == Some(Unavailable::InheritedState)
            && private_modes(frame)
                .is_some_and(|(set, modes)| !set && mode_numbers(modes).any(|m| m == Some(1049)))
        {
            // Leaving 1049 restores a primary save slot, which may predate
            // the observed initialization just like an explicit DECRC.
            self.shared = 0;
        }
        if self.reason.is_some()
            && (before_alternate != alternate
                || private_modes(frame).is_some_and(|(_, modes)| {
                    mode_numbers(modes).any(|m| {
                        m == Some(1049)
                            || m == Some(47) && !self.buffers[usize::from(alternate)].selected
                    })
                }))
        {
            // Buffer switches can transfer cursors and clear the alternate
            // buffer differently across emulators. Do not carry recovered
            // geometry through a switch; require initialization in its target.
            let tabs = self.buffers[usize::from(alternate)].tabs;
            let normal_rows = self.buffers[usize::from(alternate)].normal_rows;
            self.buffers[usize::from(alternate)] = BufferEvidence {
                tabs: if tabs == Tabs::Default {
                    Tabs::Default
                } else {
                    Tabs::Unknown
                },
                normal_rows: if normal_rows == NormalRows::Known {
                    NormalRows::Known
                } else {
                    NormalRows::default()
                },
                ..BufferEvidence::default()
            };
            if before_alternate {
                let tabs = self.buffers[1].tabs;
                let normal_rows = self.buffers[1].normal_rows;
                self.buffers[1] = BufferEvidence {
                    tabs: if tabs == Tabs::Default {
                        Tabs::Default
                    } else {
                        Tabs::Unknown
                    },
                    normal_rows: if normal_rows == NormalRows::Known {
                        NormalRows::Known
                    } else {
                        NormalRows::default()
                    },
                    ..BufferEvidence::default()
                };
            }
            self.buffers[usize::from(alternate)].selected = true;
            return;
        }

        let shared_known = self.shared == ALL_SHARED;
        let buffer = &mut self.buffers[usize::from(before_alternate)];
        // 1049 saves the departing slot, but selecting/clearing a buffer is
        // deliberately not evidence for that buffer's other inherited state.
        if private_modes(frame)
            .is_some_and(|(set, modes)| set && mode_numbers(modes).any(|mode| mode == Some(1049)))
        {
            buffer.saved = shared_known
                && buffer.cursor
                && buffer.origin
                && buffer.margins
                && buffer.normal_rows == NormalRows::Known;
        }
        let buffer = &mut self.buffers[usize::from(alternate)];
        if private_modes(frame)
            .is_some_and(|(_, modes)| mode_numbers(modes).any(|m| matches!(m, Some(47 | 1049))))
        {
            buffer.selected = true;
        }
        if frame == b"\x1b[3g" {
            buffer.tabs = Tabs::Cleared;
        }
        if frame == b"\x1b[2J" {
            buffer.cells = shared_known && buffer.normal_rows == NormalRows::Known;
        } else if (mutates_cells(frame)
            && !(shared_known
                && buffer.cursor
                && buffer.origin
                && buffer.margins
                && buffer.normal_rows == NormalRows::Known))
            || (frame == b"\t" && buffer.tabs == Tabs::Unknown)
        {
            // Clearing first is safe only if subsequent writes have known
            // coordinates. A later CUP cannot reconstruct an earlier write.
            buffer.cells = false;
            if !shared_known || frame == b"\t" || buffer.normal_rows != NormalRows::Known {
                buffer.cursor = false;
            }
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
                // Column one is an anchor even on an inherited double-width
                // line. Other columns need established normal line size.
                let params = &body[..body.len() - 1];
                let col = params.split(|b| *b == b';').nth(1).unwrap_or(b"1");
                buffer.cursor =
                    buffer.normal_rows == NormalRows::Known || matches!(col, b"" | b"0" | b"1");
            }
        }
        if frame == b"\x1b#5"
            && shared_known
            && buffer.selected
            && buffer.cursor
            && buffer.origin
            && buffer.margins
        {
            if let NormalRows::Next(next) = buffer.normal_rows {
                if screen.cursor_position().0 == next {
                    buffer.normal_rows = if next + 1 == screen.size().0.get() {
                        NormalRows::Known
                    } else {
                        NormalRows::Next(next + 1)
                    };
                }
            }
        }
        if matches!(frame, b"\x1b7" | b"\x1b[s" | b"\x1b[0s") {
            buffer.saved = shared_known
                && buffer.cursor
                && buffer.origin
                && buffer.margins
                && buffer.normal_rows == NormalRows::Known;
        } else if matches!(frame, b"\x1b8" | b"\x1b[u" | b"\x1b[0u") && !buffer.saved {
            buffer.cursor = false;
            if self.reason == Some(Unavailable::InheritedState) {
                // DECRC can restore inherited attributes, maps, wrap and origin.
                // Earlier initialization cannot prove the restored values.
                self.shared = 0;
                buffer.origin = false;
                buffer.cells = false;
            }
        }
    }

    pub fn invalidate(&mut self, reason: Unavailable) {
        self.reason = Some(reason);
        self.shared = 0;
        self.buffers = [BufferEvidence::default(); 2];
    }

    pub fn resize(&mut self) {
        // Never replace a stronger loss reason with recoverable geometry.
        if matches!(self.reason, None | Some(Unavailable::Resize)) {
            self.reason = Some(Unavailable::Resize);
        }
        self.buffers = self.buffers.map(|buffer| BufferEvidence {
            // Known default stops survive resize. Cleared stops may acquire
            // new ones in an outer terminal; require another explicit clear.
            tabs: if buffer.tabs == Tabs::Default {
                Tabs::Default
            } else {
                Tabs::Unknown
            },
            selected: buffer.selected,
            normal_rows: if buffer.normal_rows == NormalRows::Known {
                NormalRows::Known
            } else {
                NormalRows::default()
            },
            ..BufferEvidence::default()
        });
    }

    pub fn can_paint(&self, alternate: bool) -> bool {
        !matches!(
            self.reason,
            Some(Unavailable::UnsupportedSequence | Unavailable::SequenceBudget)
        ) && self.shared == ALL_SHARED
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
                        || self.reason.is_some() && (set != alternate || mode == Some(1049)))
            })
        })
    }

    pub fn supported(&self, frame: &[u8], screen: &vt100::Screen) -> bool {
        // A previous graphic character can predate the inherited baseline.
        // Local painting also changes REP state; do not broaden its support.
        if self.reason == Some(Unavailable::InheritedState) {
            if frame.starts_with(b"\x1b[") && frame.ends_with(b"b") {
                return false;
            }
            if let Some((false, modes)) = private_modes(frame) {
                let tracking = match screen.mouse_protocol_mode() {
                    vt100::MouseProtocolMode::None => None,
                    vt100::MouseProtocolMode::Press => Some(9),
                    vt100::MouseProtocolMode::PressRelease => Some(1000),
                    vt100::MouseProtocolMode::ButtonMotion => Some(1002),
                    vt100::MouseProtocolMode::AnyMotion => Some(1003),
                };
                let encoding = match screen.mouse_protocol_encoding() {
                    vt100::MouseProtocolEncoding::Default => None,
                    vt100::MouseProtocolEncoding::Utf8 => Some(1005),
                    vt100::MouseProtocolEncoding::Sgr => Some(1006),
                };
                for (group, active) in [(MOUSE_TRACKING, tracking), (MOUSE_ENCODING, encoding)] {
                    if let Some(active) = active {
                        if mode_numbers(modes).any(|m| m.is_some_and(|m| group.contains(&m)))
                            && !mode_numbers(modes).any(|m| m == Some(active))
                        {
                            // Some terminals clear the entire group while the
                            // backend clears only the matching selection.
                            return false;
                        }
                    }
                }
            }
        }
        supported(frame, screen)
    }
}

fn group_mask(group: &[u16]) -> u64 {
    INITIALIZERS
        .iter()
        .enumerate()
        .fold(0, |mask, (index, initializer)| {
            if private_modes(initializer).is_some_and(|(_, modes)| {
                mode_numbers(modes).any(|m| m.is_some_and(|m| group.contains(&m)))
            }) {
                mask | 1 << index
            } else {
                mask
            }
        })
}

fn establishes(frame: &[u8], initializer: &[u8]) -> bool {
    if let (Some((set, modes)), Some((init_set, init_modes))) =
        (private_modes(frame), private_modes(initializer))
    {
        let mode = mode_numbers(init_modes).next().flatten().unwrap();
        return (set == init_set || matches!(mode, 1 | 25 | 2004))
            && mode_numbers(modes).any(|m| m == Some(mode));
    }
    match initializer {
        b"\x1b[4l" => matches!(frame, b"\x1b[4l" | b"\x1b[4h"),
        b"\x1b>" => matches!(frame, b"\x1b>" | b"\x1b="),
        b"\x1b(B" => matches!(frame, b"\x1b(B" | b"\x1b(0"),
        b"\x1b)B" => matches!(frame, b"\x1b)B" | b"\x1b)0"),
        b"\x0f" => matches!(frame, b"\x0f" | b"\x0e"),
        b"\x1b[0m" => matches!(frame, b"\x1b[m" | b"\x1b[0m"),
        b"\x1b]8;;\x1b\\" => matches!(frame, b"\x1b]8;;\x1b\\" | b"\x1b]8;;\x07"),
        _ => frame == initializer,
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
                            ) || action == b'h' && m == 7
                                || action == b'l' && matches!(m, 5 | 45 | 69 | 1001 | 1015 | 1016)
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
        if action == b'g' {
            return args == b"3";
        }
        if action == b'q' && args == b"0\"" {
            return true;
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
            || (matches!(action, b'h' | b'l')
                && args
                    .split(|b| *b == b';')
                    .all(|p| p == b"4" || action == b'l' && p == b"20"));
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
        b"\x1bc"
            | b"\x1b7"
            | b"\x1b="
            | b"\x1b>"
            | b"\x1bM"
            | b"\x1bg"
            | b"\x1b\\"
            | b"\x1b<"
            | b"\x1b%G"
            | b"\x1b#5"
    )
}
