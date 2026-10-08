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
//!
//! Selection is an `anchor` plus the cursor: the anchor is where the selection
//! started and the cursor is the end that moves (mouse drag, Shift+arrows).
//! Typing, pasting, Backspace and Delete replace or remove the selected text;
//! plain Left/Right collapse it to its start/end.

use crate::model::BlockKind;
use crate::search::fuzzy_score;
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Debug, Default)]
pub struct EditorState {
    pub text: String,
    /// Cursor position in bytes. With a selection, this is its moving end.
    pub cursor: usize,
    /// Fixed end of the selection, if any. `None`, or equal to `cursor`,
    /// means nothing is selected.
    pub anchor: Option<usize>,
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
            anchor: None,
            marked: None,
        }
    }

    // --- selection ------------------------------------------------------------

    /// The selected byte range, or `None` if nothing is selected.
    pub fn selection(&self) -> Option<Range<usize>> {
        let anchor = self.anchor.filter(|&a| a != self.cursor)?;
        Some(anchor.min(self.cursor)..anchor.max(self.cursor))
    }

    /// The selection, or the empty range at the cursor.
    pub fn selected_range(&self) -> Range<usize> {
        self.selection().unwrap_or(self.cursor..self.cursor)
    }

    /// True when the cursor is at the start of the selection (it was made
    /// leftwards), which the OS input API wants to know.
    pub fn selection_reversed(&self) -> bool {
        self.anchor.is_some_and(|a| self.cursor < a)
    }

    pub fn clear_selection(&mut self) {
        self.anchor = None;
    }

    /// Put the cursor at `offset` with nothing selected.
    pub fn set_cursor(&mut self, offset: usize) {
        self.cursor = self.snap(offset);
        self.anchor = None;
    }

    /// Move the cursor to `offset`, selecting from where the selection
    /// started (or from the old cursor if nothing was selected).
    pub fn select_to(&mut self, offset: usize) {
        self.anchor.get_or_insert(self.cursor);
        self.cursor = self.snap(offset);
    }

    pub fn select_left(&mut self) {
        self.select_to(self.prev_boundary(self.cursor));
    }

    pub fn select_right(&mut self) {
        self.select_to(self.next_boundary(self.cursor));
    }

    pub fn select_home(&mut self) {
        self.select_to(0);
    }

    pub fn select_end(&mut self) {
        self.select_to(self.text.len());
    }

    /// Keep the cursor and anchor inside the text (after undo swaps the
    /// text underneath them).
    pub fn clamp(&mut self) {
        self.cursor = self.snap(self.cursor);
        self.anchor = self.anchor.map(|a| self.snap(a));
    }

    /// Replace `range` with `new_text` (newlines become spaces) and put the
    /// cursor after the inserted text. Clears the selection.
    pub fn replace_range(&mut self, range: Range<usize>, new_text: &str) {
        let clean = new_text.replace(['\n', '\r'], " ");
        self.text.replace_range(range.clone(), &clean);
        self.cursor = range.start + clean.len();
        self.anchor = None;
        self.marked = None;
    }

    /// Insert `s` at the cursor, replacing the selection if there is one.
    pub fn insert(&mut self, s: &str) {
        self.replace_range(self.selected_range(), s);
    }

    /// Delete the selection if there is one. Returns true if it did.
    fn delete_selection(&mut self) -> bool {
        match self.selection() {
            Some(range) => {
                self.replace_range(range, "");
                true
            }
            None => false,
        }
    }

    /// Delete the selection, or else the grapheme before the cursor. Returns
    /// false if there was nothing to delete.
    pub fn backspace(&mut self) -> bool {
        if self.delete_selection() {
            return true;
        }
        let prev = self.prev_boundary(self.cursor);
        if prev == self.cursor {
            return false;
        }
        self.text.replace_range(prev..self.cursor, "");
        self.cursor = prev;
        true
    }

    /// Delete the selection, or else the grapheme after the cursor. Returns
    /// false if there was nothing to delete.
    pub fn delete(&mut self) -> bool {
        if self.delete_selection() {
            return true;
        }
        let next = self.next_boundary(self.cursor);
        if next == self.cursor {
            return false;
        }
        self.text.replace_range(self.cursor..next, "");
        true
    }

    /// Left: collapse a selection to its start, else move one grapheme.
    pub fn move_left(&mut self) {
        self.cursor = match self.selection() {
            Some(range) => range.start,
            None => self.prev_boundary(self.cursor),
        };
        self.anchor = None;
    }

    /// Right: collapse a selection to its end, else move one grapheme.
    pub fn move_right(&mut self) {
        self.cursor = match self.selection() {
            Some(range) => range.end,
            None => self.next_boundary(self.cursor),
        };
        self.anchor = None;
    }

    pub fn move_home(&mut self) {
        self.set_cursor(0);
    }

    pub fn move_end(&mut self) {
        self.set_cursor(self.text.len());
    }

    /// Split at the cursor: keep the left half here, return the right half.
    /// Used for Enter ("new block below" carries over the text after the cursor).
    /// A selection is deleted first, like typing over it would.
    pub fn split_off_at_cursor(&mut self) -> String {
        self.delete_selection();
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

    /// The nearest grapheme boundary at or before `offset` (clamped to the
    /// text), so positions from the mouse never split a character.
    fn snap(&self, offset: usize) -> usize {
        if offset >= self.text.len() {
            return self.text.len();
        }
        self.text
            .grapheme_indices(true)
            .map(|(i, _)| i)
            .take_while(|&i| i <= offset)
            .last()
            .unwrap_or(0)
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

/// The "/" menu for changing the edited block's type.
///
/// It opens when "/" is typed into an empty block (one whose text, after any
/// type prefix such as `# `, is empty) with the cursor at the end, or when
/// "/" is typed while text is selected. The "/" is *inserted* (at the end of
/// the selection, which is not replaced) and the text typed after it filters
/// the menu. That "/query" is provisional: `app.rs` keeps a snapshot from
/// before the "/" and rebuilds the block from it when the menu closes.
#[derive(Clone, Debug, PartialEq)]
pub struct SlashMenu {
    /// Byte offset of the "/" in the editor text.
    pub slash: usize,
    /// Highlighted entry, an index into `matches()`.
    pub selected: usize,
}

impl SlashMenu {
    /// If inserting `typed` over `range` in `editor` should open the menu,
    /// return it (positioned at the "/" about to be inserted). "/" anywhere
    /// else is ordinary text.
    pub fn open_for(editor: &EditorState, range: &Range<usize>, typed: &str) -> Option<Self> {
        if typed != "/" || editor.marked.is_some() || *range != editor.selected_range() {
            return None;
        }
        let slash = match editor.selection() {
            Some(selection) => selection.end,
            None => {
                let at_end = range.start == editor.text.len();
                let empty_block = BlockKind::parse(&editor.text).1.is_empty();
                (at_end && empty_block).then_some(range.start)?
            }
        };
        Some(SlashMenu { slash, selected: 0 })
    }

    /// The filter text typed after the "/" (up to the cursor), or `None` if
    /// the menu no longer applies (the "/" was deleted or the cursor moved
    /// before it).
    pub fn query<'a>(&self, editor: &'a EditorState) -> Option<&'a str> {
        if editor.text.get(self.slash..self.slash + 1) != Some("/") || editor.cursor <= self.slash {
            return None;
        }
        editor.text.get(self.slash + 1..editor.cursor)
    }

    /// The "/query" as typed, including the "/" (see `query`).
    pub fn typed<'a>(&self, editor: &'a EditorState) -> Option<&'a str> {
        self.query(editor)?;
        editor.text.get(self.slash..editor.cursor)
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
    fn shift_arrows_extend_and_shrink_the_selection() {
        let mut e = EditorState::new("hello");
        assert_eq!(e.selection(), None);
        e.select_left();
        e.select_left();
        assert_eq!(e.selection(), Some(3..5));
        assert!(e.selection_reversed());
        e.select_right();
        assert_eq!(e.selection(), Some(4..5));
        // Back to the anchor: nothing selected.
        e.select_right();
        assert_eq!(e.selection(), None);
        e.select_home();
        assert_eq!(e.selection(), Some(0..5));
        e.select_end();
        assert_eq!(e.selection(), None);
    }

    #[test]
    fn arrows_collapse_the_selection() {
        let mut e = EditorState::new("hello");
        e.select_to(1);
        e.move_right();
        assert_eq!((e.cursor, e.selection()), (5, None));
        e.select_to(1);
        e.move_left();
        assert_eq!((e.cursor, e.selection()), (1, None));
    }

    #[test]
    fn typing_and_deleting_replace_the_selection() {
        let mut e = EditorState::new("hello world");
        e.cursor = 6;
        e.select_to(11);
        e.insert("there");
        assert_eq!(e.text, "hello there");
        assert_eq!((e.cursor, e.selection()), (11, None));

        e.select_to(5);
        assert!(e.backspace());
        assert_eq!(e.text, "hello");
        e.set_cursor(0);
        e.select_to(2);
        assert!(e.delete());
        assert_eq!((e.text.as_str(), e.cursor), ("llo", 0));

        // Enter (split) drops the selection first.
        let mut e = EditorState::new("abcd");
        e.cursor = 1;
        e.select_to(3);
        assert_eq!(e.split_off_at_cursor(), "d");
        assert_eq!(e.text, "a");
    }

    #[test]
    fn selection_never_splits_a_grapheme() {
        let mut e = EditorState::new("a😀b"); // the emoji is bytes 1..5
        e.set_cursor(3);
        assert_eq!(e.cursor, 1, "snapped back to the emoji's start");
        e.select_to(99);
        assert_eq!(e.selection(), Some(1..6));
        e.text.truncate(1);
        e.clamp();
        assert_eq!((e.cursor, e.anchor), (1, Some(1)));
    }

    #[test]
    fn slash_with_a_selection_goes_after_it() {
        let mut e = EditorState::new("hello world");
        e.cursor = 6;
        e.select_to(11);
        let sel = e.selected_range();
        let menu = SlashMenu::open_for(&e, &sel, "/").unwrap();
        assert_eq!(menu.slash, 11);
        // Only when typed over the whole selection, not elsewhere.
        assert_eq!(SlashMenu::open_for(&e, &(0..0), "/"), None);
        e.clear_selection();
        e.insert("/he");
        assert_eq!(menu.query(&e), Some("he"));
        assert_eq!(menu.typed(&e), Some("/he"));
        // The query ends at the cursor, even with text after the "/".
        let mut e = EditorState::new("ab/qc");
        e.cursor = 4;
        let menu = SlashMenu {
            slash: 2,
            selected: 0,
        };
        assert_eq!(menu.query(&e), Some("q"));
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
