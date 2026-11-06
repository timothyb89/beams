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

use crate::terminal_stream::{TerminalStream, CONTROL_SEQUENCE_LIMIT};

const MAX_EVENTS: usize = 128;
const MAX_EVENT_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalEvent {
    AudibleBell,
    VisualBell,
    ResizeRequest { rows: u16, cols: u16 },
    WindowTitle(Vec<u8>),
    WindowIconName(Vec<u8>),
    CopyToClipboard {
        target: Vec<u8>,
        data: Vec<u8>,
    },
    PasteFromClipboard {
        target: Vec<u8>,
    },
    UnhandledOsc(Vec<u8>),
    /// Queries about the remote cursor are answered from the model at the
    /// instant of the query, rather than from the local dialog's cursor.
    Reply(Vec<u8>),
    /// Device/color/window queries still belong to the physical terminal.
    Query(Vec<u8>),
    /// Bounded DCS callback exposure; semantic query dispatch is still pending.
    UnhandledDcs(Vec<u8>),
    /// Explicit diagnostic for a consumer that did not drain its event queue.
    EventOverflow,
    UnhandledCsi(char),
    UnhandledEscape,
    UnhandledControl(u8),
    UnhandledCharacter(char),
}

#[derive(Default)]
struct EventCallbacks {
    events: VecDeque<TerminalEvent>,
    event_bytes: usize,
    dcs: Option<Vec<u8>>,
    reset_observed: bool,
}

impl EventCallbacks {
    fn push(&mut self, event: TerminalEvent) {
        let bytes = match &event {
            TerminalEvent::WindowTitle(bytes)
            | TerminalEvent::WindowIconName(bytes)
            | TerminalEvent::UnhandledOsc(bytes)
            | TerminalEvent::Reply(bytes)
            | TerminalEvent::Query(bytes)
            | TerminalEvent::UnhandledDcs(bytes) => bytes.len(),
            TerminalEvent::CopyToClipboard { target, data } => target.len() + data.len(),
            TerminalEvent::PasteFromClipboard { target } => target.len(),
            _ => 0,
        };
        if self.events.len() >= MAX_EVENTS || self.event_bytes + bytes > MAX_EVENT_BYTES {
            // Live query/effect frames are drained immediately. An offline
            // consumer or a malformed-text diagnostic flood gets a loss marker.
            self.events.clear();
            self.event_bytes = 0;
            self.events.push_back(TerminalEvent::EventOverflow);
        }
        self.event_bytes += bytes;
        self.events.push_back(event);
    }

    fn take(&mut self) -> Vec<TerminalEvent> {
        self.event_bytes = 0;
        self.events.drain(..).collect()
    }
}

impl Callbacks for EventCallbacks {
    fn terminal_reset(&mut self, _: &mut Screen) {
        self.reset_observed = true;
        self.dcs = None;
    }

    fn dcs_hook(
        &mut self,
        _: &mut Screen,
        params: &[&[u16]],
        intermediates: &[u8],
        ignore: bool,
        action: char,
    ) {
        if ignore {
            self.dcs = None;
            return;
        }
        let mut header = csi(
            intermediates.first().copied(),
            intermediates.get(1).copied(),
            if params == [&[0][..]] { &[] } else { params },
            action,
        );
        header[1] = b'P';
        self.dcs = Some(header);
    }

    fn dcs_put(&mut self, _: &mut Screen, byte: u8) {
        if let Some(dcs) = &mut self.dcs {
            if dcs.len() < CONTROL_SEQUENCE_LIMIT {
                dcs.push(byte);
            } else {
                self.dcs = None;
                self.push(TerminalEvent::EventOverflow);
            }
        }
    }

    fn dcs_unhook(&mut self, _: &mut Screen) {
        if let Some(mut dcs) = self.dcs.take() {
            dcs.extend_from_slice(b"\x1b\\");
            self.push(TerminalEvent::UnhandledDcs(dcs));
        }
    }

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

    fn unhandled_osc(&mut self, _: &mut Screen, params: &[&[u8]]) {
        self.push(TerminalEvent::UnhandledOsc(osc(params)));
    }

