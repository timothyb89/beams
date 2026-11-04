//! A shadow terminal model for the managed PTY.
//!
//! The normal session still writes remote bytes straight through to the real
//! terminal. This model observes the same bytes so an experimental local
//! overlay can repaint the remote screen underneath a ratatui widget later.
//! Keeping the model separate from the PTY relay makes it possible to measure
//! fidelity before making it the default output path.

use std::collections::VecDeque;
use std::num::NonZeroU16;

use portable_pty::PtySize;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color as TuiColor, Modifier, Style};
use ratatui::Frame;
use vt100::{Callbacks, Color as VtColor, Parser, Screen};

const MAX_EVENTS: usize = 128;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalEvent {
    AudibleBell,
    VisualBell,
    ResizeRequest { rows: u16, cols: u16 },
    WindowTitle(Vec<u8>),
    WindowIconName(Vec<u8>),
    CopyToClipboard { target: Vec<u8>, data: Vec<u8> },
    PasteFromClipboard { target: Vec<u8> },
    UnhandledOsc,
    UnhandledCsi(char),
    UnhandledEscape,
    UnhandledControl(u8),
    UnhandledCharacter(char),
}

#[derive(Default)]
struct EventCallbacks {
    events: VecDeque<TerminalEvent>,
}

impl EventCallbacks {
    fn push(&mut self, event: TerminalEvent) {
        if self.events.len() >= MAX_EVENTS {
            self.events.pop_front();
        }
        self.events.push_back(event);
    }

    fn take(&mut self) -> Vec<TerminalEvent> {
        self.events.drain(..).collect()
    }
}

impl Callbacks for EventCallbacks {
    fn audible_bell(&mut self, _: &mut Screen) {
        self.push(TerminalEvent::AudibleBell);
    }

    fn visual_bell(&mut self, _: &mut Screen) {
        self.push(TerminalEvent::VisualBell);
    }

    fn resize(&mut self, _: &mut Screen, request: (u16, u16)) {
        self.push(TerminalEvent::ResizeRequest {
            rows: request.0,
            cols: request.1,
        });
    }

    fn set_window_icon_name(&mut self, _: &mut Screen, name: &[u8]) {
        self.push(TerminalEvent::WindowIconName(name.to_vec()));
    }

    fn set_window_title(&mut self, _: &mut Screen, title: &[u8]) {
        self.push(TerminalEvent::WindowTitle(title.to_vec()));
    }

    fn copy_to_clipboard(&mut self, _: &mut Screen, target: &[u8], data: &[u8]) {
        self.push(TerminalEvent::CopyToClipboard {
            target: target.to_vec(),
            data: data.to_vec(),
        });
    }

    fn paste_from_clipboard(&mut self, _: &mut Screen, target: &[u8]) {
        self.push(TerminalEvent::PasteFromClipboard {
            target: target.to_vec(),
        });
    }

    fn unhandled_osc(&mut self, _: &mut Screen, _: &[&[u8]]) {
        self.push(TerminalEvent::UnhandledOsc);
    }

    fn unhandled_csi(
        &mut self,
        _: &mut Screen,
        _: Option<u8>,
        _: Option<u8>,
        _: &[&[u16]],
        final_byte: char,
    ) {
        self.push(TerminalEvent::UnhandledCsi(final_byte));
    }

    fn unhandled_escape(&mut self, _: &mut Screen, _: Option<u8>, _: Option<u8>, _: u8) {
        self.push(TerminalEvent::UnhandledEscape);
    }

    fn unhandled_control(&mut self, _: &mut Screen, byte: u8) {
        self.push(TerminalEvent::UnhandledControl(byte));
    }

    fn unhandled_char(&mut self, _: &mut Screen, character: char) {
        self.push(TerminalEvent::UnhandledCharacter(character));
    }
}

pub struct TerminalModel {
    parser: Parser<EventCallbacks>,
}

impl TerminalModel {
    pub fn new(size: PtySize) -> Self {
        Self {
            parser: Parser::new_with_callbacks(
                nonzero(size.rows),
                nonzero(size.cols),
                usize::from(size.rows).saturating_mul(4),
                EventCallbacks::default(),
            ),
        }
    }

    pub fn process(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    pub fn resize(&mut self, size: PtySize) {
        self.parser.set_size(nonzero(size.rows), nonzero(size.cols));
    }

    pub fn screen(&self) -> &Screen {
        self.parser.screen()
    }

    pub fn take_events(&mut self) -> Vec<TerminalEvent> {
        self.parser.callbacks_mut().take()
    }

    /// Paints the terminal model into a ratatui frame. Widgets rendered after
    /// this call naturally appear above the captured remote screen.
    pub fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        self.render_into(frame.buffer_mut(), area);
    }

