//! Observe outer-terminal settings before forwarding any child output.
//! Unknown settings remain unknown; cleanup never invents shell defaults.

use std::collections::{BTreeMap, BTreeSet};

const MODES: &[u16] = &[
    1, 5, 6, 7, 9, 25, 45, 47, 66, 69, 1000, 1001, 1002, 1003, 1004, 1005, 1006, 1015, 1016,
    2004, 2026,
];
const MOUSE_MODES: &[u16] = &[9, 1000, 1001, 1002, 1003, 1005, 1006, 1015, 1016];

/// Overlay sessions write these once, before any remote output: state no
/// terminal can report, reset to the defaults an ordinary shell already has.
/// Parser boundary, UTF-8, ASCII in G0/G1 with G0 active, unprotected
/// characters and no open hyperlink. Queryable settings are observed instead.
pub const ENTRY_RESETS: &[u8] = b"\x18\x1b\\\x1b%G\x1b(B\x1b)B\x0f\x1b[0\"q\x1b]8;;\x1b\\";
const REPLY_LIMIT: usize = 4096;
const SENTINEL: &[u8] = b"\x1b[c";
const MOUSE_GROUPS: &[&[u16]] = &[&[9, 1000, 1001, 1002, 1003], &[1005, 1006, 1015, 1016]];

/// A DECSTBM report. Some terminals (iTerm2) report the full screen with
/// no parameters, so its rows are only known at the point of use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Margins {
    Full,
    Rows(u16, u16),
}

impl Margins {
    fn rows(self, rows: u16) -> (u16, u16) {
        match self {
            Margins::Full => (0, rows.saturating_sub(1)),
            Margins::Rows(top, bottom) => (top, bottom),
        }
    }
}

#[derive(Default)]
pub struct Baseline {
    modes: BTreeMap<u16, bool>,
    unrecognized: BTreeSet<u16>,
    insert: Option<bool>,
    linefeed: Option<bool>,
    attributes: Option<Vec<u8>>,
    margins: Option<Margins>,
    cursor_style: Option<u8>,
    kitty: Option<u32>,
    modifiers: Option<u8>,
}