    fn unhandled_csi(
        &mut self,
        screen: &mut Screen,
        i1: Option<u8>,
        i2: Option<u8>,
        params: &[&[u16]],
        final_byte: char,
    ) {
        if i2.is_none() && final_byte == 'n' {
            if i1.is_none() && params == [&[5][..]] {
                self.push(TerminalEvent::Reply(b"\x1b[0n".to_vec()));
                return;
            }
            if matches!(i1, None | Some(b'?')) && params == [&[6][..]] {
                let (row, col) = screen.cursor_position();
                let state = screen.drawing_state();
                let buffer = if state.alternate_screen {
                    state.alternate
                } else {
                    state.primary
                };
                let row = if buffer.cursor.origin_mode {
                    row.saturating_sub(buffer.scroll_region.0)
                } else {
                    row
                };
                let cols = screen.size().1.get();
                let private = i1 == Some(b'?');
                self.push(TerminalEvent::Reply(
                    format!(
                        "\x1b[{}{};{}{}R",
                        if private { "?" } else { "" },
                        row + 1,
                        col.min(cols - 1) + 1,
                        if private { ";1" } else { "" }
                    )
                    .into_bytes(),
                ));
                return;
            }
        }
        if i1 == Some(b'?') && i2 == Some(b'$') && final_byte == 'p' && params.len() == 1 {
            let mode = params[0].first().copied().unwrap_or(0);
            let enabled = match mode {
                1 => Some(screen.application_cursor()),
                25 => Some(!screen.hide_cursor()),
                2004 => Some(screen.bracketed_paste()),
                _ => None,
            };
            if let Some(enabled) = enabled {
                self.push(TerminalEvent::Reply(
                    format!("\x1b[?{mode};{}$y", if enabled { 1 } else { 2 }).into_bytes(),
                ));
                return;
            }
        }
        let window_report = final_byte == 't'
            && params
                .first()
                .and_then(|p| p.first())
                .is_some_and(|p| (11..=21).contains(p));
        if matches!(final_byte, 'c' | 'n')
            || window_report
            || (final_byte == 'u' && i1 == Some(b'?'))
        {
            self.push(TerminalEvent::Query(csi(i1, i2, params, final_byte)));
            return;
        }
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

fn osc(params: &[&[u8]]) -> Vec<u8> {
    let mut out = b"\x1b]".to_vec();
    for (index, param) in params.iter().enumerate() {
        if index > 0 {
            out.push(b';');
        }
        out.extend_from_slice(param);
    }
    out.extend_from_slice(b"\x1b\\");
    out
}

fn csi(i1: Option<u8>, i2: Option<u8>, params: &[&[u16]], final_byte: char) -> Vec<u8> {
    let private = i1.filter(|byte| (0x3c..=0x3f).contains(byte));
    let mut out = "\x1b[".to_string();
    if let Some(private) = private {
        out.push(char::from(private));
    }
    for (index, param) in params.iter().enumerate() {
        if index > 0 {
            out.push(';');
        }
        for (sub_index, value) in param.iter().enumerate() {
            if sub_index > 0 {
                out.push(':');
            }
            out.push_str(&value.to_string());
        }
    }
    for intermediate in [i1.filter(|_| private.is_none()), i2].into_iter().flatten() {
        out.push(char::from(intermediate));
    }
    out.push(final_byte);
    out.into_bytes()
}

impl TerminalEvent {
    /// Screen-independent effects to relay while the overlay hides screen
    /// writes. Reconstruct complete commands from parser callbacks, so an OSC
    /// split across the menu boundary still reaches the terminal exactly once.
    pub fn terminal_bytes(&self) -> Option<Vec<u8>> {
        Some(match self {
            Self::AudibleBell => vec![b'\x07'],
            Self::VisualBell => b"\x1bg".to_vec(),
            Self::WindowTitle(title) => osc(&[b"2", title]),
            Self::WindowIconName(name) => osc(&[b"1", name]),
            Self::CopyToClipboard { target, data } => osc(&[b"52", target, data]),
            Self::PasteFromClipboard { target } => osc(&[b"52", target, b"?"]),
            Self::UnhandledOsc(bytes) | Self::Query(bytes) => bytes.clone(),
            Self::ResizeRequest { rows, cols } => format!("\x1b[8;{rows};{cols}t").into_bytes(),
            _ => return None,
        })
    }
}

pub struct TerminalModel {
    parser: Parser<EventCallbacks>,
    stream: TerminalStream,
    integrity_lost: bool,
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
            stream: TerminalStream::default(),
            integrity_lost: false,
        }
    }

