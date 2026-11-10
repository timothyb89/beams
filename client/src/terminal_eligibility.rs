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
    fn drawing_known(self) -> bool {
        self.cursor
            && self.origin
            && self.margins
            && self.selected
            && self.normal_rows == NormalRows::Known
    }

    fn complete(self) -> bool {
        self.drawing_known() && self.saved && self.tabs != Tabs::Unknown
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
    // Full-width rows established by EL 2 or a complete overwrite. Never
    // infer inherited cells from the backend's default blanks. Storage is
    // bounded by terminal height.
    known_rows: [Vec<bool>; 2],
    // A row being rewritten left to right from column one: (buffer, row,
    // next column). Reaching the right edge writes every cell of the row.
    overwrite: Option<(bool, u16, u16)>,
    // An observed RIS, with no subsequent remote alternate use. Entry mode
    // reports and primary initialization cannot establish scratch ownership.
    unused_alternate: bool,
}

impl Default for Evidence {
    fn default() -> Self {
        Self {
            reason: Some(Unavailable::InheritedState),
            shared: 0,
            buffers: [BufferEvidence::default(); 2],
            known_rows: [Vec::new(), Vec::new()],
            overwrite: None,
            unused_alternate: false,
        }
    }
}

impl Evidence {
    pub fn observe(
        &mut self,
        frame: &[u8],
        before_alternate: bool,
        before_cursor: (u16, u16),
        screen: &vt100::Screen,
    ) {
        // Any early return (reset, boundary, buffer switch) ends a run.
        let run = self.overwrite.take();
        self.observe_frame(frame, before_alternate, before_cursor, screen);
        self.track_overwrite(run, frame, before_alternate, before_cursor, screen);
    }

    /// Contiguous text from column one to the right edge, with only
    /// non-moving, non-mutating controls (SGR, links, modes) in between,
    /// writes every cell of that row with known state: the row is known.
    /// This needs no clear, so a redraw such as zellij's attach (which
    /// relies on its own full repaint) establishes content.
    fn track_overwrite(
        &mut self,
        run: Option<(bool, u16, u16)>,
        frame: &[u8],
        before_alternate: bool,
        before_cursor: (u16, u16),
        screen: &vt100::Screen,
    ) {
        let alternate = screen.alternate_screen();
        let index = usize::from(alternate);
        if alternate != before_alternate
            || !(self.shared_known() && self.buffers[index].drawing_known())
            || self.buffers[index].cells
        {
            return;
        }
        let after = screen.cursor_position();
        let (rows, cols) = (screen.size().0.get(), screen.size().1.get());
        let text = !frame.is_empty()
            && !frame.starts_with(b"\x1b")
            && frame.iter().all(|b| *b >= 0x20 && *b != 0x7f);
        if let Some((buffer, row, col)) = run {
            if buffer == alternate && before_cursor == (row, col) {
                if text && after.0 == row && after.1 > col {
                    if after.1 < cols {
                        self.overwrite = Some((buffer, row, after.1));
                        return;
                    }
                    // Pending wrap at the right edge: every cell written.
                    let known = &mut self.known_rows[index];
                    known.resize(usize::from(rows), false);
                    known[usize::from(row)] = true;
                    if known.iter().all(|k| *k) {
                        self.buffers[index].cells = true;
                        known.clear();
                    }
                    return;
                }
                if !text && !mutates_cells(frame) && after == before_cursor {
                    self.overwrite = run;
                    return;
                }
            }
        }
        if !text && !mutates_cells(frame) && after.1 == 0 {
            self.overwrite = Some((alternate, after.0, 0));
        }
    }

