//! Toasts: small notices in the bottom-right corner, drawn over the live
//! remote screen *without* taking input. Typing keeps going to the remote
//! session; the menu prefix opens the menu as usual.
//!
//! Drawing on top of another program's output is only safe at certain
//! moments, so the painter is careful about:
//!
//! - **Where the remote stream is.** `OutputTracker` follows every byte
//!   written to the real terminal and only allows painting in the ground
//!   state: never in the middle of an escape sequence, an OSC/DCS string,
//!   or a split UTF-8 character, and never inside the remote's own
//!   synchronized-output block (`CSI ? 2026 h`).
//! - **Restoring state.** The cursor position/visibility and the current
//!   drawing attributes are restored from the shadow `TerminalModel` rather
//!   than with DECSC/DECRC, whose single save slot the remote may be using.
//! - **Being painted over.** Remote output can overwrite the toast, so it is
//!   repainted once output settles (or periodically during a long burst).
//! - **Going away.** The cells under a toast are repainted from the model.
//!
//! Known gaps: a remote that enabled origin mode or insert mode, or switched
//! to the DEC line-drawing character set, would see the toast drawn
//! incorrectly; the model doesn't expose those modes.

use std::io::Write;
use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType, Clear, Padding, Paragraph, Widget};
use unicode_width::UnicodeWidthStr;

use super::ui::theme;
use crate::terminal_model::TerminalModel;

/// Output must have been quiet this long before painting, so a toast isn't
/// drawn into the middle of a screen the remote is busy redrawing...
const SETTLE: Duration = Duration::from_millis(30);
/// ...unless output never stops, in which case repaint this often anyway.
const BURST_REPAINT: Duration = Duration::from_millis(250);
const MIN_WIDTH: u16 = 26;
const MAX_WIDTH: u16 = 56;

/// Tracks physical output with the same bounded framing as the shadow model.
#[derive(Debug, Default)]
pub struct OutputTracker {
    stream: crate::terminal_stream::TerminalStream,
    last_output: Option<Instant>,
}

impl OutputTracker {
    pub fn feed(&mut self, bytes: &[u8], now: Instant) {
        if !bytes.is_empty() {
            self.last_output = Some(now);
            self.stream.observe(bytes);
        }
    }

    pub fn pending(&self) -> &[u8] {
        self.stream.pending()
    }

    pub fn synchronized_update(&self) -> bool {
        self.stream.synchronized_update()
    }

    pub fn quiet_for(&self, now: Instant) -> Duration {
        self.last_output
            .map(|last| now.saturating_duration_since(last))
            .unwrap_or(Duration::MAX)
    }

