//! A small form toolkit for the local menu: a single-line text editor, a
//! handful of field types, and a `Form` that owns focus, key handling,
//! validation errors, and rendering into ratatui `Line`s.
//!
//! Concrete forms (port forward, file copy, folder sync, approval path
//! confirmation) are just a list of `Field`s built in `session.rs`; nothing
//! here knows what a form is for.

use std::path::{Path, PathBuf};

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::keys::Key;
use super::picker::{Picker, Source};
use super::ui::{theme, wrap};

/// Dropdown rows shown at once (PageUp/PageDown move by this much).
pub const DROPDOWN_ROWS: usize = 8;

/// A single-line editable text buffer with a cursor.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LineEdit {
    chars: Vec<char>,
    cursor: usize,
}

impl LineEdit {
    pub fn new(text: &str) -> Self {
        let chars: Vec<char> = text.chars().collect();
        Self {
            cursor: chars.len(),
            chars,
        }
    }

    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    pub fn at_end(&self) -> bool {
        self.cursor == self.chars.len()
    }

    fn insert_str(&mut self, text: &str, accept: &dyn Fn(char) -> bool) -> bool {
        let mut changed = false;
        for ch in text.chars().filter(|ch| !ch.is_control() && accept(*ch)) {
            self.chars.insert(self.cursor, ch);
            self.cursor += 1;
            changed = true;
        }
        changed
    }

    /// Applies an editing key. Returns whether the key was consumed.
    /// `accept` filters inserted characters (e.g. digits only).
    pub fn handle(&mut self, key: &Key, accept: &dyn Fn(char) -> bool) -> bool {
        match key {
            Key::Char(ch) => {
                self.insert_str(&ch.to_string(), accept);
            }
            Key::Paste(text) => {
                // A multi-line paste into a one-line field keeps only the
                // first line rather than smuggling in newlines.
                let first = text.lines().next().unwrap_or_default();
                self.insert_str(first, accept);
            }
            Key::Backspace | Key::Ctrl('h') => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    self.chars.remove(self.cursor);
                }
            }
            Key::Delete | Key::Ctrl('d') => {
                if self.cursor < self.chars.len() {
                    self.chars.remove(self.cursor);
                }
            }
            Key::Left | Key::Ctrl('b') => self.cursor = self.cursor.saturating_sub(1),
            Key::Right | Key::Ctrl('f') => self.cursor = (self.cursor + 1).min(self.chars.len()),
            Key::Home | Key::Ctrl('a') => self.cursor = 0,
            Key::End | Key::Ctrl('e') => self.cursor = self.chars.len(),
            Key::Ctrl('u') => {
                self.chars.drain(..self.cursor);
                self.cursor = 0;
            }
            Key::Ctrl('k') => {
                self.chars.truncate(self.cursor);
            }
            Key::Ctrl('w') => {
                // Delete back to the previous word or path component.
                let mut start = self.cursor;
                while start > 0 && is_separator(self.chars[start - 1]) {
                    start -= 1;
                }
                while start > 0 && !is_separator(self.chars[start - 1]) {
                    start -= 1;
                }
                self.chars.drain(start..self.cursor);
                self.cursor = start;
            }
            _ => return false,
        }
        true
    }

    /// The slice of the text visible in `width` columns, scrolled so the
    /// cursor stays in view, plus the cursor's column within that slice.
    pub fn view(&self, width: usize) -> (String, usize) {
        let width = width.max(1);
        let widths: Vec<usize> = self
            .chars
            .iter()
            .map(|ch| ch.width().unwrap_or(0))
            .collect();
        // Leave one column for the cursor itself.
        let mut start = 0;
        while widths[start..self.cursor].iter().sum::<usize>() > width - 1 {
            start += 1;
        }
        let mut visible = String::new();
        let mut used = 0;
        for (ch, w) in self.chars[start..].iter().zip(&widths[start..]) {
            if used + w > width {
                break;
            }
            visible.push(*ch);
            used += w;
        }
        let cursor = widths[start..self.cursor].iter().sum();
        (visible, cursor)
    }
}

