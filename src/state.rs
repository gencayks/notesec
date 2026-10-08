//! UI state that should survive a restart but is not a setting: favorite
//! pages and recently opened pages, stored as `state.toml` in the graph
//! folder.
//!
//! ```toml
//! favorites = ["Projects", "Reading list"]
//! recent = ["2026-10-08", "Projects"]   # most recent first
//! ```
//!
//! This lives apart from `config.toml` on purpose: `recent` changes on every
//! page you open, and rewriting the user's hand-edited settings file that
//! often would be rude. Pages are identified by title; matching ignores case,
//! like page lookup does. The file follows `config.rs`: missing keys default,
//! a missing file gives an empty state, and an invalid file gives an empty
//! state after being copied to `state.toml.bak`.

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
        self.recent.truncate(MAX_RECENT);
        self
    }

    /// Write state to `<root>/state.toml` atomically.
    pub fn save(&self, root: &Path) -> std::io::Result<()> {
        let body = toml::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let text = format!("# notesec UI state (favorites, recent pages)\n{body}");
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
                "favorites = [\"A\", \"\", \"a\", \"  \", \"B\"]\nrecent = [{}]\n",
                recent.join(", ")
            ),
        )
        .unwrap();
        let state = UiState::load(&dir);
        assert_eq!(state.favorites, titles(&["A", "B"]));
        assert_eq!(state.recent.len(), MAX_RECENT);
        assert_eq!(state.recent[0], "P0");
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
