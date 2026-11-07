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

use crate::terminal_eligibility::{Evidence, Unavailable};
use crate::terminal_stream::{TerminalStream, CONTROL_SEQUENCE_LIMIT};

const MAX_EVENTS: usize = 128;
const MAX_EVENT_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalEvent {
    AudibleBell,
    VisualBell,
    ResizeRequest {
        rows: u16,
        cols: u16,
    },
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
    /// Unsupported DCS commands are diagnostic-only while output is hidden.
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
    ended_dcs: Option<Vec<u8>>,
    attributes_unknown: bool,
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
        self.ended_dcs = None;
        self.attributes_unknown = false;
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
        // vte unhooks on ESC as well as CAN/SUB. The stream controller must
        // confirm ST before dispatching this candidate.
        self.ended_dcs = self.dcs.take();
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
        if i1.is_none() && i2.is_none() && final_byte == 'm' {
            // The backend rejected an attribute. Do not report a partial SGR
            // state as complete; a full reset reestablishes confidence.
            self.attributes_unknown = true;
        }
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
        if final_byte == 'p'
            && ((i1 == Some(b'?') && i2 == Some(b'$')) || (i1 == Some(b'$') && i2.is_none()))
            && params.len() == 1
            && params[0].len() == 1
        {
            let mode = params[0].first().copied().unwrap_or(0);
            let private = i1 == Some(b'?');
            let enabled = match (private, mode) {
                (false, 4) => Some(screen.drawing_state().insert_mode),
                (true, 1) => Some(screen.application_cursor()),
                (true, 6) => {
                    let state = screen.drawing_state();
                    Some(if state.alternate_screen {
                        state.alternate.cursor.origin_mode
                    } else {
                        state.primary.cursor.origin_mode
                    })
                }
                (true, 25) => Some(!screen.hide_cursor()),
                (true, 47 | 1049) => Some(screen.alternate_screen()),
                (true, 66) => Some(screen.application_keypad()),
                (true, 9) => Some(screen.mouse_protocol_mode() == vt100::MouseProtocolMode::Press),
                (true, 1000) => {
                    Some(screen.mouse_protocol_mode() == vt100::MouseProtocolMode::PressRelease)
                }
                (true, 1002) => {
                    Some(screen.mouse_protocol_mode() == vt100::MouseProtocolMode::ButtonMotion)
                }
                (true, 1003) => {
                    Some(screen.mouse_protocol_mode() == vt100::MouseProtocolMode::AnyMotion)
                }
                (true, 1005) => {
                    Some(screen.mouse_protocol_encoding() == vt100::MouseProtocolEncoding::Utf8)
                }
                (true, 1006) => {
                    Some(screen.mouse_protocol_encoding() == vt100::MouseProtocolEncoding::Sgr)
                }
                (true, 2004) => Some(screen.bracketed_paste()),
                _ => None,
            };
            self.push(TerminalEvent::Reply(
                format!(
                    "\x1b[{}{mode};{}$y",
                    if private { "?" } else { "" },
                    enabled.map_or(0, |enabled| if enabled { 1 } else { 2 })
                )
                .into_bytes(),
            ));
            return;
        }
        let window_report = final_byte == 't'
            && params
                .first()
                .and_then(|p| p.first())
                .is_some_and(|p| (11..=21).contains(p));
        if matches!(final_byte, 'c' | 'n')
            || window_report
            || (final_byte == 'u' && i1 == Some(b'?') && i2.is_none())
            || (final_byte == 'm' && i1 == Some(b'?') && i2.is_none())
            || (final_byte == 'q' && i1 == Some(b'>') && i2.is_none() && params == [&[0][..]])
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

/// The framer delivers a complete ST in the same frame as its string, and
/// separates an aborting ESC command. Never infer completion from unhook.
fn process_frame(parser: &mut Parser<EventCallbacks>, frame: &[u8]) {
    parser.process(frame);
    let Some(mut dcs) = parser.callbacks_mut().ended_dcs.take() else {
        return;
    };
    if !frame.ends_with(b"\x1b\\") {
        return;
    }
    let event = if let Some(setting) = dcs.strip_prefix(b"\x1bP$q") {
        let state = parser.screen().drawing_state();
        let value = match setting {
            b"m" if !parser.callbacks().attributes_unknown => {
                // Snapshot SGR is a reset followed by attribute commands.
                // DECRQSS returns their parameters as one CSI payload.
                let params = state
                    .attributes
                    .split("\x1b[")
                    .filter_map(|s| s.strip_suffix('m'))
                    .map(|s| if s.is_empty() { "0" } else { s })
                    .collect::<Vec<_>>()
                    .join(";");
                Some(format!("{params}m"))
            }
            b"r" => {
                let buffer = if state.alternate_screen {
                    state.alternate
                } else {
                    state.primary
                };
                let (top, bottom) = buffer.scroll_region;
                Some(format!("{};{}r", top + 1, bottom + 1))
            }
            _ => None,
        };
        TerminalEvent::Reply(match value {
            Some(value) => format!("\x1bP1$r{value}\x1b\\").into_bytes(),
            None => b"\x1bP0$r\x1b\\".to_vec(),
        })
    } else if dcs.starts_with(b"\x1bP+q") {
        // XTGETTCAP includes dynamically adjusted key capabilities. Local UI
        // changes input modes; do not relay probes against those settings.
        TerminalEvent::Reply(b"\x1bP0+r\x1b\\".to_vec())
    } else if dcs.starts_with(b"\x1bP+Q") {
        TerminalEvent::Reply(b"\x1bP0+R\x1b\\".to_vec())
    } else {
        dcs.extend_from_slice(b"\x1b\\");
        TerminalEvent::UnhandledDcs(dcs)
    };
    parser.callbacks_mut().push(event);
}

pub struct TerminalModel {
    parser: Parser<EventCallbacks>,
    stream: TerminalStream,
    integrity_lost: bool,
    evidence: Evidence,
    overlay_buffers: Option<[bool; 2]>,
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
            evidence: Evidence::default(),
            overlay_buffers: None,
        }
    }

    pub fn process(&mut self, bytes: &[u8]) {
        let mut remaining = bytes;
        while !remaining.is_empty() {
            let (consumed, _) = self
                .process_guarded_with_events(
                    remaining,
                    false,
                    |_| {
                        // Preserve events for offline callers, as before.
                        Ok::<(), std::convert::Infallible>(())
                    },
                    false,
                )
                .unwrap();
            remaining = &remaining[consumed..];
            if !remaining.is_empty() {
                self.discard_oversized_sequence();
            }
        }
    }

    /// Drain events at each frame so query/effect floods cannot overflow the
    /// live relay's bounded event queue. Stop before exceeding the byte budget.
    #[cfg(test)]
    pub fn process_with_events<E>(
        &mut self,
        bytes: &[u8],
        mut event: impl FnMut(TerminalEvent) -> Result<(), E>,
    ) -> Result<usize, E> {
        self.process_guarded_with_events(bytes, false, &mut event, true)
            .map(|(consumed, _)| consumed)
    }

    pub fn process_guarded_with_events<E>(
        &mut self,
        bytes: &[u8],
        covered: bool,
        mut event: impl FnMut(TerminalEvent) -> Result<(), E>,
        drain: bool,
    ) -> Result<(usize, Option<Vec<u8>>), E> {
        self.stream.advance_guarded(bytes, |frame, original| {
            let supported = self.evidence.supported(original, self.parser.screen());
            if covered
                && (!supported
                    || original == b"\x1bc"
                    || self
                        .evidence
                        .enters_unknown_buffer(original, self.parser.screen().alternate_screen()))
            {
                return Ok(false);
            }
            if !supported {
                self.evidence.invalidate(Unavailable::UnsupportedSequence);
            }
            let before_alternate = self.parser.screen().alternate_screen();
            if let Some(buffers) = &mut self.overlay_buffers {
                buffers[usize::from(before_alternate)] = true;
            }
            process_frame(&mut self.parser, frame);
            if std::mem::take(&mut self.parser.callbacks_mut().reset_observed) {
                self.integrity_lost = false;
                self.evidence.observe(b"\x1bc", false, self.parser.screen());
            } else {
                self.evidence
                    .observe(original, before_alternate, self.parser.screen());
            }
            if let Some(buffers) = &mut self.overlay_buffers {
                buffers[usize::from(self.parser.screen().alternate_screen())] = true;
            }
            if drain {
                for item in self.parser.callbacks_mut().take() {
                    event(item)?;
                }
            }
            Ok(true)
        })
    }

    pub fn discard_oversized_sequence(&mut self) {
        self.integrity_lost = true;
        self.evidence.invalidate(Unavailable::SequenceBudget);
        self.stream.discard_sequence();
    }

    /// Budget exhaustion makes repainting unsafe until an observed full reset.
    pub fn can_paint(&self) -> bool {
        self.unavailable_reason().is_none()
            && self.can_paint_buffer(self.screen().alternate_screen())
    }

    pub fn can_paint_buffer(&self, alternate: bool) -> bool {
        !self.integrity_lost && self.evidence.can_paint(alternate)
    }

    pub fn unavailable_reason(&self) -> Option<Unavailable> {
        self.evidence
            .unavailable_reason(self.screen().alternate_screen())
    }

    pub fn begin_overlay(&mut self) {
        self.overlay_buffers = Some([
            !self.screen().alternate_screen(),
            self.screen().alternate_screen(),
        ]);
    }

    pub fn end_overlay(&mut self) {
        self.overlay_buffers = None;
    }

    pub fn drawing_state(&self) -> vt100::DrawingState {
        self.screen().drawing_state()
    }

    pub fn resize(&mut self, size: PtySize) {
        self.parser.set_size(nonzero(size.rows), nonzero(size.cols));
        self.evidence.resize();
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
        let mut bytes = crate::terminal_paint::BEGIN.to_vec();
        let touched = self.overlay_buffers.unwrap_or([false; 2]);
        let mut buffers = Vec::new();
        for alternate in [!active, active] {
            if alternate == active
                || alternate == covered_alternate
                || touched[usize::from(alternate)]
            {
                buffers.push(alternate);
            }
        }
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
            bytes.extend_from_slice(crate::terminal_paint::BEGIN);
            bytes.extend_from_slice(copy.screen().contents_formatted().as_bytes());
            crate::terminal_paint::restore(copy.screen(), true, &mut bytes);
        }
        bytes.extend_from_slice(b"\x1b[?9l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l");
        bytes.extend_from_slice(screen.input_mode_formatted().as_bytes());
        bytes.extend_from_slice(crate::terminal_paint::END);
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
    if cell.blink() {
        style = style.add_modifier(Modifier::SLOW_BLINK);
    }
    if cell.hidden() {
        style = style.add_modifier(Modifier::HIDDEN);
    }
    if cell.strike() {
        style = style.add_modifier(Modifier::CROSSED_OUT);
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
    fn inherited_cells_and_modes_never_become_known_from_cursor_or_buffer_selection() {
        let mut model = TerminalModel::new(size());
        for trace in [
            b"c".as_slice(),
            b"\x1b[3;4H",
            b"\x1b[6n",
            b"\x1b[?1049h",
            b"\x1b[2J",
        ] {
            model.process(trace);
            assert!(!model.can_paint(), "trace {trace:?}");
            assert_eq!(
                model.unavailable_reason(),
                Some(Unavailable::InheritedState)
            );
        }
        model.process(b"\x1bc");
        assert!(model.can_paint());
        model.resize(size());
        assert_eq!(model.unavailable_reason(), Some(Unavailable::Resize));
        assert!(!model.can_paint());
        model.process(b"\x1b[2J\x1b[H");
        assert!(
            !model.can_paint(),
            "resize needs drawing/saved-state evidence, not just cells"
        );
        model.process(b"\x1bc");
        assert!(model.can_paint());
    }

    const REINITIALIZE: &[u8] = b"\x1b[?6l\x1b[r\x1b[H\x1b7\x1b[2J";
    const NORMAL_ROWS: &[u8] = b"\x1b[1;1H\x1b#5\x1b[2;1H\x1b#5\x1b[3;1H\x1b#5\x1b[4;1H\x1b#5";

    const INHERITED_SHARED: &[&[u8]] = &[
        b"\x18",
        b"\x1b\\",
        b"\x1b<",
        b"\x1b%G",
        b"\x1b[?7h",
        b"\x1b[?5l",
        b"\x1b[?45l",
        b"\x1b[?69l",
        b"\x1b[20l",
        b"\x1b[0\"q",
        b"\x1b(B",
        b"\x1b)B",
        b"\x0f",
        b"\x1b[0m",
        b"\x1b]8;;\x1b\\",
        b"\x1b[4l",
        b"\x1b[?1l",
        b"\x1b>",
        b"\x1b[?25h",
        b"\x1b[?2004l",
        b"\x1b[?2026l",
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

    fn inherited_initialization(alternate: bool) -> Vec<u8> {
        [
            INHERITED_SHARED.concat().as_slice(),
            if alternate {
                b"\x1b[?47h"
            } else {
                b"\x1b[?47l"
            },
            b"\x1b[3g",
            b"\x1b[?6l\x1b[r",
            NORMAL_ROWS,
            REINITIALIZE,
        ]
        .concat()
    }

    #[test]
    fn inherited_initialization_requires_shared_state_selection_tabs_and_geometry_at_every_split() {
        for alternate in [false, true] {
            let trace = inherited_initialization(alternate);
            for split in 0..=trace.len() {
                let mut model = TerminalModel::new(size());
                model.process(&trace[..split]);
                model.process(&trace[split..]);
                assert!(model.can_paint(), "buffer {alternate}, split {split}");
                assert_eq!(model.unavailable_reason(), None);
                assert_eq!(model.screen().alternate_screen(), alternate);
                assert!(!model.evidence.can_paint(!alternate));
                assert!(!model.restore_bytes(alternate).windows(6).any(|s| {
                    s == if alternate {
                        b"\x1b[?47l"
                    } else {
                        b"\x1b[?47h"
                    }
                }));
            }
        }
        for missing in 0..INHERITED_SHARED.len() {
            let mut model = TerminalModel::new(size());
            for (index, component) in INHERITED_SHARED.iter().enumerate() {
                if index != missing {
                    model.process(component);
                }
            }
            model.process(b"\x1b[?47l\x1b[3g");
            model.process(b"\x1b[?6l\x1b[r");
            model.process(NORMAL_ROWS);
            model.process(REINITIALIZE);
            assert!(
                !model.can_paint(),
                "missing {:?}",
                INHERITED_SHARED[missing]
            );
        }
        for missing in [
            b"\x1b[?47l".as_slice(),
            b"\x1b[3g",
            b"\x1b[?6l",
            b"\x1b[r",
            NORMAL_ROWS,
            b"\x1b7",
            b"\x1b[2J",
        ] {
            let mut model = TerminalModel::new(size());
            model.process(&INHERITED_SHARED.concat());
            for component in [
                b"\x1b[?47l".as_slice(),
                b"\x1b[3g",
                b"\x1b[?6l",
                b"\x1b[r",
                NORMAL_ROWS,
                b"\x1b7",
                b"\x1b[2J",
            ] {
                if component != missing {
                    model.process(component);
                }
            }
            assert!(!model.can_paint(), "missing {missing:?}");
        }
    }

    #[test]
    fn inherited_unknown_tabs_writes_and_restores_cannot_be_proven_retroactively() {
        let mut model = TerminalModel::new(size());
        model.process(&INHERITED_SHARED.concat());
        model.process(b"\x1b[?47l");
        model.process(b"\x1b[?6l\x1b[r");
        model.process(NORMAL_ROWS);
        model.process(REINITIALIZE);
        model.process(b"\t\x1b[3g\x1b7\x1b[2J");
        assert!(
            !model.can_paint(),
            "tab at unknown stops loses cursor evidence"
        );
        model.process(b"\x1b[H\x1b7");
        assert!(model.can_paint());

        let mut model = TerminalModel::new(size());
        model.process(b"\x18\x1b\\\x1b[?47l\x1b[3g\x1b[?6l\x1b[r\x1b[H\x1b[2J");
        model.process("界 unknown modes".as_bytes());
        model.process(&INHERITED_SHARED.concat());
        model.process(b"\x1b7\x1b[2J");
        assert!(
            !model.can_paint(),
            "earlier printing cannot establish coordinates"
        );
        model.process(NORMAL_ROWS);
        model.process(b"\x1b[H\x1b7\x1b[2J");
        assert!(model.can_paint());

        let mut model = TerminalModel::new(size());
        model.process(&INHERITED_SHARED.concat());
        model.process(b"\x1b[?47l\x1b[3g\x1b[?6l\x1b[r");
        model.process(NORMAL_ROWS);
        model.process(b"\x1b[H\x1b[2J\x1b8");
        model.process(b"\x1b[?6l\x1b[H\x1b7\x1b[2J");
        assert!(
            !model.can_paint(),
            "DECRC can restore unknown attributes/modes"
        );
        model.process(&inherited_initialization(false));
        assert!(model.can_paint());

        let mut model = TerminalModel::new(size());
        model.process(&INHERITED_SHARED.concat());
        model.process(b"\x1b[3g");
        model.process(b"\x1b[?6l\x1b[r");
        model.process(NORMAL_ROWS);
        model.process(REINITIALIZE);
        model.process(b"\x1b[?47l");
        assert!(
            !model.can_paint(),
            "a first selection cannot prove earlier buffer-local state"
        );
        model.process(b"\x1b[3g");
        model.process(b"\x1b[?6l\x1b[r");
        model.process(REINITIALIZE);
        assert!(
            !model.can_paint(),
            "line resets before selection do not prove the target"
        );
        model.process(NORMAL_ROWS);
        model.process(REINITIALIZE);
        assert!(model.can_paint());
    }

    #[test]
    fn inherited_initialization_does_not_recover_unsupported_output_or_budget_loss() {
        for unsupported in [
            b"\x1b[?7l".as_slice(),
            b"\x1b[?45h",
            b"\x1b[?69h",
            b"\x1b[20h",
            b"\x1b[1\"q",
            b"\x1b[?1016h",
            b"\x1b[0g",
            b"\x1bH",
            b"\x1b[3b",
            b"\x1b[!p",
            b"\x1b%@",
        ] {
            let mut model = TerminalModel::new(size());
            model.process(&inherited_initialization(true));
            let (_, rejected) = model
                .process_guarded_with_events(unsupported, true, |_| Ok::<(), ()>(()), true)
                .unwrap();
            assert_eq!(rejected.as_deref(), Some(unsupported));
            assert!(model.can_paint(), "last safe state retained");
            model.process(unsupported);
            model.resize(size());
            model.process(&inherited_initialization(true));
            assert!(!model.can_paint(), "{unsupported:?}");
            assert_eq!(
                model.unavailable_reason(),
                Some(Unavailable::UnsupportedSequence)
            );
            model.process(b"\x1bc");
            assert!(model.can_paint());
        }
        let mut model = TerminalModel::new(size());
        model.process(&inherited_initialization(false));
        model.discard_oversized_sequence();
        model.process(b"\x18");
        model.process(&inherited_initialization(false));
        assert!(!model.can_paint());
        assert_eq!(
            model.unavailable_reason(),
            Some(Unavailable::SequenceBudget)
        );
    }

    #[test]
    fn inherited_buffer_switch_replays_before_mutation_and_requires_fresh_target_evidence() {
        for frame in [b"\x1b[?00047l".as_slice(), b"\x1b[?1049l", b"\x1b[?1049h"] {
            for split in 0..=frame.len() {
                let mut model = TerminalModel::new(size());
                model.process(&inherited_initialization(true));
                model.process(b"known");
                model.begin_overlay();
                let snapshot = model.drawing_state();
                let (_, first) = model
                    .process_guarded_with_events(&frame[..split], true, |_| Ok::<(), ()>(()), true)
                    .unwrap();
                let rejected = first.or_else(|| {
                    model
                        .process_guarded_with_events(
                            &frame[split..],
                            true,
                            |_| Ok::<(), ()>(()),
                            true,
                        )
                        .unwrap()
                        .1
                });
                assert_eq!(rejected.as_deref(), Some(frame));
                assert_eq!(model.drawing_state(), snapshot);
                assert!(model.can_paint());
                model.end_overlay();
                model.process(frame);
                assert!(!model.can_paint());
                model.process(REINITIALIZE);
                assert!(!model.can_paint(), "target tabs are unknown");
                model.process(b"\x1b[3g");
                model.process(NORMAL_ROWS);
                model.process(REINITIALIZE);
                if frame == b"\x1b[?1049l" {
                    assert!(
                        !model.can_paint(),
                        "1049 also restores an inherited primary slot"
                    );
                    model.process(&inherited_initialization(false));
                }
                assert!(
                    model.can_paint(),
                    "target coordinates/cells now explicitly initialized"
                );
            }
        }
    }

    #[test]
    fn inherited_tab_clear_needs_new_evidence_after_resize_and_matches_future_printing() {
        let mut model = TerminalModel::new(size());
        model.process(&inherited_initialization(false));
        model.process(b"A\tB");
        assert_eq!(model.screen().cell(0, 11).unwrap().contents(), "B");
        model.resize(PtySize { cols: 16, ..size() });
        model.process(REINITIALIZE);
        assert!(
            !model.can_paint(),
            "resize may create stops on the outer terminal"
        );
        model.process(b"\x1b[3gA\tB");
        assert!(model.can_paint());
        assert_eq!(model.screen().cell(0, 15).unwrap().contents(), "B");
    }

    #[test]
    fn inherited_mouse_evidence_requires_complete_groups_and_defers_ambiguous_resets() {
        let mut model = TerminalModel::new(size());
        for component in INHERITED_SHARED {
            if *component != b"\x1b[?1003l" {
                model.process(component);
            }
        }
        model.process(b"\x1b[?1002h\x1b[?1002l\x1b[?1003l\x1b[?47l\x1b[3g");
        model.process(REINITIALIZE);
        assert!(
            !model.can_paint(),
            "an enable makes earlier partial group resets stale"
        );
        model.process(b"\x1b[?9;1000;1001;1002;1003l");
        model.process(NORMAL_ROWS);
        model.process(REINITIALIZE);
        assert!(model.can_paint());
        for (enable, ambiguous) in [
            (b"\x1b[?1002h".as_slice(), b"\x1b[?1003l".as_slice()),
            (b"\x1b[?1002h", b"\x1b[?1001l"),
            (b"\x1b[?1006h", b"\x1b[?1016l"),
        ] {
            let mut model = TerminalModel::new(size());
            model.process(&inherited_initialization(false));
            model.process(enable);
            assert!(model.can_paint(), "represented enabled modes stay usable");
            let (_, rejected) = model
                .process_guarded_with_events(ambiguous, true, |_| Ok::<(), ()>(()), true)
                .unwrap();
            assert_eq!(rejected.as_deref(), Some(ambiguous));
            assert!(model.can_paint());
        }
    }

    #[test]
    fn inherited_row_size_requires_every_row_before_certifying_cells_and_save_slots() {
        let mut model = TerminalModel::new(size());
        model.process(&INHERITED_SHARED.concat());
        model.process(b"\x1b[?47l\x1b[3g\x1b[?6l\x1b[r\x1b[H\x1b#5\x1b#5\x1b#5\x1b#5");
        model.process(b"\x1b7\x1b[2J");
        assert!(
            !model.can_paint(),
            "repeated resets of one row are not full-screen evidence"
        );
        model.process(NORMAL_ROWS);
        assert!(
            !model.can_paint(),
            "normalizing later cannot prove an earlier save or clear"
        );
        model.process(b"\x1b[H\x1b7\x1b[2J");
        assert!(model.can_paint());

        let mut model = TerminalModel::new(size());
        model.process(&inherited_initialization(false)[3..]);
        model.process(b"\x18\x1b\\");
        model.process(REINITIALIZE);
        assert!(
            !model.can_paint(),
            "controls before the parser boundary cannot be certified later"
        );
        model.process(&inherited_initialization(false));
        assert!(model.can_paint());
    }

    #[test]
    fn inherited_physical_modes_and_content_survive_painting_without_observing_a_reset() {
        use std::io::Write;
        use std::process::{Command, Stdio};

        // Only the physical/reference terminal sees this pre-session state.
        // The shadow starts with no knowledge of cells, modes or save slots.
        let inherited = b"WORKSTATION PRIMARY\x1b[1;31m\x1b7\x1b[?47h\x1b[2;3r\x1b[?6h\
            \x1b[?7l\x1b[?45h\x1b[?1h\x1b=\x1b[?25l\x1b[?2004h\x1b[?1003h\x1b[?1016h\
            \x1b[4h\x1b[20h\x1b[3g\x1b[2;3H\x1bH\x1b(0\x1b)0\x0e\x1b[1\"q\
            \x1b]8;id=old;https://example.test/old\x1b\\\x1b]2;inherited unfinished string";
        let mut traces = Vec::new();
        for alternate in [false, true] {
            for return_to_primary in [false, true] {
                if !alternate && return_to_primary {
                    continue;
                }
                let mut model = TerminalModel::new(size());
                let mut physical = Parser::new(nonzero(4), nonzero(12), 0);
                physical.process(inherited);
                let before = inherited_initialization(alternate);
                model.process(&before);
                physical.process(&before);
                assert!(model.can_paint());
                let paint = [
                    crate::terminal_paint::BEGIN,
                    b"\x1b[2J\x1b[HLOCAL MENU",
                    crate::terminal_paint::END,
                ]
                .concat();
                physical.process(&paint);
                let hidden = b"\x1b[2;2H\x1b[32m\x1b7\x1b[3;1HA\tB\x1b[1;1H";
                model.begin_overlay();
                let (_, rejected) = model
                    .process_guarded_with_events(hidden, true, |_| Ok::<(), ()>(()), true)
                    .unwrap();
                assert!(rejected.is_none());
                let restore = model.restore_bytes(alternate);
                physical.process(&restore);
                model.end_overlay();
                let after = if return_to_primary && alternate {
                    b"\x1b8!\x1b[?47l\x1b8P".as_slice()
                } else {
                    b"\x1b8!\tT\r\nNEXT"
                };
                model.process(after);
                physical.process(after);
                if !(return_to_primary && alternate) {
                    assert_eq!(
                        physical.screen().contents_formatted(),
                        model.screen().contents_formatted()
                    );
                    assert_eq!(
                        physical.screen().cursor_position(),
                        model.screen().cursor_position()
                    );
                }
                traces.push(serde_json::json!({
                    "name": format!("unobserved inherited state, alternate {alternate}, primary {return_to_primary}"),
                    "rows": 4, "cols": 12, "inherited": inherited.as_slice(),
                    "before": before, "hidden": hidden.as_slice(), "after": after,
                    "paint": paint, "restore": restore,
                }));
            }
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

    #[test]
    fn resized_buffer_requires_each_initialization_component_at_every_split() {
        let components: [&[u8]; 5] = [b"\x1b[?6l", b"\x1b[r", b"\x1b[H", b"\x1b7", b"\x1b[2J"];
        for missing in 0..components.len() {
            let mut model = TerminalModel::new(size());
            model.process(b"\x1bc\x1b[?1049h");
            model.resize(size());
            for (index, component) in components.iter().enumerate() {
                if index != missing {
                    model.process(component);
                }
            }
            assert!(!model.can_paint(), "missing component {missing}");
            assert_eq!(model.unavailable_reason(), Some(Unavailable::Resize));
        }
        for alternate in [false, true] {
            for split in 0..=REINITIALIZE.len() {
                let mut model = TerminalModel::new(size());
                model.process(b"\x1bc");
                if alternate {
                    model.process(b"\x1b[?1049h");
                }
                model.resize(size());
                model.process(&REINITIALIZE[..split]);
                model.process(&REINITIALIZE[split..]);
                assert!(model.can_paint(), "buffer {alternate}, split {split}");
                assert_eq!(model.unavailable_reason(), None);
                assert!(!model.evidence.can_paint(!alternate));
            }
        }
    }

    #[test]
    fn initialization_does_not_retroactively_prove_unknown_writes_or_saved_positions() {
        for mutation in [b"unknown".as_slice(), b"\n", b"\x1b[K", b"\x1bM"] {
            let mut model = TerminalModel::new(size());
            model.process(b"\x1bc");
            model.resize(size());
            model.process(b"\x1b[2J");
            model.process(mutation);
            model.process(b"\x1b[?6l\x1b[r\x1b[H\x1b7");
            assert!(!model.can_paint(), "mutation {mutation:?}");
            model.process(b"\x1b[2J");
            assert!(model.can_paint());
        }
        let mut model = TerminalModel::new(size());
        model.process(b"\x1bc");
        model.resize(size());
        model.process(b"\x1b7\x1b[?6l\x1b[r\x1b[H\x1b[2J");
        assert!(
            !model.can_paint(),
            "a later CUP cannot prove an earlier save"
        );
        model.process(b"\x1b8\x1b7");
        assert!(
            !model.can_paint(),
            "restoring unknown coordinates stays unknown"
        );
        model.process(b"\x1b[H\x1b[s");
        assert!(
            model.can_paint(),
            "explicit coordinates and a new save recover"
        );
    }

    #[test]
    fn geometry_initialization_and_resize_cannot_recover_unknown_shared_state() {
        for reason in [
            Unavailable::InheritedState,
            Unavailable::UnsupportedSequence,
            Unavailable::SequenceBudget,
        ] {
            let mut model = TerminalModel::new(size());
            match reason {
                Unavailable::InheritedState => {}
                Unavailable::UnsupportedSequence => model.process(b"\x1bc\x1b[7777h"),
                Unavailable::SequenceBudget => {
                    model.process(b"\x1bc");
                    model.discard_oversized_sequence();
                    model.process(b"\x18");
                }
                _ => unreachable!(),
            }
            model.resize(size());
            model.process(REINITIALIZE);
            assert!(!model.can_paint(), "{reason:?}");
            assert_eq!(model.unavailable_reason(), Some(reason));
            model.process(b"\x1bc");
            assert!(model.can_paint());
        }
    }

    #[test]
    fn recovered_buffer_switch_stops_before_mutation_and_replays_at_every_split() {
        for frame in [
            b"\x1b[?47l".as_slice(),
            b"\x1b[?00047l",
            b"\x1b[?25;47l",
            b"\x1b[?1049l",
            b"\x1b[?01049l",
            b"\x1b[?1049h",
        ] {
            for split in 0..=frame.len() {
                let mut model = TerminalModel::new(size());
                model.process(b"\x1bc\x1b[?1049h");
                model.resize(size());
                model.process(REINITIALIZE);
                model.process(b"last safe");
                model.begin_overlay();
                let snapshot = model.drawing_state();
                let (consumed, first) = model
                    .process_guarded_with_events(&frame[..split], true, |_| Ok::<(), ()>(()), true)
                    .unwrap();
                assert_eq!(consumed, split);
                let replay = first.or_else(|| {
                    model
                        .process_guarded_with_events(
                            &frame[split..],
                            true,
                            |_| Ok::<(), ()>(()),
                            true,
                        )
                        .unwrap()
                        .1
                });
                assert_eq!(replay.as_deref(), Some(frame), "split {split}");
                assert_eq!(model.drawing_state(), snapshot);
                assert!(model.can_paint());
                assert_eq!(model.screen().contents(), "last safe");
                let restoration = model.restore_bytes(true);
                assert!(
                    !restoration.windows(6).any(|s| s == b"\x1b[?47l"),
                    "restoration must leave the unknown primary untouched"
                );
                model.end_overlay();
                model.process(replay.as_ref().unwrap());
                assert!(!model.can_paint());
                assert_eq!(model.unavailable_reason(), Some(Unavailable::Resize));
                model.process(REINITIALIZE);
                assert!(model.can_paint(), "target explicitly reinitialized");
            }
        }
    }

    #[test]
    fn resized_reinitialization_restores_active_state_and_leaves_inactive_content_untouched() {
        use std::io::Write;
        use std::process::{Command, Stdio};

        let before_resize =
            b"\x1bcPRIMARY\x1b7\x1b[?1049h\x1b[2;3r\x1b[?6h\x1b[1;2H\x1b[31mOLD\x1b7";
        let reinitialized = [REINITIALIZE, "界e\u{301} resized".as_bytes()].concat();
        let hidden = b"\x1b[4;2H\x1b[32mhidden\x1b7\x1b[2;2H\x1b[34m";
        let mut traces = Vec::new();
        for after in [b"\x1b8!".as_slice(), b"\x1b8!\x1b[?47l\x1b8P"] {
            let mut model = TerminalModel::new(size());
            let mut physical = Parser::new(nonzero(4), nonzero(12), 0);
            model.process(before_resize);
            physical.process(before_resize);
            model.resize(PtySize {
                rows: 6,
                cols: 16,
                ..size()
            });
            physical.set_size(nonzero(6), nonzero(16));
            model.process(&reinitialized);
            physical.process(&reinitialized);
            assert!(model.can_paint());
            assert!(!model.evidence.can_paint(false));
            model.begin_overlay();
            let paint = [
                crate::terminal_paint::BEGIN,
                b"\x1b[2J\x1b[HLOCAL MENU",
                crate::terminal_paint::END,
            ]
            .concat();
            physical.process(&paint);
            let (_, rejected) = model
                .process_guarded_with_events(hidden, true, |_| Ok::<(), ()>(()), true)
                .unwrap();
            assert!(rejected.is_none());
            let restore = model.restore_bytes(true);
            physical.process(&restore);
            model.end_overlay();
            model.process(after);
            physical.process(after);
            assert_eq!(
                physical.screen().contents_formatted(),
                model.screen().contents_formatted()
            );
            assert_eq!(physical.screen().drawing_state(), model.drawing_state());
            traces.push(serde_json::json!({
                "name": format!("reinitialized resize, after {after:?}"),
                "initialRows": 4, "initialCols": 12, "rows": 6, "cols": 16,
                "beforeResize": before_resize.as_slice(),
                "before": reinitialized, "hidden": hidden.as_slice(), "after": after,
                "paint": paint, "restore": restore,
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

    #[test]
    fn covered_unsupported_frames_stop_before_mutation_and_replay_exactly_at_every_split() {
        for unsafe_frame in [
            b"\x1b_Ga=q;opaque qy\x1b\\".as_slice(),
            b"\x1b[7777h",
            b"\x1b]50;unmodeled-font\x1b\\",
            b"\x1b[3J",
            b"\x1b[?999n",
            b"\x1b[?1;2$p",
            b"\x05",
        ] {
            let trace = [b"\x1b[31msafe".as_slice(), unsafe_frame, b"after"].concat();
            for split in 0..=trace.len() {
                let mut model = TerminalModel::new(size());
                model.process(b"\x1bc\x1b[?1049h");
                model.begin_overlay();
                let chunks = [&trace[..split], &trace[split..]];
                let mut replay = None;
                for (index, chunk) in chunks.iter().enumerate() {
                    let (consumed, rejected) = model
                        .process_guarded_with_events(chunk, true, |_| Ok::<(), ()>(()), true)
                        .unwrap();
                    if let Some(mut rejected) = rejected {
                        rejected.extend_from_slice(&chunk[consumed..]);
                        for tail in &chunks[index + 1..] {
                            rejected.extend_from_slice(tail);
                        }
                        replay = Some(rejected);
                        break;
                    }
                }
                assert!(model.can_paint(), "last good snapshot remains restorable");
                assert_eq!(model.screen().contents(), "safe");
                assert_eq!(
                    replay.as_deref(),
                    Some([unsafe_frame, b"after"].concat().as_slice()),
                    "split {split}"
                );
                model.end_overlay();
                model.process(replay.as_ref().unwrap());
                assert_eq!(
                    model.unavailable_reason(),
                    Some(Unavailable::UnsupportedSequence)
                );
                model.process(b"\x1bc");
                assert!(model.can_paint());
            }
        }
    }

    #[test]
    fn restoration_includes_inactive_buffers_touched_during_a_round_trip() {
        let mut model = TerminalModel::new(size());
        let before = b"\x1bc\x1b[?1049hentry";
        model.process(before);
        model.begin_overlay();
        model.process(b"\x1b[?47lprimary\x1b7\x1b[?47halternate");
        let mut physical = Parser::new(nonzero(4), nonzero(12), 0);
        physical.process(before);
        physical.process(b"\x1b[HLOCAL MENU");
        physical.process(&model.restore_bytes(true));
        physical.process(b"\x1b[?47l\x1b8!");
        model.process(b"\x1b[?47l\x1b8!");
        assert_eq!(physical.screen().contents(), model.screen().contents());
        assert_eq!(physical.screen().drawing_state(), model.drawing_state());
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
    fn common_drawing_state_is_eligible_and_split_invariant_but_malformed_sgr_is_not() {
        let trace = b"\x1bc\x1b(0lqqk\x1b(B\x1b)0\x0ex\x0f\x1b[4h\x1b[1;2;5;8;9m\x1b]8;id=one;https://example.test\x1b\\LINK\x1b[0mNEXT\x1b]8;;\x1b\\";
        let mut expected = TerminalModel::new(size());
        expected.process(trace);
        assert!(expected.can_paint());
        assert!(expected.drawing_state().insert_mode);
        assert_eq!(expected.screen().cell(0, 0).unwrap().contents(), "┌");
        assert!(expected.screen().cell(0, 5).unwrap().bold());
        assert!(expected.screen().cell(0, 5).unwrap().dim());
        assert!(expected.screen().cell(0, 5).unwrap().blink());
        assert!(expected.screen().cell(0, 5).unwrap().hidden());
        assert!(expected.screen().cell(0, 5).unwrap().strike());
        assert!(
            expected.screen().cell(0, 9).unwrap().hyperlink().is_some(),
            "SGR 0 keeps the link"
        );
        for split in 0..=trace.len() {
            let mut model = TerminalModel::new(size());
            model.process(&trace[..split]);
            model.process(&trace[split..]);
            assert!(model.can_paint(), "split {split}");
            assert_eq!(
                model.screen().contents_formatted(),
                expected.screen().contents_formatted()
            );
            assert_eq!(model.drawing_state(), expected.drawing_state());
        }
        for unsupported in [
            b"\x1b[38;2;1m".as_slice(),
            b"\x1b[38;5;256m",
            b"\x1b[4:2m",
            b"\x1b[7 q",
            b"\x1b(A",
            b"\x1b]8;id=x;\x1b\\",
            b"\x1b)0\x0e\x1b7\x1b)B\x0f\x1b8",
        ] {
            let mut model = TerminalModel::new(size());
            model.process(b"\x1bc");
            model.process(unsupported);
            assert!(!model.can_paint(), "{unsupported:?}");
        }
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
    fn mode_queries_use_remote_mouse_keypad_and_buffer_state_or_report_unknown() {
        let trace = b"\x1b[?1049h\x1b=\x1b[?1002h\x1b[?1006h\x1b[?1002$p\x1b[?1000$p\x1b[?1006$p\x1b[?1005$p\x1b[?66$p\x1b[?47$p\x1b[?1049$p\x1b[?9999$p\x1b[4$p";
        for split in 0..=trace.len() {
            let mut model = TerminalModel::new(size());
            model.process(&trace[..split]);
            model.process(&trace[split..]);
            assert_eq!(
                model.take_events(),
                vec![
                    TerminalEvent::Reply(b"\x1b[?1002;1$y".to_vec()),
                    TerminalEvent::Reply(b"\x1b[?1000;2$y".to_vec()),
                    TerminalEvent::Reply(b"\x1b[?1006;1$y".to_vec()),
                    TerminalEvent::Reply(b"\x1b[?1005;2$y".to_vec()),
                    TerminalEvent::Reply(b"\x1b[?66;1$y".to_vec()),
                    TerminalEvent::Reply(b"\x1b[?47;1$y".to_vec()),
                    TerminalEvent::Reply(b"\x1b[?1049;1$y".to_vec()),
                    TerminalEvent::Reply(b"\x1b[?9999;0$y".to_vec()),
                    TerminalEvent::Reply(b"\x1b[4;2$y".to_vec()),
                ],
                "split {split}"
            );
        }
    }

    #[test]
    fn keyboard_and_version_queries_are_physical_but_window_mutations_are_not_queries() {
        let mut model = TerminalModel::new(size());
        model.process(b"\x1b[?4m\x1b[?u\x1b[>0q\x1b[18t\x1b[22t\x1b[8;8;30t");
        assert_eq!(
            model.take_events(),
            vec![
                TerminalEvent::Query(b"\x1b[?4m".to_vec()),
                TerminalEvent::Query(b"\x1b[?0u".to_vec()),
                TerminalEvent::Query(b"\x1b[>0q".to_vec()),
                TerminalEvent::Query(b"\x1b[18t".to_vec()),
                TerminalEvent::UnhandledCsi('t'),
                TerminalEvent::ResizeRequest { rows: 8, cols: 30 },
            ]
        );
        model.process(b"\x1bP+Q666f6e74\x1b\\");
        assert_eq!(
            model.take_events(),
            vec![TerminalEvent::Reply(b"\x1bP0+R\x1b\\".to_vec())]
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

    // Executable default-readiness gates. Resolved gates run in the ordinary
    // suite; the remaining DCS gap stays explicitly ignored (see the audit).
    #[test]
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
    fn menu_restoration_survives_saved_attributes_wrap_and_buffer_switches() {
        use std::io::Write as _;
        use std::process::{Command, Stdio};

        let cases = [
            (
                "saved DEC attributes and origin",
                "\x1b[2;4r\x1b[?6h\x1b[2;3H\x1b[31mold\x1b7",
                "\x1b[3;7H\x1b[32m\x1b7\x1b[?6l\x1b[H\x1b[34m",
                "\x1b8X\r\nnext",
            ),
            (
                "ANSI save DEC restore",
                "\x1b[2;3H\x1b[31m\x1b[s",
                "\x1b[3;7H\x1b[32m\x1b[s\x1b[H",
                "\x1b8X",
            ),
            (
                "DEC save ANSI restore",
                "\x1b[2;3H\x1b7",
                "\x1b[3;7H\x1b[32m\x1b7\x1b[H",
                "\x1b[uX",
            ),
            (
                "per-buffer saved attributes",
                "\x1b[2;3H\x1b[31m\x1b7primary",
                "\x1b[3;7H\x1b[32m\x1b7\x1b[?1049halternate\x1b[2;5H\x1b[33m\x1b7\x1b[H",
                "\x1b8A\x1b[?47l\x1b8P",
            ),
            (
                "saved narrow wrap",
                "",
                "\x1b[31mabcdefghijkl\x1b7\x1b[H\x1b[32m",
                "\x1b8Z",
            ),
            (
                "saved wide wrap",
                "",
                "\x1b[31mabcdefghij界\x1b7\x1b[H\x1b[32m",
                "\x1b8Z",
            ),
            (
                "current wrap at scroll bottom",
                "\x1b[2;4r\x1b[?6h",
                "\x1b[3;1H\x1b[31mabcdefghijkl",
                "Z\r\nnext",
            ),
            (
                "saved origin with new margins",
                "\x1b[3;4r\x1b[?6h\x1b[2;4H\x1b7",
                "\x1b[1;2r\x1b[?6l\x1b[H",
                "\x1b8X",
            ),
            (
                "hidden margins change",
                "\x1b[2;4r\x1b[?6h\x1b[2;3Hbefore",
                "\x1b[1;3r\x1b[?6h\x1b[3;1Hafter",
                "\r\nnext",
            ),
            (
                "insert mode and independent SGR",
                "abcdefghijkl\x1b[H\x1b[4h\x1b[1;2;5;8;9m",
                "\x1b[2;2Hhidden\x1b[H",
                "XY\x1b[22;25;28;29mZ",
            ),
            (
                "DEC G0 and G1 with saved graphics",
                "\x1b(0lqqk\x1b7\x1b(B\x1b)0\x0e",
                "\x1b[2;3Hxqx\x1b[H",
                "\x0fq\x1b8x\x1b(B\x1b)B\x0fA",
            ),
            (
                "OSC 8 cells and current link",
                "\x1b]8;id=one;https://example.test/one\x1b\\link\x1b]8;;\x1b\\\x1b7",
                "\x1b[2;2H\x1b]8;id=two;https://example.test/two\x1b\\new\x1b[0m",
                "!\x1b]8;;\x1b\\\x1b8X",
            ),
        ];
        let mut traces = Vec::new();
        for (name, before, hidden, after) in cases {
            let mut model = TerminalModel::new(size());
            let mut physical = Parser::new(nonzero(4), nonzero(12), 0);
            model.process(before.as_bytes());
            physical.process(before.as_bytes());
            let covered = model.screen().alternate_screen();
            let mut paint = crate::terminal_paint::BEGIN.to_vec();
            paint.extend_from_slice(b"\x1b[2J\x1b[HLOCAL MENU");
            paint.extend_from_slice(crate::terminal_paint::END);
            physical.process(&paint);
            model.process(hidden.as_bytes());
            let restore = model.restore_bytes(covered);
            physical.process(&restore);
            model.process(after.as_bytes());
            physical.process(after.as_bytes());
            assert_eq!(
                visible(physical.screen()),
                visible(model.screen()),
                "{name}"
            );
            assert_eq!(
                physical.screen().cursor_position(),
                model.screen().cursor_position(),
                "{name}"
            );
            assert_eq!(
                physical.screen().drawing_state(),
                model.drawing_state(),
                "{name}"
            );
            for row in 0..4 {
                for col in 0..12 {
                    let actual = physical.screen().cell(row, col).unwrap();
                    let expected = model.screen().cell(row, col).unwrap();
                    if expected.has_contents() {
                        assert_eq!(actual, expected, "{name}: ({row}, {col})");
                    }
                }
            }
            traces.push(serde_json::json!({
                "name": name, "rows": 4, "cols": 12,
                "before": before.as_bytes(), "hidden": hidden.as_bytes(), "after": after.as_bytes(),
                "paint": paint, "restore": restore,
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
                .expect("starting independent xterm.js reference");
            reference
                .stdin
                .take()
                .unwrap()
                .write_all(&serde_json::to_vec(&traces).unwrap())
                .unwrap();
            assert!(
                reference.wait().unwrap().success(),
                "independent reference disagreed"
            );
        }
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
        assert_eq!(before.primary.saved_attributes, "\x1b[m\x1b[31m");
        model.process(b"\x1b[?47h\x1b[1;3r\x1b[3;7H\x1b[32m\x1b7\x1b[H");
        let hidden = model.drawing_state();
        assert!(hidden.alternate_screen);
        assert_eq!(hidden.primary, before.primary);
        assert_eq!(hidden.alternate.saved_cursor.row, 2);
        assert_eq!(hidden.alternate.saved_cursor.col, 6);
        assert_eq!(hidden.alternate.scroll_region, (0, 2));
        assert_eq!(hidden.alternate.saved_attributes, "\x1b[m\x1b[32m");
    }

    #[test]
    fn origin_relative_cursor_reports_use_the_remote_scroll_region() {
        let mut model = TerminalModel::new(size());
        model.process(b"\x1b[2;4r\x1b[?6h\x1b[2;3H\x1b[6n\x1b[?6n\x1b[?6$p\x1b[?6l\x1b[?6$p");
        assert_eq!(
            model.take_events(),
            vec![
                TerminalEvent::Reply(b"\x1b[2;3R".to_vec()),
                TerminalEvent::Reply(b"\x1b[?2;3;1R".to_vec()),
                TerminalEvent::Reply(b"\x1b[?6;1$y".to_vec()),
                TerminalEvent::Reply(b"\x1b[?6;2$y".to_vec()),
            ],
        );
    }

    #[test]
    fn dcs_queries_preserve_split_headers_and_bodies() {
        let query = b"\x1b[31m\x1bP$qm\x1b\\\x1b[32m";
        for split in 0..=query.len() {
            let mut model = TerminalModel::new(size());
            model.process(&query[..split]);
            model.process(&query[split..]);
            assert_eq!(
                model.take_events(),
                vec![TerminalEvent::Reply(b"\x1bP1$r0;31m\x1b\\".to_vec())],
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
    fn overlay_acceptance_dcs_query() {
        let mut model = TerminalModel::new(size());
        model.process(b"\x1b[1;38;2;10;20;30m\x1b[2;4r\x1bP$qm\x1b\\\x1bP$qr\x1b\\\x1b[0m\x1b[r");
        assert_eq!(
            model.take_events(),
            vec![
                TerminalEvent::Reply(b"\x1bP1$r0;38;2;10;20;30;1m\x1b\\".to_vec()),
                TerminalEvent::Reply(b"\x1bP1$r2;4r\x1b\\".to_vec()),
            ]
        );
    }

    #[test]
    fn dcs_cancellation_and_overflow_never_answer_a_partial_request() {
        for cancel in [b"\x18".as_slice(), b"\x1a", b"\x1b[31m", b"\x1bc"] {
            let mut trace = b"\x1bP$qm".to_vec();
            trace.extend_from_slice(cancel);
            trace.extend_from_slice(b"after\x1bP$qr\x1b\\");
            for split in 0..=trace.len() {
                let mut model = TerminalModel::new(size());
                model.process(&trace[..split]);
                model.process(&trace[split..]);
                let replies: Vec<_> = model
                    .take_events()
                    .into_iter()
                    .filter(|event| {
                        matches!(
                            event,
                            TerminalEvent::Reply(_) | TerminalEvent::UnhandledDcs(_)
                        )
                    })
                    .collect();
                assert_eq!(
                    replies,
                    vec![TerminalEvent::Reply(b"\x1bP1$r1;4r\x1b\\".to_vec())],
                    "cancel {cancel:?}, split {split}"
                );
            }
        }
        let mut model = TerminalModel::new(size());
        let mut trace = b"\x1bP$q".to_vec();
        trace.extend(std::iter::repeat_n(b'm', CONTROL_SEQUENCE_LIMIT));
        trace.extend_from_slice(b"\x1b\\");
        model.process(&trace);
        assert!(model.take_events().is_empty());
        assert!(!model.can_paint());
    }

    #[test]
    fn unsupported_setting_and_capability_queries_receive_negative_replies() {
        let mut model = TerminalModel::new(size());
        model.process(b"\x1bP$q q\x1b\\\x1bP$qunknown\x1b\\\x1bP+q436f\x1b\\");
        assert_eq!(
            model.take_events(),
            vec![
                TerminalEvent::Reply(b"\x1bP0$r\x1b\\".to_vec()),
                TerminalEvent::Reply(b"\x1bP0$r\x1b\\".to_vec()),
                TerminalEvent::Reply(b"\x1bP0+r\x1b\\".to_vec()),
            ]
        );
        model.process(b"\x1b[21m\x1bP$qm\x1b\\");
        assert!(model
            .take_events()
            .contains(&TerminalEvent::Reply(b"\x1bP0$r\x1b\\".to_vec())));
        model.process(b"\x1bc\x1bP$qm\x1b\\");
        assert_eq!(
            model.take_events(),
            vec![TerminalEvent::Reply(b"\x1bP1$r0m\x1b\\".to_vec())]
        );
    }
}