impl Baseline {
    pub fn observed_final_screen<'a>(
        &self,
        model: &'a crate::terminal_model::TerminalModel,
    ) -> Option<&'a vt100::Screen> {
        self.modes.get(&47).and_then(|&alternate| {
            model.can_paint_buffer(alternate).then_some(model.screen())
        })
    }

    pub fn probes() -> Vec<u8> {
        let mut bytes = Vec::new();
        for mode in MODES {
            bytes.extend_from_slice(format!("\x1b[?{mode}$p").as_bytes());
        }
        bytes.extend_from_slice(
            b"\x1b[4$p\x1b[20$p\x1bP$qm\x1b\\\x1bP$qr\x1b\\\x1bP$q q\x1b\\\x1b[?u\x1b[?4m",
        );
        // Every terminal answers DA1, and answers in order: once its reply
        // arrives, any probe still unanswered never will be.
        bytes.extend_from_slice(SENTINEL);
        bytes
    }

    /// A synthesized initialization equal to what the terminal reported
    /// before any remote output. The model processes it as observed
    /// evidence; none of it is written. A mode the terminal does not
    /// recognize counts as off: that feature cannot be active. Values the
    /// model cannot represent, or that went unanswered, stay unknown.
    pub fn entry_evidence(&self, rows: u16) -> Vec<u8> {
        let mut bytes = Vec::new();
        if self.modes.is_empty() {
            // No ANSI reply at all: not even the parser mode is known.
            return bytes;
        }
        // Answering CSI reports proves ANSI (not VT52) mode.
        bytes.extend_from_slice(b"\x1b<");
        let off = |mode: u16| {
            self.modes.get(&mode) == Some(&false) || self.unrecognized.contains(&mode)
        };
        let mut set = |mode: u16, enabled: bool| {
            bytes.extend_from_slice(
                format!("\x1b[?{mode}{}", if enabled { 'h' } else { 'l' }).as_bytes(),
            );
        };
        // Select first: a first selection discards earlier buffer evidence.
        if self.modes.get(&47) == Some(&false) {
            set(47, false);
        }
        if self.modes.get(&7) == Some(&true) {
            set(7, true);
        }
        for mode in [5, 45, 69, 2026].into_iter().chain(MOUSE_MODES.iter().copied()) {
            if off(mode) {
                set(mode, false);
            }
        }
        for mode in [1, 25, 2004] {
            if let Some(enabled) = self.modes.get(&mode) {
                set(mode, *enabled);
            }
        }
        if self.modes.get(&47) == Some(&false) && self.modes.get(&6) == Some(&false) {
            set(6, false);
        }
        if let Some(keypad) = self.modes.get(&66) {
            bytes.extend_from_slice(if *keypad { b"\x1b=" } else { b"\x1b>" });
        }
        if let Some(insert) = self.insert {
            bytes.extend_from_slice(if insert { b"\x1b[4h" } else { b"\x1b[4l" });
        }
        if self.linefeed == Some(false) {
            bytes.extend_from_slice(b"\x1b[20l");
        }
        if let Some(attributes) = self.attributes.as_ref().filter(|sgr| {
            // An attribute the model cannot represent stays unknown rather
            // than invalidating eligibility for the whole session.
            let probe = vt100::Parser::new(std::num::NonZeroU16::MIN, std::num::NonZeroU16::MIN, 0);
            crate::terminal_eligibility::supported(sgr, probe.screen())
        }) {
            bytes.extend_from_slice(b"\x1b[0m");
            bytes.extend_from_slice(attributes);
        }
        if self.modes.get(&47) == Some(&false)
            && self.margins.map(|m| m.rows(rows)) == Some((0, rows.saturating_sub(1)))
        {
            bytes.extend_from_slice(b"\x1b[r");
        }
        bytes
    }

    pub fn restore(&self, final_screen: Option<&vt100::Screen>) -> Vec<u8> {
        let mut bytes = b"\x18\x1b\\".to_vec();
        // Select without clearing a buffer or overwriting its saved cursor.
        if let Some(alternate) = self.modes.get(&47) {
            bytes.extend_from_slice(if *alternate {
                b"\x1b[?47h"
            } else {
                b"\x1b[?47l"
            });
        }
        // Disable first, then enable: some terminals reset the entire mouse
        // protocol when any tracking mode is reset.
        for (mode, enabled) in self
            .modes
            .iter()
            .filter(|(_, enabled)| !**enabled)
            .chain(self.modes.iter().filter(|(_, enabled)| **enabled))
        {
            if matches!(*mode, 6 | 47 | 66 | 2026) {
                continue;
            }
            if MOUSE_GROUPS.iter().any(|group| {
                group.contains(mode)
                    && !group
                        .iter()
                        .all(|m| self.modes.contains_key(m) || self.unrecognized.contains(m))
                    && !group.iter().any(|m| self.modes.get(m) == Some(&true))
            }) {
                continue;
            }
            bytes.extend_from_slice(
                format!("\x1b[?{mode}{}", if *enabled { 'h' } else { 'l' }).as_bytes(),
            );
        }
        if let Some(keypad) = self.modes.get(&66) {
            bytes.extend_from_slice(if *keypad { b"\x1b=" } else { b"\x1b>" });
        }
        if let Some(screen) = final_screen.filter(|_| self.modes.contains_key(&47)) {
            // Changing origin/margins homes the cursor. Use an observed
            // target-buffer snapshot to put it back, never an initial guess.
            if let Some(origin) = self.modes.get(&6) {
                if !origin || self.margins.is_some() {
                    let (rows, cols) = screen.size();
                    let mut copy = vt100::Parser::new(rows, cols, 0);
                    *copy.screen_mut() = screen.clone();
                    if let Some(alternate) = self.modes.get(&47) {
                        copy.process(if *alternate {
                            b"\x1b[?47h"
                        } else {
                            b"\x1b[?47l"
                        });
                    }
                    let state = copy.screen().drawing_state();
                    let buffer = if state.alternate_screen {
                        state.alternate
                    } else {
                        state.primary
                    };
                    let margins = self
                        .margins
                        .map(|m| m.rows(rows.get()))
                        .unwrap_or(buffer.scroll_region);
                    let changed = margins.1 < rows.get()
                        && (margins != buffer.scroll_region
                            || *origin != buffer.cursor.origin_mode);
                    if changed && margins != buffer.scroll_region {
                        bytes.extend_from_slice(
                            format!("\x1b[{};{}r", margins.0 + 1, margins.1 + 1).as_bytes(),
                        );
                    }
                    if changed {
                        bytes.extend_from_slice(b"\x1b[4l\x1b(B\x1b)B\x0f\x1b]8;;\x1b\\");
                        let mut cursor = buffer.cursor;
                        cursor.origin_mode = *origin;
                        if *origin {
                            cursor.row = cursor.row.clamp(margins.0, margins.1);
                        }
                        crate::terminal_paint::cursor(copy.screen(), cursor, margins, &mut bytes);
                        crate::terminal_paint::character_sets(state.character_sets, &mut bytes);
                        bytes.extend_from_slice(
                            state
                                .hyperlink
                                .as_deref()
                                .unwrap_or("\x1b]8;;\x1b\\")
                                .as_bytes(),
                        );
                    }
                }
            }
        }
        if let Some(insert) = self.insert {
            bytes.extend_from_slice(if insert { b"\x1b[4h" } else { b"\x1b[4l" });
        }
        if let Some(linefeed) = self.linefeed {
            bytes.extend_from_slice(if linefeed { b"\x1b[20h" } else { b"\x1b[20l" });
        }
        if let Some(attributes) = &self.attributes {
            bytes.extend_from_slice(attributes);
        }
        if let Some(style) = self.cursor_style {
            bytes.extend_from_slice(format!("\x1b[{style} q").as_bytes());
        }
        if let Some(flags) = self.kitty.filter(|_| self.modes.contains_key(&47)) {
            // Restore the current flags without popping an unowned stack.
            bytes.extend_from_slice(format!("\x1b[={flags}u").as_bytes());
        }
        if let Some(value) = self.modifiers {
            bytes.extend_from_slice(format!("\x1b[>4;{value}m").as_bytes());
        }
        // Finish with the observed sync setting so cleanup remains visible.
        if let Some(sync) = self.modes.get(&2026) {
            bytes.extend_from_slice(if *sync {
                b"\x1b[?2026h"
            } else {
                b"\x1b[?2026l"
            });
        }
        bytes
    }
}