    fn observe_frame(
        &mut self,
        frame: &[u8],
        before_alternate: bool,
        before_cursor: (u16, u16),
        screen: &vt100::Screen,
    ) {
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
            self.known_rows.iter_mut().for_each(Vec::clear);
            self.unused_alternate = true;
            return;
        }
        if before_alternate
            || private_modes(frame).is_some_and(|(set, modes)| {
                mode_numbers(modes).any(|m| m == Some(1049) || set && m == Some(47))
            })
        {
            self.unused_alternate = false;
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
                self.known_rows.iter_mut().for_each(Vec::clear);
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
            self.known_rows[usize::from(alternate)].clear();
            // Emulators keep DECOM/DECSTBM either globally (xterm) or per
            // buffer with defaults (reset origin, full margins). When the
            // departing buffer's values are known defaults and the model
            // reports defaults for both buffers, every semantics agrees.
            let departing = self.buffers[usize::from(before_alternate)];
            let state = screen.drawing_state();
            let full = (0, screen.size().0.get() - 1);
            let default_origin = !state.primary.cursor.origin_mode
                && !state.alternate.cursor.origin_mode;
            let default_margins =
                state.primary.scroll_region == full && state.alternate.scroll_region == full;
            let carried_origin = departing.origin && default_origin;
            let carried_margins = departing.margins && default_margins;
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
                self.known_rows[1].clear();
            }
            let target = &mut self.buffers[usize::from(alternate)];
            target.selected = true;
            target.origin = carried_origin;
            target.margins = carried_margins;
            if alternate
                && private_modes(frame).is_some_and(|(set, modes)| {
                    set && mode_numbers(modes).any(|m| m == Some(1049))
                })
            {
                // Entering with 1049 clears the alternate buffer, which also
                // resets DEC line sizes (xterm `ClearBufRows`). Like ED 2,
                // an explicit assumption verified per terminal. Cells are
                // not assumed: they need erasure or a complete overwrite.
                target.normal_rows = NormalRows::Known;
            }
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
        let rows = &mut self.known_rows[usize::from(alternate)];
        let positioned = shared_known && buffer.drawing_known();
        if frame == b"\x1b[2J" {
            // Erasing whole rows resets DEC double-width/height line sizes
            // (xterm `ClearBufRows`). This is an explicit cross-terminal
            // assumption, verified per terminal in the workstation matrix.
            buffer.normal_rows = NormalRows::Known;
            buffer.cells = shared_known;
            rows.clear();
        } else if frame == b"\x1b[2K" && positioned && !buffer.cells {
            rows.resize(usize::from(screen.size().0.get()), false);
            rows[usize::from(before_cursor.0)] = true;
            if rows.iter().all(|known| *known) {
                // Once every row is observed, ordinary supported scrolling
                // has fully known source cells just as it does after ED 2.
                buffer.cells = true;
                rows.clear();
            }
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
            rows.clear();
            if !shared_known || frame == b"\t" || buffer.normal_rows != NormalRows::Known {
                buffer.cursor = false;
            }
        } else if !buffer.cells && mutates_cells(frame) {
            // Preserve row evidence only for operations confined to one row.
            // Bound printed columns without emulating the cursor: ASCII uses
            // one, other codepoints at most two in the supported width model.
            // Crossing the edge may scroll even when the final cursor is
            // still on the same row. Do not infer scroll provenance.
            let text_in_row = !frame.starts_with(b"\x1b")
                && frame.iter().all(|b| *b >= 0x20)
                && text_fits_row(
                    frame,
                    usize::from(screen.size().1.get().saturating_sub(before_cursor.1)),
                );
            let row_edit = frame
                .strip_prefix(b"\x1b[")
                .is_some_and(|body| matches!(body.last(), Some(b'@' | b'K' | b'P' | b'X')));
            if !positioned || !(text_in_row || row_edit) {
                rows.clear();
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
                rows.clear();
            }
        }
    }

    pub fn invalidate(&mut self, reason: Unavailable) {
        self.reason = Some(reason);
        self.shared = 0;
        self.buffers = [BufferEvidence::default(); 2];
        self.known_rows.iter_mut().for_each(Vec::clear);
        self.unused_alternate = false;
    }

    pub fn resize(&mut self) {
        self.known_rows.iter_mut().for_each(Vec::clear);
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
            // Origin mode is a pure flag resize never touches (the vendored
            // model leaves it alone; DECOM has no size dependency on any
            // target terminal). Margins are a resize-time invariant, not an
            // explicit reset: the model's own `set_size` always recomputes
            // them to the full new screen, matching "xterm always resets
            // the scroll region... when the screen is resized" (see
            // `vendor/vt100/src/grid.rs`). Known evidence for either stays
            // valid; a later explicit `CSI r` still updates it normally.
            // Cursor/cells/saved slot are not safe assumptions and reset
            // below. A later ordinary CUP (now unblocked by origin/margins
            // already being known) and the resize redraw pulse typically
            // reestablish cursor and cells without any explicit remote
            // reinitialization.
            origin: buffer.origin,
            margins: buffer.margins,
            ..BufferEvidence::default()
        });
    }

    fn shared_known(&self) -> bool {
        !matches!(
            self.reason,
            Some(Unavailable::UnsupportedSequence | Unavailable::SequenceBudget)
        ) && self.shared == ALL_SHARED
    }

    fn rows_known(&self, alternate: bool, top: u16, bottom: u16) -> bool {
        let index = usize::from(alternate);
        self.buffers[index].cells
            || self.known_rows[index]
                .get(usize::from(top)..usize::from(bottom))
                .is_some_and(|rows| !rows.is_empty() && rows.iter().all(|known| *known))
    }

    /// A saved slot, tab stops, or cell content may remain unknown: painting
    /// never uses a saved slot or tab stops, and restoring unseen cells as
    /// blank is within terminal norms (the same as `clear`, an inline
    /// picker, or takeover itself). Covered output that depends on a saved
    /// slot or tabs closes first (`depends_on_unknown`); restoration never
    /// writes an unknown saved slot. `doubtful_restore` reports when a paint
    /// actually covered unobserved cells, for callers that want to prompt a
    /// corrective remote repaint afterward.
    pub fn can_paint(&self, alternate: bool) -> bool {
        self.shared_known() && self.buffers[usize::from(alternate)].drawing_known()
    }

    /// Whether painting `[top, bottom)` of `alternate` would restore any
    /// cell content Beams never actually observed (restored as blank). Not
    /// a painting precondition; informs an optional post-close repaint.
    pub fn doubtful_restore(&self, alternate: bool, top: u16, bottom: u16) -> bool {
        !self.rows_known(alternate, top, bottom)
    }

    pub fn saved_known(&self, alternate: bool) -> bool {
        self.buffers[usize::from(alternate)].saved
    }

    /// Hidden output whose effect depends on state that was never observed:
    /// restoring an unknown saved cursor, or a tab with unknown stops. The
    /// caller closes the menu and replays it raw instead of guessing.
    pub fn depends_on_unknown(&self, frame: &[u8], alternate: bool) -> bool {
        let buffer = self.buffers[usize::from(alternate)];
        (matches!(frame, b"\x1b8" | b"\x1b[u" | b"\x1b[0u") && !buffer.saved)
            || (frame == b"\t" && buffer.tabs == Tabs::Unknown)
            // A save the evidence cannot certify would change the remote's
            // slot in the model without a faithful physical restoration.
            || (matches!(frame, b"\x1b7" | b"\x1b[s" | b"\x1b[0s")
                && !(self.shared_known() && buffer.drawing_known()))
    }

    pub fn can_borrow(&self, screen: &vt100::Screen) -> bool {
        let state = screen.drawing_state();
        self.unused_alternate
            && self.shared_known()
            && !state.alternate_screen
            && self.buffers[0].complete()
            && !state.primary.cursor.origin_mode
            && state.primary.scroll_region == (0, screen.size().0.get() - 1)
            && state.primary.cursor.col < screen.size().1.get()
    }

    /// Toasts do not use the application's saved cursor or tab stops, and
    /// may restore unobserved cells as blank like any other paint. They
    /// still need the last glyph known when restoring a pending wrap: that
    /// guards reprinting a wide character, not general cell certainty.
    pub fn can_paint_rows(
        &self,
        alternate: bool,
        _top: u16,
        _bottom: u16,
        screen: &vt100::Screen,
    ) -> bool {
        let (row, col) = screen.cursor_position();
        self.shared_known()
            && self.buffers[usize::from(alternate)].drawing_known()
            && (col < screen.size().1.get() || self.rows_known(alternate, row, row + 1))
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

fn text_fits_row(frame: &[u8], columns: usize) -> bool {
    use unicode_width::UnicodeWidthChar as _;
    // Bytes are already an upper bound; only inspect UTF-8 near the edge.
    // Otherwise use the backend's own per-character width (the same
    // unicode-width crate). Anything wider than two stays conservative.
    frame.len() <= columns
        || std::str::from_utf8(frame).is_ok_and(|text| {
            text.chars()
                .map(|c| match c.width() {
                    Some(width @ 0..=2) => width,
                    _ => columns + 1,
                })
                .try_fold(0usize, |sum, width| sum.checked_add(width))
                .is_some_and(|sum| sum <= columns)
        })
}

/// Borrowed menus retain only drawing. Everything else closes before replay
/// so replies/effects come once from the real primary after earlier drawing.
pub fn borrowed_drawing(frame: &[u8]) -> bool {
    if !frame.starts_with(b"\x1b") {
        return frame.iter().all(|b| b.is_ascii() && *b != 7);
    }
    if let Some(body) = frame.strip_prefix(b"\x1b[") {
        let Some((&action, args)) = body.split_last() else {
            return false;
        };
        if let Some(modes) = args.strip_prefix(b"?") {
            return matches!(action, b'h' | b'l')
                && !mode_numbers(modes).any(|m| matches!(m, Some(47 | 1049)));
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
                | b'd'
                | b'f'
                | b'r'
                | b'm'
                | b's'
                | b'u'
                | b'h'
                | b'l'
                | b'g'
                | b'q'
        ) && !args.starts_with(b">")
            && !args.starts_with(b"=");
    }
    // OSC 8 changes drawing state; other OSCs have physical effects.
    frame.starts_with(b"\x1b]8;")
        || matches!(
            frame,
            b"\x1b7"
                | b"\x1b8"
                | b"\x1bM"
                | b"\x1b(B"
                | b"\x1b(0"
                | b"\x1b)B"
                | b"\x1b)0"
                | b"\x1b="
                | b"\x1b>"
                | b"\x1b#5"
                | b"\x1b<"
                | b"\x1b%G"
                | b"\x1b\\"
        )
}

/// Zellij announces its session to the outer terminal with a private DCS
/// (`DCS 26661 n <base64> ST`). Terminals ignore it; it never draws cells.
/// Accept only that exact shape, bounded, so other DCS stay unsupported.
pub fn zellij_identity(frame: &[u8]) -> bool {
    frame
        .strip_prefix(b"\x1bP26661n")
        .and_then(|rest| rest.strip_suffix(b"\x1b\\"))
        .is_some_and(|payload| {
            payload.len() <= 1024
                && payload
                    .iter()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
        })
}

/// A Kitty graphics `a=q` query asks whether the protocol is supported and
/// stores or draws nothing. Every other graphics command stays unsupported.
pub fn kitty_graphics_query(frame: &[u8]) -> bool {
    frame
        .strip_prefix(b"\x1b_G")
        .and_then(|rest| rest.strip_suffix(b"\x1b\\"))
        .is_some_and(|body| {
            let control = body.split(|b| *b == b';').next().unwrap_or_default();
            body.len() <= 4096
                && control.split(|b| *b == b',').any(|kv| kv == b"a=q")
                && !body.iter().any(|b| *b < 0x20)
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
                                || action == b'l' && matches!(m, 5 | 45 | 69 | 1001 | 1016)
                                // Cursor blink, focus/theme reports and urxvt
                                // mouse encoding affect input or cursor style,
                                // never cells. Local menus decode none of their
                                // reports; replies route to the remote.
                                || matches!(m, 12 | 1004 | 1015 | 2031)
                        })
                });
            }
            return action == b'c'
                // CPR, or the terminal's color-scheme (theme) report.
                || (action == b'n' && matches!(modes, b"6" | b"996"))
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
        if action == b'u' && (args.starts_with(b">") || args.starts_with(b"<")) {
            // Kitty keyboard push/pop changes key encoding only; the menu
            // decodes CSI-u keys and cleanup never pops an unowned stack.
            return args[1..].iter().all(u8::is_ascii_digit);
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
        if frame == b"\x1b[59m" {
            // Reset underline color. Setting one (SGR 58) stays unsupported,
            // so the represented underline color is already the default.
            return true;
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
            || frame.starts_with(b"\x1bP+Q")
            || zellij_identity(frame);
    }
    if frame.starts_with(b"\x1b_") {
        return kitty_graphics_query(frame);
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
