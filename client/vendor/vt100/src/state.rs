//! Read-only drawing-state snapshots for applications sharing a terminal.

use alloc::string::String;

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
}
