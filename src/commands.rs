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
//! its handler in `app.rs`, which a key binding needs anyway). A default
//! key is one more row in `app::shortcuts`. The command then shows up in
//! Settings > Shortcuts by itself and can be rebound there (decision 41,
//! `hotkeys.rs`).

use crate::app;
use gpui::{Action, KeyBinding, KeybindingKeystroke, Keymap};

/// What a command needs before it makes sense to offer it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Needs {
    /// Nothing: works from any tab, even with no tabs open.
    Nothing,
    /// A current page: a page tab is showing (not the graph, the agenda,
    /// the trash or the empty state). The palette hides it otherwise.
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

            /// A stable name for debug selectors (`command-NewPage`) and
            /// the key of its override in state.toml's `[shortcuts]`.
            pub fn name(self) -> &'static str {
                match self {
                    $(Command::$name => stringify!($name),)*
                }
            }

            /// A binding of `keys` (valid GPUI keystrokes; panics
            /// otherwise, like `KeyBinding::new`) to the command's action,
            /// in its key context.
            pub fn binding(self, keys: &str) -> KeyBinding {
                let context = self.key_context();
                match self {
                    $(Command::$name => KeyBinding::new(keys, app::$name, context),)*
                }
            }
        }
    };
}

impl Command {
    /// The key context of the command's bindings: "BlockEditor" for the
    /// commands that need a block being edited, else none (global).
    pub fn key_context(self) -> Option<&'static str> {
        (self.needs() == Needs::Editing).then_some("BlockEditor")
    }
}

