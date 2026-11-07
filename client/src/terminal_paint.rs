//! Shared drawing environment and state restoration for menus and toasts.
//!
//! Local absolute positioning uses a full scroll region with origin mode off.
//! A toast restores current state without borrowing the application's save
//! slot. Closing a menu also serializes save slots changed by hidden output.

use std::io::Write;

use vt100::{BufferState, CursorState, Screen};

/// Cancel only the physical parser, hide local drawing, and use absolute rows.
pub const BEGIN: &[u8] =
    b"\x18\x1b\\\x1b[?2026h\x1b[?25l\x1b[?6l\x1b[r\x1b[4l\x1b(B\x1b)B\x0f\x1b]8;;\x1b\\\x1b[0m";
pub const END: &[u8] = b"\x1b[?2026l";

/// Restore the represented active-buffer state after local drawing.
pub fn restore(screen: &Screen, restore_saved: bool, out: &mut Vec<u8>) {
    let snapshot = screen.drawing_state();
    let buffer = if snapshot.alternate_screen {
        &snapshot.alternate
    } else {
        &snapshot.primary
    };
    if restore_saved {
        // Saving under full margins can represent a saved origin-relative
        // cursor whose old row lies outside the current scroll region.
        out.extend_from_slice(b"\x1b[?6l\x1b[r");
        cursor(
            screen,
            buffer.saved_cursor,
            (0, screen.size().0.get() - 1),
            out,
        );
        out.extend_from_slice(buffer.saved_attributes.as_bytes());
        character_sets(buffer.saved_character_sets, out);
        out.extend_from_slice(b"\x1b7");
        // Restoration text is Unicode and must not inherit DEC mapping.
        character_sets(vt100::CharacterSets::default(), out);
    }
    margins(buffer, out);
    cursor(screen, buffer.cursor, buffer.scroll_region, out);
    out.extend_from_slice(snapshot.attributes.as_bytes());
    out.extend_from_slice(
        snapshot
            .hyperlink
            .as_deref()
            .unwrap_or("\x1b]8;;\x1b\\")
            .as_bytes(),
    );
    character_sets(snapshot.character_sets, out);
    out.extend_from_slice(if snapshot.insert_mode {
        b"\x1b[4h"
    } else {
        b"\x1b[4l"
    });
    out.extend_from_slice(if screen.hide_cursor() {
        b"\x1b[?25l"
    } else {
        b"\x1b[?25h"
    });
}

pub(crate) fn character_sets(sets: vt100::CharacterSets, out: &mut Vec<u8>) {
    out.extend_from_slice(if sets.g0_dec { b"\x1b(0" } else { b"\x1b(B" });
    out.extend_from_slice(if sets.g1_dec { b"\x1b)0" } else { b"\x1b)B" });
    out.push(if sets.shift_out { 0x0e } else { 0x0f });
}

fn margins(buffer: &BufferState, out: &mut Vec<u8>) {
    write!(
        out,
        "\x1b[{};{}r",
        buffer.scroll_region.0 + 1,
        buffer.scroll_region.1 + 1,
    )
    .expect("writing to a Vec");
}

pub(crate) fn cursor(screen: &Screen, cursor: CursorState, margins: (u16, u16), out: &mut Vec<u8>) {
    out.extend_from_slice(if cursor.origin_mode {
        b"\x1b[?6h"
    } else {
        b"\x1b[?6l"
    });
    let row = if cursor.origin_mode {
        cursor.row.saturating_sub(margins.0)
    } else {
        cursor.row
    };
    let cols = screen.size().1.get();
    let mut col = cursor.col.min(cols - 1);
    let wrapped = cursor.col >= cols;
    if wrapped
        && screen
            .cell(cursor.row, col)
            .is_some_and(|cell| cell.is_wide_continuation())
    {
        col = col.saturating_sub(1);
    }
    write!(out, "\x1b[{};{}H", row + 1, col + 1).expect("writing to a Vec");
    if wrapped {
        // CUP alone cannot recreate delayed wrap. Reprint the last glyph
        // with its existing attributes; SGR afterwards preserves that flag.
        if let Some(cell) = screen.cell(cursor.row, col) {
            out.extend_from_slice(cell.attributes_formatted().as_bytes());
            out.extend_from_slice(if cell.has_contents() {
                cell.contents().as_bytes()
            } else {
                b" "
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU16;

    #[test]
    fn takeover_replays_deferred_output_from_entry_drawing_state() {
        let parser =
            || vt100::Parser::new(NonZeroU16::new(6).unwrap(), NonZeroU16::new(12).unwrap(), 0);
        let before = b"\x1b[2;5r\x1b[?6h\x1b[2;3H\x1b[31m\x1b7prompt";
        let deferred = b"\x1b[1;3r\x1b[?6l\x1b[32m\x1b[Hremote";
        let after = b"\x1b8X\r\nnext";
        let mut expected = parser();
        expected.process(before);
        let entry = expected.screen().clone();
        expected.process(deferred);
        expected.process(after);

        let mut actual = parser();
        actual.process(before);
        actual.process(BEGIN);
        actual.process(b"\x1b[2J\x1b[HLOCAL MENU");
        let mut restore = Vec::new();
        super::restore(&entry, false, &mut restore);
        restore.extend_from_slice(END);
        actual.process(&restore);
        actual.process(deferred);
        actual.process(after);
        assert_eq!(
            actual.screen().drawing_state(),
            expected.screen().drawing_state()
        );
        let (row, col) = expected.screen().cursor_position();
        assert_eq!(
            actual.screen().cell(row, col - 1),
            expected.screen().cell(row, col - 1)
        );
    }
}