    /// Silence is not a sequence terminator. Wait for an actual boundary.
    pub fn at_boundary(&mut self, _now: Instant) -> bool {
        self.stream.at_boundary()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Info,
    Ok,
    Error,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Toast {
    pub tone: Tone,
    pub title: String,
    pub lines: Vec<Line<'static>>,
}

impl Toast {
    fn border_style(&self) -> Style {
        match self.tone {
            Tone::Info => theme::accent(),
            Tone::Ok => theme::ok(),
            Tone::Error => theme::error(),
        }
    }
}

/// Where `toast` goes on a `rows`×`cols` screen: bottom-right, clear of the
/// last row and column (status bars, autowrap). `None` if it doesn't fit.
pub fn placement(toast: &Toast, rows: u16, cols: u16) -> Option<Rect> {
    let content = toast
        .lines
        .iter()
        .map(|line| line.width())
        .max()
        .unwrap_or(0)
        .max(toast.title.width() + 2) as u16;
    // Borders plus a column of padding on each side.
    let width = (content + 4).clamp(MIN_WIDTH, MAX_WIDTH);
    let height = toast.lines.len() as u16 + 2;
    if cols < width + 2 || rows < height + 2 {
        return None;
    }
    Some(Rect {
        x: cols - width - 1,
        y: rows - height - 1,
        width,
        height,
    })
}

fn render_toast(toast: &Toast, area: Rect, buf: &mut Buffer) {
    Clear.render(area, buf);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(toast.border_style())
        .padding(Padding::horizontal(1))
        .title(Line::styled(format!(" {} ", toast.title), theme::title()));
    let inner = block.inner(area);
    block.render(area, buf);
    // Truncate rather than wrap: a toast is a fixed size.
    Paragraph::new(toast.lines.clone()).render(inner, buf);
}

/// Owns what's currently drawn and decides when to (re)paint.
#[derive(Debug, Default)]
pub struct ToastPainter {
    shown: Option<(Rect, Toast)>,
    /// Remote output may have drawn over the toast since it was painted.
    dirty: bool,
    last_paint: Option<Instant>,
}

impl ToastPainter {
    /// Remote output was written to the terminal.
    pub fn output_written(&mut self) {
        if self.shown.is_some() {
            self.dirty = true;
        }
    }

    /// The screen was cleared or redrawn by someone else (the menu opened);
    /// nothing of ours is on it any more.
    pub fn forget(&mut self) {
        self.shown = None;
        self.dirty = false;
    }

    #[cfg(test)]
    pub fn is_shown(&self) -> bool {
        self.shown.is_some()
    }

    /// Takes the toast down now, whatever the remote stream is doing; for
    /// when the session is ending and there won't be a better moment.
    pub fn clear(&mut self, model: &TerminalModel) -> Vec<u8> {
        if !model.can_paint() {
            self.forget();
            return Vec::new();
        }
        let Some((rect, _)) = self.shown.take() else {
            return Vec::new();
        };
        let (rows, cols) = model.screen().size();
        if rect.right() > cols.get() || rect.bottom() > rows.get() {
            return Vec::new();
        }
        let rect = restoration_area(rect, cols.get());
        let mut out = Vec::new();
        let mut buf = Buffer::empty(rect);
        model.render_into(&mut buf, rect);
        encode_area(&buf, rect, &mut out);
        out.extend_from_slice(model.screen().cursor_state_formatted().as_bytes());
        out.extend_from_slice(model.screen().attributes_formatted().as_bytes());
        out
    }

    /// Brings the screen in line with `desired`, if now is a safe moment.
    /// Returns the bytes to write (empty when nothing needs doing).
    pub fn sync(
        &mut self,
        desired: Option<&Toast>,
        model: &TerminalModel,
        tracker: &mut OutputTracker,
        now: Instant,
    ) -> Vec<u8> {
        if !model.can_paint() {
            self.forget();
            return Vec::new();
        }
        let (rows, cols) = model.screen().size();
        let target = desired
            .and_then(|toast| placement(toast, rows.get(), cols.get()).map(|rect| (rect, toast)));
        let unchanged = match (&self.shown, &target) {
            (None, None) => true,
            (Some((rect, shown)), Some((target_rect, toast))) => {
                rect == target_rect && shown == *toast && !self.dirty
            }
            _ => false,
        };
        if unchanged {
            return Vec::new();
        }
        if !tracker.at_boundary(now) {
            return Vec::new();
        }
        let settled = tracker.quiet_for(now) >= SETTLE;
        let overdue = self
            .last_paint
            .is_none_or(|last| now.saturating_duration_since(last) >= BURST_REPAINT);
        if !settled && !overdue {
            return Vec::new();
        }

        let mut out = Vec::new();
        out.extend_from_slice(b"\x1b[?2026h\x1b[?25l");
        if let Some((old, _)) = &self.shown {
            if target.as_ref().is_none_or(|(rect, _)| rect != old) {
                // Put back what the old toast covered.
                let area = restoration_area(*old, cols.get());
                let mut buf = Buffer::empty(area);
                model.render_into(&mut buf, area);
                encode_area(&buf, area, &mut out);
            }
        }
        match target {
            Some((rect, toast)) => {
                let mut buf = Buffer::empty(rect);
                render_toast(toast, rect, &mut buf);
                encode_area(&buf, rect, &mut out);
                self.shown = Some((rect, toast.clone()));
            }
            None => self.shown = None,
        }
        out.extend_from_slice(model.screen().cursor_state_formatted().as_bytes());
        out.extend_from_slice(model.screen().attributes_formatted().as_bytes());
        out.extend_from_slice(b"\x1b[?2026l");
        self.dirty = false;
        self.last_paint = Some(now);
        out
    }

    /// Paints immediately, regardless of the tracker. For tests.
    #[cfg(test)]
    fn sync_now(&mut self, desired: Option<&Toast>, model: &TerminalModel) -> Vec<u8> {
        let mut tracker = OutputTracker::default();
        self.last_paint = None;
        self.sync(desired, model, &mut tracker, Instant::now())
    }
}

fn restoration_area(area: Rect, cols: u16) -> Rect {
    // A toast edge can bisect a double-width character. Repaint one column
    // on either side so both halves are restored, including a leading cell
    // just outside the toast.
    let left = area.x.saturating_sub(1);
    let right = area.right().saturating_add(1).min(cols);
    Rect::new(left, area.y, right.saturating_sub(left), area.height)
}

/// Writes the cells of `area` from `buf` as positioned, styled text.
pub fn encode_area(buf: &Buffer, area: Rect, out: &mut Vec<u8>) {
    for y in area.top()..area.bottom() {
        write!(out, "\x1b[{};{}H", y + 1, area.x + 1).expect("writing to a Vec");
        let mut current: Option<Style> = None;
        let mut skip = 0usize;
        for x in area.left()..area.right() {
            if skip > 0 {
                skip -= 1;
                continue;
            }
            let Some(cell) = buf.cell((x, y)) else {
                continue;
            };
            let style = cell.style();
            if current != Some(style) {
                out.extend_from_slice(sgr(style).as_bytes());
                current = Some(style);
            }
            let symbol = cell.symbol();
            let symbol = if symbol.is_empty() { " " } else { symbol };
            let width = symbol.width().max(1);
            if x + width as u16 > area.right() {
                // A wide character that doesn't fit: blank instead of
                // spilling past the area.
                out.push(b' ');
                continue;
            }
            out.extend_from_slice(symbol.as_bytes());
            skip = width - 1;
        }
    }
    out.extend_from_slice(b"\x1b[0m");
}

fn sgr(style: Style) -> String {
    let mut codes = vec!["0".to_string()];
    let modifier = style.add_modifier;
    for (flag, code) in [
        (Modifier::BOLD, "1"),
        (Modifier::DIM, "2"),
        (Modifier::ITALIC, "3"),
        (Modifier::UNDERLINED, "4"),
        (Modifier::REVERSED, "7"),
        (Modifier::CROSSED_OUT, "9"),
    ] {
        if modifier.contains(flag) {
            codes.push(code.to_string());
        }
    }
    if let Some(code) = style.fg.and_then(|color| color_code(color, false)) {
        codes.push(code);
    }
    if let Some(code) = style.bg.and_then(|color| color_code(color, true)) {
        codes.push(code);
    }
    format!("\x1b[{}m", codes.join(";"))
}

fn color_code(color: Color, background: bool) -> Option<String> {
    let base = if background { 40 } else { 30 };
    let bright = if background { 100 } else { 90 };
    Some(match color {
        Color::Reset => return None,
        Color::Black => (base).to_string(),
        Color::Red => (base + 1).to_string(),
        Color::Green => (base + 2).to_string(),
        Color::Yellow => (base + 3).to_string(),
        Color::Blue => (base + 4).to_string(),
        Color::Magenta => (base + 5).to_string(),
        Color::Cyan => (base + 6).to_string(),
        Color::Gray => (base + 7).to_string(),
        Color::DarkGray => (bright).to_string(),
        Color::LightRed => (bright + 1).to_string(),
        Color::LightGreen => (bright + 2).to_string(),
        Color::LightYellow => (bright + 3).to_string(),
        Color::LightBlue => (bright + 4).to_string(),
        Color::LightMagenta => (bright + 5).to_string(),
        Color::LightCyan => (bright + 6).to_string(),
        Color::White => (bright + 7).to_string(),
        Color::Indexed(index) => format!("{};5;{index}", base + 8),
        Color::Rgb(r, g, b) => format!("{};2;{r};{g};{b}", base + 8),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use portable_pty::PtySize;
    use ratatui::text::Span;

    fn model(rows: u16, cols: u16) -> TerminalModel {
        TerminalModel::new(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
    }

    fn toast(text: &str) -> Toast {
        Toast {
            tone: Tone::Info,
            title: "Beams".to_string(),
            lines: vec![Line::from(Span::raw(text.to_string()))],
        }
    }

    /// Applies `bytes` (remote output, then our paint) to a fresh emulator
    /// the way the real terminal would see them.
    fn screen_after(rows: u16, cols: u16, chunks: &[&[u8]]) -> vt100::Parser {
        let mut parser = vt100::Parser::new(
            std::num::NonZeroU16::new(rows).unwrap(),
            std::num::NonZeroU16::new(cols).unwrap(),
            0,
        );
        for chunk in chunks {
            parser.process(chunk);
        }
        parser
    }

    /// What's visible: written spaces and never-written cells look alike.
    fn visible(screen: &vt100::Screen) -> String {
        screen
            .contents()
            .lines()
            .map(str::trim_end)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn tracker_only_allows_painting_between_sequences() {
        let now = Instant::now();
        let mut tracker = OutputTracker::default();
        assert!(tracker.at_boundary(now));
        tracker.feed(b"hello \x1b[3", now);
        assert!(!tracker.at_boundary(now), "inside a CSI");
        tracker.feed(b"1m red", now);
        assert!(tracker.at_boundary(now));
        tracker.feed(b"\x1b]0;title", now);
        assert!(!tracker.at_boundary(now), "inside an OSC");
        tracker.feed(b"\x07", now);
        assert!(tracker.at_boundary(now));
        tracker.feed(b"\x1bP+q", now);
        assert!(!tracker.at_boundary(now), "inside a DCS");
        tracker.feed(b"\x1b\\", now);
        assert!(tracker.at_boundary(now));
        tracker.feed(&"界".as_bytes()[..2], now);
        assert!(!tracker.at_boundary(now), "inside a UTF-8 character");
        tracker.feed(&"界".as_bytes()[2..], now);
        assert!(tracker.at_boundary(now));
        tracker.feed(b"\x1b[?2026h frame", now);
        assert!(
            !tracker.at_boundary(now),
            "inside the remote's synchronized update"
        );
        tracker.feed(b"\x1b[?2026l", now);
        assert!(tracker.at_boundary(now));
        tracker.feed(b"\x1b(B", now);
        assert!(tracker.at_boundary(now), "charset designation finished");
    }

    #[test]
    fn silence_does_not_end_an_unfinished_string() {
        let start = Instant::now();
        let mut tracker = OutputTracker::default();
        tracker.feed(b"\x1b]0;never ends", start);
        assert!(!tracker.at_boundary(start + Duration::from_millis(500)));
        assert!(!tracker.at_boundary(start + Duration::from_secs(20)));
        tracker.feed(b"\x1b\\", start + Duration::from_secs(21));
        assert!(tracker.at_boundary(start + Duration::from_secs(21)));
    }

    #[test]
    fn toasts_sit_bottom_right_clear_of_the_edges_and_skip_tiny_screens() {
        let rect = placement(&toast("Fetch big.bin (3.0 MB)"), 24, 80).unwrap();
        assert_eq!(rect.right(), 79);
        assert_eq!(rect.bottom(), 23);
        assert_eq!(rect.height, 3);
        assert_eq!(rect.width, MIN_WIDTH);
        assert!(placement(&toast("x"), 4, 80).is_none());
        assert!(placement(&toast("x"), 24, 20).is_none());
    }

    #[test]
    fn painting_restores_the_remote_cursor_and_attributes() {
        let mut model = model(10, 60);
        let remote: &[u8] = b"prompt$ \x1b[1;31m";
        model.process(remote);
        let mut painter = ToastPainter::default();
        let paint = painter.sync_now(Some(&toast("Ctrl-] to review")), &model);
        let parser = screen_after(10, 60, &[remote, &paint]);
        let screen = parser.screen();
        assert_eq!(
            screen.cursor_position(),
            (0, 8),
            "cursor back after the prompt"
        );
        assert!(
            screen.bold() && screen.fgcolor() == vt100::Color::Idx(1),
            "remote's SGR restored"
        );
        assert!(screen.contents().contains("Ctrl-] to review"));
        assert!(screen.contents().contains("╭"));
    }

    #[test]
    fn hiding_puts_back_what_the_toast_covered() {
        let mut model = model(10, 60);
        let mut remote = Vec::new();
        for row in 0..10 {
            remote.extend_from_slice(format!("line {row} {}\r\n", "x".repeat(50)).as_bytes());
        }
        remote.truncate(remote.len() - 2);
        model.process(&remote);
        let mut painter = ToastPainter::default();
        let paint = painter.sync_now(Some(&toast("hello")), &model);
        assert!(painter.is_shown());
        let hide = painter.sync_now(None, &model);
        assert!(!painter.is_shown());
        let parser = screen_after(10, 60, &[&remote, &paint, &hide]);
        assert_eq!(visible(parser.screen()), visible(model.screen()));
        assert_eq!(
            parser.screen().cursor_position(),
            model.screen().cursor_position()
        );
    }

    #[test]
    fn repaints_only_when_something_changed_or_was_drawn_over() {
        let model = model(10, 60);
        let mut painter = ToastPainter::default();
        let first = toast("one");
        assert!(!painter.sync_now(Some(&first), &model).is_empty());
        assert!(
            painter.sync_now(Some(&first), &model).is_empty(),
            "nothing changed"
        );
        painter.output_written();
        assert!(
            !painter.sync_now(Some(&first), &model).is_empty(),
            "drawn over"
        );
        assert!(
            !painter.sync_now(Some(&toast("two")), &model).is_empty(),
            "new content"
        );
    }

    #[test]
    fn waits_for_output_to_settle_or_a_long_burst() {
        let model = model(10, 60);
        let mut painter = ToastPainter::default();
        let mut tracker = OutputTracker::default();
        let start = Instant::now();
        let notice = toast("hi");
        tracker.feed(b"busy", start);
        // Never painted before, so the first paint is "overdue".
        assert!(!painter
            .sync(Some(&notice), &model, &mut tracker, start)
            .is_empty());
        painter.output_written();
        tracker.feed(b"more", start + Duration::from_millis(10));
        assert!(
            painter
                .sync(
                    Some(&notice),
                    &model,
                    &mut tracker,
                    start + Duration::from_millis(15)
                )
                .is_empty(),
            "output still flowing"
        );
        assert!(!painter
            .sync(
                Some(&notice),
                &model,
                &mut tracker,
                start + Duration::from_millis(50)
            )
            .is_empty());
        tracker.feed(b"\x1b[3", start + Duration::from_millis(400));
        painter.output_written();
        assert!(
            painter
                .sync(
                    Some(&notice),
                    &model,
                    &mut tracker,
                    start + Duration::from_millis(600)
                )
                .is_empty(),
            "mid-sequence"
        );
    }

    #[test]
    fn encoding_keeps_wide_characters_aligned() {
        let area = Rect::new(0, 0, 6, 1);
        let mut buf = Buffer::empty(area);
        buf.set_string(0, 0, "界ab", Style::default());
        let mut out = Vec::new();
        encode_area(&buf, area, &mut out);
        let parser = screen_after(1, 6, &[&out]);
        assert_eq!(visible(parser.screen()), "界ab");
    }

    #[test]
    fn hiding_a_toast_restores_wide_glyphs_bisected_by_its_edges() {
        let mut model = model(10, 60);
        let notice = toast("hello");
        let rect = placement(&notice, 10, 60).unwrap();
        let remote = format!(
            "\x1b[{};{}H界\x1b[{};{}H界",
            rect.y + 1,
            rect.x,
            rect.y + 2,
            rect.right(),
        );
        model.process(remote.as_bytes());
        let mut painter = ToastPainter::default();
        let paint = painter.sync_now(Some(&notice), &model);
        let clear = painter.clear(&model);
        let physical = screen_after(10, 60, &[remote.as_bytes(), &paint, &clear]);
        assert_eq!(
            visible(physical.screen()).trim_end(),
            visible(model.screen()).trim_end()
        );
    }

    #[test]
    fn unfinished_remote_prefix_can_be_replayed_after_local_painting() {
        let now = Instant::now();
        for (before, after) in [
            (b"\x1b[38;2;".as_slice(), b"1;2;3mred".as_slice()),
            (b"\x1b]2;ti".as_slice(), b"tle\x07".as_slice()),
            (&"界".as_bytes()[..2], &"界".as_bytes()[2..]),
        ] {
            let mut tracker = OutputTracker::default();
            tracker.feed(before, now);
            assert_eq!(tracker.pending(), before);
            let mut physical = screen_after(10, 60, &[before, b"\x18\x1b\\LOCAL"]);
            physical.process(b"\x1b[2J\x1b[H");
            physical.process(tracker.pending());
            physical.process(after);
            let expected = screen_after(10, 60, &[before, after]);
            assert_eq!(visible(physical.screen()), visible(expected.screen()));
            tracker.feed(after, now);
            assert!(tracker.pending().is_empty());
        }
    }
}