commands! {
    // Pages and navigation
    NewPage => "New page", ["create", "add page"], Nothing;
    OpenToday => "Open today's journal", ["today", "daily", "journal"], Nothing;
    QuickCapture => "Quick capture", ["capture", "quick", "inbox", "journal"], Nothing;
    OpenAgenda => "Open agenda", ["tasks", "todo", "scheduled", "deadline"], Nothing;
    OpenCalendar => "Calendar", ["calendar", "month", "journal", "date", "daily"], Nothing;
    OpenTrash => "Open trash", ["deleted", "restore", "bin", "recycle", "undelete"], Nothing;
    SearchAllPages => "Search all pages", ["global search", "full text", "find in pages"], Nothing;
    RenamePage => "Rename current page", ["title", "name"], Page;
    DeletePage => "Delete current page", ["remove", "trash"], Page;
    CopyPageTitle => "Copy page title", ["clipboard", "name"], Page;
    ExportHtml => "Export page to HTML", ["export", "html", "save", "share", "web page"], Page;
    ExportPdf => "Export page as PDF", ["export", "pdf", "print", "save", "share"], Page;
    PublishPage => "Publish page", ["publish", "share", "website", "static site", "html", "host"], Page;
    ImportObsidian => "Import from Obsidian\u{2026}", ["import", "obsidian", "vault", "markdown", "migrate"], Nothing;
    ImportLogseq => "Import from Logseq\u{2026}", ["import", "logseq", "graph", "migrate"], Nothing;
    ImportNotion => "Import from Notion\u{2026}", ["import", "notion", "export", "csv", "migrate"], Nothing;
    PublishPageWithLinks => "Publish page with linked pages", ["publish", "share", "website", "static site", "links"], Page;
    RecordVoiceNote => "Record voice note", ["voice", "audio", "microphone", "mic", "dictate", "memo"], Nothing;
    StopRecording => "Stop recording", ["voice", "audio", "save", "finish"], Nothing;
    CancelRecording => "Cancel recording", ["voice", "audio", "discard"], Nothing;
    TranscribeVoiceNotes => "Transcribe voice notes on page", ["whisper", "speech", "text", "voice"], Page;
    NewWhiteboard => "New whiteboard", ["canvas", "board", "diagram", "cards", "mind map", "draw"], Nothing;
    WhiteboardFit => "Whiteboard: fit all", ["zoom to fit", "canvas", "board"], Page;
    WhiteboardZoomReset => "Whiteboard: zoom 100%", ["actual size", "canvas", "board", "reset zoom"], Page;
    WhiteboardAddPage => "Whiteboard: add page or block card\u{2026}", ["canvas", "board", "card", "insert"], Page;
    ToggleWhiteboardOutline => "Whiteboard: open as outline / canvas", ["canvas", "board", "blocks", "text"], Page;
    ExportVault => "Export encrypted vault\u{2026}", ["encrypt", "backup", "passphrase", "password", "secure", "usb", "cloud"], Nothing;
    ImportVault => "Import encrypted vault\u{2026}", ["decrypt", "restore", "passphrase", "password", "secure"], Nothing;
    ToggleFavorite => "Toggle favorite", ["star", "unstar", "bookmark", "favourite"], Page;
    SortPagesAz => "Sort pages A-Z", ["alphabetical", "order"], Nothing;
    // Editing
    Undo => "Undo", [], Nothing;
    Redo => "Redo", [], Nothing;
    InsertTemplate => "Insert template", ["snippet"], Page;
    CycleTask => "Cycle task state", ["todo", "doing", "done", "checkbox"], Editing;
    MoveBlockUp => "Move block up", ["reorder", "swap"], Editing;
    MoveBlockDown => "Move block down", ["reorder", "swap"], Editing;
    Paste => "Insert image from clipboard", ["paste", "image", "picture", "screenshot"], Editing;
    CollapseAll => "Collapse all", ["fold all", "outline"], Page;
    ExpandAll => "Expand all", ["unfold all", "outline"], Page;
    // Graph
    ToggleGraph => "Toggle graph view", ["graph", "network"], Nothing;
    ToggleLocalGraph => "Toggle local graph", ["global graph", "neighbours"], Nothing;
    FitGraph => "Fit graph", ["zoom to fit", "center graph"], Nothing;
    ToggleGraphJournals => "Toggle journals in graph", ["daily notes", "hide journals"], Nothing;
    // View
    ToggleTheme => "Switch theme", ["toggle dark/light theme", "dark mode", "light mode"], Nothing;
    ToggleKanban => "Kanban view", ["kanban", "board", "columns", "todo", "doing", "done"], Page;
    IncreaseFont => "Increase font size", ["zoom in", "bigger text"], Nothing;
    DecreaseFont => "Decrease font size", ["zoom out", "smaller text"], Nothing;
    ResetFont => "Reset font size", ["default font size"], Nothing;
    // Tabs
    CloseTab => "Close tab", [], Nothing;
    NextTab => "Next tab", [], Nothing;
    PrevTab => "Previous tab", [], Nothing;
    SplitRight => "Split right", ["split view", "side by side", "second pane", "two pages"], Nothing;
    ClosePane => "Close pane", ["unsplit", "single pane"], Nothing;
    FocusOtherPane => "Focus other pane", ["switch pane", "other side"], Nothing;
    // App
    OpenSettings => "Open settings", ["preferences", "theme", "font"], Nothing;
    ToggleGitBackup => "Toggle git auto-backup", ["git", "backup", "version", "history", "autosave"], Nothing;
    SyncNow => "Sync now", ["sync", "push", "pull", "git", "remote", "machines"], Nothing;
    ToggleVimMode => "Toggle vim mode", ["vim", "vi", "modal editing", "keybindings", "normal mode"], Nothing;
    ShowShortcuts => "Keyboard shortcuts", ["keys", "keymap", "cheatsheet", "help"], Nothing;
    CustomizeShortcuts => "Change keyboard shortcuts", ["customize", "rebind", "hotkeys", "key bindings", "remap"], Nothing;
    Quit => "Quit", ["exit", "close app"], Nothing;
    // AI (decisions 42-46)
    AskMyNotes => "Ask my notes", ["ai", "chat", "question", "llm", "assistant", "answer"], Nothing;
    ToggleChat => "AI chat", ["ai", "chat", "sidebar", "assistant", "copilot", "conversation"], Nothing;
    SemanticSearch => "Semantic search", ["meaning", "similar", "embeddings", "ai search", "find by meaning"], Nothing;
    SuggestTags => "Suggest tags", ["auto tag", "tagging", "ai tags", "keywords", "label"], Page;
    SaveSearch => "Save search", ["smart folder", "saved search", "keep search", "bookmark search"], Nothing;
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