#[derive(Default)]
pub struct Capture {
    pub baseline: Baseline,
    pending: Vec<u8>,
    pub input: Vec<u8>,
    seen_modes: BTreeSet<u16>,
    seen_insert: bool,
    seen_linefeed: bool,
    seen_attributes: bool,
    seen_margins: bool,
    seen_style: bool,
    negative_settings: u8,
    seen_kitty: bool,
    seen_modifiers: bool,
    seen_sentinel: bool,
}

impl Capture {
    /// Every probe has been answered, or the sentinel proved the rest
    /// never will be. Until then, replies to the probes are still ours.
    pub fn finished(&self) -> bool {
        self.seen_sentinel || self.complete()
    }

    pub fn complete(&self) -> bool {
        self.seen_modes.len() == MODES.len()
            && self.seen_insert
            && self.seen_linefeed
            && (u8::from(self.seen_attributes)
                + u8::from(self.seen_margins)
                + u8::from(self.seen_style)
                + self.negative_settings
                >= 3)
            && self.seen_kitty
            && self.seen_modifiers
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.pending.extend_from_slice(bytes);
        let mut consumed = 0;
        while consumed < self.pending.len() {
            let bytes = &self.pending[consumed..];
            let end = if bytes[0] != 0x1b {
                Some(1)
            } else if bytes.len() == 1 {
                None
            } else if bytes[1] == b'[' {
                bytes[2..]
                    .iter()
                    .position(|b| (0x40..=0x7e).contains(b))
                    .map(|n| n + 3)
            } else if bytes[1] == b'P' {
                bytes
                    .windows(2)
                    .position(|pair| pair == b"\x1b\\")
                    .map(|n| n + 2)
            } else {
                Some(1)
            };
            let Some(end) = end.or_else(|| (bytes.len() > REPLY_LIMIT).then_some(1)) else {
                break;
            };
            let frame = bytes[..end].to_vec();
            if !self.reply(&frame) {
                self.input.extend_from_slice(&frame);
            }
            consumed += end;
        }
        self.pending.drain(..consumed);
    }

    /// Ordinary input decoded so far. A partial sequence older than
    /// `flush_partial` is released as input too: a lone Escape key must not
    /// wait for a reply that never comes.
    pub fn take_input(&mut self, flush_partial: bool) -> Vec<u8> {
        if flush_partial {
            let pending = std::mem::take(&mut self.pending);
            self.input.extend_from_slice(&pending);
        }
        std::mem::take(&mut self.input)
    }

    pub fn has_partial(&self) -> bool {
        !self.pending.is_empty()
    }

    pub fn finish(mut self) -> (Baseline, Vec<u8>) {
        self.input.extend_from_slice(&self.pending);
        (self.baseline, self.input)
    }

    fn reply(&mut self, frame: &[u8]) -> bool {
        if let Some(body) = frame
            .strip_prefix(b"\x1b[")
            .and_then(|b| b.strip_suffix(b"$y"))
        {
            let private = body.starts_with(b"?");
            let body = if private { &body[1..] } else { body };
            let Some((mode, value)) = numbers(body) else {
                return false;
            };
            if value > 4 {
                return false;
            }
            let enabled = match value {
                1 | 3 => Some(true),
                2 | 4 => Some(false),
                _ => None,
            };
            // Each probe has one reply. A second report of the same kind
            // answers a remote query and must reach the remote.
            if private && mode <= u16::MAX as u32 && MODES.contains(&(mode as u16))
                && !self.seen_modes.contains(&(mode as u16))
            {
                self.seen_modes.insert(mode as u16);
                if let Some(enabled) = enabled {
                    self.baseline.modes.insert(mode as u16, enabled);
                } else {
                    self.baseline.unrecognized.insert(mode as u16);
                }
                return true;
            }
            if !private && mode == 4 && !self.seen_insert {
                self.seen_insert = true;
                self.baseline.insert = enabled;
                return true;
            }
            if !private && mode == 20 && !self.seen_linefeed {
                self.seen_linefeed = true;
                // Unrecognized LNM cannot be set: newline does not imply CR.
                self.baseline.linefeed = enabled.or(Some(false).filter(|_| value == 0));
                return true;
            }
        }
        if frame == b"\x1bP0$r\x1b\\" {
            if self.negative_settings == 3 {
                return false;
            }
            self.negative_settings += 1;
            return true;
        }
        if let Some(style) = frame
            .strip_prefix(b"\x1bP1$r")
            .filter(|_| !self.seen_style)
            .and_then(|s| s.strip_suffix(b" q\x1b\\"))
            .and_then(number)
        {
            if style <= 6 {
                self.seen_style = true;
                self.baseline.cursor_style = Some(style as u8);
                return true;
            }
        }
        if let Some(body) = frame
            .strip_prefix(b"\x1bP1$r")
            .filter(|_| !self.seen_margins)
            .and_then(|s| s.strip_suffix(b"r\x1b\\"))
        {
            if body.is_empty() {
                self.seen_margins = true;
                self.baseline.margins = Some(Margins::Full);
                return true;
            }
            if let Some((top, bottom)) = numbers(body) {
                if top > 0 && top < bottom && bottom <= u16::MAX as u32 {
                    self.seen_margins = true;
                    self.baseline.margins = Some(Margins::Rows(top as u16 - 1, bottom as u16 - 1));
                    return true;
                }
            }
        }
        if let Some(sgr) = frame
            .strip_prefix(b"\x1bP1$r")
            .filter(|_| !self.seen_attributes)
            .and_then(|s| s.strip_suffix(b"m\x1b\\"))
        {
            if sgr.len() < REPLY_LIMIT
                && sgr
                    .iter()
                    .all(|b| b.is_ascii_digit() || matches!(*b, b';' | b':'))
            {
                self.seen_attributes = true;
                self.baseline.attributes = Some([b"\x1b[".as_slice(), sgr, b"m"].concat());
                return true;
            }
        }
        if let Some(flags) = frame
            .strip_prefix(b"\x1b[?")
            .filter(|_| !self.seen_kitty)
            .and_then(|s| s.strip_suffix(b"u"))
            .and_then(number)
        {
            self.seen_kitty = true;
            self.baseline.kitty = Some(flags);
            return true;
        }
        if let Some(body) = frame
            .strip_prefix(b"\x1b[>")
            .filter(|_| !self.seen_modifiers)
            .and_then(|s| s.strip_suffix(b"m"))
        {
            if let Some((4, value @ 0..=2)) = numbers(body) {
                self.seen_modifiers = true;
                self.baseline.modifiers = Some(value as u8);
                return true;
            }
        }
        if !self.seen_sentinel
            && frame.starts_with(b"\x1b[?")
            && frame.ends_with(b"c")
            && frame[3..frame.len() - 1].iter().all(|b| b.is_ascii_digit() || *b == b';')
        {
            self.seen_sentinel = true;
            return true;
        }
        false
    }
}

