//! Bounded raw drawing retention for an explicitly unused alternate buffer.
//! Queries/effects close before mutation and run once after primary replay.

use std::io::{self, Write};

use crate::terminal_model::TerminalModel;
use crate::terminal_paint;

pub const OUTPUT_LIMIT: usize = 256 * 1024;

pub struct BorrowedMenu {
    entry: vt100::Screen,
    replay: Vec<u8>,
    closing: Option<Vec<u8>>,
    written: usize,
}

pub struct Update {
    pub consumed: usize,
    pub resume: Option<Vec<u8>>,
    pub replay_pending: bool,
}

impl BorrowedMenu {
    pub fn new(screen: &vt100::Screen) -> Self {
        Self {
            entry: screen.clone(),
            replay: Vec::new(),
            closing: None,
            written: 0,
        }
    }

    pub fn process(&mut self, model: &mut TerminalModel, bytes: &[u8]) -> Update {
        if bytes.len() > OUTPUT_LIMIT.saturating_sub(self.replay.len()) {
            // Stop before consuming any of this chunk. Previous partial
            // controls remain in both the model and retained raw output.
            return Update {
                consumed: 0,
                resume: Some(bytes.to_vec()),
                replay_pending: true,
            };
        }
        let (consumed, rejected) = model.process_borrowed(bytes);
        self.replay.extend_from_slice(&bytes[..consumed]);
        if let Some(mut rejected) = rejected {
            // Rejected frames include prefixes retained from earlier reads.
            // Remove that whole frame before close replays accepted bytes.
            self.replay.truncate(self.replay.len() - rejected.len());
            rejected.extend_from_slice(&bytes[consumed..]);
            return Update {
                consumed,
                resume: Some(rejected),
                replay_pending: false,
            };
        }
        if consumed < bytes.len() {
            model.discard_oversized_sequence();
        }
        Update {
            consumed,
            resume: (consumed < bytes.len()).then(|| bytes[consumed..].to_vec()),
            replay_pending: true,
        }
    }

    fn close_bytes(&self) -> Vec<u8> {
        // Setup can fail partway through the selector. Cancel that prefix
        // and explicitly select the owned scratch before any cell erasure.
        let mut bytes = b"\x18\x1b\\\x1b[?47h".to_vec();
        bytes.extend_from_slice(terminal_paint::BEGIN);
        // Only the unused alternate is cleared. No primary cells or save
        // slots are written, including when primary content is uncertain.
        bytes.extend_from_slice(b"\x1b[2J\x1b[H\x1b[?47l");
        terminal_paint::restore(&self.entry, false, &mut bytes);
        bytes.extend_from_slice(if self.entry.bracketed_paste() {
            b"\x1b[?2004h"
        } else {
            b"\x1b[?2004l"
        });
        bytes.extend_from_slice(terminal_paint::END);
        bytes.extend_from_slice(&self.replay);
        bytes
    }

