use crate::term::BufWrite as _;
use alloc::string::String;

/// Represents a foreground or background color for cells.
#[derive(Eq, PartialEq, Debug, Copy, Clone, Default)]
pub enum Color {
    /// The default terminal color.
    #[default]
    Default,

    /// An indexed terminal color.
    Idx(u8),

    /// An RGB terminal color. The parameters are (red, green, blue).
    Rgb(u8, u8, u8),
}

const TEXT_MODE_INTENSITY: u8 = 0b0000_0011;
const TEXT_MODE_BOLD: u8 = 0b0000_0001;
const TEXT_MODE_DIM: u8 = 0b0000_0010;
const TEXT_MODE_ITALIC: u8 = 0b0000_0100;
const TEXT_MODE_UNDERLINE: u8 = 0b0000_1000;
const TEXT_MODE_INVERSE: u8 = 0b0001_0000;
const TEXT_MODE_BLINK: u8 = 0b0010_0000;
const TEXT_MODE_HIDDEN: u8 = 0b0100_0000;
const TEXT_MODE_STRIKE: u8 = 0b1000_0000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Attrs {
    pub fgcolor: Color,
    pub bgcolor: Color,
    pub mode: u8,
}

impl Default for Attrs {
    fn default() -> Self {
        Self::new()
    }
}

impl Attrs {
    pub const fn new() -> Self {
        Self {
            fgcolor: Color::Default,
            bgcolor: Color::Default,
            mode: 0,
        }
    }

    pub fn bold(&self) -> bool {
        self.mode & TEXT_MODE_BOLD != 0
    }

    pub fn dim(&self) -> bool {
        self.mode & TEXT_MODE_DIM != 0
    }

    fn intensity(&self) -> u8 {
        self.mode & TEXT_MODE_INTENSITY
    }

    pub fn set_bold(&mut self) {
        self.mode |= TEXT_MODE_BOLD;
    }

    pub fn set_dim(&mut self) {
        self.mode |= TEXT_MODE_DIM;
    }

    pub fn set_normal_intensity(&mut self) {
        self.mode &= !TEXT_MODE_INTENSITY;
    }

    pub fn italic(&self) -> bool {
        self.mode & TEXT_MODE_ITALIC != 0
    }

    pub fn set_italic(&mut self, italic: bool) {
        if italic {
            self.mode |= TEXT_MODE_ITALIC;
        } else {
            self.mode &= !TEXT_MODE_ITALIC;
        }
    }

    pub fn underline(&self) -> bool {
        self.mode & TEXT_MODE_UNDERLINE != 0
    }

    pub fn set_underline(&mut self, underline: bool) {
        if underline {
            self.mode |= TEXT_MODE_UNDERLINE;
        } else {
            self.mode &= !TEXT_MODE_UNDERLINE;
        }
    }

    pub fn inverse(&self) -> bool {
        self.mode & TEXT_MODE_INVERSE != 0
    }

    pub fn set_inverse(&mut self, inverse: bool) {
        if inverse {
            self.mode |= TEXT_MODE_INVERSE;
        } else {
            self.mode &= !TEXT_MODE_INVERSE;
        }
    }

    pub fn blink(&self) -> bool { self.mode & TEXT_MODE_BLINK != 0 }
    pub fn hidden(&self) -> bool { self.mode & TEXT_MODE_HIDDEN != 0 }
    pub fn strike(&self) -> bool { self.mode & TEXT_MODE_STRIKE != 0 }
    pub fn set_blink(&mut self, enabled: bool) { self.set_flag(TEXT_MODE_BLINK, enabled); }
    pub fn set_hidden(&mut self, enabled: bool) { self.set_flag(TEXT_MODE_HIDDEN, enabled); }
    pub fn set_strike(&mut self, enabled: bool) { self.set_flag(TEXT_MODE_STRIKE, enabled); }
    fn set_flag(&mut self, flag: u8, enabled: bool) {
        if enabled { self.mode |= flag; } else { self.mode &= !flag; }
    }

    pub fn write_escape_code_diff(
        &self,
        contents: &mut String,
        other: &Self,
    ) {
        self.write_escape_code_diff_fmt(contents, other)
            .expect("writing to a String cannot fail");
    }

    pub fn write_escape_code_diff_fmt(
        &self,
        writer: &mut impl alloc::fmt::Write,
        other: &Self,
    ) -> alloc::fmt::Result {
        if self != other && self == &Self::default() {
            return crate::term::ClearAttrs.write_fmt(writer);
        }

        let attrs = crate::term::Attrs::default();

        let attrs = if self.fgcolor == other.fgcolor {
            attrs
        } else {
            attrs.fgcolor(self.fgcolor)
        };
        let attrs = if self.bgcolor == other.bgcolor {
            attrs
        } else {
            attrs.bgcolor(self.bgcolor)
        };
        let attrs = if self.intensity() == other.intensity() {
            attrs
        } else {
            attrs.intensity(match self.intensity() {
                0 => crate::term::Intensity::Normal,
                TEXT_MODE_BOLD => crate::term::Intensity::Bold,
                TEXT_MODE_DIM => crate::term::Intensity::Dim,
                // Both bold and faint may be independently enabled.
                TEXT_MODE_INTENSITY => crate::term::Intensity::Normal,
                _ => unreachable!(),
            })
        };
        let attrs = if self.italic() == other.italic() {
            attrs
        } else {
            attrs.italic(self.italic())
        };
        let attrs = if self.underline() == other.underline() {
            attrs
        } else {
            attrs.underline(self.underline())
        };
        let attrs = if self.inverse() == other.inverse() {
            attrs
        } else {
            attrs.inverse(self.inverse())
        };

        // SGR 1 and 2 enable independently; switching intensity must clear
        // the old bit before enabling the requested one.
        if self.intensity() != other.intensity() && other.intensity() != 0
            && self.intensity() != 0 && self.intensity() != TEXT_MODE_INTENSITY {
            writer.write_str("\x1b[22m")?;
        }
        attrs.write_fmt(writer)?;
        if self.intensity() != other.intensity()
            && self.intensity() == TEXT_MODE_INTENSITY {
            writer.write_str("\x1b[1;2m")?;
        }
        for (enabled, previous, on, off) in [
            (self.blink(), other.blink(), 5, 25),
            (self.hidden(), other.hidden(), 8, 28),
            (self.strike(), other.strike(), 9, 29),
        ] {
            if enabled != previous {
                write!(writer, "\x1b[{}m", if enabled { on } else { off })?;
            }
        }
        Ok(())
    }
}
