// The cursor is always a valid byte position within `lines[cursor.0]` and the
// wrap function returns byte ranges into the same string it was given. Both
// invariants are maintained by every method that mutates state, so indexing
// and slicing here are bounds-safe by construction.
#![allow(clippy::indexing_slicing)]

//! Single-line/multi-line text editor widget for the note dialog.
//!
//! Model: `Vec<String>` of logical lines plus a `(row, byte_col)` cursor.
//! View: visual rows produced by a UAX #14 line-break pass over each logical
//! line, measured in terminal cells via `unicode-width`. The scroll offset is
//! stored in visual rows, so the scrollbar and Up/Down motion track what the
//! user actually sees rather than logical-line indices.

use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::Style;
use std::ops::Range;
use unicode_linebreak::{BreakClass, BreakOpportunity, break_property, linebreaks};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub struct TextArea {
    lines: Vec<String>,
    /// (row, byte offset within that line).
    cursor: (usize, usize),
    /// First visible visual row.
    scroll: u16,
    /// Width passed to the most recent `render`. Used by `input` so the
    /// caller doesn't have to thread the dialog width through key handling.
    last_width: u16,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    /// Kind of the most recent mutation. Successive edits of the same kind
    /// coalesce into one undo step (typing a word is one undo, not N).
    last_edit: EditKind,
}

#[derive(Clone)]
struct Snapshot {
    lines: Vec<String>,
    cursor: (usize, usize),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EditKind {
    None,
    Insert,
    Backspace,
    Delete,
    /// Any structural edit that should not coalesce with the next one
    /// (newline, kill-line, word delete).
    Structural,
}

impl TextArea {
    pub fn new(text: &str) -> Self {
        let lines: Vec<String> = text.split('\n').map(|s| s.to_string()).collect();
        let last = lines.len().saturating_sub(1);
        let col = lines[last].len();
        Self {
            lines,
            cursor: (last, col),
            scroll: 0,
            last_width: 0,
            undo: Vec::new(),
            redo: Vec::new(),
            last_edit: EditKind::None,
        }
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            lines: self.lines.clone(),
            cursor: self.cursor,
        }
    }

    fn restore(&mut self, snap: Snapshot) {
        self.lines = snap.lines;
        self.cursor = snap.cursor;
    }

    /// Pushes the current state onto the undo stack and clears redo. Coalesces
    /// with the previous push when both edits are the same kind, so a run of
    /// character inserts collapses to one undo step.
    fn checkpoint(&mut self, kind: EditKind) {
        let coalesce = kind != EditKind::Structural && kind == self.last_edit;
        if !coalesce {
            self.undo.push(self.snapshot());
        }
        self.redo.clear();
        self.last_edit = kind;
    }

    fn undo(&mut self) {
        if let Some(prev) = self.undo.pop() {
            let current = self.snapshot();
            self.restore(prev);
            self.redo.push(current);
            self.last_edit = EditKind::Structural;
        }
    }