    /// Track successful writes so cleanup resumes a partial close without
    /// duplicating retained drawing after a short write or interrupted error.
    pub fn close(&mut self, writer: &mut impl Write) -> io::Result<()> {
        if self.closing.is_none() {
            self.closing = Some(self.close_bytes());
        }
        let bytes = self.closing.as_ref().unwrap();
        while self.written < bytes.len() {
            match writer.write(&bytes[self.written..]) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => self.written += n,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) => return Err(err),
            }
        }
        writer.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use portable_pty::PtySize;
    use std::num::NonZeroU16;
    use std::process::{Command, Stdio};

    fn size() -> PtySize {
        PtySize {
            rows: 6,
            cols: 18,
            pixel_width: 0,
            pixel_height: 0,
        }
    }

    const INITIALIZE: &[u8] = b"\x1b[?6l\x1b[r\x1b[2;3H\x1b[31m\x1b7\x1b[32m";

    fn model() -> TerminalModel {
        let mut model = TerminalModel::new(size());
        model.process(b"\x1bcPRIMARY SENTINEL");
        model.resize(size());
        model.process(INITIALIZE);
        assert!(model.can_borrow());
        model
    }

    #[test]
    fn borrowed_query_effect_and_switch_close_before_mutation_at_every_split() {
        for trigger in [
            b"\x1b[6n".as_slice(),
            b"\x1b[?6n",
            b"\x1b[?47$p",
            b"\x1bP$qm\x1b\\",
            b"\x1bP+q544e\x1b\\",
            b"\x1b[?u",
            b"\x1b[>0q",
            b"\x1b]2;title\x1b\\",
            b"\x1b]52;c;payload\x07",
            b"\x07",
            b"\x1b[?00047h",
            b"\x1b[?47l",
            b"\x1b[?1049h",
            b"\x1bc",
            b"\x1b_Ggraphics\x1b\\",
            "界".as_bytes(),
            b"\x1b[3b",
        ] {
            for split in 0..=trigger.len() {
                let mut model = model();
                let mut menu = BorrowedMenu::new(model.screen());
                assert!(menu
                    .process(&mut model, b"\x1b[3;4HRETAINED")
                    .resume
                    .is_none());
                let first = menu.process(&mut model, &trigger[..split]);
                let resume = match first.resume {
                    Some(mut resume) => {
                        resume.extend_from_slice(&trigger[split..]);
                        resume
                    }
                    None => menu.process(&mut model, &trigger[split..]).resume.unwrap(),
                };
                assert_eq!(resume, trigger, "trigger {trigger:?}, split {split}");
                assert_eq!(menu.replay, b"\x1b[3;4HRETAINED");
                assert!(
                    model.can_borrow(),
                    "guard must precede reset/buffer mutation"
                );
                assert!(
                    model.take_events().is_empty(),
                    "queries/effects wait for raw replay"
                );
                let mut close = Vec::new();
                menu.close(&mut close).unwrap();
                assert!(close.ends_with(&menu.replay));
                assert_eq!(close.windows(8).filter(|s| *s == b"RETAINED").count(), 1);
            }
        }
    }

    #[test]
    fn borrowed_budget_closes_before_consuming_or_truncating_a_partial_control() {
        let mut model = model();
        let mut menu = BorrowedMenu::new(model.screen());
        let text = vec![b'x'; OUTPUT_LIMIT - 2];
        assert!(menu.process(&mut model, &text).resume.is_none());
        assert!(menu.process(&mut model, b"\x1b[").resume.is_none());
        let state = model.drawing_state();
        let update = menu.process(&mut model, b"31mTAIL");
        assert_eq!(update.consumed, 0);
        assert_eq!(update.resume.as_deref(), Some(b"31mTAIL".as_slice()));
        assert!(update.replay_pending);
        assert_eq!(model.drawing_state(), state);
        assert_eq!(menu.replay.len(), OUTPUT_LIMIT);
        let mut close = Vec::new();
        menu.close(&mut close).unwrap();
        assert!(close.ends_with(b"\x1b["));
        assert_eq!(close.iter().filter(|b| **b == b'x').count(), text.len());
        model.process(update.resume.as_ref().unwrap());
        let (row, col) = model.screen().cursor_position();
        assert_eq!(
            model.screen().cell(row, col - 1).unwrap().fgcolor(),
            vt100::Color::Idx(1)
        );
    }

    #[test]
    fn borrowed_sequence_overflow_replays_its_entire_prefix_before_the_tail() {
        let mut model = model();
        let mut menu = BorrowedMenu::new(model.screen());
        let mut output = b"\x1b]8;;".to_vec();
        output.extend(vec![
            b'x';
            crate::terminal_stream::CONTROL_SEQUENCE_LIMIT + 1024
        ]);
        output.extend_from_slice(b"\x1b\\TAIL");
        let update = menu.process(&mut model, &output);
        assert!(update.replay_pending);
        assert!(!model.can_borrow());
        assert_eq!(
            menu.replay.len(),
            crate::terminal_stream::CONTROL_SEQUENCE_LIMIT
        );
        let mut close = Vec::new();
        menu.close(&mut close).unwrap();
        assert!(close.ends_with(&output[..update.consumed]));
        let mut replay = menu.replay.clone();
        replay.extend_from_slice(update.resume.as_ref().unwrap());
        assert_eq!(replay, output);
    }

    #[test]
    fn borrowed_manual_close_replays_an_unfinished_drawing_prefix_once() {
        for split in 1..b"\x1b[31m".len() {
            let mut model = model();
            let mut menu = BorrowedMenu::new(model.screen());
            let control = b"\x1b[31m";
            assert!(menu.process(&mut model, &control[..split]).resume.is_none());
            let mut close = Vec::new();
            menu.close(&mut close).unwrap();
            assert!(close.ends_with(&control[..split]));
            model.process(&control[split..]);
            model.process(b"TAIL");
            assert!(model.take_events().is_empty());
            let (row, col) = model.screen().cursor_position();
            assert_eq!(
                model.screen().cell(row, col - 1).unwrap().fgcolor(),
                vt100::Color::Idx(1)
            );
        }
    }

    #[test]
    fn borrowed_close_resumes_short_writes_and_errors_without_duplicate_replay() {
        struct Writer {
            bytes: Vec<u8>,
            fail: bool,
        }
        impl Write for Writer {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if self.fail && self.bytes.len() >= 39 {
                    self.fail = false;
                    return Err(io::ErrorKind::WouldBlock.into());
                }
                let n = bytes.len().min(13);
                self.bytes.extend_from_slice(&bytes[..n]);
                Ok(n)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut model = model();
        let mut menu = BorrowedMenu::new(model.screen());
        assert!(menu
            .process(&mut model, b"\x1b[4;1HEXACT REPLAY")
            .resume
            .is_none());
        let expected = menu.close_bytes();
        let mut writer = Writer {
            bytes: Vec::new(),
            fail: true,
        };
        assert_eq!(
            menu.close(&mut writer).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        menu.close(&mut writer).unwrap();
        menu.close(&mut writer).unwrap();
        assert_eq!(writer.bytes, expected);
    }

    #[test]
    fn borrowed_setup_error_never_clears_primary_after_a_partial_selector() {
        for split in 0..=b"\x1b[?47h".len() {
            let model = model();
            let mut menu = BorrowedMenu::new(model.screen());
            let parser = || vt100::Parser::new(
                NonZeroU16::new(6).unwrap(), NonZeroU16::new(18).unwrap(), 0,
            );
            let mut expected = parser();
            let mut actual = parser();
            for parser in [&mut expected, &mut actual] {
                parser.process(b"\x1bcPRIMARY SENTINEL");
                parser.process(INITIALIZE);
            }
            actual.process(&b"\x1b[?47h"[..split]);
            let mut close = Vec::new();
            menu.close(&mut close).unwrap();
            actual.process(&close);
            assert_eq!(actual.screen().contents(), expected.screen().contents(), "split {split}");
            assert_eq!(actual.screen().drawing_state(), expected.screen().drawing_state());
            for parser in [&mut expected, &mut actual] { parser.process(b"\x1b8X"); }
            assert_eq!(actual.screen().contents(), expected.screen().contents());
        }
    }

    #[test]
    fn borrowed_replay_preserves_primary_and_unused_alternate_in_independent_terminal() {
        let before_resize = b"\x1bcPRIMARY SENTINEL LONG LINE\x1b[3;2H\x1b[1;31m\x1b7";
        let mut traces = Vec::new();
        for after in [
            b"\x1b8!\x1b[0m\r\nAFTER MENU".as_slice(),
            b"\x1b[?47h\x1b8Z\x1b[?47l\x1b[2;1HRETURNED",
        ] {
            let mut model = TerminalModel::new(PtySize {
                rows: 4,
                cols: 12,
                ..size()
            });
            model.process(before_resize);
            model.resize(size());
            model.process(INITIALIZE);
            assert!(model.can_borrow());
            let mut menu = BorrowedMenu::new(model.screen());
            let hidden =
                b"\x1b[4;2HA\x1b[35m\x1b7\x1b]8;id=remote;https://example.test/link\x1b\\LINK\
                \x1b]8;;\x1b\\\x1b[3;6r\x1b[?6h\x1b[2;3Htext\x1b[?6l\x1b[r";
            assert!(menu.process(&mut model, hidden).resume.is_none());
            let paint = [
                b"\x1b[?47h".as_slice(),
                terminal_paint::BEGIN,
                b"\x1b[?2004h\x1b[2J\x1b[HLOCAL BORROWED MENU",
                terminal_paint::END,
            ]
            .concat();
            let mut restore = Vec::new();
            menu.close(&mut restore).unwrap();
            let prefix = &restore[..restore.len() - hidden.len()];
            assert!(!prefix.windows(2).any(|s| s == b"\x1b7"));
            let primary = prefix.windows(6).position(|s| s == b"\x1b[?47l").unwrap();
            assert!(!prefix[primary..].windows(4).any(|s| s == b"\x1b[2J"));
            let mut expected =
                vt100::Parser::new(NonZeroU16::new(4).unwrap(), NonZeroU16::new(12).unwrap(), 0);
            let mut actual =
                vt100::Parser::new(NonZeroU16::new(4).unwrap(), NonZeroU16::new(12).unwrap(), 0);
            for parser in [&mut expected, &mut actual] {
                parser.process(before_resize);
                parser.set_size(NonZeroU16::new(6).unwrap(), NonZeroU16::new(18).unwrap());
                parser.process(INITIALIZE);
            }
            expected.process(hidden);
            actual.process(&paint);
            actual.process(&restore);
            expected.process(after);
            actual.process(after);
            assert_eq!(actual.screen().contents(), expected.screen().contents());
            assert_eq!(
                actual.screen().drawing_state(),
                expected.screen().drawing_state()
            );
            for parser in [&mut expected, &mut actual] {
                parser.process(b"\x1b[?47h");
            }
            assert_eq!(
                actual.screen().contents(),
                expected.screen().contents(),
                "unused alternate must remain blank"
            );
            assert_eq!(
                actual.screen().drawing_state(),
                expected.screen().drawing_state()
            );
            traces.push(serde_json::json!({
                "name": format!("owned borrowed buffer replay, after {after:?}"),
                "rows": 6, "cols": 18, "initialRows": 4, "initialCols": 12,
                "beforeResize": before_resize.as_slice(), "before": INITIALIZE,
                "hidden": hidden.as_slice(), "paint": paint, "restore": restore, "after": after,
                "checkInactive": true,
            }));
        }
        if let Some(module) = std::env::var_os("BEAMS_TEST_XTERM_MODULE") {
            let mut reference = Command::new("node")
                .arg(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/terminal-paint-reference.cjs"
                ))
                .arg(module)
                .stdin(Stdio::piped())
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
}