fn is_separator(ch: char) -> bool {
    ch == '/' || ch.is_whitespace()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Completion {
    Paths,
    Directories,
}

#[derive(Clone, Debug)]
pub struct ChoiceOption {
    pub label: &'static str,
    pub help: &'static str,
    pub warn: bool,
}

#[derive(Clone, Debug)]
pub enum Input {
    Text {
        edit: LineEdit,
        placeholder: String,
        digits_only: bool,
        completion: Option<Completion>,
        /// Completion suffix shown dimmed after the text (fish-style);
        /// Right/End at the end of the text accepts it.
        suggestion: String,
        /// The path dropdown, for fields with a completion kind.
        picker: Option<Picker>,
    },
    Toggle {
        on: bool,
        on_label: &'static str,
        off_label: &'static str,
    },
    Choice {
        options: Vec<ChoiceOption>,
        selected: usize,
    },
}

#[derive(Clone, Debug)]
pub struct Field {
    pub id: &'static str,
    pub label: &'static str,
    pub input: Input,
    pub error: Option<String>,
}

impl Field {
    pub fn text(id: &'static str, label: &'static str) -> Self {
        Self {
            id,
            label,
            input: Input::Text {
                edit: LineEdit::default(),
                placeholder: String::new(),
                digits_only: false,
                completion: None,
                suggestion: String::new(),
                picker: None,
            },
            error: None,
        }
    }

    pub fn number(id: &'static str, label: &'static str) -> Self {
        let mut field = Self::text(id, label);
        if let Input::Text { digits_only, .. } = &mut field.input {
            *digits_only = true;
        }
        field
    }

    pub fn toggle(
        id: &'static str,
        label: &'static str,
        on: bool,
        on_label: &'static str,
        off_label: &'static str,
    ) -> Self {
        Self {
            id,
            label,
            input: Input::Toggle {
                on,
                on_label,
                off_label,
            },
            error: None,
        }
    }

    pub fn choice(
        id: &'static str,
        label: &'static str,
        options: Vec<ChoiceOption>,
        selected: usize,
    ) -> Self {
        Self {
            id,
            label,
            input: Input::Choice { options, selected },
            error: None,
        }
    }

    pub fn placeholder(mut self, text: impl Into<String>) -> Self {
        if let Input::Text { placeholder, .. } = &mut self.input {
            *placeholder = text.into();
        }
        self
    }

    pub fn value(mut self, text: &str) -> Self {
        if let Input::Text { edit, .. } = &mut self.input {
            *edit = LineEdit::new(text);
        }
        self
    }

    pub fn complete(mut self, kind: Completion) -> Self {
        if let Input::Text {
            completion, picker, ..
        } = &mut self.input
        {
            *completion = Some(kind);
            *picker = Some(Picker::new(kind));
        }
        self
    }

    /// The dropdown, if it's showing.
    pub fn dropdown(&self) -> Option<&Picker> {
        match &self.input {
            Input::Text {
                picker: Some(picker),
                ..
            } if picker.visible() => Some(picker),
            _ => None,
        }
    }

    /// Brings the dropdown and the inline suggestion up to date with the
    /// text (which builders such as `value` set without either).
    fn refresh_picker(&mut self) {
        if let Input::Text {
            edit,
            completion: Some(kind),
            suggestion,
            picker: Some(picker),
            ..
        } = &mut self.input
        {
            let text = edit.text();
            if picker.refresh(&text) {
                *suggestion = if edit.at_end() {
                    path_suggestion(&text, *kind)
                } else {
                    String::new()
                };
            }
        }
    }

    fn set_text(&mut self, text: &str) {
        if let Input::Text {
            edit,
            completion,
            suggestion,
            ..
        } = &mut self.input
        {
            *edit = LineEdit::new(text);
            *suggestion = match completion {
                Some(kind) => path_suggestion(text, *kind),
                None => String::new(),
            };
        }
        self.error = None;
        self.refresh_picker();
    }

    /// Keys the open dropdown takes before the form sees them: ↑/↓ and
    /// PageUp/PageDown highlight, Tab completes, Enter takes a highlighted
    /// entry, Esc hides the dropdown.
    fn dropdown_key(&mut self, key: &Key) -> bool {
        let Input::Text {
            edit,
            suggestion,
            picker: Some(picker),
            ..
        } = &mut self.input
        else {
            return false;
        };
        if !picker.visible() {
            return false;
        }
        let chosen = match key {
            Key::Down | Key::Ctrl('n') => {
                picker.highlight_next(1);
                return true;
            }
            Key::PageDown => {
                picker.highlight_next(DROPDOWN_ROWS);
                return true;
            }
            Key::Up | Key::Ctrl('p') if picker.highlighted.is_some() => {
                picker.highlight_previous(1);
                return true;
            }
            Key::PageUp if picker.highlighted.is_some() => {
                picker.highlight_previous(DROPDOWN_ROWS);
                return true;
            }
            Key::Esc => {
                picker.dismissed = true;
                return true;
            }
            Key::Enter => match picker.highlighted {
                Some(index) => index,
                None => return false,
            },
            Key::Tab => {
                // Like a shell, an unambiguous common prefix completes
                // first; otherwise the highlighted (or best) entry does.
                if picker.highlighted.is_none() && !suggestion.is_empty() && edit.at_end() {
                    let text = format!("{}{}", edit.text(), suggestion);
                    self.set_text(&text);
                    return true;
                }
                let index = picker.highlighted.unwrap_or(0);
                if picker.items[index].replacement == edit.text() {
                    // Already complete: let Tab move to the next field.
                    return false;
                }
                index
            }
            _ => return false,
        };
        let item = picker.items[chosen].clone();
        self.set_text(&item.replacement);
        if !item.is_dir {
            // A chosen file is final; the next Enter submits.
            if let Input::Text {
                picker: Some(picker),
                ..
            } = &mut self.input
            {
                picker.dismissed = true;
            }
        }
        true
    }

    fn handle(&mut self, key: &Key) -> bool {
        match &mut self.input {
            Input::Text {
                edit,
                digits_only,
                completion,
                suggestion,
                ..
            } => {
                if matches!(key, Key::Right | Key::End | Key::Ctrl('f') | Key::Ctrl('e'))
                    && edit.at_end()
                    && !suggestion.is_empty()
                {
                    let accepted = std::mem::take(suggestion);
                    edit.insert_str(&accepted, &|_| true);
                } else {
                    let digits_only = *digits_only;
                    let accept = move |ch: char| !digits_only || ch.is_ascii_digit();
                    if !edit.handle(key, &accept) {
                        return false;
                    }
                }
                *suggestion = match completion {
                    Some(kind) if edit.at_end() => path_suggestion(&edit.text(), *kind),
                    _ => String::new(),
                };
                self.error = None;
                true
            }
            Input::Toggle { on, .. } => match key {
                Key::Char(' ') | Key::Left | Key::Right => {
                    *on = !*on;
                    true
                }
                _ => false,
            },
            Input::Choice { options, selected } => {
                let count = options.len().max(1);
                match key {
                    Key::Char(' ') | Key::Right => *selected = (*selected + 1) % count,
                    Key::Left => *selected = (*selected + count - 1) % count,
                    _ => return false,
                }
                true
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum FormEvent {
    Handled,
    Ignored,
    Submit,
    Cancel,
}

#[derive(Clone, Debug)]
pub struct Form {
    pub title: String,
    /// Wrapped explanatory text above the fields: `(text, warn)`.
    pub intro: Vec<(String, bool)>,
    pub fields: Vec<Field>,
    pub notes: Vec<String>,
    pub submit_label: &'static str,
    pub focus: usize,
    /// A form-level error (e.g. the daemon rejected the request).
    pub error: Option<String>,
}

impl Form {
    pub fn new(title: impl Into<String>, submit_label: &'static str, fields: Vec<Field>) -> Self {
        let mut form = Self {
            title: title.into(),
            intro: Vec::new(),
            fields,
            notes: Vec::new(),
            submit_label,
            focus: 0,
            error: None,
        };
        form.refresh_focused();
        form
    }

    pub fn intro(mut self, text: impl Into<String>, warn: bool) -> Self {
        self.intro.push((text.into(), warn));
        self
    }

    pub fn note(mut self, text: impl Into<String>) -> Self {
        self.notes.push(text.into());
        self
    }

    pub fn handle_key(&mut self, key: &Key) -> FormEvent {
        let event = self.dispatch_key(key);
        self.refresh_focused();
        event
    }

    /// Gives every path field's dropdown the recently used directories.
    pub fn set_recents(&mut self, recents: Vec<String>) {
        for field in &mut self.fields {
            if let Input::Text {
                picker: Some(picker),
                ..
            } = &mut field.input
            {
                picker.set_recents(recents.clone());
            }
        }
        self.refresh_focused();
    }

    /// Brings the focused field's dropdown up to date with its text.
    fn refresh_focused(&mut self) {
        if let Some(field) = self.fields.get_mut(self.focus) {
            field.refresh_picker();
        }
    }

    fn dispatch_key(&mut self, key: &Key) -> FormEvent {
        if self
            .fields
            .get_mut(self.focus)
            .is_some_and(|field| field.dropdown_key(key))
        {
            return FormEvent::Handled;
        }
        let count = self.fields.len();
        match key {
            Key::Esc => return FormEvent::Cancel,
            Key::Enter => return FormEvent::Submit,
            Key::Tab | Key::Down if count > 0 => self.focus = (self.focus + 1) % count,
            Key::BackTab | Key::Up if count > 0 => self.focus = (self.focus + count - 1) % count,
            _ => {
                let handled = self
                    .fields
                    .get_mut(self.focus)
                    .is_some_and(|field| field.handle(key));
                return if handled {
                    FormEvent::Handled
                } else {
                    FormEvent::Ignored
                };
            }
        }
        FormEvent::Handled
    }

    fn field(&self, id: &str) -> Option<&Field> {
        self.fields.iter().find(|field| field.id == id)
    }

    pub fn text(&self, id: &str) -> String {
        match self.field(id).map(|field| &field.input) {
            Some(Input::Text { edit, .. }) => edit.text().trim().to_string(),
            _ => String::new(),
        }
    }

    pub fn toggle(&self, id: &str) -> bool {
        matches!(
            self.field(id).map(|field| &field.input),
            Some(Input::Toggle { on: true, .. })
        )
    }

    pub fn choice(&self, id: &str) -> usize {
        match self.field(id).map(|field| &field.input) {
            Some(Input::Choice { selected, .. }) => *selected,
            _ => 0,
        }
    }

    pub fn clear_errors(&mut self) {
        self.error = None;
        for field in &mut self.fields {
            field.error = None;
        }
    }

    /// Sets a field error and moves focus to the first field in error.
    pub fn set_error(&mut self, id: &str, message: impl Into<String>) {
        if let Some(index) = self.fields.iter().position(|field| field.id == id) {
            self.fields[index].error = Some(message.into());
            if self.fields[..index]
                .iter()
                .all(|field| field.error.is_none())
            {
                self.focus = index;
                self.refresh_focused();
            }
        }
    }

    pub fn has_errors(&self) -> bool {
        self.fields.iter().any(|field| field.error.is_some())
    }

    pub fn focused(&self) -> Option<&Field> {
        self.fields.get(self.focus)
    }

    /// Renders the form body into `width` columns: the lines, the terminal
    /// cursor position (row, column) within them if the focused field is a
    /// text field, and the focused field's open dropdown.
    pub fn render(&self, width: usize) -> RenderedForm<'_> {
        let mut lines = Vec::new();
        let mut cursor = None;
        let mut dropdown = None;

        for (text, warn) in &self.intro {
            let style = if *warn {
                theme::warn()
            } else {
                Style::default()
            };
            for line in wrap(text, width) {
                lines.push(Line::from(Span::styled(line, style)));
            }
        }
        if !self.intro.is_empty() {
            lines.push(Line::default());
        }

        let label_width = self
            .fields
            .iter()
            .map(|field| field.label.width())
            .max()
            .unwrap_or(0);
        // "› " + label + "  " + value
        let indent = 2 + label_width + 2;
        let value_width = width.saturating_sub(indent).max(8);

        for (index, field) in self.fields.iter().enumerate() {
            let focused = index == self.focus;
            let mut spans = vec![
                Span::styled(if focused { "› " } else { "  " }, theme::accent()),
                Span::styled(
                    format!("{:>label_width$}", field.label),
                    if focused {
                        theme::label_focused()
                    } else {
                        theme::label()
                    },
                ),
                Span::raw("  "),
            ];
            let mut help: Option<(String, Style)> = None;
            match &field.input {
                Input::Text {
                    edit,
                    placeholder,
                    suggestion,
                    ..
                } => {
                    let box_style = if focused {
                        theme::input_focused()
                    } else {
                        theme::input()
                    };
                    let (visible, cursor_col) = edit.view(value_width);
                    let mut used = visible.width();
                    if edit.is_empty() && !placeholder.is_empty() {
                        let shown: String = truncate(placeholder, value_width);
                        used = shown.width();
                        spans.push(Span::styled(shown, box_style.patch(theme::placeholder())));
                    } else {
                        spans.push(Span::styled(visible, box_style));
                        let highlighting = field
                            .dropdown()
                            .is_some_and(|picker| picker.highlighted.is_some());
                        if focused && !suggestion.is_empty() && !highlighting {
                            let shown = truncate(suggestion, value_width.saturating_sub(used));
                            used += shown.width();
                            spans.push(Span::styled(shown, box_style.patch(theme::placeholder())));
                        }
                    }
                    spans.push(Span::styled(
                        " ".repeat(value_width.saturating_sub(used)),
                        box_style,
                    ));
                    if focused {
                        cursor = Some((lines.len() as u16, (indent + cursor_col) as u16));
                        // The popup's left border sits in the gap before
                        // the value and its right border at the value's
                        // last column, leaving `value_width - 1` inside.
                        let inner = value_width.saturating_sub(1);
                        dropdown = field.dropdown().map(|picker| Dropdown {
                            row: lines.len() as u16,
                            col: indent as u16,
                            width: inner as u16,
                            picker,
                            tab_takes_best: suggestion.is_empty(),
                        });
                    }
                }
                Input::Toggle {
                    on,
                    on_label,
                    off_label,
                } => {
                    let style = if focused {
                        theme::label_focused()
                    } else {
                        Style::default()
                    };
                    spans.push(Span::styled(if *on { "[x] " } else { "[ ] " }, style));
                    spans.push(Span::raw(if *on { *on_label } else { *off_label }));
                }
                Input::Choice { options, selected } => {
                    if let Some(option) = options.get(*selected) {
                        let style = if focused {
                            theme::label_focused()
                        } else {
                            Style::default()
                        };
                        spans.push(Span::styled(
                            if focused { "‹ " } else { "  " },
                            theme::accent(),
                        ));
                        spans.push(Span::styled(option.label, style));
                        spans.push(Span::styled(
                            if focused { " ›" } else { "  " },
                            theme::accent(),
                        ));
                        spans.push(Span::styled(
                            format!("  {}/{}", selected + 1, options.len()),
                            theme::dim(),
                        ));
                        help = Some((
                            option.help.to_string(),
                            if option.warn {
                                theme::warn()
                            } else {
                                theme::dim()
                            },
                        ));
                    }
                }
            }
            lines.push(Line::from(spans));
            if let Some(error) = &field.error {
                for line in wrap(&format!("✗ {error}"), value_width) {
                    lines.push(Line::from(vec![
                        Span::raw(" ".repeat(indent)),
                        Span::styled(line, theme::error()),
                    ]));
                }
            } else if let Some((text, style)) = help {
                for line in wrap(&text, value_width) {
                    lines.push(Line::from(vec![
                        Span::raw(" ".repeat(indent)),
                        Span::styled(line, style),
                    ]));
                }
            }
        }

        if !self.notes.is_empty() {
            lines.push(Line::default());
            for note in &self.notes {
                for line in wrap(note, width) {
                    lines.push(Line::from(Span::styled(line, theme::dim())));
                }
            }
        }
        if let Some(error) = &self.error {
            lines.push(Line::default());
            for line in wrap(&format!("✗ {error}"), width) {
                lines.push(Line::from(Span::styled(line, theme::error())));
            }
        }
        RenderedForm {
            lines,
            cursor,
            dropdown,
        }
    }
}

pub struct RenderedForm<'a> {
    pub lines: Vec<Line<'static>>,
    pub cursor: Option<(u16, u16)>,
    pub dropdown: Option<Dropdown<'a>>,
}

/// A dropdown to draw over the form, anchored under the field at `row`
/// (a line index into the form body) with its text starting at `col`;
/// `width` is the space inside its border.
pub struct Dropdown<'a> {
    pub row: u16,
    pub col: u16,
    pub width: u16,
    picker: &'a Picker,
    /// Tab takes the best entry (rather than a common prefix).
    tab_takes_best: bool,
}

impl Dropdown<'_> {
    /// The rows wanted with unlimited space: entries plus a position
    /// footer when they don't all fit.
    pub fn wanted_rows(&self) -> usize {
        let total = self.picker.items.len();
        DROPDOWN_ROWS.min(total) + usize::from(total > DROPDOWN_ROWS)
    }

    /// The dropdown's lines in at most `max_rows` rows, scrolled to keep
    /// the highlighted entry visible.
    pub fn lines(&self, max_rows: usize) -> Vec<Line<'static>> {
        dropdown_lines(
            self.picker,
            usize::from(self.width),
            self.tab_takes_best,
            max_rows,
        )
    }
}

