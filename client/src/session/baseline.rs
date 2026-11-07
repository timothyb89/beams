//! Observe outer-terminal settings before forwarding any child output.
//! Unknown settings remain unknown; cleanup never invents shell defaults.

use std::collections::{BTreeMap, BTreeSet};

const MODES: &[u16] = &[
    1, 6, 9, 25, 47, 66, 1000, 1001, 1002, 1003, 1004, 1005, 1006, 1015, 1016, 2004, 2026,
];
const REPLY_LIMIT: usize = 4096;
const MOUSE_GROUPS: &[&[u16]] = &[&[9, 1000, 1001, 1002, 1003], &[1005, 1006, 1015, 1016]];

#[derive(Default)]
pub struct Baseline {
    modes: BTreeMap<u16, bool>,
    unrecognized: BTreeSet<u16>,
    insert: Option<bool>,
    attributes: Option<Vec<u8>>,
    margins: Option<(u16, u16)>,
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
            b"\x1b[4$p\x1bP$qm\x1b\\\x1bP$qr\x1b\\\x1bP$q q\x1b\\\x1b[?u\x1b[?4m",
        );
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
                    let margins = self.margins.unwrap_or(buffer.scroll_region);
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
    seen_attributes: bool,
    seen_margins: bool,
    seen_style: bool,
    negative_settings: u8,
    seen_kitty: bool,
    seen_modifiers: bool,
}

impl Capture {
    pub fn complete(&self) -> bool {
        self.seen_modes.len() == MODES.len()
            && self.seen_insert
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
            if private && MODES.contains(&(mode as u16)) && mode <= u16::MAX as u32 {
                self.seen_modes.insert(mode as u16);
                if let Some(enabled) = enabled {
                    self.baseline.modes.insert(mode as u16, enabled);
                } else {
                    self.baseline.unrecognized.insert(mode as u16);
                }
                return true;
            }
            if !private && mode == 4 {
                self.seen_insert = true;
                self.baseline.insert = enabled;
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
            .and_then(|s| s.strip_suffix(b"r\x1b\\"))
        {
            if let Some((top, bottom)) = numbers(body) {
                if top > 0 && top < bottom && bottom <= u16::MAX as u32 {
                    self.seen_margins = true;
                    self.baseline.margins = Some((top as u16 - 1, bottom as u16 - 1));
                    return true;
                }
            }
        }
        if let Some(sgr) = frame
            .strip_prefix(b"\x1bP1$r")
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
            .and_then(|s| s.strip_suffix(b"u"))
            .and_then(number)
        {
            self.seen_kitty = true;
            self.baseline.kitty = Some(flags);
            return true;
        }
        if let Some(body) = frame
            .strip_prefix(b"\x1b[>")
            .and_then(|s| s.strip_suffix(b"m"))
        {
            if let Some((4, value @ 0..=2)) = numbers(body) {
                self.seen_modifiers = true;
                self.baseline.modifiers = Some(value as u8);
                return true;
            }
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
    fn one_negative_setting_reply_does_not_finish_three_pending_probes() {
        let mut capture = Capture::default();
        for mode in MODES {
            capture.feed(format!("\x1b[?{mode};0$y").as_bytes());
        }
        capture.feed(b"\x1b[4;0$y\x1b[?0u\x1b[>4;0m\x1bP0$r\x1b\\");
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