    fn redo(&mut self) {
        if let Some(next) = self.redo.pop() {
            let current = self.snapshot();
            self.restore(next);
            self.undo.push(current);
            self.last_edit = EditKind::Structural;
        }
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    /// Visual-row count when wrapped at `width`. Includes one row for every
    /// empty logical line.
    pub fn visual_row_count(&self, width: u16) -> usize {
        self.lines
            .iter()
            .map(|l| wrap_line(l, width).len().max(1))
            .sum()
    }

    /// Visual row of the cursor when wrapped at `width`.
    pub fn visual_cursor_row(&self, width: u16) -> usize {
        let (row, col) = self.cursor;
        let prior: usize = self.lines[..row]
            .iter()
            .map(|l| wrap_line(l, width).len().max(1))
            .sum();
        prior + sub_row_for_col(&self.lines[row], col, width)
    }

    /// Dispatches a key event. Returns false for keys the widget does not
    /// consume so the caller can layer its own bindings (Enter, Esc).
    pub fn input(&mut self, key: KeyEvent) -> bool {
        let width = self.last_width.max(1);
        let m = key.modifiers;
        let ctrl = m.contains(KeyModifiers::CONTROL);
        let alt = m.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char(c) if !ctrl && !alt => self.insert_char(c),
            KeyCode::Char('z') if ctrl => self.undo(),
            KeyCode::Char('y') if ctrl => self.redo(),
            KeyCode::Char('w') if ctrl => self.delete_word_left(),
            KeyCode::Char('u') if ctrl => self.kill_to_bol(),
            KeyCode::Char('k') if ctrl => self.kill_to_eol(),
            KeyCode::Char('a') if ctrl => self.move_bol(),
            KeyCode::Char('e') if ctrl => self.move_eol(),
            KeyCode::Char('b') if alt => self.move_word_left(),
            KeyCode::Char('f') if alt => self.move_word_right(),
            KeyCode::Char('d') if alt => self.delete_word_right(),
            KeyCode::Backspace if alt || ctrl => self.delete_word_left(),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete if alt || ctrl => self.delete_word_right(),
            KeyCode::Delete => self.delete_forward(),
            KeyCode::Left if alt || ctrl => self.move_word_left(),
            KeyCode::Right if alt || ctrl => self.move_word_right(),
            KeyCode::Left => self.move_left(),
            KeyCode::Right => self.move_right(),
            KeyCode::Up => self.move_up(width),
            KeyCode::Down => self.move_down(width),
            KeyCode::Home => self.move_bol(),
            KeyCode::End => self.move_eol(),
            _ => return false,
        }
        true
    }

    pub fn insert_newline(&mut self) {
        self.checkpoint(EditKind::Structural);
        let (r, c) = self.cursor;
        let tail = self.lines[r].split_off(c);
        self.lines.insert(r + 1, tail);
        self.cursor = (r + 1, 0);
    }

    fn insert_char(&mut self, ch: char) {
        self.checkpoint(EditKind::Insert);
        let (r, c) = self.cursor;
        self.lines[r].insert(c, ch);
        self.cursor.1 = c + ch.len_utf8();
    }

    fn backspace(&mut self) {
        self.checkpoint(EditKind::Backspace);
        let (r, c) = self.cursor;
        if c > 0 {
            let prev = prev_grapheme_boundary(&self.lines[r], c);
            self.lines[r].replace_range(prev..c, "");
            self.cursor.1 = prev;
        } else if r > 0 {
            let prev_line = self.lines.remove(r);
            let join_at = self.lines[r - 1].len();
            self.lines[r - 1].push_str(&prev_line);
            self.cursor = (r - 1, join_at);
        }
    }

    fn delete_forward(&mut self) {
        self.checkpoint(EditKind::Delete);
        let (r, c) = self.cursor;
        if c < self.lines[r].len() {
            let next = next_grapheme_boundary(&self.lines[r], c);
            self.lines[r].replace_range(c..next, "");
        } else if r + 1 < self.lines.len() {
            let next_line = self.lines.remove(r + 1);
            self.lines[r].push_str(&next_line);
        }
    }

    fn delete_word_left(&mut self) {
        let (r, c) = self.cursor;
        if c == 0 {
            self.backspace();
            return;
        }
        self.checkpoint(EditKind::Structural);
        let target = word_left(&self.lines[r], c);
        self.lines[r].replace_range(target..c, "");
        self.cursor.1 = target;
    }

    fn delete_word_right(&mut self) {
        let (r, c) = self.cursor;
        if c == self.lines[r].len() {
            self.delete_forward();
            return;
        }
        self.checkpoint(EditKind::Structural);
        let target = word_right(&self.lines[r], c);
        self.lines[r].replace_range(c..target, "");
    }

    fn kill_to_eol(&mut self) {
        self.checkpoint(EditKind::Structural);
        let (r, c) = self.cursor;
        if c < self.lines[r].len() {
            self.lines[r].truncate(c);
        } else if r + 1 < self.lines.len() {
            let next_line = self.lines.remove(r + 1);
            self.lines[r].push_str(&next_line);
        }
    }

    fn kill_to_bol(&mut self) {
        self.checkpoint(EditKind::Structural);
        let (r, c) = self.cursor;
        self.lines[r].replace_range(0..c, "");
        self.cursor.1 = 0;
    }