fn dropdown_lines(
    picker: &Picker,
    width: usize,
    tab_takes_best: bool,
    max_rows: usize,
) -> Vec<Line<'static>> {
    let total = picker.items.len();
    let mut rows = DROPDOWN_ROWS.min(total).min(max_rows);
    if rows < total && rows == max_rows {
        // Keep a row for the position footer.
        rows = rows.saturating_sub(1);
    }
    if rows == 0 {
        return Vec::new();
    }
    let start = picker
        .highlighted
        .map(|index| index.saturating_sub(rows - 1))
        .unwrap_or(0);
    let mut lines: Vec<Line<'static>> = picker.items[start..start + rows]
        .iter()
        .enumerate()
        .map(|(offset, item)| {
            let index = start + offset;
            let highlighted = picker.highlighted == Some(index);
            let base = if highlighted {
                theme::selected_row()
            } else {
                Style::default()
            };
            let tag = match (&item.source, highlighted) {
                (Source::Recent, _) => "recent",
                (_, true) => "enter",
                _ if index == 0 && picker.highlighted.is_none() && tab_takes_best => "tab",
                _ => "",
            };
            let label_width = width.saturating_sub(2 + tag.width() + 1);
            let name_style = if item.is_dir {
                base.patch(theme::accent())
            } else {
                base
            };
            let mut spans = vec![Span::styled(
                if highlighted { "› " } else { "  " },
                base.patch(theme::accent()),
            )];
            let mut used = 2;
            let shown = truncate(&item.label, label_width);
            let cut = shown.chars().count() < item.label.chars().count();
            let shown = if cut {
                let mut keep = truncate(&item.label, label_width.saturating_sub(1));
                keep.push('…');
                keep
            } else {
                shown
            };
            for (char_index, ch) in shown.chars().enumerate() {
                let style = if item.positions.contains(&char_index)
                    && !(cut && char_index + 1 == shown.chars().count())
                {
                    name_style.patch(theme::key())
                } else {
                    name_style
                };
                used += ch.width().unwrap_or(0);
                spans.push(Span::styled(ch.to_string(), style));
            }
            let pad = width.saturating_sub(used + tag.width());
            spans.push(Span::styled(" ".repeat(pad), base));
            spans.push(Span::styled(tag, base.patch(theme::dim())));
            Line::from(spans)
        })
        .collect();
    if total > rows {
        lines.push(Line::from(Span::styled(
            format!("  {}–{} of {total}", start + 1, start + rows),
            theme::dim(),
        )));
    }
    lines
}

