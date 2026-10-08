//! Everything the Ctrl-K palette can run, in one table, plus how key
//! bindings are written for people ("Ctrl+Shift+T").
//!
//! Each row names a GPUI action (declared in `app.rs`) that the command
//! runs: the palette dispatches the action, exactly as its key binding
//! would, so a command and its shortcut can never do different things.
//! The shortcut hint shown next to a command is looked up in the keymap
//! by that same action (`binding_hint`), so it can't drift from the real
//! binding either.
//!
//! Adding a command is one row in `commands!` below (plus the action and
//! its handler in `app.rs`, which a key binding needs anyway).

use crate::app;
use gpui::{Action, KeyBinding, KeybindingKeystroke, Keymap};

/// What a command needs before it makes sense to offer it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Needs {
    /// Nothing: works from any tab, even with no tabs open.
    Nothing,
    /// A current page: a page tab is showing (not the graph, the agenda
    /// or the empty state). The palette hides it otherwise.
    Page,
    /// A block being edited: offered only when the palette was opened
    /// while editing one. Running it goes back to that block (cursor and
    /// selection as they were) and then dispatches the action, as its
    /// "BlockEditor" key binding would.
    Editing,
}

/// Declares `Command` and its table. One row per command:
/// `Name => "Label", [keywords], Needs;` where `Name` is both the enum
/// variant and the action in `app.rs` that the command dispatches.
macro_rules! commands {
    ($($name:ident => $label:literal, [$($keyword:literal),* $(,)?], $needs:ident;)*) => {
        /// A palette command (see the module docs).
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum Command {
            $($name,)*
        }

        impl Command {
            /// Every command, in palette order (the order with an empty
            /// query).
            pub const ALL: &'static [Command] = &[$(Command::$name,)*];

            pub fn label(self) -> &'static str {
                match self {
                    $(Command::$name => $label,)*
                }
            }

            /// Other words that find the command (ranked below label
            /// matches, see `search::KEYWORD_PENALTY`).
            pub fn keywords(self) -> &'static [&'static str] {
                match self {
                    $(Command::$name => &[$($keyword),*],)*
                }
            }

            pub fn needs(self) -> Needs {
                match self {
                    $(Command::$name => Needs::$needs,)*
                }
            }

            /// The action the command dispatches.
            pub fn action(self) -> Box<dyn Action> {
                match self {
                    $(Command::$name => Box::new(app::$name),)*
                }
            }

            /// A stable name for debug selectors (`command-NewPage`).
            pub fn name(self) -> &'static str {
                match self {
                    $(Command::$name => stringify!($name),)*
                }
            }
        }
    };
}

commands! {
    // Pages and navigation
    NewPage => "New page", ["create", "add page"], Nothing;
    OpenToday => "Open today's journal", ["today", "daily", "journal"], Nothing;
    OpenAgenda => "Open agenda", ["tasks", "todo", "scheduled", "deadline"], Nothing;
    RenamePage => "Rename current page", ["title", "name"], Page;
    DeletePage => "Delete current page", ["remove", "trash"], Page;
    CopyPageTitle => "Copy page title", ["clipboard", "name"], Page;
    ToggleFavorite => "Toggle favorite", ["star", "unstar", "bookmark", "favourite"], Page;
    SortPagesAz => "Sort pages A-Z", ["alphabetical", "order"], Nothing;
    // Editing
    Undo => "Undo", [], Nothing;
    Redo => "Redo", [], Nothing;
    InsertTemplate => "Insert template", ["snippet"], Page;
    CycleTask => "Cycle task state", ["todo", "doing", "done", "checkbox"], Editing;
    CollapseAll => "Collapse all", ["fold all", "outline"], Page;
    ExpandAll => "Expand all", ["unfold all", "outline"], Page;
    // Graph
    ToggleGraph => "Toggle graph view", ["graph", "network"], Nothing;
    ToggleLocalGraph => "Toggle local graph", ["global graph", "neighbours"], Nothing;
    FitGraph => "Fit graph", ["zoom to fit", "center graph"], Nothing;
    ToggleGraphJournals => "Toggle journals in graph", ["daily notes", "hide journals"], Nothing;
    // View
    ToggleTheme => "Switch theme", ["toggle dark/light theme", "dark mode", "light mode"], Nothing;
    IncreaseFont => "Increase font size", ["zoom in", "bigger text"], Nothing;
    DecreaseFont => "Decrease font size", ["zoom out", "smaller text"], Nothing;
    ResetFont => "Reset font size", ["default font size"], Nothing;
    // Tabs
    CloseTab => "Close tab", [], Nothing;
    NextTab => "Next tab", [], Nothing;
    PrevTab => "Previous tab", [], Nothing;
    // App
    OpenSettings => "Open settings", ["preferences", "theme", "font"], Nothing;
    ShowShortcuts => "Keyboard shortcuts", ["keys", "keymap", "cheatsheet", "help"], Nothing;
    Quit => "Quit", ["exit", "close app"], Nothing;
}

