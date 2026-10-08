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

use crate::model::BlockKind;
use crate::search::fuzzy_score;
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

/// The "/" menu for changing the edited block's type.
///
/// It opens when "/" is typed into an empty block (one whose text, after any
/// type prefix such as `# `, is empty) with the cursor at the end. The text
/// typed after the "/" stays in the editor and filters the menu; `app.rs`
/// removes it again when a type is chosen or the menu is dismissed.
#[derive(Clone, Debug, PartialEq)]
pub struct SlashMenu {
    /// Byte offset of the "/" in the editor text. Everything before it is the
    /// block's text from before the menu opened.
    pub slash: usize,
    /// Highlighted entry, an index into `matches()`.
    pub selected: usize,
}

impl SlashMenu {
    /// If inserting `typed` over `range` in `editor` should open the menu,
    /// return it (positioned at the "/" about to be inserted). "/" anywhere
    /// else is ordinary text.
    pub fn open_for(editor: &EditorState, range: &Range<usize>, typed: &str) -> Option<Self> {
        let at_end = range.start == editor.text.len() && range.end == editor.text.len();
        let empty_block = BlockKind::parse(&editor.text).1.is_empty();
        (typed == "/" && at_end && empty_block && editor.marked.is_none()).then_some(SlashMenu {
            slash: range.start,
            selected: 0,
        })
    }

    /// The filter text typed after the "/", or `None` if the menu no longer
    /// applies (the "/" was deleted or the cursor moved before it).
    pub fn query<'a>(&self, editor: &'a EditorState) -> Option<&'a str> {
        if editor.text.get(self.slash..self.slash + 1) != Some("/") || editor.cursor <= self.slash {
            return None;
        }
        editor.text.get(self.slash + 1..)
    }

    /// Block kinds matching `query`, best first. An empty query lists every
    /// kind in menu order; ties keep menu order too.
    pub fn matches(query: &str) -> Vec<BlockKind> {
        let mut scored: Vec<(i32, BlockKind)> = BlockKind::ALL
            .iter()
            .filter_map(|&kind| fuzzy_score(query, kind.label()).map(|score| (score, kind)))
            .collect();
        // `sort_by` is stable, so equal scores stay in menu order.
        scored.sort_by(|a, b| b.0.cmp(&a.0));
        scored.into_iter().map(|(_, kind)| kind).collect()
    }

    /// Move the highlight by `delta`, clamped to the `count` visible entries.
    pub fn move_selection(&mut self, delta: isize, count: usize) {
        if count > 0 {
            self.selected = (self.selected as isize + delta).clamp(0, count as isize - 1) as usize;
        }
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

    fn type_slash(text: &str) -> Option<SlashMenu> {
        let e = EditorState::new(text);
        let end = e.text.len();
        SlashMenu::open_for(&e, &(end..end), "/")
    }

    #[test]
    fn slash_opens_only_in_an_empty_block() {
        assert_eq!(
            type_slash(""),
            Some(SlashMenu {
                slash: 0,
                selected: 0
            })
        );
        // An empty heading still counts as empty: the "/" goes after `# `.
        assert_eq!(type_slash("# ").map(|m| m.slash), Some(2));
        // Text in the block, or the cursor not at the end: a literal "/".
        assert_eq!(type_slash("a"), None);
        assert_eq!(type_slash("# a"), None);
        let mut e = EditorState::new("# ");
        e.cursor = 0;
        assert_eq!(SlashMenu::open_for(&e, &(0..0), "/"), None);
        // Other characters never open it.
        let e = EditorState::new("");
        assert_eq!(SlashMenu::open_for(&e, &(0..0), "x"), None);
    }

    #[test]
    fn slash_query_follows_the_editor() {
        let mut e = EditorState::new("");
        let menu = SlashMenu::open_for(&e, &(0..0), "/").unwrap();
        e.insert("/he");
        assert_eq!(menu.query(&e), Some("he"));
        e.backspace();
        e.backspace();
        assert_eq!(menu.query(&e), Some(""));
        e.backspace();
        assert_eq!(menu.query(&e), None, "the / itself was deleted");
    }

    #[test]
    fn slash_filter_narrows_and_ranks() {
        assert_eq!(SlashMenu::matches(""), BlockKind::ALL.to_vec());
        assert_eq!(
            SlashMenu::matches("head"),
            vec![
                BlockKind::Heading1,
                BlockKind::Heading2,
                BlockKind::Heading3
            ]
        );
        assert_eq!(SlashMenu::matches("h2"), vec![BlockKind::Heading2]);
        assert_eq!(SlashMenu::matches("QUO"), vec![BlockKind::Quote]);
        assert!(SlashMenu::matches("zzz").is_empty());
    }

    #[test]
    fn slash_selection_clamps() {
        let mut m = SlashMenu {
            slash: 0,
            selected: 0,
        };
        m.move_selection(-1, 5);
        assert_eq!(m.selected, 0);
        m.move_selection(3, 5);
        m.move_selection(3, 5);
        assert_eq!(m.selected, 4);
        m.move_selection(1, 0);
        assert_eq!(m.selected, 4, "no entries: unchanged");
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