    /// Copies the model's cells into `buf` at `area`, where `area` is in
    /// screen coordinates (so a sub-rectangle copies just that part of the
    /// screen).
    pub fn render_into(&self, buf: &mut Buffer, area: Rect) {
        let screen = self.screen();
        let (rows, cols) = screen.size();
        let bottom = area.bottom().min(rows.get());
        let right = area.right().min(cols.get());
        for row in area.top()..bottom {
            for col in area.left()..right {
                let Some(cell) = screen.cell(row, col) else {
                    continue;
                };
                if let Some(tui_cell) = buf.cell_mut((col, row)) {
                    tui_cell
                        .set_symbol(if cell.is_wide_continuation() {
                            " "
                        } else if cell.has_contents() {
                            cell.contents()
                        } else {
                            " "
                        })
                        .set_style(cell_style(cell));
                }
            }
        }
    }
}

fn nonzero(value: u16) -> NonZeroU16 {
    NonZeroU16::new(value.max(1)).expect("terminal dimensions are non-zero")
}

fn cell_style(cell: &vt100::Cell) -> Style {
    let mut fg = tui_color(cell.fgcolor());
    let mut bg = tui_color(cell.bgcolor());
    if cell.inverse() {
        std::mem::swap(&mut fg, &mut bg);
    }

    let mut style = Style::default().fg(fg).bg(bg);
    if cell.bold() {
        style = style.add_modifier(Modifier::BOLD);
    }
    if cell.dim() {
        style = style.add_modifier(Modifier::DIM);
    }
    if cell.italic() {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if cell.underline() {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    style
}

fn tui_color(color: VtColor) -> TuiColor {
    match color {
        VtColor::Default => TuiColor::Reset,
        VtColor::Idx(index) => TuiColor::Indexed(index),
        VtColor::Rgb(red, green, blue) => TuiColor::Rgb(red, green, blue),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::widgets::Paragraph;
    use ratatui::Terminal;

    fn size() -> PtySize {
        PtySize {
            rows: 4,
            cols: 12,
            pixel_width: 0,
            pixel_height: 0,
        }
    }

    #[test]
    fn tracks_screen_content_and_styles() {
        let mut model = TerminalModel::new(size());
        model.process(b"hello \x1b[31mred\x1b[0m");

        assert_eq!(model.screen().cell(0, 0).unwrap().contents(), "h");
        assert_eq!(
            model.screen().cell(0, 6).unwrap().fgcolor(),
            VtColor::Idx(1)
        );
        assert_eq!(model.screen().rows(0, 12).next().unwrap(), "hello red");
    }

    #[test]
    fn tracks_alternate_screen_resize_mouse_and_paste_modes() {
        let mut model = TerminalModel::new(size());
        model.process(b"\x1b[?1049h\x1b[?1006h\x1b[?1002h\x1b[?2004h");

        assert!(model.screen().alternate_screen());
        assert_eq!(
            model.screen().mouse_protocol_encoding(),
            vt100::MouseProtocolEncoding::Sgr
        );
        assert_eq!(
            model.screen().mouse_protocol_mode(),
            vt100::MouseProtocolMode::ButtonMotion
        );
        assert!(model.screen().bracketed_paste());

        model.resize(PtySize {
            rows: 6,
            cols: 20,
            pixel_width: 0,
            pixel_height: 0,
        });
        assert_eq!(model.screen().size().0.get(), 6);
        assert_eq!(model.screen().size().1.get(), 20);
    }

    #[test]
    fn tracks_wide_and_combining_cell_contents() {
        let mut model = TerminalModel::new(size());
        model.process("界e\u{301}".as_bytes());

        assert_eq!(model.screen().cell(0, 0).unwrap().contents(), "界");
        assert!(model.screen().cell(0, 0).unwrap().is_wide());
        assert!(model.screen().cell(0, 1).unwrap().is_wide_continuation());
        assert_eq!(model.screen().cell(0, 2).unwrap().contents(), "e\u{301}");
    }

    #[test]
    fn preserves_private_use_nerd_font_glyphs() {
        let mut model = TerminalModel::new(size());
        model.process("\u{f120} shell".as_bytes());

        let cell = model.screen().cell(0, 0).unwrap();
        assert_eq!(cell.contents(), "\u{f120}");
        assert!(!cell.is_wide());
        assert_eq!(model.screen().cell(0, 1).unwrap().contents(), " ");
    }

    #[test]
    fn exposes_osc_title_and_clipboard_events_without_acting_on_them() {
        let mut model = TerminalModel::new(size());
        model.process(b"\x1b]2;beam title\x07\x1b]52;c;SGVsbG8=\x07");

        assert_eq!(
            model.take_events(),
            vec![
                TerminalEvent::WindowTitle(b"beam title".to_vec()),
                TerminalEvent::CopyToClipboard {
                    target: b"c".to_vec(),
                    data: b"SGVsbG8=".to_vec(),
                },
            ]
        );
    }

    #[test]
    fn renders_shadow_screen_before_local_overlay_widgets() {
        let mut model = TerminalModel::new(size());
        model.process(b"remote zellij content");

        let backend = TestBackend::new(12, 4);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                model.render(frame, area);
                frame.render_widget(Paragraph::new("APPROVAL"), Rect::new(2, 1, 8, 1));
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer.cell((0, 0)).unwrap().symbol(), "r");
        assert_eq!(buffer.cell((2, 1)).unwrap().symbol(), "A");
        assert_eq!(buffer.cell((7, 0)).unwrap().symbol(), "z");
    }
}
