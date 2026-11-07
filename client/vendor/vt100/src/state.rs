//! Read-only drawing-state snapshots for applications sharing a terminal.

use alloc::string::String;

/// The supported ISO 2022 character designations and active GL set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CharacterSets {
    /// Whether G0 uses DEC special graphics rather than ASCII.
    pub g0_dec: bool,
    /// Whether G1 uses DEC special graphics rather than ASCII.
    pub g1_dec: bool,
    /// Whether SO has selected G1 rather than SI selecting G0.
    pub shift_out: bool,
}

impl CharacterSets {
    pub(crate) fn map(self, c: char) -> char {
        if !(if self.shift_out { self.g1_dec } else { self.g0_dec }) {
            return c;
        }
        match c {
            '`' => '◆', 'a' => '▒', 'b' => '␉', 'c' => '␌',
            'd' => '␍', 'e' => '␊', 'f' => '°', 'g' => '±',
            'h' => '␤', 'i' => '␋', 'j' => '┘', 'k' => '┐',
            'l' => '┌', 'm' => '└', 'n' => '┼', 'o' => '⎺',
            'p' => '⎻', 'q' => '─', 'r' => '⎼', 's' => '⎽',
            't' => '├', 'u' => '┤', 'v' => '┴', 'w' => '┬',
            'x' => '│', 'y' => '≤', 'z' => '≥', '{' => 'π',
            '|' => '≠', '}' => '£', '~' => '·', _ => c,
        }
    }
}

/// A cursor position and the modes saved with it. Coordinates are zero-based.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CursorState {
    /// Absolute row in the screen buffer.
    pub row: u16,
    /// Column; equal to the screen width when a wrap is pending.
    pub col: u16,
    /// Whether cursor addressing is relative to the scroll region.
    pub origin_mode: bool,
}

/// Drawing state owned by one screen buffer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BufferState {
    /// Current cursor.
    pub cursor: CursorState,
    /// DECSC saved cursor.
    pub saved_cursor: CursorState,
    /// Inclusive zero-based top and bottom scroll margins.
    pub scroll_region: (u16, u16),
    /// SGR commands that establish this buffer's DECSC saved attributes.
    pub saved_attributes: String,
    /// Character designations/GL selection saved by DECSC.
    pub saved_character_sets: CharacterSets,
}

/// The represented drawing state of both buffers and the shared attributes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DrawingState {
    /// Primary buffer.
    pub primary: BufferState,
    /// Alternate buffer.
    pub alternate: BufferState,
    /// Which buffer is currently active.
    pub alternate_screen: bool,
    /// SGR commands that establish the current attributes from any attributes.
    pub attributes: String,
    /// Current character designations/GL selection.
    pub character_sets: CharacterSets,
    /// ANSI insert mode (IRM).
    pub insert_mode: bool,
    /// Current complete OSC 8 opener; SGR and cursor saves do not change it.
    pub hyperlink: Option<String>,
}