fn truncate(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = ch.width().unwrap_or(0);
        if used + w > width {
            break;
        }
        out.push(ch);
        used += w;
    }
    out
}

/// Expands `~` and resolves a relative path against this process's working
/// directory - the directory `beams ssh` was started from, which is what a
/// user typing a relative workstation path means (the local daemon's own
/// working directory is unrelated).
pub fn resolve_local_path(text: &str) -> PathBuf {
    let home = || std::env::var_os("HOME").map(PathBuf::from);
    let expanded = if text == "~" {
        home().unwrap_or_else(|| PathBuf::from(text))
    } else if let Some(rest) = text.strip_prefix("~/") {
        home()
            .map(|home| home.join(rest))
            .unwrap_or_else(|| PathBuf::from(text))
    } else {
        PathBuf::from(text)
    };
    if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(&expanded))
            .unwrap_or(expanded)
    }
}

const MAX_COMPLETION_ENTRIES: usize = 4096;

/// The completion suffix for a partially typed workstation path: the longest
/// common extension of every matching directory entry, plus a trailing `/`
/// when that lands exactly on a single directory.
pub fn path_suggestion(text: &str, kind: Completion) -> String {
    if text.is_empty() {
        return String::new();
    }
    if text == "~" {
        return "/".to_string();
    }
    let (dir_text, prefix) = match text.rfind('/') {
        Some(index) => (&text[..=index], &text[index + 1..]),
        None => ("", text),
    };
    let dir = if dir_text.is_empty() {
        resolve_local_path(".")
    } else {
        resolve_local_path(dir_text)
    };
    complete_in(&dir, prefix, kind)
}

