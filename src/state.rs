//! UI state that should survive a restart but is not a setting: favorite
//! pages, recently opened pages and the sidebar's custom page order, stored
//! as `state.toml` in the graph folder.
//!
//! ```toml
//! favorites = ["Projects", "Reading list"]
//! recent = ["2026-10-08", "Projects"]   # most recent first
//! page_order = ["Projects", "Inbox"]    # absent: alphabetical
//!
//! [shortcuts]                           # absent: the default keys
//! SplitRight = "ctrl-alt-s"             # see hotkeys.rs (decision 41)
//! Quit = ""                             # unbound
//! ```
//!
//! This lives apart from `config.toml` on purpose: `recent` changes on every
//! page you open, and rewriting the user's hand-edited settings file that
//! often would be rude. Pages are identified by title; matching ignores case,
//! like page lookup does. The file follows `config.rs`: missing keys default,
//! a missing file gives an empty state, and an invalid file gives an empty
//! state after being copied to `state.toml.bak`.

use crate::hotkeys::Overrides;
use crate::storage::write_atomic;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// How many pages the RECENT list keeps.
pub const MAX_RECENT: usize = 10;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
// Any key missing from the file falls back to its default (an empty list).
#[serde(default)]
pub struct UiState {
    /// Favorite page titles, in the order they were starred.
    pub favorites: Vec<String>,
    /// Recently opened page titles, most recent first, at most `MAX_RECENT`.
    pub recent: Vec<String>,
    /// Custom order of the sidebar's regular (non-journal) pages, set by
    /// dragging them. Empty means alphabetical. Pages not listed come after
    /// the listed ones, alphabetically; entries without a page are ignored
    /// (see `app::sort_pages`).
    pub page_order: Vec<String>,
    /// Custom keys: command name -> keystroke (`""`: unbound). Read
    /// leniently by `hotkeys::effective_shortcuts`: unknown names and bad
    /// keys are ignored there (and kept here, so nothing is lost).
    #[serde(
        skip_serializing_if = "Overrides::is_empty",
        deserialize_with = "lenient_shortcuts"
    )]
    pub shortcuts: Overrides,
}

/// `[shortcuts]` read so a hand-editing slip there can't cost the rest of
/// the file: entries whose value isn't a string are dropped, and anything
/// but a table reads as no overrides.
fn lenient_shortcuts<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Overrides, D::Error> {
    let value = toml::Value::deserialize(deserializer)?;
    Ok(match value {
        toml::Value::Table(table) => table
            .into_iter()
            .filter_map(|(name, value)| value.as_str().map(|key| (name, key.to_string())))
            .collect(),
        _ => Overrides::new(),
    })
}

impl UiState {
    pub fn path(root: &Path) -> PathBuf {
        root.join("state.toml")
    }

    /// Load state from `<root>/state.toml`.
    ///
    /// A missing file gives an empty state. An unreadable or invalid file
    /// also does, but first the bad file is copied to `state.toml.bak` so the
    /// next save cannot silently destroy it.
    pub fn load(root: &Path) -> UiState {
        let path = Self::path(root);
        let Ok(text) = fs::read_to_string(&path) else {
            return UiState::default();
        };
        match toml::from_str::<UiState>(&text) {
            Ok(state) => state.sanitized(),
            Err(err) => {
                eprintln!("notesec: ignoring invalid {}: {err}", path.display());
                let backup = path.with_extension("toml.bak");
                if let Err(e) = fs::copy(&path, &backup) {
                    eprintln!("notesec: could not back up {}: {e}", path.display());
                }
                UiState::default()
            }
        }
    }

    /// Drop empty titles and case-insensitive duplicates (keeping the first),
    /// and cap `recent`, so a hand-edited file can't confuse the sidebar.
    fn sanitized(mut self) -> Self {
        dedupe(&mut self.favorites);
        dedupe(&mut self.recent);
        dedupe(&mut self.page_order);
        self.recent.truncate(MAX_RECENT);
        self
    }

    /// Write state to `<root>/state.toml` atomically.
    pub fn save(&self, root: &Path) -> std::io::Result<()> {
        let body = toml::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let text = format!(
            "# notesec UI state (favorites, recent pages, page order, custom keys)\n{body}"
        );
        write_atomic(&Self::path(root), &text)
    }

