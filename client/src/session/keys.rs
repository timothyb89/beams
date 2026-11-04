//! Decodes terminal input into logical keys while the local menu owns the
//! terminal.
//!
//! In passthrough mode the session never interprets keys beyond spotting the
//! menu prefix (see `InputRouter`); every byte is forwarded exactly as
//! received. Once the menu is open, though, the local UI needs real keys:
//! arrows, Home/End, Shift-Tab, UTF-8 characters, pasted text. This module
//! maps one already-split input unit (a byte, a UTF-8 character, or a
//! complete `ESC [`/`ESC O` sequence, as produced by `parse_terminal_key`)
//! to a `Key`.
//!
//! Remote applications such as zellij may have switched the outer terminal
//! into application-cursor mode (`ESC O A` arrows), the Kitty keyboard
//! protocol (`ESC [ 9 ; 2 u` for Shift-Tab), or xterm modifyOtherKeys
//! (`ESC [ 27 ; 5 ; 97 ~` for Ctrl-A). All of those are accepted here.

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Key {
    Char(char),
    /// A control chord with a letter, normalized to lowercase (`Ctrl('a')`).
    Ctrl(char),
    Enter,
    Esc,
    Tab,
    BackTab,
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    /// A complete bracketed paste, assembled by the input router.
    Paste(String),
}

/// One decoded unit of menu input.
#[derive(Debug, PartialEq, Eq)]
pub enum Decoded {
    Key(Key),
    PasteStart,
    PasteEnd,
}

const SHIFT: u32 = 1;
const ALT: u32 = 2;
const CONTROL: u32 = 4;
const LOCKS: u32 = 64 | 128;

/// Decodes one input unit. Returns `None` for input the menu has no use for
/// (Alt chords, function keys, mouse reports, focus events, ...).
pub fn decode(encoded: &[u8]) -> Option<Decoded> {
    match encoded {
        [] => None,
        [byte] => decode_byte(*byte).map(Decoded::Key),
        [b'\x1b', b'[', rest @ ..] => decode_csi(rest),
        [b'\x1b', b'O', final_byte] => decode_cursor(*final_byte, 1).map(Decoded::Key),
        _ => {
            let text = std::str::from_utf8(encoded).ok()?;
            let mut chars = text.chars();
            match (chars.next(), chars.next()) {
                (Some(ch), None) if !ch.is_control() => Some(Decoded::Key(Key::Char(ch))),
                _ => None,
            }
        }
    }
}

fn decode_byte(byte: u8) -> Option<Key> {
    Some(match byte {
        b'\r' | b'\n' => Key::Enter,
        b'\t' => Key::Tab,
        b'\x1b' => Key::Esc,
        b'\x7f' | b'\x08' => Key::Backspace,
        0x01..=0x1a => Key::Ctrl(char::from(b'a' + byte - 1)),
        0x20..=0x7e => Key::Char(char::from(byte)),
        _ => return None,
    })
}

/// `rest` is everything after `ESC [`, ending in the final byte.
fn decode_csi(rest: &[u8]) -> Option<Decoded> {
    let (&final_byte, parameters) = rest.split_last()?;
    if parameters
        .first()
        .is_some_and(|byte| matches!(byte, b'<' | b'?' | b'>' | b'='))
    {
        // Mouse reports and private replies.
        return None;
    }
    let fields: Vec<Option<u32>> = if parameters.is_empty() {
        Vec::new()
    } else {
        parameters
            .split(|byte| *byte == b';')
            .map(|field| {
                let primary = field.split(|byte| *byte == b':').next()?;
                std::str::from_utf8(primary).ok()?.parse().ok()
            })
            .collect()
    };
    let field = |index: usize| fields.get(index).copied().flatten();
    let modifiers = field(1).unwrap_or(1).saturating_sub(1) & !LOCKS;

    match final_byte {
        b'A' | b'B' | b'C' | b'D' | b'H' | b'F' => {
            decode_cursor(final_byte, modifiers).map(Decoded::Key)
        }
        b'Z' => Some(Decoded::Key(Key::BackTab)),
        b'u' => decode_codepoint(field(0)?, modifiers).map(Decoded::Key),
        b'~' => match field(0)? {
            200 => Some(Decoded::PasteStart),
            201 => Some(Decoded::PasteEnd),
            27 => {
                let modifiers = field(1)?.saturating_sub(1) & !LOCKS;
                decode_codepoint(field(2)?, modifiers).map(Decoded::Key)
            }
            _ if modifiers & (CONTROL | ALT) != 0 => None,
            1 | 7 => Some(Decoded::Key(Key::Home)),
            4 | 8 => Some(Decoded::Key(Key::End)),
            3 => Some(Decoded::Key(Key::Delete)),
            5 => Some(Decoded::Key(Key::PageUp)),
            6 => Some(Decoded::Key(Key::PageDown)),
            _ => None,
        },
        _ => None,
    }
}