fn number(bytes: &[u8]) -> Option<u32> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(bytes).ok()?.parse().ok()
}

fn numbers(bytes: &[u8]) -> Option<(u32, u32)> {
    let mut parts = bytes.split(|b| *b == b';');
    let a = number(parts.next()?)?;
    let b = number(parts.next()?)?;
    parts.next().is_none().then_some((a, b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal_eligibility::Unavailable;

    /// Strict-passthrough terminal recordings (see "Adding terminal
    /// recordings" in docs/development/terminal-overlay.md). Each fixture is
    /// the remote output from the multiplexer's alternate-screen entry up to,
    /// and including, its exit; `replies` are that terminal's recorded
    /// answers to `Baseline::probes`, when captured.
    struct Recording {
        name: &'static str,
        bytes: &'static [u8],
        rows: u16,
        cols: u16,
        replies: Option<&'static [u8]>,
    }

    const RECORDINGS: &[Recording] = &[Recording {
        name: "zellij 0.45.1 + claude, iTerm2 3.6.10, reattach",
        bytes: include_bytes!("../../tests/fixtures/zellij-iterm2-reattach.bin"),
        rows: 65,
        cols: 208,
        replies: None,
    }];

    /// Representative replies for a terminal that answers every probe with
    /// defaults (autowrap and cursor visible). `settings: false` answers no
    /// DECRQSS request.
    fn replies(settings: bool, rows: u16) -> Vec<u8> {
        let mut replies = Vec::new();
        for mode in MODES {
            let value = if matches!(mode, 7 | 25) { 1 } else { 2 };
            replies.extend_from_slice(format!("\x1b[?{mode};{value}$y").as_bytes());
        }
        replies.extend_from_slice(b"\x1b[4;2$y\x1b[20;2$y\x1b[?0u\x1b[>4;0m");
        if settings {
            replies.extend_from_slice(b"\x1bP1$r0m\x1b\\");
            replies.extend_from_slice(format!("\x1bP1$r1;{rows}r\x1b\\").as_bytes());
            replies.extend_from_slice(b"\x1bP1$r0 q\x1b\\");
        } else {
            replies.extend_from_slice(b"\x1bP0$r\x1b\\\x1bP0$r\x1b\\\x1bP0$r\x1b\\");
        }
        replies
    }

    fn entry_model(replies: &[u8], rows: u16, cols: u16) -> crate::terminal_model::TerminalModel {
        let mut capture = Capture::default();
        capture.feed(replies);
        let (baseline, input) = capture.finish();
        assert!(input.is_empty(), "every recorded reply is recognized");
        let mut model = crate::terminal_model::TerminalModel::new(portable_pty::PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
        model.process(ENTRY_RESETS);
        model.process(&baseline.entry_evidence(rows));
        model
    }

    /// Reply sets to replay a recording with: the recorded replies when
    /// present, otherwise both representative sets.
    fn reply_sets(recording: &Recording) -> Vec<(&'static str, Vec<u8>)> {
        match recording.replies {
            Some(replies) => vec![("recorded replies", replies.to_vec())],
            None => vec![
                ("representative replies", replies(true, recording.rows)),
                ("no DECRQSS replies", replies(false, recording.rows)),
            ],
        }
    }

    /// Distinct frames outside the supported inventory, for failure
    /// messages: these are what a new recording needs classified.
    fn unsupported_frames(bytes: &[u8], rows: u16, cols: u16) -> Vec<String> {
        let mut parser = vt100::Parser::new(
            std::num::NonZeroU16::new(rows).unwrap(),
            std::num::NonZeroU16::new(cols).unwrap(),
            0,
        );
        let mut stream = crate::terminal_stream::TerminalStream::default();
        let mut found = BTreeMap::<String, usize>::new();
        let _ = stream.advance_guarded(bytes, |frame, original| {
            if !crate::terminal_eligibility::supported(original, parser.screen()) {
                let shown: String = original
                    .iter()
                    .take(48)
                    .flat_map(|b| std::ascii::escape_default(*b))
                    .map(char::from)
                    .collect();
                *found.entry(shown).or_default() += 1;
            }
            parser.process(frame);
            Ok::<_, ()>(true)
        });
        found.into_iter().map(|(frame, count)| format!("{frame} x{count}")).collect()
    }

    /// Where the multiplexer leaves the alternate screen (its last `?1049l`).
    fn exit_offset(bytes: &[u8]) -> usize {
        bytes.windows(8).rposition(|w| w == b"\x1b[?1049l").expect("recording includes the exit")
    }

    /// Offsets just before an ESC: every earlier sequence and character is
    /// complete, as a real paint boundary requires.
    fn boundaries(bytes: &[u8]) -> impl Iterator<Item = usize> + '_ {
        (1..bytes.len()).filter(|&i| bytes[i] == 0x1b)
    }

    /// Paint a toast at the first eligible boundary and a menu covering
    /// output from there to a later boundary; replay the rest. An unpainted
    /// terminal must end identical, including the workstation primary that
    /// the model never saw.
    #[test]
    fn painting_over_recordings_matches_an_unpainted_terminal() {
        use super::super::toast::{OutputTracker, Toast, ToastPainter, Tone};
        let mut traces = Vec::new();
        for recording in RECORDINGS {
            let bytes = recording.bytes;
            let exit = exit_offset(bytes);
            let inherited = [
                b"\x1b[5;1H\x1b[1;31mUNSEEN WORKSTATION PRIMARY\x1b[7;9H\x1b7".as_slice(),
                ENTRY_RESETS,
            ]
            .concat();
            let parser = |chunks: &[&[u8]]| {
                let mut parser = vt100::Parser::new(
                    std::num::NonZeroU16::new(recording.rows).unwrap(),
                    std::num::NonZeroU16::new(recording.cols).unwrap(),
                    0,
                );
                for chunk in chunks {
                    parser.process(chunk);
                }
                parser
            };
            let (_, replies) = &reply_sets(recording)[0];
            // The first boundary where painting is allowed.
            let mut probe = entry_model(replies, recording.rows, recording.cols);
            let mut processed = 0;
            let first = boundaries(&bytes[..exit])
                .find(|&offset| {
                    probe.process(&bytes[processed..offset]);
                    processed = offset;
                    probe.can_paint()
                })
                .unwrap_or_else(|| panic!("{}: never eligible", recording.name));
            let second = boundaries(&bytes[..exit])
                .find(|&offset| offset > first + (exit - first) / 2)
                .unwrap();
            let name = recording.name;

            let mut model = entry_model(replies, recording.rows, recording.cols);
            model.process(&bytes[..first]);
            let notice = Toast {
                tone: Tone::Info,
                title: "Beams".to_string(),
                lines: vec![ratatui::text::Line::from("request pending approval")],
            };
            let mut painter = ToastPainter::default();
            let paint = painter.sync(
                Some(&notice),
                &model,
                &mut OutputTracker::default(),
                std::time::Instant::now(),
            );
            assert!(!paint.is_empty(), "{name}: toast paints");
            let restore = painter.clear(&model);

            model.begin_overlay();
            let mut menu = crate::terminal_paint::BEGIN.to_vec();
            menu.extend_from_slice(b"\x1b[2J\x1b[HLOCAL MENU");
            menu.extend_from_slice(crate::terminal_paint::END);
            let hidden = &bytes[first..second];
            let (consumed, rejected) = model
                .process_guarded_with_events(hidden, true, |_| Ok::<(), ()>(()), true)
                .unwrap();
            assert_eq!((consumed, rejected), (hidden.len(), None), "{name}: hidden output");
            assert!(model.can_paint(), "{name}");
            let close = model.restore_bytes(model.screen().alternate_screen());
            model.end_overlay();
            let after = &bytes[second..];

            let painted = |end: usize| {
                parser(&[&inherited, &bytes[..first], &paint, &restore, &menu, &close,
                    &bytes[second..end]])
            };
            let unpainted = |end: usize| parser(&[&inherited, &bytes[..end]]);
            for end in [exit, bytes.len()] {
                let (actual, expected) = (painted(end), unpainted(end));
                assert_eq!(actual.screen().contents(), expected.screen().contents(), "{name}");
                assert_eq!(
                    actual.screen().drawing_state(),
                    expected.screen().drawing_state(),
                    "{name}"
                );
            }
            // After the exit: the workstation primary, never seen or touched.
            assert!(unpainted(bytes.len()).screen().contents().contains("UNSEEN WORKSTATION PRIMARY"));
            traces.push(serde_json::json!({
                "name": format!("toast over {name}"),
                "rows": recording.rows, "cols": recording.cols, "inherited": inherited,
                "before": &bytes[..first], "hidden": [],
                "paint": paint, "restore": restore, "after": &bytes[first..],
            }));
            traces.push(serde_json::json!({
                "name": format!("menu over {name}"),
                "rows": recording.rows, "cols": recording.cols, "inherited": inherited,
                "before": &bytes[..first], "hidden": hidden,
                "paint": menu, "restore": close, "after": after,
            }));
        }
        if let Some(module) = std::env::var_os("BEAMS_TEST_XTERM_MODULE") {
            use std::io::Write as _;
            let mut reference = std::process::Command::new("node")
                .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/terminal-paint-reference.cjs"))
                .arg(module)
                .stdin(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            reference
                .stdin
                .take()
                .unwrap()
                .write_all(&serde_json::to_vec(&traces).unwrap())
                .unwrap();
            assert!(reference.wait().unwrap().success());
        }
    }

    #[test]
    fn entry_evidence_seeds_only_reported_representable_settings() {
        let shared_known = |replies: &[u8]| {
            let mut model = entry_model(replies, 65, 208);
            // A RIS-free explicit buffer initialization isolates shared state.
            model.process(b"\x1b[?6l\x1b[r\x1b[H\x1b[2J");
            model.can_paint()
        };
        assert!(shared_known(&replies(true, 65)));
        // Modes a terminal does not recognize cannot be active.
        let unrecognized: Vec<u8> = String::from_utf8(replies(true, 65))
            .unwrap()
            .replace("\x1b[?2026;2$y", "\x1b[?2026;0$y")
            .replace("\x1b[?1016;2$y", "\x1b[?1016;0$y")
            .replace("\x1b[?69;2$y", "\x1b[?69;0$y")
            .replace("\x1b[20;2$y", "\x1b[20;0$y")
            .into_bytes();
        assert!(shared_known(&unrecognized));
        // Autowrap off is unrepresented; unanswered autowrap is unknown.
        for autowrap in ["\x1b[?7;2$y", "\x1b[?7;0$y", ""] {
            let replies = String::from_utf8(replies(true, 65))
                .unwrap()
                .replace("\x1b[?7;1$y", autowrap)
                .into_bytes();
            assert!(!shared_known(&replies), "autowrap reply {autowrap:?}");
        }
        // An exotic attribute stays unknown instead of invalidating the
        // session; a later observed SGR reset establishes it.
        let exotic = String::from_utf8(replies(true, 65))
            .unwrap()
            .replace("\x1bP1$r0m", "\x1bP1$r0;58;5;1m")
            .into_bytes();
        let mut model = entry_model(&exotic, 65, 208);
        model.process(b"\x1b[?6l\x1b[r\x1b[H\x1b[2J");
        assert!(!model.can_paint());
        assert_eq!(model.unavailable_reason(), Some(Unavailable::InheritedState));
        model.process(b"\x1b[m\x1b[H\x1b[2J");
        assert!(model.can_paint());
        // Non-default represented values are seeded as reported.
        let custom = String::from_utf8(replies(true, 65))
            .unwrap()
            .replace("\x1b[?1;2$y", "\x1b[?1;1$y")
            .replace("\x1b[?2004;2$y", "\x1b[?2004;1$y")
            .replace("\x1bP1$r0m", "\x1bP1$r0;1;31m")
            .into_bytes();
        let mut model = entry_model(&custom, 65, 208);
        assert!(model.screen().application_cursor());
        assert!(model.screen().bracketed_paste());
        let (row, col) = model.screen().cursor_position();
        model.process(b"X");
        let cell = model.screen().cell(row, col).unwrap();
        assert!(cell.bold());
        assert_eq!(cell.fgcolor(), vt100::Color::Idx(1));
    }

    /// `client/tests/terminal-probe-replies.py` must send exactly the probes
    /// a session sends, or recorded replies would not match real sessions.
    #[test]
    fn probe_reply_script_sends_the_session_probes() {
        let script = include_str!("../../tests/terminal-probe-replies.py");
        let modes: Vec<String> = MODES.iter().map(u16::to_string).collect();
        assert!(script.contains(&format!("MODES = [{}]", modes.join(", "))));
        let mode_probes: Vec<u8> = MODES
            .iter()
            .flat_map(|mode| format!("\x1b[?{mode}$p").into_bytes())
            .collect();
        let probes = Baseline::probes();
        let extra = probes.strip_prefix(mode_probes.as_slice()).unwrap();
        let literal: String = extra
            .iter()
            .map(|b| match b {
                0x1b => "\\x1b".to_string(),
                b'\\' => "\\\\".to_string(),
                b => char::from(*b).to_string(),
            })
            .collect();
        assert!(script.contains(&format!("EXTRA = b\"{literal}\"")), "{literal}");
    }

    #[test]
    fn recordings_are_paintable_from_observed_entry_state() {
        for recording in RECORDINGS {
            let bytes = recording.bytes;
            let exit = exit_offset(bytes);
            // Synchronized frames, when the terminal supports them, give an
            // exact expectation: eligible right after the first full frame.
            let first_frame = bytes
                .windows(8)
                .position(|w| w == b"\x1b[?2026l")
                .filter(|end| *end < exit)
                .map(|end| end + 8);
            for (replies_name, replies) in reply_sets(recording) {
                let context = format!("{}, {replies_name}", recording.name);
                let mut model = entry_model(&replies, recording.rows, recording.cols);
                assert!(!model.can_paint(), "{context}: the inherited primary stays unknown");
                let mut first = None;
                for (index, chunk) in bytes[..exit].chunks(512).enumerate() {
                    let (consumed, rejected) = model
                        .process_guarded_with_events(chunk, false, |_| Ok::<(), ()>(()), true)
                        .unwrap();
                    assert_eq!((consumed, rejected), (chunk.len(), None), "{context}");
                    if model.can_paint() {
                        first.get_or_insert(index);
                    } else {
                        assert!(
                            first.is_none(),
                            "{context}: eligibility lost at chunk {index}: {:?}; unsupported: {:#?}",
                            model.unavailable_reason(),
                            unsupported_frames(&bytes[..exit], recording.rows, recording.cols)
                        );
                    }
                }
                let first = first.unwrap_or_else(|| {
                    // UnsupportedSequence: classify the frames listed here.
                    // InheritedState: entry replies or redraw evidence lack
                    // something (see "Adding terminal recordings").
                    panic!(
                        "{context}: never eligible: {:?}; unsupported: {:#?}",
                        model.unavailable_reason(),
                        unsupported_frames(&bytes[..exit], recording.rows, recording.cols)
                    )
                });
                if let Some(frame_end) = first_frame {
                    assert_eq!(first, frame_end / 512, "{context}: eligible after the first frame");
                }
                assert!(model.screen().alternate_screen(), "{context}");
                let toast = ratatui::layout::Rect::new(recording.cols - 58, 1, 56, 6);
                assert!(model.can_paint_area(toast), "{context}");
                model.process(&bytes[exit..]);
                assert!(!model.can_paint(), "{context}: returning to the inherited primary");
            }
            let mut model = entry_model(b"", recording.rows, recording.cols);
            model.process(bytes);
            assert!(!model.can_paint(), "{}: no reported settings, nothing assumed", recording.name);
        }
    }

    #[test]
    fn split_reports_preserve_user_input_and_nondefault_settings() {
        let trace = b"typed\x1b[?25;2$y\x1b[?2004;1$y\x1b[?47;1$y\x1b[?66;1$y\x1b[4;1$y\x1bP1$r0;1;31m\x1b\\\x1b[?9u\x1b[>4;2m\x1b[A\x1b[?9999;1$yend\x1b[";
        for split in 0..=trace.len() {
            let mut capture = Capture::default();
            capture.feed(&trace[..split]);
            capture.feed(&trace[split..]);
            let (baseline, input) = capture.finish();
            assert_eq!(input, b"typed\x1b[A\x1b[?9999;1$yend\x1b[");
            let restore = baseline.restore(None);
            for expected in [
                b"\x1b[?25l".as_slice(),
                b"\x1b[?2004h",
                b"\x1b[?47h",
                b"\x1b=",
                b"\x1b[4h",
                b"\x1b[0;1;31m",
                b"\x1b[=9u",
                b"\x1b[>4;2m",
            ] {
                assert!(restore.windows(expected.len()).any(|w| w == expected));
            }
            assert!(!restore.windows(2).any(|w| w == b"\x1b>"));
        }
    }

    #[test]
    fn unknown_modes_and_bad_or_unrelated_replies_never_invent_defaults() {
        let mut capture = Capture::default();
        capture.feed(b"\x1b[?25;0$y\x1b[4;0$y\x1bP0$r\x1b\\");
        capture.feed(b"\x1b[?25;9$y\x1bP1$r0mBAD\x1b\\\x1b[>5;2m");
        let (baseline, input) = capture.finish();
        assert_eq!(baseline.restore(None), b"\x18\x1b\\");
        assert_eq!(input, b"\x1b[?25;9$y\x1bP1$r0mBAD\x1b\\\x1b[>5;2m");
    }

    #[test]
    fn parameterless_margin_report_means_the_full_screen() {
        // iTerm2 answers DECRQSS DECSTBM with `1$rr`. Unrecognized, it
        // reached the remote as typed input and was echoed there.
        let mut capture = Capture::default();
        capture.feed(b"\x1b[?47;2$y\x1bP1$rr\x1b\\");
        let (baseline, input) = capture.finish();
        assert!(input.is_empty());
        assert_eq!(baseline.margins.map(|m| m.rows(24)), Some((0, 23)));
        assert!(baseline.entry_evidence(24).windows(3).any(|w| w == b"\x1b[r"));
    }

    #[test]
    fn the_sentinel_ends_the_capture_and_later_reports_belong_to_the_remote() {
        let mut capture = Capture::default();
        // A terminal that answers only a few probes, then the DA1 sentinel.
        capture.feed(b"\x1b[?25;1$y\x1bP0$r\x1b\\typed");
        assert!(!capture.finished());
        assert_eq!(capture.take_input(false), b"typed");
        capture.feed(b"\x1b[?62;22c");
        assert!(capture.finished() && !capture.complete());
        // The same reports again answer the remote's own queries.
        capture.feed(b"\x1b[?25;1$y\x1b[?62;22c\x1bP1$rr\x1b\\");
        let (baseline, input) = capture.finish();
        assert_eq!(baseline.modes.get(&25), Some(&true));
        assert_eq!(input, b"\x1b[?25;1$y\x1b[?62;22c");
    }

    #[test]
    fn a_partial_reply_is_released_only_when_asked() {
        let mut capture = Capture::default();
        capture.feed(b"x\x1b");
        assert_eq!(capture.take_input(false), b"x");
        assert!(capture.has_partial());
        assert_eq!(capture.take_input(true), b"\x1b");
        assert!(!capture.has_partial());
    }

    #[test]
    fn one_negative_setting_reply_does_not_finish_three_pending_probes() {
        let mut capture = Capture::default();
        for mode in MODES {
            capture.feed(format!("\x1b[?{mode};0$y").as_bytes());
        }
        capture.feed(b"\x1b[4;0$y\x1b[20;0$y\x1b[?0u\x1b[>4;0m\x1bP0$r\x1b\\");
        assert!(!capture.complete());
        capture.feed(b"\x1bP1$r1;24r\x1b\\");
        assert!(!capture.complete());
        capture.feed(b"\x1bP1$r5 q\x1b\\");
        assert!(capture.complete());
        let (baseline, input) = capture.finish();
        assert!(baseline.attributes.is_none());
        assert_eq!(baseline.cursor_style, Some(5));
        assert!(input.is_empty());
    }

    #[test]
    fn origin_and_margin_restoration_preserve_observed_cursor_without_homing_an_unchanged_shell() {
        let mut parser = vt100::Parser::new(
            std::num::NonZeroU16::new(6).unwrap(),
            std::num::NonZeroU16::new(12).unwrap(),
            0,
        );
        parser.process(b"\x1bc\x1b[4;5H");
        let mut capture = Capture::default();
        capture.feed(b"\x1b[?47;2$y\x1b[?6;2$y\x1bP1$r1;6r\x1b\\");
        let (baseline, _) = capture.finish();
        let bytes = baseline.restore(Some(parser.screen()));
        assert!(
            !bytes.windows(5).any(|w| w == b"\x1b[?6l"),
            "unchanged DECOM must not home the cursor"
        );
        parser.process(&bytes);
        assert_eq!(parser.screen().cursor_position(), (3, 4));

        let mut capture = Capture::default();
        capture.feed(b"\x1b[?47;2$y\x1b[?6;1$y\x1bP1$r2;5r\x1b\\");
        let (baseline, _) = capture.finish();
        let bytes = baseline.restore(Some(parser.screen()));
        parser.process(&bytes);
        assert_eq!(parser.screen().cursor_position(), (3, 4));
        assert!(parser.screen().drawing_state().primary.cursor.origin_mode);
        assert_eq!(
            parser.screen().drawing_state().primary.scroll_region,
            (1, 4)
        );
    }

    #[test]
    fn cleanup_geometry_requires_evidence_for_the_observed_outer_buffer() {
        let mut model = crate::terminal_model::TerminalModel::new(portable_pty::PtySize {
            rows: 6,
            cols: 12,
            pixel_width: 0,
            pixel_height: 0,
        });
        model.process(b"\x18\x1b\\\x1b<\x1b%G\x1b[?7h\x1b[?5;45;69l\x1b[20l\x1b[0\"q\
            \x1b(B\x1b)B\x0f\x1b[0m\x1b]8;;\x1b\\\x1b[4l\x1b[?1l\x1b>\
            \x1b[?25h\x1b[?2004l\x1b[?2026l\x1b[?9;1000;1001;1002;1003l\
            \x1b[?1005;1006;1015;1016l\x1b[?47h\x1b[3g\x1b[?6l\x1b[r");
        for row in 1..=6 {
            model.process(format!("\x1b[{row};1H\x1b#5").as_bytes());
        }
        model.process(b"\x1b[H\x1b7\x1b[2J");
        assert!(model.can_paint());
        for (report, known) in [
            (b"\x1b[?47;2$y".as_slice(), false),
            (b"\x1b[?47;1$y", true),
        ] {
            let mut capture = Capture::default();
            capture.feed(report);
            capture.feed(b"\x1b[?6;1$y\x1bP1$r2;5r\x1b\\");
            let (baseline, _) = capture.finish();
            let final_screen = baseline.observed_final_screen(&model);
            assert_eq!(final_screen.is_some(), known);
            let restore = baseline.restore(final_screen);
            assert_eq!(restore.windows(5).any(|b| b == b"\x1b[?6h"), known);
            assert_eq!(restore.windows(6).any(|b| b == b"\x1b[2;5r"), known);
        }
        model.process(b"\x1bc");
        let mut capture = Capture::default();
        capture.feed(b"\x1b[?47;2$y");
        let (baseline, _) = capture.finish();
        assert!(baseline.observed_final_screen(&model).is_some());
    }
}