    fn move_left(&mut self) {
        self.last_edit = EditKind::None;
        let (r, c) = self.cursor;
        if c > 0 {
            self.cursor.1 = prev_grapheme_boundary(&self.lines[r], c);
        } else if r > 0 {
            self.cursor = (r - 1, self.lines[r - 1].len());
        }
    }

    fn move_right(&mut self) {
        self.last_edit = EditKind::None;
        let (r, c) = self.cursor;
        if c < self.lines[r].len() {
            self.cursor.1 = next_grapheme_boundary(&self.lines[r], c);
        } else if r + 1 < self.lines.len() {
            self.cursor = (r + 1, 0);
        }
    }

    fn move_word_left(&mut self) {
        self.last_edit = EditKind::None;
        let (r, c) = self.cursor;
        if c > 0 {
            self.cursor.1 = word_left(&self.lines[r], c);
        } else if r > 0 {
            self.cursor = (r - 1, self.lines[r - 1].len());
        }
    }

    fn move_word_right(&mut self) {
        self.last_edit = EditKind::None;
        let (r, c) = self.cursor;
        if c < self.lines[r].len() {
            self.cursor.1 = word_right(&self.lines[r], c);
        } else if r + 1 < self.lines.len() {
            self.cursor = (r + 1, 0);
        }
    }

    fn move_bol(&mut self) {
        self.last_edit = EditKind::None;
        self.cursor.1 = 0;
    }

    fn move_eol(&mut self) {
        self.last_edit = EditKind::None;
        let r = self.cursor.0;
        self.cursor.1 = self.lines[r].len();
    }

    fn move_up(&mut self, width: u16) {
        self.last_edit = EditKind::None;
        let target_col = self.visual_col(width);
        let (r, _) = self.cursor;
        let sub = sub_row_for_col(&self.lines[r], self.cursor.1, width);
        if sub > 0 {
            let rows = wrap_line(&self.lines[r], width);
            self.cursor.1 = byte_at_visual_col(&self.lines[r], &rows[sub - 1], target_col);
        } else if r > 0 {
            let prev = &self.lines[r - 1];
            let rows = wrap_line(prev, width);
            let last = rows.last().cloned().unwrap_or(0..prev.len());
            self.cursor = (r - 1, byte_at_visual_col(prev, &last, target_col));
        }
    }

    fn move_down(&mut self, width: u16) {
        self.last_edit = EditKind::None;
        let target_col = self.visual_col(width);
        let (r, _) = self.cursor;
        let rows = wrap_line(&self.lines[r], width);
        let sub = sub_row_for_col(&self.lines[r], self.cursor.1, width);
        if sub + 1 < rows.len() {
            self.cursor.1 = byte_at_visual_col(&self.lines[r], &rows[sub + 1], target_col);
        } else if r + 1 < self.lines.len() {
            let next = &self.lines[r + 1];
            let nrows = wrap_line(next, width);
            let first = nrows.first().cloned().unwrap_or(0..next.len());
            self.cursor = (r + 1, byte_at_visual_col(next, &first, target_col));
        }
    }

    fn visual_col(&self, width: u16) -> usize {
        let (r, c) = self.cursor;
        let rows = wrap_line(&self.lines[r], width);
        let sub = sub_row_for_col(&self.lines[r], c, width);
        let row_start = rows.get(sub).map(|r| r.start).unwrap_or(0);
        UnicodeWidthStr::width(&self.lines[r][row_start..c])
    }