fn complete_in(dir: &Path, prefix: &str, kind: Completion) -> String {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return String::new();
    };
    let mut matches: Vec<(String, bool)> = Vec::new();
    for entry in entries.take(MAX_COMPLETION_ENTRIES).flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(prefix) || (name.starts_with('.') && !prefix.starts_with('.')) {
            continue;
        }
        // Follow symlinks so a link to a directory completes like one.
        let is_dir = std::fs::metadata(entry.path()).is_ok_and(|meta| meta.is_dir());
        if kind == Completion::Directories && !is_dir {
            continue;
        }
        matches.push((name, is_dir));
    }
    let Some((first, _)) = matches.first() else {
        return String::new();
    };
    let mut common: String = first.clone();
    for (name, _) in &matches[1..] {
        let shared = common
            .char_indices()
            .zip(name.chars())
            .take_while(|((_, a), b)| a == b)
            .last()
            .map(|((index, ch), _)| index + ch.len_utf8())
            .unwrap_or(0);
        common.truncate(shared);
    }
    let mut suffix = common[prefix.len()..].to_string();
    if matches.len() == 1 && matches[0].1 {
        suffix.push('/');
    }
    suffix
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(text: &str, keys: &[Key]) -> LineEdit {
        let mut edit = LineEdit::new(text);
        for key in keys {
            edit.handle(key, &|_| true);
        }
        edit
    }

    #[test]
    fn line_edit_moves_and_edits_at_the_cursor() {
        let e = edit("hllo", &[Key::Home, Key::Right, Key::Char('e')]);
        assert_eq!(e.text(), "hello");
        let e = edit(
            "hello",
            &[Key::Left, Key::Left, Key::Backspace, Key::Delete],
        );
        assert_eq!(e.text(), "heo");
        let e = edit("hello world", &[Key::Ctrl('a'), Key::Ctrl('k')]);
        assert_eq!(e.text(), "");
        let e = edit("~/src/beams", &[Key::Ctrl('w')]);
        assert_eq!(e.text(), "~/src/");
        let e = edit("~/src/beams", &[Key::Left, Key::Left, Key::Ctrl('u')]);
        assert_eq!(e.text(), "ms");
        let e = edit("", &[Key::Paste("one\ntwo".to_string())]);
        assert_eq!(e.text(), "one");
    }

    #[test]
    fn line_edit_view_scrolls_to_keep_the_cursor_visible() {
        let e = LineEdit::new("0123456789");
        assert_eq!(e.view(20), ("0123456789".to_string(), 10));
        let (visible, cursor) = e.view(5);
        assert_eq!((visible.as_str(), cursor), ("6789", 4));
        let e = edit("0123456789", &[Key::Home]);
        assert_eq!(e.view(5), ("01234".to_string(), 0));
    }

    #[test]
    fn number_fields_reject_non_digits_and_tab_moves_focus() {
        let mut form = Form::new(
            "t",
            "create",
            vec![Field::number("a", "A"), Field::text("b", "B")],
        );
        for key in [Key::Char('8'), Key::Char('x'), Key::Char('0')] {
            form.handle_key(&key);
        }
        assert_eq!(form.text("a"), "80");
        form.handle_key(&Key::Tab);
        form.handle_key(&Key::Char('x'));
        assert_eq!(form.text("b"), "x");
        assert_eq!(form.handle_key(&Key::BackTab), FormEvent::Handled);
        assert_eq!(form.focus, 0);
        assert_eq!(form.handle_key(&Key::Enter), FormEvent::Submit);
        assert_eq!(form.handle_key(&Key::Esc), FormEvent::Cancel);
    }

    #[test]
    fn toggles_and_choices_cycle_with_space_and_arrows() {
        let options = vec![
            ChoiceOption {
                label: "one",
                help: "",
                warn: false,
            },
            ChoiceOption {
                label: "two",
                help: "",
                warn: true,
            },
        ];
        let mut form = Form::new(
            "t",
            "go",
            vec![
                Field::toggle("t", "T", false, "on", "off"),
                Field::choice("c", "C", options, 0),
            ],
        );
        form.handle_key(&Key::Char(' '));
        assert!(form.toggle("t"));
        form.handle_key(&Key::Down);
        form.handle_key(&Key::Left);
        assert_eq!(form.choice("c"), 1);
        form.handle_key(&Key::Right);
        assert_eq!(form.choice("c"), 0);
    }

    #[test]
    fn errors_focus_the_first_bad_field_and_clear_on_edit() {
        let mut form = Form::new(
            "t",
            "go",
            vec![Field::text("a", "A"), Field::text("b", "B")],
        );
        form.focus = 0;
        form.set_error("b", "required");
        assert_eq!(form.focus, 1);
        form.handle_key(&Key::Char('x'));
        assert!(!form.has_errors());
    }

    #[test]
    fn render_reports_the_cursor_in_the_focused_text_field() {
        let mut form = Form::new(
            "t",
            "go",
            vec![
                Field::number("port", "Beam port"),
                Field::text("x", "X").placeholder("auto"),
            ],
        );
        form.handle_key(&Key::Char('8'));
        let RenderedForm { lines, cursor, .. } = form.render(40);
        assert_eq!(lines.len(), 2);
        // "› " + "Beam port" + "  " = 13 columns, then one typed digit.
        assert_eq!(cursor, Some((0, 14)));
        let text: String = lines[1]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(text.contains("auto"), "{text}");
    }

    #[test]
    fn path_completion_extends_to_the_common_prefix() {
        let dir = std::env::temp_dir().join(format!("beams-complete-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("project-alpha")).unwrap();
        std::fs::create_dir_all(dir.join("project-beta")).unwrap();
        std::fs::write(dir.join("projection.txt"), "").unwrap();
        std::fs::write(dir.join(".hidden"), "").unwrap();
        let base = format!("{}/", dir.display());

        assert_eq!(
            path_suggestion(&format!("{base}pro"), Completion::Paths),
            "ject"
        );
        assert_eq!(
            path_suggestion(&format!("{base}project-a"), Completion::Paths),
            "lpha/"
        );
        assert_eq!(
            path_suggestion(&format!("{base}projecti"), Completion::Paths),
            "on.txt"
        );
        assert_eq!(
            path_suggestion(&format!("{base}projecti"), Completion::Directories),
            ""
        );
        assert_eq!(
            path_suggestion(&format!("{base}project"), Completion::Directories),
            "-"
        );
        assert_eq!(
            path_suggestion(&format!("{base}nothing"), Completion::Paths),
            ""
        );
        assert_eq!(
            path_suggestion(&format!("{base}.h"), Completion::Paths),
            "idden"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn local_paths_resolve_home_and_the_session_directory() {
        let home = PathBuf::from(std::env::var("HOME").unwrap());
        assert_eq!(resolve_local_path("~/x"), home.join("x"));
        assert_eq!(resolve_local_path("~"), home);
        assert_eq!(resolve_local_path("/abs"), PathBuf::from("/abs"));
        assert_eq!(
            resolve_local_path("rel"),
            std::env::current_dir().unwrap().join("rel")
        );
    }

    fn picker_dir() -> (PathBuf, String) {
        let dir = std::env::temp_dir().join(format!("beams-form-picker-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Downloads/inner")).unwrap();
        std::fs::create_dir_all(dir.join("Documents")).unwrap();
        std::fs::write(dir.join("Downloads/report.csv"), "").unwrap();
        let base = format!("{}/", dir.display());
        (dir, base)
    }

    fn path_form(value: &str) -> Form {
        Form::new(
            "t",
            "go",
            vec![
                Field::text("local_path", "Path")
                    .value(value)
                    .complete(Completion::Paths),
                Field::text("other", "Other"),
            ],
        )
    }

    fn type_text(form: &mut Form, text: &str) {
        for ch in text.chars() {
            form.handle_key(&Key::Char(ch));
        }
    }

    #[test]
    fn dropdown_fuzzy_filters_and_tab_completes_the_best_match() {
        let (dir, base) = picker_dir();
        let mut form = path_form(&base);
        assert!(
            form.focused().unwrap().dropdown().is_some(),
            "lists the directory"
        );
        type_text(&mut form, "dl");
        let picker = form.focused().unwrap().dropdown().unwrap();
        assert_eq!(picker.items[0].label, "Downloads/");
        assert_eq!(form.handle_key(&Key::Tab), FormEvent::Handled);
        assert_eq!(form.text("local_path"), format!("{base}Downloads/"));
        assert_eq!(form.focus, 0, "completing doesn't move focus");

        // Arrow to an entry and take it with Enter, which doesn't submit.
        form.handle_key(&Key::Down);
        form.handle_key(&Key::Down);
        let picker = form.focused().unwrap().dropdown().unwrap();
        assert_eq!(picker.items[1].label, "report.csv");
        assert_eq!(form.handle_key(&Key::Enter), FormEvent::Handled);
        assert_eq!(
            form.text("local_path"),
            format!("{base}Downloads/report.csv")
        );
        assert!(
            form.focused().unwrap().dropdown().is_none(),
            "a file is final"
        );
        assert_eq!(form.handle_key(&Key::Enter), FormEvent::Submit);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn dropdown_tab_prefers_the_common_prefix_and_then_moves_on() {
        let (dir, base) = picker_dir();
        let mut form = path_form(&format!("{base}D"));
        // "Documents" and "Downloads" share only "Do".
        form.handle_key(&Key::Tab);
        assert_eq!(form.text("local_path"), format!("{base}Do"));
        type_text(&mut form, "c");
        form.handle_key(&Key::Tab);
        assert_eq!(form.text("local_path"), format!("{base}Documents/"));
        // Nothing left in Documents: the list closes and Tab moves on.
        assert!(form.focused().unwrap().dropdown().is_none());
        form.handle_key(&Key::Tab);
        assert_eq!(form.focus, 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn dropdown_esc_hides_and_arrows_return_to_form_navigation() {
        let (dir, base) = picker_dir();
        let mut form = path_form(&base);
        form.handle_key(&Key::Down);
        assert_eq!(form.focus, 0, "Down goes into the list");
        form.handle_key(&Key::Up);
        assert_eq!(form.focus, 0, "Up from the first row goes back to the text");
        assert_eq!(form.handle_key(&Key::Esc), FormEvent::Handled);
        assert!(form.focused().unwrap().dropdown().is_none());
        assert_eq!(form.handle_key(&Key::Down), FormEvent::Handled);
        assert_eq!(form.focus, 1, "hidden list: Down moves focus again");
        form.handle_key(&Key::Up);
        assert_eq!(form.handle_key(&Key::Esc), FormEvent::Cancel);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn empty_path_fields_offer_recent_directories() {
        let mut form = path_form("");
        assert!(form.focused().unwrap().dropdown().is_none());
        form.set_recents(vec!["~/Downloads/".to_string(), "/srv/".to_string()]);
        let rendered = form.render(60);
        let dropdown = rendered.dropdown.expect("recents shown");
        assert_eq!(dropdown.row, 0);
        assert_eq!(dropdown.wanted_rows(), 2);
        let lines = dropdown.lines(10);
        assert_eq!(lines.len(), 2);
        let first: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            first.contains("~/Downloads/") && first.contains("recent"),
            "{first}"
        );
        form.handle_key(&Key::Down);
        form.handle_key(&Key::Enter);
        assert_eq!(form.text("local_path"), "~/Downloads/");
    }
}