    pub fn process(&mut self, bytes: &[u8]) {
        let mut remaining = bytes;
        while !remaining.is_empty() {
            let consumed = self
                .stream
                .advance(remaining, |frame| {
                    self.parser.process(frame);
                    if std::mem::take(&mut self.parser.callbacks_mut().reset_observed) {
                        self.integrity_lost = false;
                    }
                    Ok::<(), std::convert::Infallible>(())
                })
                .unwrap();
            remaining = &remaining[consumed..];
            if !remaining.is_empty() {
                self.discard_oversized_sequence();
            }
        }
    }

    /// Drain events at each frame so query/effect floods cannot overflow the
    /// live relay's bounded event queue. Stop before exceeding the byte budget.
    pub fn process_with_events<E>(
        &mut self,
        bytes: &[u8],
        mut event: impl FnMut(TerminalEvent) -> Result<(), E>,
    ) -> Result<usize, E> {
        self.stream.advance(bytes, |frame| {
            self.parser.process(frame);
            if std::mem::take(&mut self.parser.callbacks_mut().reset_observed) {
                self.integrity_lost = false;
            }
            for item in self.parser.callbacks_mut().take() {
                event(item)?;
            }
            Ok(())
        })
    }

    pub fn discard_oversized_sequence(&mut self) {
        self.integrity_lost = true;
        self.stream.discard_sequence();
    }

    /// Budget exhaustion makes repainting unsafe until an observed full reset.
    pub fn can_paint(&self) -> bool {
        !self.integrity_lost
    }