    /// Renders the editor into `area`. Adjusts scroll so the cursor is
    /// visible. Returns the terminal cursor position so the caller can place
    /// the hardware cursor with `frame.set_cursor_position`.
    pub fn render(&mut self, area: Rect, buf: &mut Buffer, style: Style) -> Option<(u16, u16)> {
        if area.width == 0 || area.height == 0 {
            return None;
        }
        let width = area.width;
        self.last_width = width;
        let cursor_visual = self.visual_cursor_row(width) as u16;
        if cursor_visual < self.scroll {
            self.scroll = cursor_visual;
        } else if cursor_visual >= self.scroll + area.height {
            self.scroll = cursor_visual + 1 - area.height;
        }

        let mut visual_row: u16 = 0;
        let mut cursor_pos: Option<(u16, u16)> = None;
        for (li, line) in self.lines.iter().enumerate() {
            let rows = wrap_line(line, width);
            let row_count = rows.len().max(1);
            for sub in 0..row_count {
                if visual_row >= self.scroll && visual_row < self.scroll + area.height {
                    let y = area.y + (visual_row - self.scroll);
                    let slice = rows.get(sub).map(|r| &line[r.clone()]).unwrap_or("");
                    buf.set_stringn(area.x, y, slice, width as usize, style);
                    if li == self.cursor.0 && sub == sub_row_for_col(line, self.cursor.1, width) {
                        let row_start = rows.get(sub).map(|r| r.start).unwrap_or(0);
                        let col = UnicodeWidthStr::width(&line[row_start..self.cursor.1]) as u16;
                        cursor_pos = Some((area.x + col.min(width.saturating_sub(1)), y));
                    }
                }
                visual_row += 1;
            }
        }
        cursor_pos
    }
}

/// Wraps `line` at `width` cells. Returns byte ranges into `line`, one per
/// visual row. Empty input returns an empty vec — the caller treats this as
/// one blank row.
fn wrap_line(line: &str, width: u16) -> Vec<Range<usize>> {
    if line.is_empty() || width == 0 {
        return Vec::new();
    }
    let width = width as usize;
    let opps: Vec<(usize, BreakOpportunity)> = linebreaks(line).collect();
    let mut rows: Vec<Range<usize>> = Vec::new();
    let mut row_start = 0usize;
    let mut last_break: Option<usize> = None;
    let mut cell_used = 0usize;

    for (idx, g) in line.grapheme_indices(true) {
        let g_end = idx + g.len();
        let g_w = UnicodeWidthStr::width(g);

        // A break opportunity at byte offset `idx` (the start of this
        // grapheme) means "you may break before this grapheme."
        if idx > row_start
            && opps
                .iter()
                .any(|(o, _)| *o == idx && !leading_punct(line, idx))
        {
            last_break = Some(idx);
        }

        if cell_used + g_w > width && cell_used > 0 {
            let end = last_break.unwrap_or(idx);
            rows.push(row_start..end);
            // Drop leading whitespace on the new row.
            let mut new_start = end;
            while new_start < line.len() {
                let rest = &line[new_start..];
                let next = rest.grapheme_indices(true).next().map(|(_, g)| g);
                match next {
                    Some(g) if g == " " || g == "\t" => new_start += g.len(),
                    _ => break,
                }
            }
            row_start = new_start;
            last_break = None;
            // Recompute cell width from the new row start up to the current
            // grapheme; the current grapheme still needs to be placed.
            cell_used = UnicodeWidthStr::width(&line[row_start..idx]);
            if cell_used + g_w > width && cell_used > 0 {
                // Edge case: the new row is still too narrow after the break.
                // Hard-break before the current grapheme.
                rows.push(row_start..idx);
                row_start = idx;
                cell_used = 0;
            }
        }
        cell_used += g_w;
        let _ = g_end;
    }
    if row_start < line.len() {
        rows.push(row_start..line.len());
    }
    rows
}

/// True when the character at `idx` is in a line-break class that must not
/// start a new line (`.`, `,`, `?`, `!`, `;`, `:`, closing brackets, etc.).
/// UAX #14 forbids breaking before these.
fn leading_punct(line: &str, idx: usize) -> bool {
    let c = match line[idx..].chars().next() {
        Some(c) => c,
        None => return false,
    };
    matches!(
        break_property(c as u32),
        BreakClass::Exclamation
            | BreakClass::ClosePunctuation
            | BreakClass::CloseParenthesis
            | BreakClass::InfixSeparator
            | BreakClass::Postfix
            | BreakClass::Quotation
    )
}

fn sub_row_for_col(line: &str, byte: usize, width: u16) -> usize {
    let rows = wrap_line(line, width);
    for (i, r) in rows.iter().enumerate() {
        if byte >= r.start && byte <= r.end {
            // A break joins one row's `end` with the next row's `start`;
            // when the cursor sits exactly at a break, prefer the later row.
            if byte == r.end && i + 1 < rows.len() && rows[i + 1].start == byte {
                return i + 1;
            }
            return i;
        }
    }
    rows.len().saturating_sub(1)
}