    pub fn is_favorite(&self, title: &str) -> bool {
        position(&self.favorites, title).is_some()
    }

    /// Star `title`, or unstar it if it already is a favorite (matching
    /// ignores case). Returns whether it is a favorite now.
    pub fn toggle_favorite(&mut self, title: &str) -> bool {
        match position(&self.favorites, title) {
            Some(i) => {
                self.favorites.remove(i);
                false
            }
            None => {
                self.favorites.push(title.to_string());
                true
            }
        }
    }

    /// A page was renamed: entries for `old` (ignoring case) now say `new`,
    /// in place (so a renamed page keeps its spot in `page_order`).
    /// Returns whether anything changed.
    pub fn rename(&mut self, old: &str, new: &str) -> bool {
        let mut changed = false;
        for list in [&mut self.favorites, &mut self.recent, &mut self.page_order] {
            if let Some(i) = position(list, old) {
                list[i] = new.to_string();
                changed = true;
            }
            // A stale entry for a deleted page may already have the new name.
            dedupe(list);
        }
        changed
    }

    /// A page was deleted: drop it from every list. Returns whether anything
    /// changed.
    pub fn forget(&mut self, title: &str) -> bool {
        let mut changed = false;
        for list in [&mut self.favorites, &mut self.recent, &mut self.page_order] {
            if let Some(i) = position(list, title) {
                list.remove(i);
                changed = true;
            }
        }
        changed
    }

    /// Note that `title` was just opened: move it to the front of `recent`
    /// (dropping any other spelling of it) and keep at most `MAX_RECENT`.
    /// Returns whether the list changed, so callers only save when needed.
    pub fn record_recent(&mut self, title: &str) -> bool {
        if title.trim().is_empty() || self.recent.first().is_some_and(|t| t == title) {
            return false;
        }
        if let Some(i) = position(&self.recent, title) {
            self.recent.remove(i);
        }
        self.recent.insert(0, title.to_string());
        self.recent.truncate(MAX_RECENT);
        true
    }
}

/// Index of `title` in `list`, ignoring case.
fn position(list: &[String], title: &str) -> Option<usize> {
    let wanted = title.to_lowercase();
    list.iter().position(|t| t.to_lowercase() == wanted)
}