fn decode_cursor(final_byte: u8, modifiers: u32) -> Option<Key> {
    if modifiers & (CONTROL | ALT) != 0 {
        return None;
    }
    Some(match final_byte {
        b'A' => Key::Up,
        b'B' => Key::Down,
        b'C' => Key::Right,
        b'D' => Key::Left,
        b'H' => Key::Home,
        b'F' => Key::End,
        _ => return None,
    })
}

/// A Kitty CSI-u / modifyOtherKeys codepoint plus modifier flags.
fn decode_codepoint(codepoint: u32, modifiers: u32) -> Option<Key> {
    if modifiers & ALT != 0 {
        return None;
    }
    let control = modifiers & CONTROL != 0;
    let shift = modifiers & SHIFT != 0;
    let key = match codepoint {
        13 => Key::Enter,
        9 if shift => Key::BackTab,
        9 => Key::Tab,
        27 => Key::Esc,
        127 | 8 => Key::Backspace,
        _ => {
            let ch = char::from_u32(codepoint)?;
            if ch.is_control() {
                return None;
            }
            if control {
                if ch.is_ascii_alphabetic() {
                    return Some(Key::Ctrl(ch.to_ascii_lowercase()));
                }
                return None;
            }
            // Kitty reports the unshifted key plus a shift flag unless the
            // terminal also sends associated text; uppercase ASCII is the
            // only case the menu cares about.
            if shift && ch.is_ascii_lowercase() {
                Key::Char(ch.to_ascii_uppercase())
            } else {
                Key::Char(ch)
            }
        }
    };
    if control && !matches!(key, Key::Ctrl(_)) {
        return None;
    }
    Some(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(encoded: &[u8]) -> Option<Key> {
        match decode(encoded) {
            Some(Decoded::Key(key)) => Some(key),
            _ => None,
        }
    }

    #[test]
    fn decodes_plain_bytes_and_controls() {
        assert_eq!(key(b"a"), Some(Key::Char('a')));
        assert_eq!(key(b"\r"), Some(Key::Enter));
        assert_eq!(key(b"\t"), Some(Key::Tab));
        assert_eq!(key(b"\x7f"), Some(Key::Backspace));
        assert_eq!(key(b"\x1b"), Some(Key::Esc));
        assert_eq!(key(b"\x01"), Some(Key::Ctrl('a')));
        assert_eq!(key(b"\x15"), Some(Key::Ctrl('u')));
        assert_eq!(key(b"\xff"), None);
    }

    #[test]
    fn decodes_utf8_characters() {
        assert_eq!(key("é".as_bytes()), Some(Key::Char('é')));
        assert_eq!(key("界".as_bytes()), Some(Key::Char('界')));
    }

    #[test]
    fn decodes_normal_and_application_cursor_keys() {
        assert_eq!(key(b"\x1b[A"), Some(Key::Up));
        assert_eq!(key(b"\x1bOB"), Some(Key::Down));
        assert_eq!(key(b"\x1b[C"), Some(Key::Right));
        assert_eq!(key(b"\x1bOD"), Some(Key::Left));
        assert_eq!(key(b"\x1b[H"), Some(Key::Home));
        assert_eq!(key(b"\x1b[4~"), Some(Key::End));
        assert_eq!(key(b"\x1b[3~"), Some(Key::Delete));
        assert_eq!(key(b"\x1b[Z"), Some(Key::BackTab));
        assert_eq!(key(b"\x1b[1;5C"), None, "ctrl-arrow is not used");
    }

    #[test]
    fn decodes_kitty_and_modify_other_keys() {
        assert_eq!(key(b"\x1b[97;5u"), Some(Key::Ctrl('a')));
        assert_eq!(key(b"\x1b[27;5;117~"), Some(Key::Ctrl('u')));
        assert_eq!(key(b"\x1b[9;2u"), Some(Key::BackTab));
        assert_eq!(key(b"\x1b[13u"), Some(Key::Enter));
        assert_eq!(key(b"\x1b[27u"), Some(Key::Esc));
        assert_eq!(key(b"\x1b[97;3u"), None, "alt chords are ignored");
    }

    #[test]
    fn recognizes_paste_markers_and_ignores_mouse() {
        assert_eq!(decode(b"\x1b[200~"), Some(Decoded::PasteStart));
        assert_eq!(decode(b"\x1b[201~"), Some(Decoded::PasteEnd));
        assert_eq!(decode(b"\x1b[<0;12;9M"), None);
    }
}