/// A binding's keys as people write them: `Ctrl+Shift+T`, `Ctrl+=`,
/// `Shift+Left`. A multi-key binding is its strokes separated by spaces.
pub fn format_keystrokes(keystrokes: &[KeybindingKeystroke]) -> String {
    keystrokes
        .iter()
        .map(format_keystroke)
        .collect::<Vec<_>>()
        .join(" ")
}

fn format_keystroke(keystroke: &KeybindingKeystroke) -> String {
    let m = keystroke.modifiers();
    let mut parts: Vec<String> = Vec::new();
    if m.control {
        parts.push("Ctrl".into());
    }
    if m.alt {
        parts.push("Alt".into());
    }
    if m.platform {
        parts.push("Super".into());
    }
    if m.shift {
        parts.push("Shift".into());
    }
    if m.function {
        parts.push("Fn".into());
    }
    let key = keystroke.key();
    parts.push(match key {
        "escape" => "Esc".to_string(),
        key if key.chars().count() == 1 => key.to_uppercase(),
        key => {
            // "enter" -> "Enter", "pageup" stays one word.
            let mut chars = key.chars();
            chars.next().map_or(String::new(), |first| {
                first.to_uppercase().chain(chars).collect()
            })
        }
    });
    parts.join("+")
}

/// The binding shown next to a command for `action`: the first one with no
/// key context (works anywhere), else the first one at all (an editor
/// command's "BlockEditor" binding). First registered wins, so `shortcuts`
/// lists an action's main binding first.
pub fn main_binding<'a>(keymap: &'a Keymap, action: &'a dyn Action) -> Option<&'a KeyBinding> {
    keymap
        .bindings_for_action(action)
        .find(|binding| binding.predicate().is_none())
        .or_else(|| keymap.bindings_for_action(action).next())
}

/// The palette's shortcut hint for `action`, read from the keymap.
pub fn binding_hint(keymap: &Keymap, action: &dyn Action) -> Option<String> {
    main_binding(keymap, action).map(|b| format_keystrokes(b.keystrokes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(source: &str) -> String {
        let binding = KeyBinding::new(source, app::Quit, None);
        format_keystrokes(binding.keystrokes())
    }

    #[test]
    fn keystrokes_are_written_for_people() {
        assert_eq!(keys("ctrl-k"), "Ctrl+K");
        assert_eq!(keys("ctrl-shift-t"), "Ctrl+Shift+T");
        assert_eq!(keys("ctrl--"), "Ctrl+-");
        assert_eq!(keys("ctrl-="), "Ctrl+=");
        assert_eq!(keys("ctrl-,"), "Ctrl+,");
        assert_eq!(keys("ctrl-/"), "Ctrl+/");
        assert_eq!(keys("ctrl-shift-tab"), "Ctrl+Shift+Tab");
        assert_eq!(keys("shift-left"), "Shift+Left");
        assert_eq!(keys("escape"), "Esc");
        assert_eq!(keys("alt-up"), "Alt+Up");
        assert_eq!(keys("ctrl-k ctrl-s"), "Ctrl+K Ctrl+S");
    }

    #[test]
    fn hint_is_the_first_global_binding_else_the_first_binding() {
        let keymap = Keymap::new(vec![
            KeyBinding::new("escape", app::Redo, Some("BlockEditor")),
            KeyBinding::new("ctrl-shift-z", app::Redo, None),
            KeyBinding::new("ctrl-y", app::Redo, None),
            KeyBinding::new("ctrl-enter", app::CycleTask, Some("BlockEditor")),
        ]);
        assert_eq!(
            binding_hint(&keymap, &app::Redo).as_deref(),
            Some("Ctrl+Shift+Z")
        );
        assert_eq!(
            binding_hint(&keymap, &app::CycleTask).as_deref(),
            Some("Ctrl+Enter")
        );
        assert_eq!(binding_hint(&keymap, &app::Undo), None);
    }

    #[test]
    fn commands_have_unique_labels_and_names() {
        let mut labels: Vec<&str> = Command::ALL.iter().map(|c| c.label()).collect();
        labels.sort();
        labels.dedup();
        assert_eq!(labels.len(), Command::ALL.len());
        // Each command dispatches the action of the same name.
        for c in Command::ALL {
            assert_eq!(c.action().name(), format!("notesec::{}", c.name()));
        }
    }
}