/// Remove blank entries and case-insensitive repeats, keeping the first.
fn dedupe(list: &mut Vec<String>) {
    let mut seen = std::collections::HashSet::new();
    list.retain(|t| !t.trim().is_empty() && seen.insert(t.to_lowercase()));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("notesec-state-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn titles(list: &[&str]) -> Vec<String> {
        list.iter().map(|t| t.to_string()).collect()
    }

    #[test]
    fn missing_file_gives_defaults() {
        let dir = temp_dir("missing");
        assert_eq!(UiState::load(&dir), UiState::default());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn round_trip() {
        let dir = temp_dir("roundtrip");
        let state = UiState {
            favorites: titles(&["Projects", "Reading list"]),
            recent: titles(&["2026-10-08", "Projects"]),
            page_order: titles(&["Reading list", "Projects"]),
            shortcuts: [
                ("SplitRight", "ctrl-alt-s"),
                ("Quit", ""),
                ("FocusOtherPane", "ctrl-|"),
            ]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        };
        state.save(&dir).unwrap();
        assert_eq!(UiState::load(&dir), state);
        // No temp file left behind by the atomic write.
        assert!(!dir.join(".state.toml.tmp").exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn partial_files_fill_in_defaults() {
        let dir = temp_dir("partial");
        fs::write(UiState::path(&dir), "favorites = [\"A\"]\n").unwrap();
        let state = UiState::load(&dir);
        assert_eq!(state.favorites, titles(&["A"]));
        assert!(state.recent.is_empty());
        assert!(state.page_order.is_empty(), "no page_order: alphabetical");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn odd_shortcut_entries_cost_nothing_else() {
        let dir = temp_dir("shortcuts");
        fs::write(
            UiState::path(&dir),
            "favorites = [\"A\"]\n[shortcuts]\nUndo = 5\nRedo = \"ctrl-r\"\nNope = \"nonsense\"\n",
        )
        .unwrap();
        let state = UiState::load(&dir);
        assert_eq!(state.favorites, titles(&["A"]));
        // Non-strings are dropped; strings are kept for `hotkeys` to judge.
        assert_eq!(state.shortcuts.len(), 2);
        assert_eq!(state.shortcuts["Redo"], "ctrl-r");
        assert_eq!(state.shortcuts["Nope"], "nonsense");
        fs::write(UiState::path(&dir), "shortcuts = 3\nrecent = [\"B\"]\n").unwrap();
        let state = UiState::load(&dir);
        assert!(state.shortcuts.is_empty());
        assert_eq!(state.recent, titles(&["B"]));
        // Without overrides the table isn't written at all.
        UiState::default().save(&dir).unwrap();
        let text = fs::read_to_string(UiState::path(&dir)).unwrap();
        assert!(!text.contains("[shortcuts]"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn invalid_file_falls_back_and_is_backed_up() {
        let dir = temp_dir("invalid");
        fs::write(UiState::path(&dir), "favorites = \"not a list\"\n").unwrap();
        assert_eq!(UiState::load(&dir), UiState::default());
        let backup = fs::read_to_string(dir.join("state.toml.bak")).unwrap();
        assert_eq!(backup, "favorites = \"not a list\"\n");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn hand_edited_files_are_sanitized_on_load() {
        let dir = temp_dir("sanitize");
        let recent: Vec<String> = (0..15).map(|i| format!("\"P{i}\"")).collect();
        fs::write(
            UiState::path(&dir),
            format!(
                "favorites = [\"A\", \"\", \"a\", \"  \", \"B\"]\nrecent = [{}]\npage_order = [\"X\", \"x\", \"Y\"]\n",
                recent.join(", ")
            ),
        )
        .unwrap();
        let state = UiState::load(&dir);
        assert_eq!(state.favorites, titles(&["A", "B"]));
        assert_eq!(state.recent.len(), MAX_RECENT);
        assert_eq!(state.recent[0], "P0");
        assert_eq!(state.page_order, titles(&["X", "Y"]));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn recent_is_most_recent_first_deduped_and_capped() {
        let mut state = UiState::default();
        assert!(state.record_recent("A"));
        assert!(state.record_recent("B"));
        assert_eq!(state.recent, titles(&["B", "A"]));
        // Opening the page that is already first changes nothing.
        assert!(!state.record_recent("B"));
        // Reopening moves it to the front; another spelling replaces it.
        assert!(state.record_recent("a"));
        assert_eq!(state.recent, titles(&["a", "B"]));
        assert!(!state.record_recent(""));
        for i in 0..20 {
            state.record_recent(&format!("P{i}"));
        }
        assert_eq!(state.recent.len(), MAX_RECENT);
        assert_eq!(state.recent[0], "P19");
        assert_eq!(state.recent[MAX_RECENT - 1], "P10");
    }

    #[test]
    fn rename_and_forget_follow_pages() {
        let mut state = UiState {
            favorites: titles(&["Old", "B"]),
            recent: titles(&["B", "old", "New"]),
            page_order: titles(&["C", "Old", "B"]),
            ..UiState::default()
        };
        assert!(state.rename("OLD", "New"));
        assert_eq!(state.favorites, titles(&["New", "B"]));
        // The stale "New" entry merges with the renamed one.
        assert_eq!(state.recent, titles(&["B", "New"]));
        // Renamed in place: the page keeps its position.
        assert_eq!(state.page_order, titles(&["C", "New", "B"]));
        assert!(!state.rename("Missing", "X"));
        assert!(state.forget("new"));
        assert_eq!(state.favorites, titles(&["B"]));
        assert_eq!(state.recent, titles(&["B"]));
        assert_eq!(state.page_order, titles(&["C", "B"]));
        assert!(!state.forget("new"));
    }

    #[test]
    fn favorites_toggle_ignoring_case() {
        let mut state = UiState::default();
        assert!(!state.is_favorite("Projects"));
        assert!(state.toggle_favorite("Projects"));
        assert!(state.toggle_favorite("Ideas"));
        assert!(state.is_favorite("projects"));
        assert_eq!(state.favorites, titles(&["Projects", "Ideas"]));
        assert!(!state.toggle_favorite("PROJECTS"));
        assert!(!state.is_favorite("Projects"));
        assert_eq!(state.favorites, titles(&["Ideas"]));
    }
}