    pub fn drawing_state(&self) -> vt100::DrawingState {
        self.screen().drawing_state()
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

    /// Restores the buffer covered by the menu and the remote's current one.
    /// `state_formatted` alone does not select an alternate screen, preserve
    /// the inactive buffer, or disable a previously enabled mouse mode.
    pub fn restore_bytes(&self, covered_alternate: bool) -> Vec<u8> {
        let screen = self.screen();
        let (rows, cols) = screen.size();
        let mut copy = Parser::new(rows, cols, 0);
        *copy.screen_mut() = screen.clone();
        let active = self.drawing_state().alternate_screen;
        let mut bytes = b"\x18\x1b\\\x1b[?2026h".to_vec();
        let buffers = if covered_alternate == active {
            vec![active]
        } else {
            vec![covered_alternate, active]
        };
        for alternate in buffers {
            // Mode 47 selects a buffer without clearing it or overwriting
            // the application's DECSC saved-cursor slot as 1049 would.
            let select = if alternate {
                b"\x1b[?47h"
            } else {
                b"\x1b[?47l"
            };
            copy.process(select);
            bytes.extend_from_slice(select);
            bytes.extend_from_slice(copy.screen().contents_formatted().as_bytes());
        }
        bytes.extend_from_slice(b"\x1b[?9l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l");
        bytes.extend_from_slice(screen.input_mode_formatted().as_bytes());
        bytes.extend_from_slice(b"\x1b[?2026l");
        bytes
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
    let mut style = Style::default()
        .fg(tui_color(cell.fgcolor()))
        .bg(tui_color(cell.bgcolor()));
    // Swapping Reset/Reset loses inverse video with terminal-default colors.
    if cell.inverse() {
        style = style.add_modifier(Modifier::REVERSED);
    }
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

    #[test]
    fn queries_capture_the_remote_cursor_at_the_query_and_relay_device_queries() {
        let mut model = TerminalModel::new(size());
        model.process(b"\x1b[3;7H\x1b[6n\x1b[Hafter\x1b[5n\x1b[>0c\x1b[18t\x1b[?u");
        assert_eq!(
            model.take_events(),
            vec![
                TerminalEvent::Reply(b"\x1b[3;7R".to_vec()),
                TerminalEvent::Reply(b"\x1b[0n".to_vec()),
                TerminalEvent::Query(b"\x1b[>0c".to_vec()),
                TerminalEvent::Query(b"\x1b[18t".to_vec()),
                TerminalEvent::Query(b"\x1b[?0u".to_vec()),
            ]
        );
    }

    #[test]
    fn complete_osc_effects_are_relayed_once_at_every_chunk_boundary() {
        for input in [
            b"\x1b]2;beam title\x07".as_slice(),
            b"\x1b]52;c;SGVsbG8=\x1b\\".as_slice(),
            b"\x1b]52;c;?\x07".as_slice(),
            b"\x1b]10;?\x1b\\".as_slice(),
        ] {
            for split in 0..input.len() {
                let mut model = TerminalModel::new(size());
                model.process(&input[..split]);
                // vte dispatches an ST-terminated OSC at ESC, leaving the
                // final backslash to its escape parser.
                let mut events = model.take_events();
                model.process(&input[split..]);
                events.extend(model.take_events());
                let effects = events
                    .iter()
                    .filter_map(TerminalEvent::terminal_bytes)
                    .collect::<Vec<_>>();
                assert_eq!(effects.len(), 1, "split {split}: {input:?}");
                assert!(effects[0].starts_with(b"\x1b]"));
                assert!(effects[0].ends_with(b"\x1b\\"));
            }
        }
    }

    fn visible(screen: &Screen) -> String {
        screen
            .rows(0, screen.size().1.get())
            .map(|row| row.trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn restore_selects_the_remote_screen_and_removes_the_menu_from_its_buffer() {
        for (before, hidden) in [
            (b"main".as_slice(), b"\x1b[?1049halt".as_slice()),
            (b"main\x1b[?1049halt".as_slice(), b"\x1b[?1049l!".as_slice()),
            (
                b"main\x1b[?1049halt".as_slice(),
                b"\x1b[?1049l\x1b[?1049hnew".as_slice(),
            ),
        ] {
            let mut model = TerminalModel::new(size());
            let mut physical = Parser::new(nonzero(4), nonzero(12), 0);
            model.process(before);
            physical.process(before);
            let covered_alternate = physical.screen().alternate_screen();
            physical.process(b"\x1b[2J\x1b[HLOCAL MENU");
            model.process(hidden);
            physical.process(&model.restore_bytes(covered_alternate));
            assert_eq!(
                physical.screen().alternate_screen(),
                model.screen().alternate_screen()
            );
            assert_eq!(visible(physical.screen()), visible(model.screen()));
            assert_eq!(
                physical.screen().cursor_position(),
                model.screen().cursor_position()
            );
            let mut expected = Parser::new(nonzero(4), nonzero(12), 0);
            *expected.screen_mut() = model.screen().clone();
            for switch in [b"\x1b[?47h", b"\x1b[?47l"] {
                physical.process(switch);
                expected.process(switch);
                assert_eq!(visible(physical.screen()), visible(expected.screen()));
            }
        }
    }

    #[test]
    fn restore_disables_mouse_modes_and_encodings_that_changed_while_hidden() {
        let mut model = TerminalModel::new(size());
        let mut physical = Parser::new(nonzero(4), nonzero(12), 0);
        let before = b"\x1b[?1002h\x1b[?1006h\x1b[?2004h";
        model.process(before);
        physical.process(before);
        model.process(b"\x1b[?1002l\x1b[?1006l\x1b[?2004l");
        physical.process(&model.restore_bytes(false));
        assert_eq!(
            physical.screen().mouse_protocol_mode(),
            vt100::MouseProtocolMode::None
        );
        assert_eq!(
            physical.screen().mouse_protocol_encoding(),
            vt100::MouseProtocolEncoding::Default
        );
        assert!(!physical.screen().bracketed_paste());
    }

    #[test]
    fn alternate_overlay_does_not_erase_unobserved_workstation_main_screen() {
        let mut model = TerminalModel::new(size());
        let mut physical = Parser::new(nonzero(4), nonzero(12), 0);
        physical.process(b"workstation");
        let main = visible(physical.screen());
        let remote = b"\x1b[?1049hremote";
        model.process(remote);
        physical.process(remote);
        physical.process(b"\x1b[2J\x1b[HLOCAL MENU");
        physical.process(&model.restore_bytes(true));
        physical.process(b"\x1b[?1049l");
        assert_eq!(visible(physical.screen()), main);
    }

    #[test]
    fn restoration_keeps_wide_combining_rgb_inverse_and_wrap_pending_state() {
        let mut model = TerminalModel::new(size());
        model.process("\x1b[38;2;1;2;3m界e\u{301}\x1b[0;7m inverse".as_bytes());
        let mut physical = Parser::new(nonzero(4), nonzero(12), 0);
        physical.process(&model.restore_bytes(false));
        assert_eq!(visible(physical.screen()), visible(model.screen()));
        assert_eq!(
            physical.screen().cell(0, 0).unwrap().fgcolor(),
            VtColor::Rgb(1, 2, 3)
        );
        assert!(physical.screen().cell(0, 5).unwrap().inverse());
        assert!(cell_style(model.screen().cell(0, 5).unwrap())
            .add_modifier
            .contains(Modifier::REVERSED));

        model.process(b"\x1b[Habcdefghijkl");
        physical.process(&model.restore_bytes(false));
        model.process(b"Z");
        physical.process(b"Z");
        assert_eq!(visible(physical.screen()), visible(model.screen()));
        assert_eq!(
            physical.screen().cursor_position(),
            model.screen().cursor_position()
        );
    }

    // Executable default-readiness gates. Keep these visibly ignored until
    // their tracked restoration/protocol gaps are fixed; see the audit doc.
    #[test]
    #[ignore = "default gate: serialize saved cursor/attributes changed while the overlay is open"]
    fn overlay_acceptance_saved_cursor() {
        let mut model = TerminalModel::new(size());
        let mut physical = Parser::new(nonzero(4), nonzero(12), 0);
        let before = b"\x1b[2;3H\x1b7";
        model.process(before);
        physical.process(before);
        physical.process(b"\x1b[2J\x1b[HLOCAL MENU");
        model.process(b"\x1b[3;7H\x1b7\x1b[H");
        physical.process(&model.restore_bytes(false));
        model.process(b"\x1b8X");
        physical.process(b"\x1b8X");
        assert_eq!(visible(physical.screen()), visible(model.screen()));
    }

    #[test]
    #[ignore = "default gate: suspend and restore origin mode and per-buffer scroll margins"]
    fn overlay_acceptance_origin_and_margins() {
        let mut model = TerminalModel::new(size());
        let mut physical = Parser::new(nonzero(4), nonzero(12), 0);
        let before = b"\x1b[2;4r\x1b[?6h\x1b[2;3Hremote";
        model.process(before);
        physical.process(before);
        physical.process(b"\x1b[2J\x1b[HLOCAL MENU");
        physical.process(&model.restore_bytes(false));
        assert_eq!(visible(physical.screen()), visible(model.screen()));
        model.process(b"\r\nnext");
        physical.process(b"\r\nnext");
        assert_eq!(visible(physical.screen()), visible(model.screen()));
    }

    #[test]
    fn private_cursor_and_paste_mode_reports_use_remote_state() {
        let mut model = TerminalModel::new(size());
        model.process(b"\x1b[3;7H\x1b[?6n\x1b[?2004$p\x1b[?2004h\x1b[?2004$p");
        assert_eq!(
            model.take_events(),
            vec![
                TerminalEvent::Reply(b"\x1b[?3;7;1R".to_vec()),
                TerminalEvent::Reply(b"\x1b[?2004;2$y".to_vec()),
                TerminalEvent::Reply(b"\x1b[?2004;1$y".to_vec()),
            ]
        );
    }

    #[test]
    fn snapshots_include_hidden_saved_attributes_and_both_buffers() {
        let mut model = TerminalModel::new(size());
        model.process(b"\x1b[2;4r\x1b[?6h\x1b[2;3H\x1b[31m\x1b7\x1b[0m");
        let before = model.drawing_state();
        assert_eq!(before.primary.cursor.row, 2);
        assert_eq!(before.primary.saved_cursor.col, 2);
        assert!(before.primary.saved_cursor.origin_mode);
        assert_eq!(before.primary.scroll_region, (1, 3));
        assert_eq!(before.saved_attributes, "\x1b[m\x1b[31m");
        model.process(b"\x1b[?47h\x1b[1;3r\x1b[3;7H\x1b[32m\x1b7\x1b[H");
        let hidden = model.drawing_state();
        assert!(hidden.alternate_screen);
        assert_eq!(hidden.primary, before.primary);
        assert_eq!(hidden.alternate.saved_cursor.row, 2);
        assert_eq!(hidden.alternate.saved_cursor.col, 6);
        assert_eq!(hidden.alternate.scroll_region, (0, 2));
        assert_eq!(hidden.saved_attributes, "\x1b[m\x1b[32m");
    }

    #[test]
    fn origin_relative_cursor_reports_use_the_remote_scroll_region() {
        let mut model = TerminalModel::new(size());
        model.process(b"\x1b[2;4r\x1b[?6h\x1b[2;3H\x1b[6n\x1b[?6n");
        assert_eq!(
            model.take_events(),
            vec![
                TerminalEvent::Reply(b"\x1b[2;3R".to_vec()),
                TerminalEvent::Reply(b"\x1b[?2;3;1R".to_vec()),
            ],
        );
    }

    #[test]
    fn dcs_callbacks_preserve_split_headers_and_bodies() {
        let query = b"\x1bP$qm\x1b\\";
        for split in 0..=query.len() {
            let mut model = TerminalModel::new(size());
            model.process(&query[..split]);
            model.process(&query[split..]);
            assert_eq!(
                model.take_events(),
                vec![TerminalEvent::UnhandledDcs(query.to_vec())],
                "split at {split}",
            );
        }
    }

    #[test]
    fn live_query_dispatch_does_not_drop_events_under_queue_pressure() {
        let mut model = TerminalModel::new(size());
        let queries = b"\x1b[5n".repeat(MAX_EVENTS * 4);
        let mut replies = Vec::new();
        let consumed = model
            .process_with_events(&queries, |event| {
                replies.push(event);
                Ok::<(), ()>(())
            })
            .unwrap();
        assert_eq!(consumed, queries.len());
        assert_eq!(
            replies,
            vec![TerminalEvent::Reply(b"\x1b[0n".to_vec()); MAX_EVENTS * 4],
        );
        assert!(model.take_events().is_empty());
    }

    #[test]
    fn oversized_osc_is_not_partially_applied_and_a_full_reset_recovers() {
        let mut model = TerminalModel::new(size());
        let mut bytes = b"\x1b]52;c;".to_vec();
        bytes.extend(std::iter::repeat_n(b'A', CONTROL_SEQUENCE_LIMIT * 4));
        bytes.extend_from_slice(b"\x1b\\visible");
        model.process(&bytes);
        assert!(!model.can_paint());
        assert!(model.take_events().is_empty());
        assert_eq!(model.screen().contents(), "visible");
        model.process(b"\x1bc");
        assert!(model.can_paint());
        assert_eq!(model.screen().contents(), "");
    }

    #[test]
    fn full_reset_recovers_when_it_aborts_an_oversized_sequence() {
        for start in [b"\x1b]2;".as_slice(), b"\x1bP$q", b"\x1b["] {
            let mut model = TerminalModel::new(size());
            let mut bytes = start.to_vec();
            bytes.extend(std::iter::repeat_n(b'0', CONTROL_SEQUENCE_LIMIT));
            model.process(&bytes);
            assert!(!model.can_paint());
            model.process(b"\x1b");
            model.process(b"c");
            assert!(model.can_paint(), "reset after {start:?}");
            assert_eq!(model.screen().contents(), "");
        }
    }

    #[test]
    #[ignore = "default gate: DECRQSS/DCS query relay and remote-state replies"]
    fn overlay_acceptance_dcs_query() {
        let mut model = TerminalModel::new(size());
        model.process(b"\x1bP$qm\x1b\\");
        assert_eq!(
            model.take_events(),
            vec![TerminalEvent::Query(b"\x1bP$qm\x1b\\".to_vec())]
        );
    }
}
