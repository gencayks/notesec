//! Text-editing state for the block being edited, independent of any UI code.
//!
//! Keeping this separate from GPUI makes it easy to unit-test. `app.rs` owns one
//! shared `EditorState` and loads whichever block you click into it.
//!
//! All offsets are **byte** offsets into `text` (Rust strings are UTF-8) and are
//! kept on grapheme-cluster boundaries, so the cursor never lands inside a
//! multi-byte character or an emoji sequence. The OS input-method API speaks
//! UTF-16, hence the `*_utf16` converters below.
//!
//! Blocks are single-line, so newlines are stripped from anything inserted.

use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Debug, Default)]
pub struct EditorState {
    pub text: String,
    /// Cursor position in bytes.
    pub cursor: usize,
    /// Range of in-progress IME composition text (e.g. while typing `é` via a
    /// dead key), underlined by the renderer. `None` when not composing.
    pub marked: Option<Range<usize>>,
}

impl EditorState {
    /// New editor containing `text`, cursor at the end.
    pub fn new(text: &str) -> Self {
        EditorState {
            text: text.to_string(),
            cursor: text.len(),
            marked: None,
        }
    }

    /// Replace `range` with `new_text` (newlines become spaces) and put the
    /// cursor after the inserted text.
    pub fn replace_range(&mut self, range: Range<usize>, new_text: &str) {
        let clean = new_text.replace(['\n', '\r'], " ");
        self.text.replace_range(range.clone(), &clean);
        self.cursor = range.start + clean.len();
        self.marked = None;
    }

    /// Insert `s` at the cursor.
    pub fn insert(&mut self, s: &str) {
        self.replace_range(self.cursor..self.cursor, s);
    }

    /// Delete the grapheme before the cursor. Returns false if at the start.
    pub fn backspace(&mut self) -> bool {
        let prev = self.prev_boundary(self.cursor);
        if prev == self.cursor {
            return false;
        }
        self.text.replace_range(prev..self.cursor, "");
        self.cursor = prev;
        true
    }

    /// Delete the grapheme after the cursor. Returns false if at the end.
    pub fn delete(&mut self) -> bool {
        let next = self.next_boundary(self.cursor);
        if next == self.cursor {
            return false;
        }
        self.text.replace_range(self.cursor..next, "");
        true
    }

    pub fn move_left(&mut self) {
        self.cursor = self.prev_boundary(self.cursor);
    }

    pub fn move_right(&mut self) {
        self.cursor = self.next_boundary(self.cursor);
    }

    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor = self.text.len();
    }

    /// Split at the cursor: keep the left half here, return the right half.
    /// Used for Enter ("new block below" carries over the text after the cursor).
    pub fn split_off_at_cursor(&mut self) -> String {
        let rest = self.text.split_off(self.cursor);
        self.marked = None;
        rest
    }

    // --- UTF-8 <-> UTF-16 offset conversion --------------------------------

    pub fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8 = 0;
        let mut utf16 = 0;
        for ch in self.text.chars() {
            if utf16 >= offset {
                break;
            }
            utf16 += ch.len_utf16();
            utf8 += ch.len_utf8();
        }
        utf8
    }

    pub fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16 = 0;
        let mut utf8 = 0;
        for ch in self.text.chars() {
            if utf8 >= offset {
                break;
            }
            utf8 += ch.len_utf8();
            utf16 += ch.len_utf16();
        }
        utf16
    }

    pub fn range_from_utf16(&self, r: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(r.start)..self.offset_from_utf16(r.end)
    }

    pub fn range_to_utf16(&self, r: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(r.start)..self.offset_to_utf16(r.end)
    }

    // --- grapheme boundaries ------------------------------------------------

    fn prev_boundary(&self, offset: usize) -> usize {
        self.text
            .grapheme_indices(true)
            .rev()
            .find_map(|(i, _)| (i < offset).then_some(i))
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.text
            .grapheme_indices(true)
            .find_map(|(i, _)| (i > offset).then_some(i))
            .unwrap_or(self.text.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_backspace_handle_multibyte() {
        let mut e = EditorState::new("");
        e.insert("héy");
        assert_eq!(e.text, "héy");
        e.backspace();
        e.backspace();
        assert_eq!(e.text, "h");
        assert!(e.backspace());
        assert!(!e.backspace());
    }

    #[test]
    fn split_for_enter() {
        let mut e = EditorState::new("hello world");
        e.cursor = 5;
        assert_eq!(e.split_off_at_cursor(), " world");
        assert_eq!(e.text, "hello");
    }

    #[test]
    fn newlines_are_flattened() {
        let mut e = EditorState::new("");
        e.insert("a\nb");
        assert_eq!(e.text, "a b");
    }

    #[test]
    fn utf16_round_trip() {
        let e = EditorState::new("a😀b"); // the emoji is 4 bytes, 2 UTF-16 units
        assert_eq!(e.offset_to_utf16(5), 3);
        assert_eq!(e.offset_from_utf16(3), 5);
    }

    #[test]
    fn delete_forward() {
        let mut e = EditorState::new("abc");
        e.cursor = 1;
        assert!(e.delete());
        assert_eq!(e.text, "ac");
        e.move_end();
        assert!(!e.delete());
    }
}