fn byte_at_visual_col(line: &str, row: &Range<usize>, target_cells: usize) -> usize {
    let mut cells = 0usize;
    for (idx, g) in line[row.clone()].grapheme_indices(true) {
        let w = UnicodeWidthStr::width(g);
        if cells + w > target_cells {
            return row.start + idx;
        }
        cells += w;
    }
    row.end
}

fn prev_grapheme_boundary(s: &str, byte: usize) -> usize {
    s[..byte]
        .grapheme_indices(true)
        .next_back()
        .map(|(i, _)| i)
        .unwrap_or(0)
}

fn next_grapheme_boundary(s: &str, byte: usize) -> usize {
    s[byte..]
        .grapheme_indices(true)
        .nth(1)
        .map(|(i, _)| byte + i)
        .unwrap_or(s.len())
}

/// Position of the start of the word at or before `byte`. Skips trailing
/// whitespace then skips back over non-whitespace.
fn word_left(line: &str, byte: usize) -> usize {
    let bytes = line.as_bytes();
    let mut i = byte;
    while i > 0 && is_word_ws(bytes[i - 1]) {
        i -= 1;
    }
    while i > 0 && !is_word_ws(bytes[i - 1]) {
        i -= 1;
    }
    i
}

/// Position of the end of the word at or after `byte`.
fn word_right(line: &str, byte: usize) -> usize {
    let bytes = line.as_bytes();
    let mut i = byte;
    let len = bytes.len();
    while i < len && is_word_ws(bytes[i]) {
        i += 1;
    }
    while i < len && !is_word_ws(bytes[i]) {
        i += 1;
    }
    i
}

fn is_word_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_does_not_orphan_question_mark() {
        let line = "Is it really a high number? On what basis does the model make this judgement? This is a";
        let rows = wrap_line(line, 60);
        for r in &rows {
            let slice = &line[r.clone()];
            assert!(
                !slice.starts_with('?') && !slice.starts_with('!') && !slice.starts_with(','),
                "row starts with trailing punctuation: {slice:?}",
            );
        }
    }

    #[test]
    fn wrap_breaks_on_whitespace() {
        let rows = wrap_line("hello world foo", 8);
        let slices: Vec<&str> = rows
            .iter()
            .map(|r| "hello world foo"[r.clone()].trim_end())
            .collect();
        assert_eq!(slices, vec!["hello", "world", "foo"]);
    }

    #[test]
    fn insert_then_text_roundtrip() {
        let mut ta = TextArea::new("");
        for c in "hello".chars() {
            ta.insert_char(c);
        }
        ta.insert_newline();
        for c in "world".chars() {
            ta.insert_char(c);
        }
        assert_eq!(ta.text(), "hello\nworld");
    }

    #[test]
    fn backspace_joins_lines() {
        let mut ta = TextArea::new("a\nb");
        ta.cursor = (1, 0);
        ta.backspace();
        assert_eq!(ta.text(), "ab");
        assert_eq!(ta.cursor, (0, 1));
    }

    #[test]
    fn delete_word_left_removes_word_and_spaces() {
        let mut ta = TextArea::new("hello world  ");
        ta.move_eol();
        ta.delete_word_left();
        assert_eq!(ta.text(), "hello ");
    }

    #[test]
    fn undo_coalesces_inserts_then_redo() {
        let mut ta = TextArea::new("");
        for c in "hello".chars() {
            ta.insert_char(c);
        }
        ta.undo();
        assert_eq!(ta.text(), "");
        ta.redo();
        assert_eq!(ta.text(), "hello");
    }

    #[test]
    fn cursor_motion_breaks_undo_coalescing() {
        let mut ta = TextArea::new("ab");
        ta.move_eol();
        ta.insert_char('c');
        ta.move_left();
        ta.insert_char('X');
        assert_eq!(ta.text(), "abXc");
        ta.undo();
        assert_eq!(ta.text(), "abc");
        ta.undo();
        assert_eq!(ta.text(), "ab");
    }
}
