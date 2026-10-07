//! Text-editing state for a single block, independent of any UI code.
//!
//! Keeping this separate from GPUI makes it easy to unit-test. In MVP feature 2
//! a GPUI element will render this state and feed keyboard input into it.
//!
//! All offsets are **byte** offsets into `text` (Rust strings are UTF-8), and are
//! always kept on grapheme-cluster boundaries so the cursor never lands inside
//! a multi-byte character or an emoji sequence.

use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Debug, Default)]
pub struct EditorState {
    pub text: String,
    /// Cursor position in bytes.
    pub cursor: usize,
}

impl EditorState {
    pub fn new(text: &str) -> Self {
        EditorState {
            text: text.to_string(),
            cursor: text.len(),
        }
    }

    /// Insert `s` at the cursor and move the cursor after it.
    pub fn insert(&mut self, s: &str) {
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
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
        self.text.split_off(self.cursor)
    }

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
}
