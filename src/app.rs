//! The root view: sidebar of pages on the left, selected page on the right.
//!
//! Editing model: there is ONE shared `EditorState`. Clicking a block loads that
//! block's text into it; every other block is drawn as plain text. When editing
//! stops (Escape, click elsewhere, switching pages) the text is written back to
//! the block and the page is saved to disk.

use crate::agenda::{day_label, Agenda, AgendaItem};
use crate::assets::{image_markdown, is_image_path, parse_images, resolve, save_image, ImageRef};
use crate::backup::{self, BackupError, Git, Repo};
use crate::code::{split_code, CodeBlock, Part};
use crate::commands::{binding_hint, format_keystrokes, Command, Needs};
use crate::config::{Config, ThemeKind};
use crate::display::DisplayBlock;
use crate::editor::{EditorState, Emphasis, SlashMenu};
use crate::graph_view::{GraphEvent, GraphView};
use crate::hotkeys::{self, effective_shortcuts, KeyChoice, KeyOwner, Overrides};
use crate::model::{
    backlinks, cycle_task, find_block, parse_block_refs, parse_query, parse_references,
    resolve_page, tag_counts, tag_query, BlockKind, Page, TaskState,
};
use crate::search::{
    search_blocks, search_link_pages, search_templates, search_text, search_with, snippet, Hit,
    Target,
};
use crate::state::{SearchKind, UiState};
use crate::storage::{today_title, validate_title, Storage, Template, TrashEntry};
use crate::table::{parse_table, Align};
use crate::tabs::{TabTarget, Tabs};
use crate::ui::{
    block_row, drag_handle, drop_line, favorite_star, fold_arrow, fold_badge, task_checkbox,
    BlockDragPreview, Theme,
};
use gpui::{
    actions, anchored, deferred, div, fill, point, prelude::*, px, relative, size, AnyElement, App,
    Bounds, ClickEvent, ClipboardItem, Context, DragMoveEvent, ElementId, ElementInputHandler,
    Entity, EntityInputHandler, ExternalPaths, FocusHandle, FontStyle, FontWeight, GlobalElementId,
    HighlightStyle, Hsla, KeyBinding, KeyDownEvent, KeyUpEvent, Keystroke, LayoutId, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, ScrollHandle, ShapedLine,
    SharedString, Style, StyledText, Subscription, Task, TextRun, UTF16Selection, UnderlineStyle,
    Window,
};
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;
use uuid::Uuid;

/// Settings > AI and the Ask my notes panel (decision 42).
mod ai_ui;
mod mentions_ui;
use ai_ui::{AiSettings, AskState, RelatedState, SavedUi, SemanticState, TagSuggestState};
use mentions_ui::MentionsState;
mod appearance_ui;
mod capture_ui;
mod clipper_ui;
mod embed_ui;
mod import_ui;
mod plugins_ui;
mod publish_ui;
mod vault_ui;
mod vim_ui;
mod voice_ui;
mod whiteboard_ui;

// Actions are named, typed commands that key bindings map onto. The macro
// declares one unit struct per name inside the `notesec` namespace. Palette
// commands (`commands.rs`) dispatch these too.
actions!(
    notesec,
    [
        Enter,
        Tab,
        ShiftTab,
        Backspace,
        Delete,
        Left,
        Right,
        Up,
        Down,
        Home,
        End,
        Escape,
        SelectLeft,
        SelectRight,
        SelectHome,
        SelectEnd,
        Paste,
        Bold,
        Italic,
        CycleTask,
        MoveBlockUp,
        MoveBlockDown,
        NewLine,
        CloseTab,
        NextTab,
        PrevTab,
        ToggleSearch,
        SearchAllPages,
        NewPage,
        OpenToday,
        QuickCapture,
        Undo,
        Redo,
        ToggleTheme,
        ToggleGraph,
        IncreaseFont,
        DecreaseFont,
        ResetFont,
        OpenSettings,
        Quit,
        // Palette commands without a key of their own (except
        // ShowShortcuts, Ctrl+/).
        OpenAgenda,
        OpenTrash,
        ExportHtml,
        PublishPage,
        PublishPageWithLinks,
        ImportObsidian,
        ImportLogseq,
        ImportNotion,
        ToggleGitBackup,
        SplitRight,
        ClosePane,
        FocusOtherPane,
        RenamePage,
        DeletePage,
        CopyPageTitle,
        ToggleFavorite,
        SortPagesAz,
        InsertTemplate,
        CollapseAll,
        ExpandAll,
        ToggleLocalGraph,
        FitGraph,
        ToggleGraphJournals,
        ShowShortcuts,
        CustomizeShortcuts,
        // AI (decisions 42-46), no default keys.
        AskMyNotes,
        SemanticSearch,
        SuggestTags,
        SaveSearch,
        ToggleVimMode,
        RecordVoiceNote,
        StopRecording,
        CancelRecording,
        TranscribeVoiceNotes,
        NewWhiteboard,
        WhiteboardFit,
        WhiteboardZoomReset,
        WhiteboardAddPage,
        ToggleWhiteboardOutline,
        ExportVault,
        ImportVault,
        // Delete / Backspace on a whiteboard canvas (no palette row).
        WhiteboardDelete,
    ]
);

/// The headings of the keyboard shortcuts dialog, in display order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyGroup {
    Navigation,
    Editing,
    View,
    Tabs,
    App,
}

impl KeyGroup {
    pub const ALL: [KeyGroup; 5] = [
        KeyGroup::Navigation,
        KeyGroup::Editing,
        KeyGroup::View,
        KeyGroup::Tabs,
        KeyGroup::App,
    ];

    pub fn label(self) -> &'static str {
        match self {
            KeyGroup::Navigation => "Navigation",
            KeyGroup::Editing => "Editing",
            KeyGroup::View => "View",
            KeyGroup::Tabs => "Tabs",
            KeyGroup::App => "App",
        }
    }
}

/// One key binding and how the shortcuts dialog describes it. `bind_keys`
/// registers exactly these (with the user's overrides applied, see
/// `hotkeys::effective_shortcuts`) and the dialog lists exactly these, so
/// the two can't drift; the palette reads its hints back from the keymap.
pub struct Shortcut {
    pub binding: KeyBinding,
    /// The binding's key context (`None`: global).
    pub context: Option<&'static str>,
    pub group: KeyGroup,
    /// Rows with the same group and description are merged in the dialog
    /// ("Ctrl+= / Ctrl++").
    pub description: &'static str,
}

/// The DEFAULT keymap. One line per binding; an action's main binding comes
/// first (it is the one shown next to its palette command). The user's
/// overrides from state.toml go on top of this (`hotkeys.rs`, decision 41):
/// a command's rows here are its default keys, a command with no row is
/// unbound by default, and every `commands!` command can be rebound in
/// Settings > Shortcuts. Other rows (search, the editing keys, Esc in
/// dialogs) are fixed.
#[rustfmt::skip]
pub fn shortcuts() -> Vec<Shortcut> {
    use KeyGroup::*;
    fn s<A: gpui::Action>(
        keys: &str,
        action: A,
        context: Option<&'static str>,
        group: KeyGroup,
        description: &'static str,
    ) -> Shortcut {
        Shortcut {
            binding: KeyBinding::new(keys, action, context),
            context,
            group,
            description,
        }
    }
    // The "BlockEditor" context only exists while a block (or the palette's
    // query box, or the rename field) is being edited, so these keys do
    // nothing otherwise.
    let ed = Some("BlockEditor");
    vec![
        // Global (no context): work whether or not a block is being edited.
        s("ctrl-k",         ToggleSearch,  None, Navigation, "Search pages, blocks and commands"),
        s("ctrl-shift-f",   SearchAllPages, None, Navigation, "Search the text of every page and journal"),
        s("ctrl-j",         OpenToday,     None, Navigation, "Open today's journal"),
        s("ctrl-shift-c",   QuickCapture,  None, Navigation, "Quick capture to today's journal"),
        s("ctrl-n",         NewPage,       None, Navigation, "New page"),
        s("up",             Up,            ed,   Navigation, "Block above / below (or palette result)"),
        s("down",           Down,          ed,   Navigation, "Block above / below (or palette result)"),
        s("enter",          Enter,         ed,   Editing,    "New block at the cursor (palette: open the result)"),
        s("tab",            Tab,           ed,   Editing,    "Indent the block"),
        s("shift-tab",      ShiftTab,      ed,   Editing,    "Outdent the block"),
        s("backspace",      Backspace,     ed,   Editing,    "Delete back; on an empty block, remove it"),
        s("delete",         Delete,        ed,   Editing,    "Delete forward"),
        s("left",           Left,          ed,   Editing,    "Move the cursor"),
        s("right",          Right,         ed,   Editing,    "Move the cursor"),
        s("home",           Home,          ed,   Editing,    "Start / end of the block"),
        s("end",            End,           ed,   Editing,    "Start / end of the block"),
        s("shift-left",     SelectLeft,    ed,   Editing,    "Extend the selection"),
        s("shift-right",    SelectRight,   ed,   Editing,    "Extend the selection"),
        s("shift-home",     SelectHome,    ed,   Editing,    "Extend the selection"),
        s("shift-end",      SelectEnd,     ed,   Editing,    "Extend the selection"),
        s("shift-enter",    NewLine,       ed,   Editing,    "Line break inside the block"),
        s("alt-up",         MoveBlockUp,   ed,   Editing,    "Move the block up / down"),
        s("alt-down",       MoveBlockDown, ed,   Editing,    "Move the block up / down"),
        s("ctrl-v",         Paste,         ed,   Editing,    "Paste (an image is saved to assets/)"),
        s("ctrl-b",         Bold,          ed,   Editing,    "Bold"),
        s("ctrl-i",         Italic,        ed,   Editing,    "Italic"),
        s("ctrl-enter",     CycleTask,     ed,   Editing,    "Cycle task: TODO, DOING, DONE, none"),
        s("escape",         Escape,        ed,   Editing,    "Stop editing (or close the palette)"),
        s("ctrl-z",         Undo,          None, Editing,    "Undo"),
        s("ctrl-shift-z",   Redo,          None, Editing,    "Redo"),
        s("ctrl-y",         Redo,          None, Editing,    "Redo"),
        s("ctrl-g",         ToggleGraph,   None, View,       "Toggle graph view"),
        s("ctrl-shift-t",   ToggleTheme,   None, View,       "Switch theme"),
        // `=` and `+` share a key on US layouts; bind both so Ctrl-+ works
        // with or without Shift.
        s("ctrl-=",         IncreaseFont,  None, View,       "Increase font size"),
        s("ctrl-+",         IncreaseFont,  None, View,       "Increase font size"),
        s("ctrl--",         DecreaseFont,  None, View,       "Decrease font size"),
        s("ctrl-0",         ResetFont,     None, View,       "Reset font size"),
        s("ctrl-w",         CloseTab,      None, Tabs,       "Close tab"),
        s("ctrl-tab",       NextTab,       None, Tabs,       "Next tab"),
        s("ctrl-shift-tab", PrevTab,       None, Tabs,       "Previous tab"),
        // Linux reports Ctrl+Shift+\ as `ctrl-|` (the shifted symbol, with
        // Shift dropped), so that is what Focus other pane binds.
        s("ctrl-\\",        SplitRight,     None, Tabs,       "Split right"),
        s("ctrl-shift-w",   ClosePane,      None, Tabs,       "Close pane"),
        s("ctrl-|",         FocusOtherPane, None, Tabs,       "Focus other pane"),
        s("ctrl-,",         OpenSettings,  None, App,        "Open / close settings"),
        s("ctrl-/",         ShowShortcuts, None, App,        "Keyboard shortcuts (this list)"),
        s("ctrl-q",         Quit,          None, App,        "Quit"),
        // While the settings panel, a page menu (or its delete
        // confirmation), this list or a trash confirmation is open, the
        // root's key context is that instead, so Esc closes it. (Renaming
        // uses "BlockEditor": it types.)
        s("escape",         Escape,        Some("Settings"),    App, "Close a dialog or menu"),
        s("escape",         Escape,        Some("PageMenu"),    App, "Close a dialog or menu"),
        s("escape",         Escape,        Some("Shortcuts"),   App, "Close a dialog or menu"),
        s("escape",         Escape,        Some("TrashDialog"), App, "Close a dialog or menu"),
        // A whiteboard canvas with the keyboard (nothing edited, no dialog).
        s("delete",         WhiteboardDelete, Some("Whiteboard"), Editing, "Whiteboard: delete the selected card or arrow"),
        s("backspace",      WhiteboardDelete, Some("Whiteboard"), Editing, "Whiteboard: delete the selected card or arrow"),
        s("escape",         Escape,        Some("Whiteboard"),  App, "Close a dialog or menu"),
    ]
}

/// The shortcuts dialog's content: for each group (in order, empty ones
/// left out), one row per description with every key bound to it.
pub fn cheatsheet(shortcuts: &[Shortcut]) -> Vec<(KeyGroup, Vec<(String, &'static str)>)> {
    KeyGroup::ALL
        .iter()
        .map(|&group| {
            let mut rows: Vec<(Vec<String>, &'static str)> = Vec::new();
            for s in shortcuts.iter().filter(|s| s.group == group) {
                let keys = format_keystrokes(s.binding.keystrokes());
                match rows.iter_mut().find(|(_, d)| *d == s.description) {
                    Some((list, _)) => {
                        if !list.contains(&keys) {
                            list.push(keys);
                        }
                    }
                    None => rows.push((vec![keys], s.description)),
                }
            }
            let rows = rows
                .into_iter()
                .map(|(keys, description)| (keys.join(" / "), description))
                .collect();
            (group, rows)
        })
        .filter(|(_, rows): &(KeyGroup, Vec<_>)| !rows.is_empty())
        .collect()
}

/// Register keyboard shortcuts (the `shortcuts` table) and the app-wide
/// Quit handler, at startup. The window's `NoteSec::new` then re-registers
/// them with the graph's overrides applied (`register_keys`).
pub fn bind_keys(cx: &mut App) {
    register_keys(cx, &Overrides::new());
    cx.on_action(|_: &Quit, cx| cx.quit());
}

/// Replace the whole keymap with the default table plus `overrides`
/// (decision 41). Palette hints and the shortcuts list read the result.
pub fn register_keys(cx: &mut App, overrides: &Overrides) {
    cx.clear_key_bindings();
    cx.bind_keys(
        effective_shortcuts(overrides)
            .into_iter()
            .map(|s| s.binding),
    );
}

/// The palette puts a header over each group of results (see
/// `search::search`): "Commands" over the commands, and "Pages" over the
/// pages and blocks when commands are listed too. The header shown just
/// above row `i`, if any.
fn palette_header(hits: &[Hit], i: usize) -> Option<&'static str> {
    let is_command = hits[i].target.is_command();
    if i > 0 && hits[i - 1].target.is_command() == is_command {
        return None;
    }
    if is_command {
        Some("Commands")
    } else if hits.iter().any(|h| h.target.is_command()) {
        Some("Pages")
    } else {
        None
    }
}

/// The value carried by a block drag (see `BlockDragPreview` for what is
/// drawn under the mouse): the dragged block, by id so it is found again
/// even if indices shift.
#[derive(Clone, Debug)]
struct DraggedBlock {
    id: Uuid,
}

/// Where a dragged block would land if dropped now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DropGap {
    /// Just before this block, as its sibling.
    Before(Uuid),
    /// After every block, at the top level.
    End,
}

/// Which main view is showing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// A page's blocks (the default).
    Notes,
    /// The page graph.
    Graph,
    /// The agenda: open tasks by date.
    Agenda,
    /// The trash: deleted pages.
    Trash,
    /// No tab is open: an empty state with hints.
    Empty,
}

/// How a navigation shows its page (see `NoteSec::navigate`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Nav {
    /// In the active tab, replacing its page (in-page links, backlinks).
    Replace,
    /// In the tab already showing that page, else a new tab (sidebar,
    /// search, Today, new page).
    Tab,
}

/// Which reference picker is open while editing (see `NoteSec::ref_query`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RefKind {
    /// `((query`: pick a block to reference.
    Block,
    /// `[[query`: pick a page to link to.
    Page,
}

/// One entry of a reference picker.
#[derive(Clone, Debug, PartialEq, Eq)]
enum RefItem {
    /// A block, as `(page, block)` indices.
    Block(usize, usize),
    /// A page, with the alias it was found by (if that, not its title,
    /// matched the query).
    Page(usize, Option<String>),
}

/// How many page and block results the search overlay shows (commands
/// come on top of these with an empty query).
const MAX_RESULTS: usize = 12;

/// How many results global search lists (titles first, then lines).
const MAX_TEXT_RESULTS: usize = 100;

/// Longest snippet (in characters) a global search result shows.
const SNIPPET_CHARS: usize = 90;

/// Height of the palette's scrolling result list.
const PALETTE_LIST_HEIGHT: f32 = 440.0;

/// State of the Ctrl-K overlay while it is open.
struct SearchState {
    /// The query box. It reuses `EditorState`, so typing, IME, paste and cursor
    /// movement work exactly as in a block.
    query: EditorState,
    /// Highlighted result (index into the current results).
    selected: usize,
    /// The block that was being edited when the palette opened. "Insert
    /// template" puts the template's blocks after it (see
    /// `Page::insert_blocks_from`); `None` means append to the page.
    insert_after: Option<usize>,
    /// That block's editor (text, cursor, selection) when the palette
    /// opened: an editor command (`Needs::Editing`) resumes it.
    resume: Option<EditorState>,
    /// `Some` once "Insert template" was chosen: the palette then lists these
    /// templates instead of pages, blocks and commands.
    templates: Option<Vec<Template>>,
    /// The result list scrolls (an empty query lists every command); arrow
    /// keys keep the highlighted row in view through this.
    scroll: ScrollHandle,
    /// Global search (Ctrl+Shift+F): the same overlay, but the results are
    /// `search_text`'s exact matches in titles and text, not fuzzy pages,
    /// blocks and commands.
    global: bool,
}

const MAX_HISTORY: usize = 100;

/// Height of the scrollable font list in the settings panel.
const FONT_LIST_HEIGHT: f32 = 220.0;

/// Height of the scrollable command list in Settings > Shortcuts.
const HOTKEY_LIST_HEIGHT: f32 = 320.0;

/// State of the settings panel while it is open.
struct SettingsState {
    /// Installed font families (`TextSystem::all_font_names`, which sorts and
    /// dedupes), read once when the panel opens rather than on every frame.
    fonts: Vec<String>,
    /// Which half of the panel shows.
    section: SettingsSection,
    /// The command whose new key is awaited ("Press keys…"), decision 41.
    capture: Option<Command>,
    /// Why the last key pressed while capturing wasn't taken, or what the
    /// last reset did.
    hotkey_message: Option<HotkeyMessage>,
    /// Scroll position of the command list (tests bring rows into view).
    hotkey_scroll: ScrollHandle,
    /// Settings > AI (decision 42): the field being edited, the model list.
    ai: AiSettings,
}

/// The settings panel's sections (tab-like buttons at its top).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SettingsSection {
    /// Git backup and the editor's vim mode.
    General,
    /// Theme and fonts (decisions 56 and 57, `appearance_ui`).
    Appearance,
    /// Every command with its key, rebindable (decision 41).
    Shortcuts,
    /// Provider, endpoint, models and API key (decision 42).
    Ai,
    /// The web clipper (decision 51, `clipper_ui`).
    Clipper,
    /// Voice notes: recorder and transcription (decision 52, `voice_ui`).
    Voice,
    /// WASM plugins: enable / disable (decision 55, `plugins_ui`).
    Plugins,
}

/// A line under the Shortcuts list.
#[derive(Clone, Debug, PartialEq)]
struct HotkeyMessage {
    text: String,
    /// Drawn in the danger colour (a refused key) rather than muted.
    error: bool,
}

/// A sidebar page's right-click menu, and the rename / delete steps it
/// leads to. The page is held by title: indices shift when pages re-sort.
struct PageMenu {
    title: String,
    /// Where the right-click happened, in window coordinates.
    position: gpui::Point<Pixels>,
    step: MenuStep,
}

enum MenuStep {
    /// The menu: Rename, Delete, Copy page title.
    Menu,
    /// Typing the new name: the menu turns into a text field in place. It
    /// reuses `EditorState` like the palette's query box.
    Rename {
        editor: EditorState,
        /// Why the last Enter was refused, shown under the field.
        error: Option<String>,
    },
    /// The confirm dialog before deleting.
    ConfirmDelete { error: Option<String> },
}

/// The trash view's confirm step before something is deleted for good.
#[derive(Clone, Debug, PartialEq)]
enum TrashConfirm {
    /// "Delete forever" on one entry.
    DeleteForever(TrashEntry),
    /// "Empty trash": every entry.
    Empty,
}

/// Side-by-side panes (decision 40). The left pane is the tab bar's: it
/// shows the active tab. The right pane shows one page. Whichever has focus
/// is the one `selected`, `mode` and editing describe, so everything that
/// acts on "the current page" acts on the focused pane.
#[derive(Clone, Debug, PartialEq)]
struct Split {
    /// The right pane's page, by title: indices shift when pages are added,
    /// deleted or re-sorted, titles don't (renames update it).
    right: String,
    /// The right pane has focus (else the left one).
    right_focused: bool,
}

/// What `render_page_view` draws for one pane.
struct PageView {
    element: AnyElement,
    /// The reading-view text layouts by block, for tests (focused pane only).
    #[cfg(test)]
    layouts: Vec<(usize, gpui::TextLayout)>,
    /// The "Linked from" rows' reading text, for tests.
    #[cfg(test)]
    backlink_texts: Vec<String>,
    /// The embeds' text lines (`embed_ui::Embeds::lines`), for tests.
    #[cfg(test)]
    embed_lines: Vec<String>,
}

/// Debug selector prefix for the unfocused pane's page (`other-block-0`).
const OTHER_PANE: &str = "other-";

/// A short message at the bottom right (e.g. where an export was
/// written), shown for `STATUS_FOR`.
#[derive(Clone, Debug, PartialEq)]
struct Status {
    text: String,
    /// Shown in the danger colour.
    error: bool,
}

/// What a confirmation dialog says (see `NoteSec::render_confirm`).
struct Confirm {
    /// Debug selector and element id prefix.
    id: &'static str,
    title: String,
    body: String,
    /// Why the last confirm failed, shown in the dialog.
    error: Option<String>,
    /// The danger button's label.
    ok_label: &'static str,
}

/// When a trash entry was deleted, for its row: "Deleted today 14:05",
/// "Deleted yesterday 09:12", else "Deleted 2026-10-03 18:40" (local time).
fn deleted_label(deleted_at: i64, now: chrono::DateTime<chrono::Local>) -> String {
    let Some(at) = chrono::DateTime::from_timestamp_millis(deleted_at) else {
        return String::new();
    };
    let at = at.with_timezone(&chrono::Local);
    match (now.date_naive() - at.date_naive()).num_days() {
        0 => format!("Deleted today {}", at.format("%H:%M")),
        1 => format!("Deleted yesterday {}", at.format("%H:%M")),
        _ => format!("Deleted {}", at.format("%Y-%m-%d %H:%M")),
    }
}

/// What a sidebar page row carries while it is dragged (`on_drag`): the
/// page, by title.
struct DraggedPage {
    title: String,
}

/// The floating copy of a page row that follows the pointer during a drag.
struct PageDragPreview {
    title: SharedString,
    theme: Theme,
    font_size: f32,
    font_family: Option<SharedString>,
}

impl Render for PageDragPreview {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        div()
            .when_some(self.font_family.clone(), |d, family| d.font_family(family))
            .text_size(px(self.font_size))
            .w(px(220.0))
            .px_3()
            .py_1()
            .rounded_md()
            .bg(theme.sidebar_bg)
            .border_1()
            .border_color(theme.accent)
            .text_color(theme.text)
            .shadow_md()
            .opacity(0.9)
            .child(self.title.clone())
    }
}

/// Where a dragged page would land if dropped now.
#[derive(Clone, Debug, PartialEq)]
struct PageDrop {
    /// The drop zone that set this: a page row (by title) or `None` for the
    /// end-of-list zone. Only that zone clears it when the pointer leaves
    /// (every zone hears every drag move; Zed's project panel idiom).
    zone: Option<String>,
    /// Insert before this page; `None` means at the end of the list.
    before: Option<String>,
}

/// Half the gap between sidebar rows (`gap_1`): each row's drop zone
/// reaches this far past its edges, so the gaps are covered too.
const ROW_GAP_HALF: f32 = 2.0;

/// The "/" block-type menu while it is open (see `editor::SlashMenu`).
struct SlashState {
    menu: SlashMenu,
    /// Undo snapshot from just before the "/" was typed. Esc restores its
    /// editor exactly; choosing a type records it as the one undo step.
    before: HistoryState,
}

#[derive(Clone)]
struct HistoryState {
    pages: Vec<Page>,
    selected: usize,
    editing: Option<usize>,
    editor: EditorState,
}

pub struct NoteSec {
    storage: Storage,
    /// All pages in sidebar order (see `sort_pages`): journals first,
    /// newest first, then the rest in the custom or alphabetical order.
    pages: Vec<Page>,
    /// Index into `pages` of the page shown in the main pane.
    selected: usize,
    /// Persisted user settings (`config.toml`).
    config: Config,
    /// Favorite and recently opened pages (`state.toml`).
    state: UiState,
    /// Colours for `config.theme`, kept in sync by `apply_theme`.
    theme: Theme,
    /// The reading font actually in use: `state.ui_font` if that font is
    /// installed, else `None` (the system UI font). Kept apart from the
    /// state so an unknown name in the file is never overwritten by a save.
    font_family: Option<SharedString>,

    /// Handle used to give this view keyboard focus while editing.
    focus_handle: FocusHandle,
    /// Index (into the selected page's blocks) of the block being edited.
    editing: Option<usize>,
    /// The shared text editor for the block being edited.
    editor: EditorState,
    /// `Some` while the Ctrl-K search overlay is open.
    search: Option<SearchState>,
    /// `Some` while the quick-capture box is open (design pass, Feature 3).
    capture: Option<EditorState>,
    /// `Some` while the "/" block-type menu is open on the edited block.
    slash: Option<SlashState>,
    /// `Some` while the settings panel is open.
    settings: Option<SettingsState>,
    /// The reference picker's highlighted entry (the `((` block picker or
    /// the `[[` page picker), with the `((query` / `[[query` range it was
    /// chosen in (typing changes the range, which starts again from the
    /// top). The picker itself shows whenever `block_ref_query` or
    /// `link_query` finds a query; see `ref_query`.
    ref_selected: (Range<usize>, usize),
    /// Esc closed the picker for the `((` or `[[` at this offset.
    ref_dismissed: Option<usize>,
    /// `Some` while a page's context menu (or its rename / delete step) is
    /// open.
    page_menu: Option<PageMenu>,
    /// True while the keyboard shortcuts dialog is open.
    shortcuts_open: bool,
    /// The Ask my notes panel and its session history (decision 42).
    ask: AskState,
    /// The Semantic search overlay and the embedding cache (decision 43).
    semantic: SemanticState,
    /// Tag suggestions and Related pages (decision 44).
    tag_suggest: TagSuggestState,
    related: RelatedState,
    /// "Mentioned in" (decision 45).
    mentions: MentionsState,
    /// Saved searches' sidebar state and name field (decision 46).
    saved: SavedUi,
    /// The pages in the trash, newest first (`Storage::list_trash`). Read
    /// at startup, when the trash tab is focused and after every change.
    trash: Vec<TrashEntry>,
    /// `Some` while the trash view asks before deleting for good.
    trash_confirm: Option<TrashConfirm>,
    /// Why the last trash action failed (e.g. Restore with the name
    /// taken), shown at the top of the trash view until the next one.
    trash_error: Option<String>,
    /// The status message on screen, if any, and the timer that clears it
    /// (dropping it cancels it, so a newer message gets its full time).
    status: Option<Status>,
    status_task: Option<Task<()>>,
    /// Where the page being dragged in the sidebar would land. Only
    /// meaningful while a drag is active; `render` clears it otherwise.
    page_drop: Option<PageDrop>,
    /// True between a mouse-down in the edited block and the mouse-up: mouse
    /// moves in between extend the selection.
    selecting: bool,
    /// Ids of folded blocks (their descendants are hidden). UI-only: not
    /// saved, and most block ids are regenerated on load anyway.
    collapsed: HashSet<Uuid>,
    /// While a block is being dragged by its bullet: where it would land.
    /// Only meaningful while GPUI has an active drag.
    block_drop: Option<DropGap>,
    undo_stack: Vec<HistoryState>,
    redo_stack: Vec<HistoryState>,
    text_history_active: bool,
    mode: Mode,
    /// Open tabs. The active one decides `mode` and, for a page tab,
    /// `selected` (see `apply_tab`), unless the right pane has focus.
    tabs: Tabs,
    /// The second pane, if the view is split (decision 40).
    split: Option<Split>,
    /// The graph view, created the first time it is opened and then kept so it
    /// remembers node positions between visits.
    graph: Option<Entity<GraphView>>,
    /// Keeps the subscription to the graph's events alive (dropping a
    /// `Subscription` cancels it).
    _graph_subscription: Option<Subscription>,
    /// Shaped text + bounds from the last paint; needed to answer the OS
    /// input-method's questions about where characters are on screen.
    last_layout: Option<TextLines>,
    last_bounds: Option<Bounds<Pixels>>,
    /// Text layouts of the rows in reading view from the last render, so
    /// tests can find where a displayed character is on screen.
    #[cfg(test)]
    reading_layouts: Vec<(usize, gpui::TextLayout)>,
    /// The same for the unfocused pane while split (decision 40).
    #[cfg(test)]
    other_layouts: Vec<(usize, gpui::TextLayout)>,
    /// The focused pane's "Linked from" rows' text from the last render,
    /// for tests.
    #[cfg(test)]
    backlink_texts: Vec<String>,
    /// Each pane's embeds' text lines from the last render (decision 47),
    /// for tests.
    #[cfg(test)]
    embed_lines: Vec<String>,
    #[cfg(test)]
    other_embed_lines: Vec<String>,
    /// Monospace font for code blocks: the first of `MONO_FONTS` installed.
    mono_font: Option<SharedString>,
    /// The code block (block id, its number in the block) under the mouse;
    /// it shows the copy button.
    hovered_code: Option<(Uuid, usize)>,
    /// The code block whose copy button shows "Copied", until
    /// `copied_task` clears it.
    copied_code: Option<(Uuid, usize)>,
    copied_task: Option<Task<()>>,
    /// Files being dragged in from outside the app, and whether the pointer
    /// is over the page. Set while they move; used and cleared on release.
    file_drag: Option<(ExternalPaths, bool)>,
    /// Git auto-backup (decision 39): how git is run, the job getting the
    /// repository ready (`start_backup`), the debounce timer and the commit
    /// it starts (`schedule_backup`; replacing it restarts the wait),
    /// whether changes wait for that commit, and the `Storage::changes`
    /// count already seen.
    git: Git,
    backup_setup: Option<Task<()>>,
    backup_task: Option<Task<()>>,
    backup_pending: bool,
    backup_seen: u64,
    /// Watching our own notifications, quit and release (for backups).
    _backup_subscriptions: Vec<Subscription>,
    /// The keystroke interceptor behind Settings > Shortcuts' key capture.
    _key_capture: Subscription,
    /// Vim mode's state (decision 48, `vim.rs`): used while
    /// `config.vim_mode` is on and a block is being edited.
    vim: crate::vim::Vim,
    /// The last publish (decision 49, `publish_ui`).
    publish: publish_ui::PublishState,
    /// The import in progress (decision 50, `import_ui`).
    import: import_ui::ImportState,
    /// The web clipper's listener (decision 51, `clipper_ui`).
    clipper: clipper_ui::ClipperState,
    /// Voice notes: the recording and transcriptions (decision 52,
    /// `voice_ui`).
    voice: voice_ui::VoiceState,
    /// Whiteboard canvases (decision 53, `whiteboard_ui.rs`).
    whiteboard: whiteboard_ui::WhiteboardState,
    /// Encrypted vault export/import (decision 54, `vault_ui.rs`).
    vault: vault_ui::VaultState,
    /// WASM plugins (decision 55, `plugins_ui.rs`).
    plugins: plugins_ui::PluginsState,
}

impl NoteSec {
    /// Build the root view. The view takes keyboard focus straight away:
    /// GPUI only delivers key events (and so global shortcuts like Ctrl-K) to
    /// views on the focused path, and nothing is focused in a new window.
    pub fn new(
        storage: Storage,
        config: Config,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut pages = storage.load_all();

        // Auto-create today's journal if it doesn't exist yet.
        let today = today_title();
        if !pages.iter().any(|p| p.is_journal && p.title == today) {
            pages.push(create_journal(&storage, &today));
        }

        // First run: give the user something to look at.
        if pages
            .iter()
            .all(|p| p.is_journal && p.blocks.iter().all(|b| b.content.is_empty()))
        {
            let welcome = Page::from_markdown(
                "Welcome",
                false,
                "- Welcome to notesec\n  - Click a block to edit it\n  - Enter adds a block, Tab / Shift-Tab change nesting\n  - Backspace on an empty block deletes it\n",
            );
            if let Err(err) = storage.save(&welcome) {
                eprintln!("notesec: could not create Welcome page: {err}");
            }
            pages.push(welcome);
        }

        let mut state = UiState::load(storage.root());
        // A theme or fonts chosen before they moved to `state.toml` (as
        // `theme`, `font_size`, `font_family` in `config.toml`) carry over
        // once; config saves then drop those keys.
        let theme_adopted = state.adopt_legacy_theme(config.legacy_theme);
        let fonts_adopted =
            state.adopt_legacy_fonts(config.legacy_font_size, config.legacy_font_family.clone());
        if theme_adopted || fonts_adopted {
            if let Err(err) = state.save(storage.root()) {
                eprintln!("notesec: failed to save state: {err}");
            }
        }
        let theme = Theme::from_kind(state.theme.unwrap_or_default());
        sort_pages(&mut pages, &state.page_order);
        // Open on today's journal.
        let selected = pages
            .iter()
            .position(|p| p.is_journal && p.title == today)
            .unwrap_or(0);

        // Start with one tab on that page.
        let first_tab = Tabs::new(TabTarget::Page(pages[selected].title.clone()));

        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);

        let font_family = state
            .ui_font
            .as_deref()
            .and_then(|family| installed_font(family, cx));

        let mono_font = resolve_mono_font(state.mono_font.as_deref(), cx);
        let trash = storage.list_trash();
        let backup_seen = storage.changes();
        // A page file written since the last notification restarts the
        // backup timer; quitting or closing the window commits what waits.
        let backup_subscriptions = vec![
            cx.observe_self(|this, cx| this.note_storage_changes(cx)),
            cx.on_app_quit(|this, _cx| {
                this.flush_backup();
                std::future::ready(())
            }),
            cx.on_release(|this, _cx| this.flush_backup()),
        ];
        // This graph's custom keys (decision 41), and the key capture of
        // Settings > Shortcuts: interceptors run before key bindings, so a
        // captured key never also runs its old command.
        register_keys(cx, &state.shortcuts);
        let weak = cx.weak_entity();
        let key_capture = cx.intercept_keystrokes(move |event, window, cx| {
            let taken = weak
                .update(cx, |app, cx| {
                    app.capture_keystroke(&event.keystroke, cx)
                        || app.vim_keystroke(&event.keystroke, window, cx)
                })
                .unwrap_or(false);
            if taken {
                cx.stop_propagation();
            }
        });

        let mut app = NoteSec {
            storage,
            pages,
            selected,
            theme,
            config,
            state,
            font_family,
            focus_handle,
            editing: None,
            editor: EditorState::default(),
            search: None,
            capture: None,
            slash: None,
            settings: None,
            page_menu: None,
            shortcuts_open: false,
            ask: AskState::default(),
            semantic: SemanticState::default(),
            tag_suggest: TagSuggestState::default(),
            related: RelatedState::default(),
            mentions: MentionsState::default(),
            saved: SavedUi::default(),
            trash,
            trash_confirm: None,
            trash_error: None,
            status: None,
            status_task: None,
            page_drop: None,
            selecting: false,
            collapsed: HashSet::new(),
            block_drop: None,
            ref_selected: (0..0, 0),
            ref_dismissed: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            text_history_active: false,
            mode: Mode::Notes,
            tabs: first_tab,
            split: None,
            graph: None,
            _graph_subscription: None,
            last_layout: None,
            last_bounds: None,
            #[cfg(test)]
            reading_layouts: Vec::new(),
            #[cfg(test)]
            other_layouts: Vec::new(),
            #[cfg(test)]
            backlink_texts: Vec::new(),
            #[cfg(test)]
            embed_lines: Vec::new(),
            #[cfg(test)]
            other_embed_lines: Vec::new(),
            mono_font,
            hovered_code: None,
            copied_code: None,
            copied_task: None,
            file_drag: None,
            git: Git::default(),
            backup_setup: None,
            backup_task: None,
            backup_pending: false,
            backup_seen,
            _backup_subscriptions: backup_subscriptions,
            _key_capture: key_capture,
            vim: Default::default(),
            publish: Default::default(),
            import: Default::default(),
            clipper: Default::default(),
            voice: Default::default(),
            whiteboard: Default::default(),
            vault: Default::default(),
            plugins: Default::default(),
        };
        // The startup page counts as opened.
        app.record_recent();
        app.reload_plugins();
        if app.config.git_backup {
            // Commits what changed while the app was closed, too.
            app.start_backup(false, cx);
        }
        if app.config.web_clipper {
            app.start_clipper(cx);
        }
        app
    }

    // --- which editor is active ----------------------------------------------

    /// The text editor currently receiving input: the search box while the
    /// overlay is open, the capture box while it is open, the rename field
    /// while renaming a page, otherwise the block editor.
    fn active_editor(&self) -> &EditorState {
        if let Some(dialog) = &self.vault.dialog {
            return dialog.field();
        }
        if let Some(editor) = &self.capture {
            return editor;
        }
        match (&self.search, &self.page_menu) {
            (Some(s), _) => &s.query,
            (
                None,
                Some(PageMenu {
                    step: MenuStep::Rename { editor, .. },
                    ..
                }),
            ) => editor,
            _ => self.ai_editor().unwrap_or(&self.editor),
        }
    }

    fn active_editor_mut(&mut self) -> &mut EditorState {
        let ai_input = self.ai_overlay_input();
        if self.vault.dialog.is_some() {
            return self.vault.dialog.as_mut().expect("open").field_mut();
        }
        if let Some(editor) = &mut self.capture {
            return editor;
        }
        match (&mut self.search, &mut self.page_menu) {
            (Some(s), _) => &mut s.query,
            (
                None,
                Some(PageMenu {
                    step: MenuStep::Rename { editor, .. },
                    ..
                }),
            ) => editor,
            _ => ai_ui::ai_editor_mut(
                &mut self.settings,
                &mut self.ask,
                &mut self.semantic,
                &mut self.saved,
                ai_input,
            )
            .unwrap_or(&mut self.editor),
        }
    }

    /// True while the rename field of the page menu takes the typing.
    fn renaming(&self) -> bool {
        matches!(
            self.page_menu,
            Some(PageMenu {
                step: MenuStep::Rename { .. },
                ..
            })
        )
    }

    /// The palette's query box or the rename field has the keyboard (not a
    /// block): typing there records no undo history.
    fn text_input_open(&self) -> bool {
        self.search.is_some()
            || self.capture.is_some()
            || self.renaming()
            || self.ai_editor().is_some()
            || self.vault_dialog_open()
    }

    // --- settings --------------------------------------------------------------

    fn save_config(&self) {
        if let Err(err) = self.config.save(self.storage.root()) {
            eprintln!("notesec: failed to save config: {err}");
        }
    }

    fn save_state(&self) {
        if let Err(err) = self.state.save(self.storage.root()) {
            eprintln!("notesec: failed to save state: {err}");
        }
    }

    /// Put the selected page at the front of the RECENT list, saving
    /// `state.toml` only if that changed anything.
    fn record_recent(&mut self) {
        let Some(page) = self.pages.get(self.selected) else {
            return;
        };
        if self.state.record_recent(&page.title) {
            self.save_state();
        }
    }

    /// Star or unstar the page called `title` (sidebar star buttons).
    fn toggle_favorite(&mut self, title: &str, cx: &mut Context<Self>) {
        self.state.toggle_favorite(title);
        self.save_state();
        cx.notify();
    }

    /// The theme in use (the default until one is chosen).
    fn theme_kind(&self) -> ThemeKind {
        self.state.theme.unwrap_or_default()
    }

    /// Switch to theme `kind`, apply it right away and save it to
    /// `state.toml`.
    fn set_theme(&mut self, kind: ThemeKind, cx: &mut Context<Self>) {
        if self.theme_kind() == kind {
            return;
        }
        self.state.theme = Some(kind);
        self.theme = Theme::from_kind(kind);
        self.save_state();
        self.sync_graph_style(cx);
        cx.notify();
    }

    fn toggle_theme(&mut self, cx: &mut Context<Self>) {
        self.set_theme(self.theme_kind().toggled(), cx);
    }

    // --- typography (decision 57) ---------------------------------------------

    /// The reading view's font size in px.
    fn ui_size(&self) -> f32 {
        self.state.ui_size()
    }

    /// The block editor's font size in px.
    fn mono_size(&self) -> f32 {
        self.state.mono_size()
    }

    /// Save a typography change and redraw. The open graph view takes the
    /// reading font size too.
    fn typography_changed(&mut self, cx: &mut Context<Self>) {
        self.save_state();
        self.sync_graph_style(cx);
        cx.notify();
    }

    /// Use font `family` for reading (`None`: the system UI font), apply it
    /// right away and save it. A family that isn't installed is still saved,
    /// but the system font is used, as at startup.
    fn set_ui_font(&mut self, family: Option<String>, cx: &mut Context<Self>) {
        let family = family.filter(|f| !f.trim().is_empty());
        if self.state.ui_font == family {
            return;
        }
        self.font_family = family.as_deref().and_then(|f| installed_font(f, cx));
        self.state.ui_font = family;
        self.typography_changed(cx);
    }

    /// Use font `family` in the block editor (`None`: the first installed
    /// monospace font). Code blocks follow it, as they share the editor's font.
    fn set_mono_font(&mut self, family: Option<String>, cx: &mut Context<Self>) {
        let family = family.filter(|f| !f.trim().is_empty());
        if self.state.mono_font == family {
            return;
        }
        self.state.mono_font = family;
        self.mono_font = resolve_mono_font(self.state.mono_font.as_deref(), cx);
        self.typography_changed(cx);
    }

    /// `size` px for the reading view (`None`: the default), kept in range.
    fn set_ui_size(&mut self, size: Option<f32>, cx: &mut Context<Self>) {
        let size = size.map(crate::config::clamp_font_size);
        if self.state.ui_size == size {
            return;
        }
        self.state.ui_size = size;
        self.typography_changed(cx);
    }

    /// `size` px for the block editor (`None`: the default), kept in range.
    fn set_mono_size(&mut self, size: Option<f32>, cx: &mut Context<Self>) {
        let size = size.map(crate::config::clamp_font_size);
        if self.state.mono_size == size {
            return;
        }
        self.state.mono_size = size;
        self.typography_changed(cx);
    }

    fn change_ui_size(&mut self, delta: f32, cx: &mut Context<Self>) {
        self.set_ui_size(Some(self.ui_size() + delta), cx);
    }

    fn change_mono_size(&mut self, delta: f32, cx: &mut Context<Self>) {
        self.set_mono_size(Some(self.mono_size() + delta), cx);
    }

    fn reset_ui_size(&mut self, cx: &mut Context<Self>) {
        self.set_ui_size(None, cx);
    }

    fn reset_mono_size(&mut self, cx: &mut Context<Self>) {
        self.set_mono_size(None, cx);
    }

    /// Ctrl+= / Ctrl+-: make everything bigger or smaller by one step. Both
    /// sizes move together so the reading and editing views stay in
    /// proportion; Settings > Appearance sets each on its own.
    fn change_font_size(&mut self, delta: f32, cx: &mut Context<Self>) {
        let (ui, mono) = (self.ui_size() + delta, self.mono_size() + delta);
        self.set_ui_size(Some(ui), cx);
        self.set_mono_size(Some(mono), cx);
    }

    /// Ctrl+0: both sizes back to their defaults.
    fn reset_font_size(&mut self, cx: &mut Context<Self>) {
        self.set_ui_size(None, cx);
        self.set_mono_size(None, cx);
    }

    // --- git auto-backup (backup.rs, decision 39) -------------------------------

    /// Turn git auto-backup on or off and save that in config.toml. On
    /// gets the repository ready and commits right away (`start_backup`).
    fn set_git_backup(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.config.git_backup == on {
            return;
        }
        self.config.git_backup = on;
        self.save_config();
        if on {
            self.start_backup(true, cx);
        } else {
            // Nothing more is committed: drop the setup and any waiting
            // commit (dropping a task cancels it).
            self.backup_setup = None;
            self.backup_task = None;
            self.backup_pending = false;
            let status = Status {
                text: "Git backup off".to_string(),
                error: false,
            };
            self.show_status(status, cx);
        }
        cx.notify();
    }

    fn toggle_git_backup(&mut self, cx: &mut Context<Self>) {
        self.set_git_backup(!self.config.git_backup, cx);
    }

    /// Off the UI thread: find or `git init` the repository
    /// (`backup::prepare`), then commit what is there. `announce` (the
    /// toggle, not startup) says where backups go when it worked.
    fn start_backup(&mut self, announce: bool, cx: &mut Context<Self>) {
        self.backup_setup = Some(cx.spawn(async move |this, cx| {
            let Ok((git, root)) = this.update(cx, |this, _| {
                (this.git.clone(), this.storage.root().to_path_buf())
            }) else {
                return;
            };
            let result = cx
                .background_spawn(async move {
                    let repo = backup::prepare(&git, &root)?;
                    Ok((repo, backup::commit(&git, &root)))
                })
                .await;
            let _ = this.update(cx, |this, cx| this.backup_started(result, announce, cx));
        }));
    }

    /// `start_backup` finished. If the repository couldn't be set up (no
    /// git, the folder ignored by a repository above it, ...), backup is
    /// turned off again (and saved off, so the switch shows the truth) and
    /// the status says why.
    fn backup_started(
        &mut self,
        result: Result<(Repo, Result<Option<usize>, BackupError>), BackupError>,
        announce: bool,
        cx: &mut Context<Self>,
    ) {
        self.backup_setup = None;
        if !self.config.git_backup {
            return; // turned off meanwhile
        }
        let status = match result {
            Err(err) => {
                self.config.git_backup = false;
                self.save_config();
                Some(Status {
                    text: format!("Git backup is off: {err}"),
                    error: true,
                })
            }
            Ok((_, Err(err))) => Some(Status {
                text: format!("Git backup failed: {err}"),
                error: true,
            }),
            Ok((repo, Ok(_))) => announce.then(|| Status {
                text: backup_on_message(self.storage.root(), &repo),
                error: false,
            }),
        };
        if let Some(status) = status {
            self.show_status(status, cx);
        }
        cx.notify();
    }

    /// After every notification: if a page file changed on disk since the
    /// last one (`Storage::changes`), restart the backup timer.
    fn note_storage_changes(&mut self, cx: &mut Context<Self>) {
        let changes = self.storage.changes();
        if changes == self.backup_seen {
            return;
        }
        self.backup_seen = changes;
        if self.config.git_backup {
            self.schedule_backup(cx);
        }
    }

    /// Commit `BACKUP_AFTER` from now, off the UI thread. Called again
    /// before then, the wait starts over (the old task is dropped).
    fn schedule_backup(&mut self, cx: &mut Context<Self>) {
        self.backup_pending = true;
        self.backup_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(backup::BACKUP_AFTER).await;
            let Ok((git, root)) = this.update(cx, |this, _| {
                this.backup_pending = false;
                (this.git.clone(), this.storage.root().to_path_buf())
            }) else {
                return;
            };
            let result = cx
                .background_spawn(async move { backup::commit(&git, &root) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Err(err) = result {
                    // Still to do: the next change or quitting tries again.
                    this.backup_pending = true;
                    if this.config.git_backup {
                        let text = format!("Git backup failed: {err}");
                        this.show_status(Status { text, error: true }, cx);
                    }
                }
            });
        }));
    }

    /// On quit or window close: commit now if changes are waiting for the
    /// timer, else wait for a commit that is running. Blocks the UI
    /// thread, which is closing anyway.
    fn flush_backup(&mut self) {
        if !self.config.git_backup {
            return;
        }
        if self.backup_pending {
            self.backup_pending = false;
            self.backup_task = None;
            if let Err(err) = backup::commit(&self.git, self.storage.root()) {
                eprintln!("notesec: git backup failed: {err}");
            }
        } else {
            backup::wait_idle();
        }
    }

    // --- settings panel --------------------------------------------------------

    fn open_settings(&mut self, cx: &mut Context<Self>) {
        // Save the edited block and close the palette: the panel covers both.
        self.stop_edit(cx);
        self.close_search(cx);
        self.page_menu = None;
        self.shortcuts_open = false;
        self.trash_confirm = None;
        let fonts = cx.text_system().all_font_names();
        self.settings = Some(SettingsState {
            fonts,
            section: SettingsSection::General,
            capture: None,
            hotkey_message: None,
            hotkey_scroll: ScrollHandle::new(),
            ai: AiSettings::default(),
        });
        cx.notify();
    }

    /// Show `section` of the settings panel, opening it if needed. Leaving
    /// the Shortcuts section stops a key capture.
    fn show_settings_section(&mut self, section: SettingsSection, cx: &mut Context<Self>) {
        if self.settings.is_none() {
            self.open_settings(cx);
        }
        if let Some(settings) = &mut self.settings {
            if settings.section != section {
                settings.section = section;
                settings.capture = None;
                settings.hotkey_message = None;
            }
        }
        cx.notify();
    }

    // --- custom hotkeys (decision 41) -----------------------------------------

    /// The keymap in effect: the defaults plus this graph's overrides.
    fn key_table(&self) -> Vec<Shortcut> {
        effective_shortcuts(&self.state.shortcuts)
    }

    /// Register `key_table` as the app's keymap.
    fn apply_hotkeys(&self, cx: &mut Context<Self>) {
        register_keys(cx, &self.state.shortcuts);
    }

    /// The command a key capture is waiting for, if any.
    fn capturing(&self) -> Option<Command> {
        self.settings.as_ref().and_then(|s| s.capture)
    }

    fn set_hotkey_message(&mut self, message: Option<HotkeyMessage>) {
        if let Some(settings) = &mut self.settings {
            settings.hotkey_message = message;
        }
    }

    /// Click on a command's key: wait for its new key (again: stop waiting).
    fn toggle_capture(&mut self, command: Command, cx: &mut Context<Self>) {
        if let Some(settings) = &mut self.settings {
            settings.capture = (settings.capture != Some(command)).then_some(command);
            settings.hotkey_message = None;
        }
        cx.notify();
    }

    /// Every keystroke passes here first (`intercept_keystrokes`, before any
    /// key binding). While a capture waits, the key is taken and nothing
    /// else sees it (returns true): Esc cancels, Backspace or Delete
    /// unbinds, a lone modifier keeps waiting, any other key becomes the
    /// command's key if `choice_for` and the conflict check allow it.
    fn capture_keystroke(&mut self, keystroke: &Keystroke, cx: &mut Context<Self>) -> bool {
        let Some(command) = self.capturing() else {
            return false;
        };
        if hotkeys::is_modifier_key(&keystroke.key) {
            return true;
        }
        let plain = !keystroke.modifiers.modified();
        match keystroke.key.as_str() {
            "escape" if plain => {
                if let Some(settings) = &mut self.settings {
                    settings.capture = None;
                    settings.hotkey_message = None;
                }
            }
            "backspace" | "delete" if plain => self.set_hotkey(command, KeyChoice::Unbound, cx),
            _ => match hotkeys::choice_for(keystroke) {
                Err(text) => self.set_hotkey_message(Some(HotkeyMessage { text, error: true })),
                Ok(choice) => {
                    let key = hotkeys::pretty(keystroke);
                    match hotkeys::conflict(&self.key_table(), command, keystroke) {
                        Some(KeyOwner::Command(other)) => {
                            self.set_hotkey_message(Some(HotkeyMessage {
                                text: format!(
                                    "{key} is already the key of \u{201c}{}\u{201d}. Change or unbind that one first.",
                                    other.label()
                                ),
                                error: true,
                            }))
                        }
                        Some(KeyOwner::Fixed(what)) => self.set_hotkey_message(Some(HotkeyMessage {
                            text: format!(
                                "{key} is used for \u{201c}{what}\u{201d}, which can't be changed."
                            ),
                            error: true,
                        })),
                        None => self.set_hotkey(command, choice, cx),
                    }
                }
            },
        }
        cx.notify();
        true
    }

    /// Give `command` the keys `choice` asks for, save and re-register.
    /// Ends the capture.
    fn set_hotkey(&mut self, command: Command, choice: KeyChoice, cx: &mut Context<Self>) {
        match hotkeys::override_value(command, &choice) {
            Some(value) => self
                .state
                .shortcuts
                .insert(command.name().to_string(), value),
            None => self.state.shortcuts.remove(command.name()),
        };
        self.save_state();
        self.apply_hotkeys(cx);
        if let Some(settings) = &mut self.settings {
            settings.capture = None;
            settings.hotkey_message = None;
        }
        cx.notify();
    }

    /// A row's ↺: back to the command's default keys.
    fn reset_hotkey(&mut self, command: Command, cx: &mut Context<Self>) {
        if self.state.shortcuts.remove(command.name()).is_some() {
            self.save_state();
            self.apply_hotkeys(cx);
        }
        if let Some(settings) = &mut self.settings {
            settings.capture = None;
            settings.hotkey_message = None;
        }
        cx.notify();
    }

    /// "Reset to defaults": forget every override (unknown names too).
    fn reset_all_hotkeys(&mut self, cx: &mut Context<Self>) {
        let had = !self.state.shortcuts.is_empty();
        self.state.shortcuts.clear();
        if had {
            self.save_state();
            self.apply_hotkeys(cx);
        }
        if let Some(settings) = &mut self.settings {
            settings.capture = None;
            settings.hotkey_message = had.then(|| HotkeyMessage {
                text: "Every shortcut is back to its default.".into(),
                error: false,
            });
        }
        cx.notify();
    }

    fn on_customize_shortcuts(
        &mut self,
        _: &CustomizeShortcuts,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.shortcuts_open = false;
        self.show_settings_section(SettingsSection::Shortcuts, cx);
    }

    fn close_settings(&mut self, cx: &mut Context<Self>) {
        self.settings = None;
        cx.notify();
    }

    /// Ctrl-, opens the panel, or closes it if it is already open.
    fn on_toggle_git_backup(
        &mut self,
        _: &ToggleGitBackup,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_git_backup(cx);
    }

    fn on_open_settings(&mut self, _: &OpenSettings, _: &mut Window, cx: &mut Context<Self>) {
        if self.settings.is_some() {
            self.close_settings(cx);
        } else {
            self.open_settings(cx);
        }
    }

    // --- templates -------------------------------------------------------------

    /// Show the palette as a template picker. `insert_after` is the block the
    /// chosen template goes after (`None`: end of the page).
    fn open_template_picker(&mut self, insert_after: Option<usize>, cx: &mut Context<Self>) {
        self.stop_edit(cx);
        self.search = Some(SearchState {
            query: EditorState::default(),
            selected: 0,
            insert_after,
            resume: None,
            templates: Some(self.storage.load_templates()),
            scroll: ScrollHandle::new(),
            global: false,
        });
        cx.notify();
    }

    /// Insert `template`'s blocks into the selected page after block
    /// `insert_after` (replacing it if it is an empty leaf), or at the end of
    /// the page. One undo step; saved right away like other structural edits.
    fn insert_template(
        &mut self,
        template: &Template,
        insert_after: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        self.stop_edit(cx);
        let source = Page::from_markdown(&template.name, false, &template.markdown);
        if source.blocks.is_empty() {
            return;
        }
        let page = &self.pages[self.selected];
        // A page that is just one blank bullet (a fresh journal or page) gets
        // that bullet replaced rather than a template appended below it.
        let insert_after = insert_after.or_else(|| {
            (page.blocks.len() == 1 && page.blocks[0].content.trim().is_empty()).then_some(0)
        });
        self.record_edit();
        self.pages[self.selected].insert_blocks_from(insert_after, &source);
        self.save_page();
        self.show_selected(Nav::Tab, cx);
    }

    // --- search overlay --------------------------------------------------------

    fn search_results(&self) -> Vec<Hit> {
        match &self.search {
            Some(SearchState {
                query,
                templates: Some(templates),
                ..
            }) => {
                let names: Vec<&str> = templates.iter().map(|t| t.name.as_str()).collect();
                search_templates(&names, &query.text, MAX_RESULTS)
            }
            Some(s) if s.global => search_text(&self.pages, &s.query.text, MAX_TEXT_RESULTS),
            Some(s) => search_with(
                &self.pages,
                &self.available_commands(),
                &self
                    .plugin_commands()
                    .into_iter()
                    .map(|(_, _, label)| label)
                    .collect::<Vec<_>>(),
                &s.query.text,
                MAX_RESULTS,
            ),
            None => Vec::new(),
        }
    }

    /// Call after the active editor's text changed: the old highlighted row no
    /// longer means anything, so go back to the top result.
    fn text_changed(&mut self) {
        if let Some(s) = &mut self.search {
            s.selected = 0;
        }
        // A refused name's message goes away once the name is edited.
        if let Some(PageMenu {
            step: MenuStep::Rename { error, .. },
            ..
        }) = &mut self.page_menu
        {
            *error = None;
        }
    }

    fn toggle_search(&mut self, _: &ToggleSearch, window: &mut Window, cx: &mut Context<Self>) {
        if self.vault_dialog_open() {
            return;
        }
        if self.search.is_some() {
            self.close_search(cx);
            return;
        }
        // Remember where the cursor was (for "Insert template"), then save
        // whatever block is being edited before covering it.
        let insert_after = self.editing;
        let resume = insert_after.map(|_| self.editor.clone());
        self.stop_edit(cx);
        self.settings = None;
        self.page_menu = None;
        self.shortcuts_open = false;
        self.trash_confirm = None;
        self.search = Some(SearchState {
            query: EditorState::default(),
            selected: 0,
            insert_after,
            resume,
            templates: None,
            scroll: ScrollHandle::new(),
            global: false,
        });
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    /// Ctrl+Shift+F: open global search, or close it if it is open. From the
    /// Ctrl-K palette it switches over (the query starts empty).
    fn search_all_pages(
        &mut self,
        _: &SearchAllPages,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.search.as_ref().is_some_and(|s| s.global) {
            self.close_search(cx);
            return;
        }
        self.stop_edit(cx);
        self.settings = None;
        self.page_menu = None;
        self.shortcuts_open = false;
        self.search = Some(SearchState {
            query: EditorState::default(),
            selected: 0,
            insert_after: None,
            resume: None,
            templates: None,
            scroll: ScrollHandle::new(),
            global: true,
        });
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn close_search(&mut self, cx: &mut Context<Self>) {
        self.search = None;
        self.forget_whiteboard_pick();
        self.last_layout = None;
        self.last_bounds = None;
        cx.notify();
    }

    fn history_state(&self) -> HistoryState {
        HistoryState {
            pages: self.pages.clone(),
            selected: self.selected,
            editing: self.editing,
            editor: self.editor.clone(),
        }
    }

    fn record_state(&mut self, state: HistoryState) {
        self.undo_stack.push(state);
        if self.undo_stack.len() > MAX_HISTORY {
            self.undo_stack.remove(0);
        }
        self.redo_stack.clear();
    }

    fn record_edit(&mut self) {
        self.record_state(self.history_state());
    }

    fn save_all_pages(&self) {
        for page in &self.pages {
            if let Err(err) = self.storage.save(page) {
                eprintln!("notesec: failed to save {}: {err}", page.title);
            }
        }
    }

    fn restore_history(
        &mut self,
        state: HistoryState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pages = state.pages;
        self.selected = state.selected.min(self.pages.len().saturating_sub(1));
        // The snapshot may predate a drag or "Sort pages A-Z".
        self.sort_pages();
        self.editing = state.editing.filter(|&ix| {
            self.selected < self.pages.len() && ix < self.pages[self.selected].blocks.len()
        });
        self.editor = state.editor;
        self.slash = None;
        self.text_history_active = false;
        if let Some(ix) = self.editing {
            self.editor.clamp();
            self.editor.marked = None;
            self.pages[self.selected].blocks[ix].content = self.editor_content(ix);
            self.reveal(ix);
        }
        self.save_all_pages();
        // Tabs on pages that the restored state doesn't have are closed. The
        // restored page is brought to the front when a page is on screen or
        // a block is being edited (never edit a page that isn't shown).
        let pages = &self.pages;
        self.tabs.retain(|t| match t {
            TabTarget::Page(title) => pages.iter().any(|p| p.title == *title),
            TabTarget::Graph | TabTarget::Agenda | TabTarget::Trash => true,
        });
        self.prune_split();
        if let Some(split) = self.split.as_mut().filter(|s| s.right_focused) {
            // The focused right pane shows the restored page.
            split.right = self.pages[self.selected].title.clone();
            self.mode = Mode::Notes;
        } else if self.editing.is_some()
            || matches!(self.tabs.active_target(), Some(TabTarget::Page(_)))
        {
            // Not `show_page`: undo/redo isn't the user opening a page, so
            // RECENT is left alone (decision 21).
            let title = self.pages[self.selected].title.clone();
            self.tabs.open(TabTarget::Page(title));
            self.mode = Mode::Notes;
        } else {
            self.apply_tab(cx);
        }
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn undo(&mut self, _: &Undo, window: &mut Window, cx: &mut Context<Self>) {
        if self.search.is_some() || self.capture.is_some() || self.page_menu.is_some() {
            return;
        }
        self.close_slash_as_typing();
        if let Some(state) = self.undo_stack.pop() {
            self.redo_stack.push(self.history_state());
            self.restore_history(state, window, cx);
        }
    }

    fn redo(&mut self, _: &Redo, window: &mut Window, cx: &mut Context<Self>) {
        if self.search.is_some() || self.capture.is_some() || self.page_menu.is_some() {
            return;
        }
        self.close_slash_as_typing();
        if let Some(state) = self.redo_stack.pop() {
            self.undo_stack.push(self.history_state());
            self.restore_history(state, window, cx);
        }
    }

    fn new_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.stop_edit(cx);
        let title = (0..)
            .map(|n| {
                if n == 0 {
                    "Untitled".to_string()
                } else {
                    format!("Untitled {n}")
                }
            })
            .find(|title| self.find_page(title).is_none())
            .expect("page title search must find a free name");
        let page = Page::with_empty_block(&title);
        if let Err(err) = self.storage.save(&page) {
            eprintln!("notesec: failed to create page {title}: {err}");
        }
        self.add_page(page);
        let ix = self.find_page(&title).unwrap_or(0);
        self.show_page(ix, cx);
        self.start_edit(0, window, cx);
    }

    fn move_slash_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let count = self.slash_matches().len();
        if let Some(state) = &mut self.slash {
            state.menu.move_selection(delta, count);
        }
        cx.notify();
    }

    fn move_search_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let hits = self.search_results();
        if let Some(s) = &mut self.search {
            if !hits.is_empty() {
                s.selected =
                    (s.selected as isize + delta).clamp(0, hits.len() as isize - 1) as usize;
                // Children of the list are headers and rows; find the row's.
                let headers = (0..=s.selected)
                    .filter(|&i| palette_header(&hits, i).is_some())
                    .count();
                s.scroll.scroll_to_item(s.selected + headers);
            }
        }
        cx.notify();
    }

    fn confirm_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(selected) = self.search.as_ref().map(|s| s.selected) else {
            return;
        };
        if let Some(hit) = self.search_results().get(selected).cloned() {
            self.open_hit(&hit, window, cx);
        }
    }

    /// Act on a search result: open its page (and for a block hit, start
    /// editing that block) or run the command.
    fn open_hit(&mut self, hit: &Hit, window: &mut Window, cx: &mut Context<Self>) {
        let (insert_after, resume, templates) = match self.search.take() {
            Some(s) => (s.insert_after, s.resume, s.templates),
            None => (None, None, None),
        };
        // "Add page" on a whiteboard: the pick becomes a card there.
        let board = self.take_whiteboard_pick();
        self.close_search(cx);
        if board.is_some_and(|b| self.add_picked_card(&b, &hit.target, cx)) {
            return;
        }
        match hit.target {
            Target::Page(page) => {
                let title = self.pages[page].title.clone();
                self.navigate(&title, Nav::Tab, cx);
            }
            Target::Block(page, block) => {
                let title = self.pages[page].title.clone();
                self.navigate(&title, Nav::Tab, cx);
                if block < self.pages[self.selected].blocks.len() {
                    self.start_edit(block, window, cx);
                }
            }
            // Edit the block with the match selected, so it shows highlighted
            // (and typing replaces it, as with any selection).
            Target::Match {
                page,
                block,
                start,
                end,
            } => {
                let title = self.pages[page].title.clone();
                self.navigate(&title, Nav::Tab, cx);
                let blocks = &self.pages[self.selected].blocks;
                if block < blocks.len() {
                    let valid = blocks[block].content.get(start..end).is_some();
                    self.start_edit(block, window, cx);
                    if valid {
                        self.editor.anchor = Some(start);
                        self.editor.cursor = end;
                    }
                }
            }
            // Keeps the palette open, now listing templates (after the
            // block that was being edited when it opened).
            Target::Command(Command::InsertTemplate) => self.open_template_picker(insert_after, cx),
            // The same action the command's key binding dispatches. It
            // runs once this handler returns (GPUI defers it), so the
            // palette is already closed.
            Target::Command(command) => {
                // An editor command acts on the block the palette was
                // opened from: edit it again, exactly as it was.
                if command.needs() == Needs::Editing {
                    let (Some(ix), Some(editor)) = (insert_after, resume) else {
                        return;
                    };
                    if ix >= self.pages[self.selected].blocks.len() {
                        return;
                    }
                    self.start_edit(ix, window, cx);
                    self.editor = editor;
                    self.editor.clamp();
                }
                window.dispatch_action(command.action(), cx)
            }
            Target::Plugin(entry) => {
                // Opened while editing: back to that block, as it was.
                if let (Some(ix), Some(editor)) = (insert_after, resume) {
                    if ix < self.pages[self.selected].blocks.len() {
                        self.start_edit(ix, window, cx);
                        self.editor = editor;
                        self.editor.clamp();
                    }
                }
                self.run_plugin_command(entry, cx);
            }
            Target::Template(ix) => {
                if let Some(template) = templates.as_ref().and_then(|t| t.get(ix)) {
                    self.insert_template(template, insert_after, cx);
                }
            }
        }
    }

    fn on_toggle_theme(&mut self, _: &ToggleTheme, _: &mut Window, cx: &mut Context<Self>) {
        self.toggle_theme(cx);
    }

    fn on_new_page(&mut self, _: &NewPage, window: &mut Window, cx: &mut Context<Self>) {
        self.new_page(window, cx);
    }

    fn on_open_today(&mut self, _: &OpenToday, _: &mut Window, cx: &mut Context<Self>) {
        self.open_today(cx);
    }

    fn on_undo(&mut self, action: &Undo, window: &mut Window, cx: &mut Context<Self>) {
        self.undo(action, window, cx);
    }

    fn on_redo(&mut self, action: &Redo, window: &mut Window, cx: &mut Context<Self>) {
        self.redo(action, window, cx);
    }

    fn on_increase_font(&mut self, _: &IncreaseFont, _: &mut Window, cx: &mut Context<Self>) {
        self.change_font_size(1.0, cx);
    }

    fn on_decrease_font(&mut self, _: &DecreaseFont, _: &mut Window, cx: &mut Context<Self>) {
        self.change_font_size(-1.0, cx);
    }

    fn on_reset_font(&mut self, _: &ResetFont, _: &mut Window, cx: &mut Context<Self>) {
        self.reset_font_size(cx);
    }

    // --- palette commands ------------------------------------------------------

    /// The page the palette's page commands act on: the one on screen, if a
    /// page tab is showing (not the graph, the agenda, the trash or no tab
    /// at all).
    fn current_page(&self) -> Option<String> {
        (self.mode == Mode::Notes).then(|| self.pages[self.selected].title.clone())
    }

    /// Commands the palette offers right now, in table order. Page commands
    /// are left out without a current page, editor commands unless the
    /// palette was opened while editing a block, and Rename / Delete when
    /// the page menu would refuse them (a journal; the last page).
    fn available_commands(&self) -> Vec<Command> {
        let page = self.current_page();
        let editing = page.is_some()
            && self
                .search
                .as_ref()
                .is_some_and(|s| s.insert_after.is_some() && s.resume.is_some());
        Command::ALL
            .iter()
            .copied()
            .filter(|c| match c.needs() {
                Needs::Nothing => true,
                Needs::Page => page.is_some(),
                Needs::Editing => editing,
            })
            .filter(|c| match c {
                Command::RenamePage => page.as_deref().is_some_and(|t| self.can_rename(t)),
                Command::DeletePage => self.can_delete(),
                Command::ClosePane | Command::FocusOtherPane => self.split.is_some(),
                Command::RecordVoiceNote => !self.recording(),
                Command::StopRecording | Command::CancelRecording => self.recording(),
                Command::WhiteboardFit
                | Command::WhiteboardZoomReset
                | Command::WhiteboardAddPage => self.whiteboard_shown(),
                Command::ToggleWhiteboardOutline => {
                    page.is_some() && crate::whiteboard::is_whiteboard(&self.pages[self.selected])
                }
                Command::InsertTemplate | Command::CollapseAll | Command::ExpandAll => {
                    !self.whiteboard_shown()
                }
                _ => true,
            })
            .collect()
    }

    fn on_open_agenda(&mut self, _: &OpenAgenda, _: &mut Window, cx: &mut Context<Self>) {
        self.show_agenda(cx);
    }

    fn on_open_trash(&mut self, _: &OpenTrash, _: &mut Window, cx: &mut Context<Self>) {
        self.show_trash(cx);
    }

    fn on_export_html(&mut self, _: &ExportHtml, _: &mut Window, cx: &mut Context<Self>) {
        if self.current_page().is_some() {
            self.export_html(cx);
        }
    }

    fn on_sort_pages_az(&mut self, _: &SortPagesAz, _: &mut Window, cx: &mut Context<Self>) {
        self.sort_pages_az(cx);
    }

    fn on_toggle_local_graph(
        &mut self,
        _: &ToggleLocalGraph,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_local_graph(cx);
    }

    /// "Fit graph": show the graph (opening it if needed) and fit it.
    fn on_fit_graph(&mut self, _: &FitGraph, _: &mut Window, cx: &mut Context<Self>) {
        self.show_graph(cx);
        if let Some(graph) = self.graph.clone() {
            graph.update(cx, |g, cx| g.fit(cx));
        }
    }

    /// "Toggle journals in graph": show the graph and flip its Journals chip.
    fn on_toggle_graph_journals(
        &mut self,
        _: &ToggleGraphJournals,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.show_graph(cx);
        if let Some(graph) = self.graph.clone() {
            graph.update(cx, |g, cx| g.toggle_journals(cx));
        }
    }

    fn on_insert_template(&mut self, _: &InsertTemplate, _: &mut Window, cx: &mut Context<Self>) {
        if self.current_page().is_some() && !self.whiteboard_shown() {
            self.open_template_picker(self.editing, cx);
        }
    }

    /// Where the page menu opens when a command (not a right-click) opens
    /// it: over the page title, at the top left of the main pane.
    fn command_menu_position() -> gpui::Point<Pixels> {
        point(px(272.0), px(80.0))
    }

    /// "Rename current page": the page menu's rename field, for the page
    /// on screen.
    fn on_rename_page(&mut self, _: &RenamePage, _: &mut Window, cx: &mut Context<Self>) {
        let Some(title) = self.current_page().filter(|t| self.can_rename(t)) else {
            return;
        };
        self.open_page_menu(title, Self::command_menu_position(), cx);
        self.start_rename(cx);
    }

    /// "Delete current page": the page menu's confirm dialog.
    fn on_delete_page(&mut self, _: &DeletePage, _: &mut Window, cx: &mut Context<Self>) {
        let Some(title) = self.current_page() else {
            return;
        };
        self.open_page_menu(title, Self::command_menu_position(), cx);
        self.ask_delete(cx);
        // `ask_delete` refuses for the last page; don't leave the menu up.
        if matches!(
            self.page_menu,
            Some(PageMenu {
                step: MenuStep::Menu,
                ..
            })
        ) {
            self.close_page_menu(cx);
        }
    }

    fn on_copy_page_title(&mut self, _: &CopyPageTitle, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(title) = self.current_page() {
            cx.write_to_clipboard(ClipboardItem::new_string(title));
        }
    }

    fn on_toggle_favorite(&mut self, _: &ToggleFavorite, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(title) = self.current_page() {
            self.toggle_favorite(&title, cx);
        }
    }

    /// "Collapse all": fold every block of the current page that has
    /// children. Folding is UI-only (not an undo step), like the arrows.
    fn on_collapse_all(&mut self, _: &CollapseAll, _: &mut Window, cx: &mut Context<Self>) {
        if self.current_page().is_none() {
            return;
        }
        let page = &self.pages[self.selected];
        self.collapsed.extend(
            (0..page.blocks.len())
                .filter(|&ix| page.descendant_count(ix) > 0)
                .map(|ix| page.blocks[ix].id),
        );
        // A block being edited inside a folded subtree stops being edited.
        let visible = page.visible_blocks(&self.collapsed);
        if self.editing.is_some_and(|ix| !visible[ix]) {
            self.stop_edit(cx);
        }
        cx.notify();
    }

    /// "Expand all": unfold every block of the current page.
    fn on_expand_all(&mut self, _: &ExpandAll, _: &mut Window, cx: &mut Context<Self>) {
        if self.current_page().is_none() {
            return;
        }
        for block in &self.pages[self.selected].blocks {
            self.collapsed.remove(&block.id);
        }
        cx.notify();
    }

    // --- keyboard shortcuts dialog --------------------------------------------

    /// Ctrl+/ or "Keyboard shortcuts": open the list, or close it if open.
    fn on_show_shortcuts(
        &mut self,
        _: &ShowShortcuts,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.shortcuts_open {
            self.close_shortcuts(cx);
            return;
        }
        // A modal like the settings panel: save the edit, close the rest.
        self.stop_edit(cx);
        self.close_search(cx);
        self.settings = None;
        self.page_menu = None;
        self.trash_confirm = None;
        self.shortcuts_open = true;
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn close_shortcuts(&mut self, cx: &mut Context<Self>) {
        self.shortcuts_open = false;
        cx.notify();
    }

    // --- "/" block-type menu --------------------------------------------------

    /// Kinds the open menu currently lists (empty when it is closed).
    fn slash_matches(&self) -> Vec<BlockKind> {
        self.slash
            .as_ref()
            .and_then(|s| s.menu.query(&self.editor))
            .map(SlashMenu::matches)
            .unwrap_or_default()
    }

    /// After the editor changed while the menu is open: if the "/" itself
    /// was deleted, cancel (as Esc would); if nothing matches any more, keep
    /// what was typed as ordinary text; otherwise go back to the top entry.
    fn refresh_slash(&mut self) {
        let Some(state) = &self.slash else { return };
        if state.menu.query(&self.editor).is_none() {
            self.slash_cancel();
        } else if self.slash_matches().is_empty() {
            self.close_slash_as_typing();
        } else if let Some(state) = &mut self.slash {
            state.menu.selected = 0;
        }
    }

    /// Close the menu and treat the "/query" as ordinary typing: it goes in
    /// where the "/" was typed, replacing the selection if there was one
    /// (exactly what typing it with no menu would have done). Typing while
    /// the menu was open recorded no history, so this is recorded now as one
    /// undo step (and further typing joins that step).
    fn close_slash_as_typing(&mut self) {
        let Some(state) = self.slash.take() else {
            return;
        };
        let Some(typed) = state.menu.typed(&self.editor).map(str::to_string) else {
            self.editor = state.before.editor;
            return;
        };
        let mut editor = state.before.editor.clone();
        editor.insert(&typed);
        self.editor = editor;
        self.record_state(state.before);
        self.text_history_active = true;
    }

    /// Put the block (text, cursor and selection) back exactly as it was
    /// before the "/" was typed.
    fn slash_cancel(&mut self) {
        if let Some(state) = self.slash.take() {
            self.editor = state.before.editor;
        }
    }

    /// Esc with the menu open.
    fn dismiss_slash(&mut self, cx: &mut Context<Self>) {
        self.slash_cancel();
        cx.notify();
    }

    /// Turn the edited block into `kind`, keeping its text and dropping the
    /// "/query". One undo step back to before the "/"; saved right away like
    /// other structural changes.
    fn apply_slash(&mut self, kind: BlockKind, cx: &mut Context<Self>) {
        let Some(state) = self.slash.take() else {
            return;
        };
        let text = kind.apply(&state.before.editor.text);
        self.text_history_active = false;
        if text != state.before.editor.text {
            self.record_state(state.before);
        }
        self.editor = EditorState::new(&text);
        if self.sync_content() {
            self.save_page();
        }
        cx.notify();
    }

    // --- "((" block-reference and "[[" page-link pickers ----------------------

    /// The `((query` or `[[query` range the picker is open for, if it is,
    /// and which picker: while editing a block (and no other menu or
    /// overlay is up), unless Esc closed it. When both could apply (a `((`
    /// inside a `[[`, say), the one typed last wins.
    fn ref_query(&self) -> Option<(Range<usize>, RefKind)> {
        self.editing?;
        if self.search.is_some()
            || self.slash.is_some()
            || self.settings.is_some()
            || self.vim_takes_keys()
        {
            return None;
        }
        let block = self.editor.block_ref_query().map(|r| (r, RefKind::Block));
        let page = self.editor.link_query().map(|r| (r, RefKind::Page));
        let (range, kind) = match (block, page) {
            (Some(b), Some(p)) => {
                if p.0.start > b.0.start {
                    p
                } else {
                    b
                }
            }
            (b, p) => b.or(p)?,
        };
        (self.ref_dismissed != Some(range.start)).then_some((range, kind))
    }

    /// What the open picker lists; empty when closed.
    fn ref_matches(&self) -> Vec<RefItem> {
        let Some((range, kind)) = self.ref_query() else {
            return Vec::new();
        };
        let query = &self.editor.text[range.start + 2..range.end];
        match kind {
            RefKind::Block => {
                let editing_id = self
                    .editing
                    .map(|ix| self.pages[self.selected].blocks[ix].id);
                search_blocks(&self.pages, query, editing_id, 8)
                    .into_iter()
                    .map(|(page, block)| RefItem::Block(page, block))
                    .collect()
            }
            RefKind::Page => search_link_pages(&self.pages, query, 8)
                .into_iter()
                .map(|(page, alias)| RefItem::Page(page, alias))
                .collect(),
        }
    }

    /// The picker's highlighted entry, for the current query.
    fn ref_highlight(&self) -> usize {
        match self.ref_query() {
            Some((range, _)) if self.ref_selected.0 == range => self.ref_selected.1,
            _ => 0,
        }
    }

    /// True while the picker is open with something to pick, which is when
    /// it takes Up, Down, Enter and Esc.
    fn ref_menu_open(&self) -> bool {
        !self.ref_matches().is_empty()
    }

    fn move_ref_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let count = self.ref_matches().len();
        if let (Some((range, _)), true) = (self.ref_query(), count > 0) {
            let selected = (self.ref_highlight() as isize + delta).clamp(0, count as isize - 1);
            self.ref_selected = (range, selected as usize);
        }
        cx.notify();
    }

    /// Replace the typed query with the picked entry, cursor after it: a
    /// block becomes `((<its id>))`, a page `[[<its title>]]` (its real
    /// title even when an alias matched; a `]]` already right after the
    /// cursor is taken in, not doubled). One undo step; saved right away,
    /// which also writes a block's id to its page (see `save_page`).
    fn insert_ref(&mut self, item: RefItem, cx: &mut Context<Self>) {
        let Some((mut range, _)) = self.ref_query() else {
            return;
        };
        let text = match item {
            RefItem::Block(page, block) => {
                let Some(id) = self
                    .pages
                    .get(page)
                    .and_then(|page| page.blocks.get(block))
                    .map(|b| b.id)
                else {
                    return;
                };
                format!("(({id}))")
            }
            RefItem::Page(page, _) => {
                let Some(page) = self.pages.get(page) else {
                    return;
                };
                if self.editor.text[range.end..].starts_with("]]") {
                    range.end += 2;
                }
                format!("[[{}]]", page.title)
            }
        };
        self.text_history_active = false;
        let before = self.history_state();
        self.editor.replace_range(range, &text);
        self.record_state(before);
        self.sync_content();
        self.save_page();
        cx.notify();
    }

    /// Click on a block reference: go to the referenced block's page and
    /// edit that block.
    fn open_block_ref(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some((page, _)) = find_block(&self.pages, id) else {
            return;
        };
        let title = self.pages[page].title.clone();
        self.navigate(&title, Nav::Replace, cx);
        // Navigating saves first, which can reorder pages: find it again.
        if let Some(ix) = self.pages[self.selected]
            .blocks
            .iter()
            .position(|b| b.id == id)
        {
            self.reveal(ix);
            self.start_edit(ix, window, cx);
        }
    }

    // --- persistence helpers -------------------------------------------------

    /// Save the selected page, then create any pages its `[[links]]` point to.
    ///
    /// Doing link-target creation here (rather than per keystroke) means typing
    /// `[[Ne` never creates a half-named page: links are only acted on once the
    /// block is committed.
    ///
    /// Blocks this page references with `((id))` get their id written to
    /// their own page first (once), so the reference still finds them after
    /// a restart.
    fn save_page(&mut self) {
        self.keep_referenced_ids();
        let page = &self.pages[self.selected];
        if let Err(err) = self.storage.save(page) {
            eprintln!("notesec: failed to save {}: {err}", page.title);
        }
        self.ensure_link_targets();
    }

    /// Mark every block the selected page references as one whose id is
    /// saved, and save the other pages that changed because of it.
    fn keep_referenced_ids(&mut self) {
        let ids: Vec<Uuid> = self.pages[self.selected]
            .blocks
            .iter()
            .flat_map(|b| parse_block_refs(&b.content))
            .map(|(_, id)| id)
            .collect();
        let mut changed = Vec::new();
        for id in ids {
            if let Some((page, _)) = find_block(&self.pages, id) {
                if self.pages[page].saved_ids.insert(id) && page != self.selected {
                    changed.push(page);
                }
            }
        }
        changed.sort_unstable();
        changed.dedup();
        for page in changed {
            if let Err(err) = self.storage.save(&self.pages[page]) {
                eprintln!("notesec: failed to save {}: {err}", self.pages[page].title);
            }
        }
    }

    /// Index of the page called `title`. Matching ignores case, like Logseq.
    fn find_page(&self, title: &str) -> Option<usize> {
        let wanted = title.to_lowercase();
        self.pages
            .iter()
            .position(|p| p.title.to_lowercase() == wanted)
    }

    /// The page a `[[link]]` (or `#tag`) to `name` goes to: the page with
    /// that title, else the first page declaring it as an `alias::` (see
    /// `model::resolve_page`).
    fn link_target(&self, name: &str) -> Option<usize> {
        resolve_page(&self.pages, name)
    }

    /// Add a page, keeping the sidebar sorted and `selected` pointing at the
    /// same page as before (sorting can shift indices). With a custom order
    /// the new page goes at the end of it.
    fn add_page(&mut self, page: Page) {
        if !page.is_journal && !self.state.page_order.is_empty() {
            self.state.page_order.push(page.title.clone());
            self.save_state();
        }
        self.pages.push(page);
        self.sort_pages();
    }

    /// Put `pages` in sidebar order (`sort_pages`, with the custom order
    /// from `state.toml`), keeping `selected` on the same page. Every
    /// re-sort goes through here: startup aside, that is adding, renaming,
    /// dropping a dragged page, "Sort pages A-Z" and undo/redo.
    fn sort_pages(&mut self) {
        let current = self
            .pages
            .get(self.selected)
            .map(|p| (p.title.clone(), p.is_journal));
        sort_pages(&mut self.pages, &self.state.page_order);
        if let Some((title, is_journal)) = current {
            self.selected = self
                .pages
                .iter()
                .position(|p| p.title == title && p.is_journal == is_journal)
                .unwrap_or(0);
        }
    }

    /// Create (and save) a page for every `[[link]]` on the selected page that
    /// doesn't have one yet (a link to an alias has one).
    fn ensure_link_targets(&mut self) {
        let targets: Vec<String> = self.pages[self.selected]
            .blocks
            .iter()
            .flat_map(|b| parse_references(&b.content))
            .map(|link| link.target)
            .collect();
        for target in targets {
            if self.link_target(&target).is_none() {
                let page = Page::with_empty_block(&target);
                if let Err(err) = self.storage.save(&page) {
                    eprintln!("notesec: failed to create page {target}: {err}");
                }
                self.add_page(page);
            }
        }
    }

    /// In-page navigation (a `[[link]]` or `#tag` in a block, a backlink, a
    /// graph node): show the page called `title` in the active tab, creating
    /// the page if needed. See `navigate`.
    fn open_page(&mut self, title: &str, cx: &mut Context<Self>) {
        self.navigate(title, Nav::Replace, cx);
    }

    /// Show the page called `title` (or that has it as an alias), creating
    /// it if needed, in a tab as `nav` says.
    fn navigate(&mut self, title: &str, nav: Nav, cx: &mut Context<Self>) {
        // Leave edit mode first so the block being edited is saved (which may
        // itself create pages and reorder the sidebar).
        self.stop_edit(cx);
        let ix = match self.link_target(title) {
            Some(ix) => ix,
            None => {
                let page = Page::with_empty_block(title);
                if let Err(err) = self.storage.save(&page) {
                    eprintln!("notesec: failed to create page {title}: {err}");
                }
                self.add_page(page);
                self.find_page(title).unwrap_or(0)
            }
        };
        self.show_page_in(ix, nav, cx);
    }

    /// Show page `ix` in its tab, or a new tab. Every navigation to a page
    /// (sidebar rows, favorites/recent, links, search, graph, Today, new
    /// page) goes through `show_page_in`, so the RECENT list sees all of
    /// them. Callers leave edit mode first (`stop_edit`), since saving can
    /// add pages and shift indices.
    fn show_page(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.show_page_in(ix, Nav::Tab, cx);
    }

    /// Show page `ix` in a tab as `nav` says. `Replace` only replaces a page
    /// tab: from the graph tab (or with no tabs) it opens or focuses a page
    /// tab instead, so the graph tab stays.
    fn show_page_in(&mut self, ix: usize, nav: Nav, cx: &mut Context<Self>) {
        let Some(page) = self.pages.get(ix) else {
            return;
        };
        // The focused right pane shows the page itself (it has no tabs).
        if let Some(split) = self.split.as_mut().filter(|s| s.right_focused) {
            split.right = page.title.clone();
            self.enter_page(ix, cx);
            return;
        }
        let target = TabTarget::Page(page.title.clone());
        match (nav, self.tabs.active_target()) {
            (Nav::Replace, Some(TabTarget::Page(_))) => self.tabs.replace_active(target),
            _ => self.tabs.open(target),
        }
        self.enter_page(ix, cx);
    }

    /// `show_page_in` for page `selected`.
    fn show_selected(&mut self, nav: Nav, cx: &mut Context<Self>) {
        self.show_page_in(self.selected, nav, cx);
    }

    /// Put page `ix` in the main pane and record it in RECENT. The tab bar
    /// is left alone: callers have already opened or focused its tab.
    fn enter_page(&mut self, ix: usize, cx: &mut Context<Self>) {
        // A trash confirmation belongs to the trash tab.
        self.trash_confirm = None;
        self.selected = ix;
        self.mode = Mode::Notes;
        self.record_recent();
        cx.notify();
    }

    /// Make `mode` and `selected` match the active tab, or the right pane's
    /// page while that pane has focus. A right pane whose page is gone is
    /// closed first.
    fn apply_tab(&mut self, cx: &mut Context<Self>) {
        self.prune_split();
        if let Some(ix) = self.right_focused().then(|| self.right_page()).flatten() {
            self.enter_page(ix, cx);
            return;
        }
        match self.tabs.active_target().cloned() {
            Some(TabTarget::Page(title)) => {
                // Focusing a page tab (click, Ctrl+Tab, or the neighbour after
                // a close) counts as a visit for RECENT.
                let ix = self.find_page(&title).unwrap_or(self.selected);
                self.enter_page(ix, cx);
            }
            Some(TabTarget::Graph) => {
                self.trash_confirm = None;
                self.refresh_graph(cx);
                self.mode = Mode::Graph;
                cx.notify();
            }
            // Built from the pages on every render, so it is never stale.
            Some(TabTarget::Agenda) => {
                self.trash_confirm = None;
                self.mode = Mode::Agenda;
                cx.notify();
            }
            // Read from disk again, in case files changed meanwhile.
            Some(TabTarget::Trash) => {
                self.refresh_trash();
                self.mode = Mode::Trash;
                cx.notify();
            }
            None => {
                self.trash_confirm = None;
                self.mode = Mode::Empty;
                cx.notify();
            }
        }
    }

    /// Focus tab `ix` (clicking it). The block being edited is saved and
    /// editing ends, as with any navigation.
    fn activate_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.stop_edit(cx);
        self.leave_right_pane();
        self.tabs.select(ix);
        self.apply_tab(cx);
    }

    /// Close tab `ix` (its × or Ctrl+W), saving the block being edited
    /// first. Closing the last tab shows the empty state.
    fn close_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.stop_edit(cx);
        self.leave_right_pane();
        self.tabs.close(ix);
        self.apply_tab(cx);
    }

    /// The Ctrl-K palette, the settings panel, a page menu, the shortcuts
    /// list, a trash confirmation or the import clash dialog covers the
    /// page. All are modal, so the tab keys do nothing while one is open.
    fn overlay_open(&self) -> bool {
        self.search.is_some()
            || self.capture.is_some()
            || self.settings.is_some()
            || self.page_menu.is_some()
            || self.shortcuts_open
            || self.trash_confirm.is_some()
            || self.import_dialog_open()
            || self.vault_dialog_open()
    }

    /// Ctrl+W. Ignored while an overlay is open. In the focused right pane
    /// (which is like a single tab) it closes that pane.
    fn on_close_tab(&mut self, _: &CloseTab, _: &mut Window, cx: &mut Context<Self>) {
        if !self.overlay_open() {
            if self.right_focused() {
                self.close_pane(true, cx);
            } else if let Some(ix) = self.tabs.active {
                self.close_tab(ix, cx);
            }
        }
    }

    fn cycle_tabs(&mut self, forward: bool, cx: &mut Context<Self>) {
        if self.overlay_open() {
            return;
        }
        self.stop_edit(cx);
        self.leave_right_pane();
        self.tabs.cycle(forward);
        self.apply_tab(cx);
    }

    fn on_next_tab(&mut self, _: &NextTab, _: &mut Window, cx: &mut Context<Self>) {
        self.cycle_tabs(true, cx);
    }

    fn on_prev_tab(&mut self, _: &PrevTab, _: &mut Window, cx: &mut Context<Self>) {
        self.cycle_tabs(false, cx);
    }

    /// Index of the journal page for `title` (`YYYY-MM-DD`). Only journals
    /// match, so a regular page that happens to have a date as its name is
    /// never mistaken for the daily note.
    fn find_journal(&self, title: &str) -> Option<usize> {
        self.pages
            .iter()
            .position(|p| p.is_journal && p.title == title)
    }

    /// Show today's journal ("Today" button / Ctrl-J), creating it if it
    /// doesn't exist yet. Startup already creates it, but the app may have been
    /// left open past midnight, or the file deleted behind our back.
    fn open_today(&mut self, cx: &mut Context<Self>) {
        // Save the block being edited first (this may add pages and reorder
        // the sidebar, so look the journal up afterwards).
        self.stop_edit(cx);
        let today = today_title();
        if self.find_journal(&today).is_none() {
            let page = create_journal(&self.storage, &today);
            self.add_page(page);
        }
        let ix = self.find_journal(&today).unwrap_or(0);
        self.show_page(ix, cx);
    }

    // --- split panes (decision 40) ----------------------------------------------

    /// The right pane has focus.
    fn right_focused(&self) -> bool {
        self.split.as_ref().is_some_and(|s| s.right_focused)
    }

    /// The index of the right pane's page, if split and it still exists.
    fn right_page(&self) -> Option<usize> {
        self.split.as_ref().and_then(|s| self.find_page(&s.right))
    }

    /// Close the right pane if its page no longer exists (deleted, or gone
    /// after an undo), so it never shows a stale page.
    fn prune_split(&mut self) {
        if self.split.is_some() && self.right_page().is_none() {
            self.split = None;
        }
    }

    /// Give focus back to the left pane before a tab action (the tab bar
    /// belongs to it); the caller then applies the active tab.
    fn leave_right_pane(&mut self) {
        if let Some(split) = &mut self.split {
            split.right_focused = false;
        }
    }

    /// Split right (Ctrl+\): show the current page (the page last shown,
    /// from the graph, agenda or trash tab) in a right pane, and focus it.
    /// When already split, just focus the right pane.
    fn split_right(&mut self, cx: &mut Context<Self>) {
        if self.split.is_none() {
            self.stop_edit(cx);
            let right = self.pages[self.selected].title.clone();
            self.split = Some(Split {
                right,
                right_focused: false,
            });
        }
        self.focus_pane(true, cx);
    }

    /// Focus the right (`right`) or left pane. The block being edited is
    /// saved and editing stops: there is one editor, in the focused pane.
    fn focus_pane(&mut self, right: bool, cx: &mut Context<Self>) {
        if self.split.as_ref().is_none_or(|s| s.right_focused == right) {
            return;
        }
        self.stop_edit(cx);
        if let Some(split) = &mut self.split {
            split.right_focused = right;
        }
        self.apply_tab(cx);
    }

    /// Close the right (`right`) or left pane; the other one becomes the
    /// only one. Closing the left pane moves the right pane's page into the
    /// tab bar (its tab, or a new one), so the tabs stay as they were.
    fn close_pane(&mut self, right: bool, cx: &mut Context<Self>) {
        if self.split.is_none() {
            return;
        }
        self.stop_edit(cx);
        let Some(split) = self.split.take() else {
            return;
        };
        if !right {
            if let Some(ix) = self.find_page(&split.right) {
                let title = self.pages[ix].title.clone();
                self.tabs.open(TabTarget::Page(title));
            }
        }
        self.apply_tab(cx);
    }

    fn on_split_right(&mut self, _: &SplitRight, _: &mut Window, cx: &mut Context<Self>) {
        if !self.overlay_open() {
            self.split_right(cx);
        }
    }

    /// Ctrl+Shift+W: close the focused pane (nothing when not split).
    fn on_close_pane(&mut self, _: &ClosePane, _: &mut Window, cx: &mut Context<Self>) {
        if !self.overlay_open() {
            let right = self.right_focused();
            self.close_pane(right, cx);
        }
    }

    fn on_focus_other_pane(&mut self, _: &FocusOtherPane, _: &mut Window, cx: &mut Context<Self>) {
        if !self.overlay_open() {
            let right = !self.right_focused();
            self.focus_pane(right, cx);
        }
    }

    // --- page context menu: rename, delete, copy title ----------------------

    /// Right-click on a sidebar page: open its menu at `position`.
    fn open_page_menu(
        &mut self,
        title: String,
        position: gpui::Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        // Save the edited block first: Rename and Delete act on the file.
        self.stop_edit(cx);
        self.page_menu = Some(PageMenu {
            title,
            position,
            step: MenuStep::Menu,
        });
        cx.notify();
    }

    fn close_page_menu(&mut self, cx: &mut Context<Self>) {
        if self.page_menu.take().is_some() {
            // The rename field's layout is stale now.
            self.last_layout = None;
            self.last_bounds = None;
        }
        cx.notify();
    }

    /// Journals are named by their date (`journals/YYYY_MM_DD.md`), so only
    /// regular pages can be renamed.
    fn can_rename(&self, title: &str) -> bool {
        self.find_page(title)
            .is_some_and(|ix| !self.pages[ix].is_journal)
    }

    /// The last page can't be moved to the trash: the app always has a page
    /// to show (`pages[selected]`). That it could be restored later doesn't
    /// change this; the rule is about the pages loaded now.
    fn can_delete(&self) -> bool {
        self.pages.len() > 1
    }

    /// "Rename": turn the menu into a text field holding the title, all
    /// selected so typing replaces it.
    fn start_rename(&mut self, cx: &mut Context<Self>) {
        let Some(title) = self.page_menu.as_ref().map(|m| m.title.clone()) else {
            return;
        };
        if !self.can_rename(&title) {
            return;
        }
        let mut editor = EditorState::new(&title);
        editor.select_home();
        if let Some(menu) = &mut self.page_menu {
            menu.step = MenuStep::Rename {
                editor,
                error: None,
            };
        }
        cx.notify();
    }

    /// Enter in the rename field: rename, or show why not and stay open.
    fn confirm_rename(&mut self, cx: &mut Context<Self>) {
        let Some(PageMenu {
            title,
            step: MenuStep::Rename { editor, .. },
            ..
        }) = &self.page_menu
        else {
            return;
        };
        let (old, new) = (title.clone(), editor.text.clone());
        match self.rename_page(&old, &new, cx) {
            Ok(()) => self.close_page_menu(cx),
            Err(message) => {
                if let Some(PageMenu {
                    step: MenuStep::Rename { error, .. },
                    ..
                }) = &mut self.page_menu
                {
                    *error = Some(message);
                }
                cx.notify();
            }
        }
    }

    /// Rename the page called `old` to `new`: its file, its title, and
    /// everything that refers to it by title (selection, tabs, favorites,
    /// recent). `[[links]]` to the old name are left as they are (v1).
    /// Returns a message for the user if the name can't be used.
    fn rename_page(&mut self, old: &str, new: &str, cx: &mut Context<Self>) -> Result<(), String> {
        let new = validate_title(new)?;
        let ix = self
            .find_page(old)
            .ok_or_else(|| "This page no longer exists".to_string())?;
        if self.pages[ix].is_journal {
            return Err("Journal pages are named by their date".into());
        }
        let old = self.pages[ix].title.clone();
        if new == old {
            return Ok(());
        }
        // Another page with that name, ignoring case like links do (a
        // case-only rename of this page is fine).
        if self.find_page(&new).is_some_and(|other| other != ix) {
            return Err(format!("A page named \u{201c}{new}\u{201d} already exists"));
        }
        self.stop_edit(cx);
        self.storage
            .rename(&self.pages[ix], &new)
            .map_err(|err| format!("Could not rename the file: {err}"))?;

        self.pages[ix].rename(&new);
        // Favorites, recent and the custom order follow (in place) before
        // re-sorting, so the page keeps its spot in a custom order.
        if self.state.rename(&old, &new) {
            self.save_state();
        }
        self.sort_pages();
        self.tabs.rename(&old, &new);
        if let Some(split) = self.split.as_mut().filter(|s| s.right == old) {
            split.right = new.clone();
        }
        self.forget_history();
        if self.mode == Mode::Graph {
            self.refresh_graph(cx);
        }
        cx.notify();
        Ok(())
    }

    /// "Delete" -> the confirm dialog.
    fn ask_delete(&mut self, cx: &mut Context<Self>) {
        if !self.can_delete() {
            return;
        }
        if let Some(menu) = &mut self.page_menu {
            menu.step = MenuStep::ConfirmDelete { error: None };
        }
        cx.notify();
    }

    /// The confirm dialog's Delete button.
    fn confirm_delete(&mut self, cx: &mut Context<Self>) {
        let Some(title) = self.page_menu.as_ref().map(|m| m.title.clone()) else {
            return;
        };
        match self.delete_page(&title, cx) {
            Ok(()) => self.close_page_menu(cx),
            Err(message) => {
                if let Some(PageMenu {
                    step: MenuStep::ConfirmDelete { error },
                    ..
                }) = &mut self.page_menu
                {
                    *error = Some(message);
                }
                cx.notify();
            }
        }
    }

    /// Delete the page called `title`: move its file to the trash, and drop
    /// the page, its tabs (the neighbour tab takes focus, as when closing a
    /// tab) and its favorites/recent/order entries (a restored page comes
    /// back like a new one).
    fn delete_page(&mut self, title: &str, cx: &mut Context<Self>) -> Result<(), String> {
        let ix = self
            .find_page(title)
            .ok_or_else(|| "This page no longer exists".to_string())?;
        if !self.can_delete() {
            return Err("The only page can't be deleted".into());
        }
        self.stop_edit(cx);
        self.storage
            .trash(&self.pages[ix])
            .map_err(|err| format!("Could not move the file to the trash: {err}"))?;
        self.refresh_trash();

        let current = &self.pages[self.selected];
        let current = (current.title.clone(), current.is_journal);
        let removed = self.pages.remove(ix);
        // Keep `selected` valid; if it was the deleted page, `apply_tab`
        // below moves it to whatever the focused tab shows.
        self.selected = self
            .pages
            .iter()
            .position(|p| p.title == current.0 && p.is_journal == current.1)
            .unwrap_or(ix.min(self.pages.len() - 1));
        self.tabs
            .retain(|t| !matches!(t, TabTarget::Page(p) if *p == removed.title));
        if self.state.forget(&removed.title) {
            self.save_state();
        }
        self.forget_history();
        self.apply_tab(cx);
        Ok(())
    }

    /// Undo snapshots hold whole pages under their titles, and restoring
    /// one saves every page in it, so replaying a snapshot from before a
    /// rename or delete would write the old file back (and one from before
    /// a restore would drop the restored page). Those actions can't be
    /// undone (v1), and edits from before them can't either.
    fn forget_history(&mut self) {
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.text_history_active = false;
    }

    /// "Copy page title": put the title on the clipboard.
    fn copy_page_title(&mut self, cx: &mut Context<Self>) {
        if let Some(menu) = &self.page_menu {
            cx.write_to_clipboard(ClipboardItem::new_string(menu.title.clone()));
        }
        self.close_page_menu(cx);
    }

    // --- custom page order: drag to reorder, "Sort pages A-Z" -----------------

    /// Titles of the regular (non-journal) pages, in sidebar order.
    fn regular_titles(&self) -> Vec<String> {
        self.pages
            .iter()
            .filter(|p| !p.is_journal)
            .map(|p| p.title.clone())
            .collect()
    }

    /// The order of the regular pages after moving `dragged` to just before
    /// `before` (`None`: to the end), or `None` if that changes nothing or
    /// either page is unknown (journals aren't in this list).
    fn reordered(&self, dragged: &str, before: Option<&str>) -> Option<Vec<String>> {
        let mut order = self.regular_titles();
        let from = order.iter().position(|t| t == dragged)?;
        let to = match before {
            Some(before) => order.iter().position(|t| t == before)?,
            None => order.len(),
        };
        // Removing the page first shifts the places after it up by one.
        let to = if to > from { to - 1 } else { to };
        if to == from {
            return None;
        }
        let title = order.remove(from);
        order.insert(to, title);
        Some(order)
    }

    /// A page is dragged over the drop zone `zone` (a page row by title, or
    /// `None` for the end-of-list zone). In the zone's upper half the page
    /// would go before `upper`, in its lower half before `lower` (`None`:
    /// at the end). Every zone hears every move, so a zone only clears the
    /// drop position it set itself.
    fn drag_over_zone(
        &mut self,
        event: &DragMoveEvent<DraggedPage>,
        zone: Option<String>,
        upper: Option<String>,
        lower: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let position = event.event.position;
        if !event.bounds.dilate(px(ROW_GAP_HALF)).contains(&position) {
            if self.page_drop.as_ref().is_some_and(|d| d.zone == zone) {
                self.page_drop = None;
                cx.notify();
            }
            return;
        }
        let before = if position.y < event.bounds.center().y {
            upper
        } else {
            lower
        };
        let dragged = event.drag(cx).title.clone();
        // No indicator where dropping would leave the order as it is.
        let drop = self
            .reordered(&dragged, before.as_deref())
            .map(|_| PageDrop { zone, before });
        if self.page_drop != drop {
            self.page_drop = drop;
            cx.notify();
        }
    }

    /// A dragged page was released over the sidebar: move it to the drop
    /// position shown, and save that as the custom order.
    fn drop_page(&mut self, dragged: &str, cx: &mut Context<Self>) {
        if let Some(drop) = self.page_drop.take() {
            if let Some(order) = self.reordered(dragged, drop.before.as_deref()) {
                self.state.page_order = order;
                self.save_state();
                self.sort_pages();
            }
        }
        cx.notify();
    }

    /// "Sort pages A-Z" (palette or page menu): forget the custom order.
    fn sort_pages_az(&mut self, cx: &mut Context<Self>) {
        self.close_page_menu(cx);
        if !self.state.page_order.is_empty() {
            self.state.page_order.clear();
            self.save_state();
            self.sort_pages();
        }
        cx.notify();
    }

    // --- agenda ----------------------------------------------------------------

    /// "Open agenda" (palette, sidebar): open or focus the agenda tab.
    fn show_agenda(&mut self, cx: &mut Context<Self>) {
        // Save the block being edited so the agenda sees it.
        self.stop_edit(cx);
        self.leave_right_pane();
        self.tabs.open(TabTarget::Agenda);
        self.apply_tab(cx);
    }

    /// The agenda as of now.
    fn agenda(&self) -> Agenda {
        Agenda::build(&self.pages, chrono::Local::now().date_naive())
    }

    /// Clicking an agenda item: open its page in a tab (the agenda tab
    /// stays, like the graph's) and unfold the task's block. It is not put
    /// in edit mode: the block holds the date marker on a second line, and
    /// the editor is single-line on this branch.
    fn open_agenda_item(&mut self, page: &str, block: uuid::Uuid, cx: &mut Context<Self>) {
        self.navigate(page, Nav::Tab, cx);
        if let Some(ix) = self.pages[self.selected]
            .blocks
            .iter()
            .position(|b| b.id == block)
        {
            self.reveal(ix);
        }
        cx.notify();
    }

    // --- trash -----------------------------------------------------------------

    /// "Open trash" (palette, sidebar): open or focus the trash tab.
    fn show_trash(&mut self, cx: &mut Context<Self>) {
        self.stop_edit(cx);
        self.trash_error = None;
        self.leave_right_pane();
        self.tabs.open(TabTarget::Trash);
        self.apply_tab(cx);
    }

    /// Read the trash list from disk again.
    fn refresh_trash(&mut self) {
        self.trash = self.storage.list_trash();
    }

    /// The trash entry with folder `id`, if it is still listed.
    fn trash_entry(&self, id: &str) -> Option<TrashEntry> {
        self.trash.iter().find(|e| e.id == id).cloned()
    }

    /// "Restore": move the page's file back and load it, so it is in the
    /// sidebar, graph, agenda and search straight away (at the end of a
    /// custom order, like a new page). Refused, with the reason shown in
    /// the trash view, if a page with that name exists again (ignoring
    /// case for regular pages, as links do; the same date for journals):
    /// the user renames or deletes that one first. Nothing is renamed
    /// automatically, since `[[links]]` find pages by name.
    fn restore_from_trash(&mut self, id: &str, cx: &mut Context<Self>) {
        self.trash_error = None;
        if let Err(message) = self.try_restore(id, cx) {
            self.trash_error = Some(message);
        }
        self.refresh_trash();
        cx.notify();
    }

    fn try_restore(&mut self, id: &str, cx: &mut Context<Self>) -> Result<(), String> {
        let entry = self
            .trash_entry(id)
            .ok_or_else(|| "This page is no longer in the trash".to_string())?;
        let taken = if entry.is_journal {
            self.find_journal(&entry.title).is_some()
        } else {
            self.find_page(&entry.title).is_some()
        };
        let taken_message = || {
            format!(
                "Can't restore \u{201c}{}\u{201d}: a page with that name exists. \
                 Rename or delete it first.",
                entry.title
            )
        };
        if taken {
            return Err(taken_message());
        }
        self.stop_edit(cx);
        let page = self.storage.restore(&entry).map_err(|err| {
            if err.kind() == std::io::ErrorKind::AlreadyExists {
                taken_message()
            } else {
                format!("Could not restore the file: {err}")
            }
        })?;
        self.add_page(page);
        self.forget_history();
        Ok(())
    }

    /// "Delete forever" on a trash row: ask first.
    fn ask_delete_forever(&mut self, id: &str, cx: &mut Context<Self>) {
        self.trash_error = None;
        if let Some(entry) = self.trash_entry(id) {
            self.trash_confirm = Some(TrashConfirm::DeleteForever(entry));
        }
        cx.notify();
    }

    /// "Empty trash": ask first (nothing to ask with an empty trash).
    fn ask_empty_trash(&mut self, cx: &mut Context<Self>) {
        self.trash_error = None;
        if !self.trash.is_empty() {
            self.trash_confirm = Some(TrashConfirm::Empty);
        }
        cx.notify();
    }

    fn close_trash_confirm(&mut self, cx: &mut Context<Self>) {
        self.trash_confirm = None;
        cx.notify();
    }

    /// The confirmation's danger button: delete the entry, or every entry,
    /// for good. The loaded pages are not affected.
    fn confirm_trash(&mut self, cx: &mut Context<Self>) {
        let result = match self.trash_confirm.take() {
            Some(TrashConfirm::DeleteForever(entry)) => self.storage.delete_forever(&entry),
            Some(TrashConfirm::Empty) => self.storage.empty_trash().map(|_| ()),
            None => Ok(()),
        };
        if let Err(err) = result {
            self.trash_error = Some(format!("Could not delete from the trash: {err}"));
        }
        self.refresh_trash();
        cx.notify();
    }

    // --- export, status message ------------------------------------------------

    /// "Export page to HTML": write the page on screen as one
    /// self-contained HTML file (`export::page_html_with_embeds`) to
    /// `<graph>/exports/<page file>.html`, replacing an earlier export, and
    /// say where. The block being edited is saved first. Folded blocks are
    /// exported unfolded; images are embedded.
    fn export_html(&mut self, cx: &mut Context<Self>) {
        self.stop_edit(cx);
        let page = &self.pages[self.selected];
        let pages = &self.pages;
        let resolve_ref =
            |id| find_block(pages, id).map(|(p, b)| pages[p].blocks[b].content.clone());
        let root = self.storage.root();
        let load_image = |target: &str| {
            resolve(root, target)
                .filter(|path| is_image_path(path))
                .and_then(|path| std::fs::read(path).ok())
        };
        let resolver = crate::embed::Resolver::new(pages, None);
        let host = Some(self.selected);
        let resolve_embeds = |content: &str| resolver.resolve(content, host);
        let html =
            crate::export::page_html_with_embeds(page, &resolve_ref, &resolve_embeds, &load_image);
        let status = match self.storage.write_export(page, &html) {
            Ok(path) => Status {
                text: format!("Exported to {}", path.display()),
                error: false,
            },
            Err(err) => Status {
                text: format!("Export failed: {err}"),
                error: true,
            },
        };
        self.show_status(status, cx);
    }

    /// Show `status` at the bottom right for `STATUS_FOR`.
    fn show_status(&mut self, status: Status, cx: &mut Context<Self>) {
        self.status = Some(status);
        self.status_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(STATUS_FOR).await;
            let _ = this.update(cx, |this, cx| {
                this.status = None;
                cx.notify();
            });
        }));
        cx.notify();
    }

    // --- graph view ------------------------------------------------------------

    /// Open (or focus) the graph tab, creating the graph on first use.
    fn show_graph(&mut self, cx: &mut Context<Self>) {
        // Save the block being edited so the graph sees up-to-date links.
        self.stop_edit(cx);
        self.leave_right_pane();
        self.tabs.open(TabTarget::Graph);
        self.apply_tab(cx);
    }

    /// "Toggle local graph" (palette): show the graph tab and switch it
    /// between the whole graph and the current page's neighbourhood.
    fn toggle_local_graph(&mut self, cx: &mut Context<Self>) {
        self.show_graph(cx);
        if let Some(graph) = self.graph.clone() {
            graph.update(cx, |g, cx| g.toggle_scope(cx));
        }
    }

    /// Create the graph view, or give it the current pages.
    fn refresh_graph(&mut self, cx: &mut Context<Self>) {
        let current = self.pages[self.selected].title.clone();
        let pages = self.pages.clone();
        if let Some(graph) = self.graph.clone() {
            graph.update(cx, |g, cx| g.refresh(pages, current, cx));
        } else {
            let (theme, size, reduce_motion) = (self.theme, self.ui_size(), cx.reduce_motion());
            let graph = cx.new(|_| GraphView::new(pages, current, theme, size, reduce_motion));
            // Clicking a node asks us to open that page. From the graph tab
            // that opens or focuses a page tab (see `show_selected`).
            self._graph_subscription = Some(cx.subscribe(
                &graph,
                |this, _graph, event: &GraphEvent, cx| {
                    let GraphEvent::OpenPage(title) = event;
                    this.open_page(title, cx);
                },
            ));
            self.graph = Some(graph);
        }
    }

    /// Ctrl-G / "Graph view": open or focus the graph tab; from the graph
    /// tab itself, close it.
    fn toggle_graph(&mut self, cx: &mut Context<Self>) {
        if self.mode == Mode::Graph {
            if let Some(ix) = self.tabs.active {
                self.close_tab(ix, cx);
            }
        } else {
            self.show_graph(cx);
        }
    }

    fn on_toggle_graph(&mut self, _: &ToggleGraph, _: &mut Window, cx: &mut Context<Self>) {
        self.toggle_graph(cx);
    }

    /// Push theme and font changes into the graph view if it exists.
    fn sync_graph_style(&self, cx: &mut Context<Self>) {
        if let Some(graph) = &self.graph {
            let (theme, size) = (self.theme, self.ui_size());
            graph.update(cx, |g, cx| g.set_style(theme, size, cx));
        }
    }

    /// Copy the editor's text into the block being edited. Returns true if the
    /// block's content actually changed.
    fn sync_content(&mut self) -> bool {
        let Some(ix) = self.editing else { return false };
        // A whiteboard card keeps its x::/y::/… lines (decision 53).
        let content = self.editor_content(ix);
        let block = &mut self.pages[self.selected].blocks[ix];
        if block.content == content {
            return false;
        }
        block.content = content;
        true
    }

    /// Write the editor text back to its block and save if anything changed.
    fn commit(&mut self) {
        self.close_slash_as_typing();
        if self.sync_content() {
            self.save_page();
        }
    }

    // --- editing lifecycle ---------------------------------------------------

    /// Load block `ix` into the shared editor. `cursor_at_start` places the
    /// cursor at the beginning (used for a freshly split block); otherwise at
    /// the end.
    fn load_editor(&mut self, ix: usize, cursor_at_start: bool) {
        let content = self.editor_source(ix);
        self.editor = EditorState::new(&content);
        self.slash = None;
        self.selecting = false;
        // The last layout belongs to the previous block until the next paint;
        // without it, mouse positions fall back to the cursor.
        self.last_layout = None;
        self.last_bounds = None;
        if cursor_at_start {
            self.editor.cursor = 0;
        }
        self.editing = Some(ix);
        // Whatever route led here (search, Enter, Backspace, undo), the
        // edited block must be visible.
        self.reveal(ix);
    }

    /// Unfold every ancestor of block `ix` on the current page.
    fn reveal(&mut self, ix: usize) {
        for id in self.pages[self.selected].ancestor_ids(ix) {
            self.collapsed.remove(&id);
        }
    }

    /// The nearest visible block before (`forward == false`) or after block
    /// `ix`, skipping blocks hidden in folded subtrees.
    fn visible_neighbor(&self, ix: usize, forward: bool) -> Option<usize> {
        let visible = self.pages[self.selected].visible_blocks(&self.collapsed);
        if forward {
            (ix + 1..visible.len()).find(|&i| visible[i])
        } else {
            (0..ix).rev().find(|&i| visible[i])
        }
    }

    /// Fold arrow click: fold or unfold block `ix`. Folding a block whose
    /// descendant is being edited leaves edit mode (saving it). UI-only, so
    /// not an undo step.
    fn toggle_fold(&mut self, ix: usize, cx: &mut Context<Self>) {
        let page = &self.pages[self.selected];
        let id = page.blocks[ix].id;
        if !self.collapsed.remove(&id) {
            self.collapsed.insert(id);
            let subtree = ix + 1..page.subtree_end(ix);
            if self.editing.is_some_and(|e| subtree.contains(&e)) {
                self.stop_edit(cx);
            }
        }
        cx.notify();
    }

    fn start_edit(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        // Never edit under the settings panel, a page menu or the shortcuts
        // list (e.g. Ctrl-N while one is open).
        self.settings = None;
        self.page_menu = None;
        self.shortcuts_open = false;
        self.trash_confirm = None;
        self.commit();
        self.text_history_active = false;
        self.load_editor(ix, false);
        self.vim.reset();
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn stop_edit(&mut self, cx: &mut Context<Self>) {
        self.vim.reset();
        self.commit();
        self.text_history_active = false;
        self.editing = None;
        self.last_layout = None;
        self.last_bounds = None;
        cx.notify();
    }

    /// Commit the current block and start editing block `ix` (arrow-key
    /// navigation between blocks).
    fn move_edit(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.commit();
        self.text_history_active = false;
        self.load_editor(ix, false);
        cx.notify();
    }

    // --- action handlers -----------------------------------------------------

    /// Enter: split the block at the cursor; the right half becomes a new block.
    fn enter(&mut self, _: &Enter, window: &mut Window, cx: &mut Context<Self>) {
        if self.vault_dialog_open() {
            return self.confirm_vault(cx);
        }
        if self.renaming() {
            self.confirm_rename(cx);
            return;
        }
        if self.search.is_some() {
            self.confirm_search(window, cx);
            return;
        }
        if self.capture.is_some() {
            self.confirm_quick_capture(cx);
            return;
        }
        if self.ai_enter(window, cx) {
            return;
        }
        if let Some(state) = &self.slash {
            if let Some(&kind) = self.slash_matches().get(state.menu.selected) {
                self.apply_slash(kind, cx);
            }
            return;
        }
        if let Some(item) = self.ref_matches().get(self.ref_highlight()).cloned() {
            self.insert_ref(item, cx);
            return;
        }
        let Some(ix) = self.editing else { return };
        if self.card_editing() {
            // A whiteboard card has no "next block": Enter finishes it.
            return self.stop_edit(cx);
        }
        self.text_history_active = false;
        self.record_edit();
        let rest = self.editor.split_off_at_cursor();
        self.sync_content();
        let page = &mut self.pages[self.selected];
        // On a folded block the new block goes after its hidden subtree
        // rather than becoming its (hidden) first child.
        let folded = self.collapsed.contains(&page.blocks[ix].id) && page.descendant_count(ix) > 0;
        let new_ix = if folded {
            page.insert_after_subtree(ix, rest)
        } else {
            page.insert_after(ix, rest)
        };
        self.save_page();
        self.load_editor(new_ix, true);
        cx.notify();
    }

    fn tab(&mut self, _: &Tab, _: &mut Window, cx: &mut Context<Self>) {
        if self.vault_dialog_open() {
            return self.vault_next_field(cx);
        }
        self.restructure(cx, |page, ix| page.indent(ix));
    }

    fn shift_tab(&mut self, _: &ShiftTab, _: &mut Window, cx: &mut Context<Self>) {
        if self.vault_dialog_open() {
            return self.vault_next_field(cx);
        }
        self.restructure(cx, |page, ix| page.outdent(ix));
    }

    /// Shared body of Tab / Shift-Tab: apply `op` to the edited block, then
    /// save. The block keeps its index (document order never changes).
    fn restructure(&mut self, cx: &mut Context<Self>, op: impl FnOnce(&mut Page, usize) -> bool) {
        let Some(ix) = self.editing else { return };
        if self.card_editing() {
            return; // cards aren't nested
        }
        self.close_slash_as_typing();
        self.text_history_active = false;
        let before = self.history_state();
        self.sync_content();
        if op(&mut self.pages[self.selected], ix) {
            self.record_state(before);
            self.save_page();
            // Indenting under a folded block unfolds it.
            self.reveal(ix);
        }
        cx.notify();
    }

    fn on_move_block_up(&mut self, _: &MoveBlockUp, _: &mut Window, cx: &mut Context<Self>) {
        self.move_edited_block(true, cx);
    }

    fn on_move_block_down(&mut self, _: &MoveBlockDown, _: &mut Window, cx: &mut Context<Self>) {
        self.move_edited_block(false, cx);
    }

    /// Alt+Up / Alt+Down: swap the edited block (and its children) with its
    /// previous / next sibling. Editing continues in the moved block.
    fn move_edited_block(&mut self, up: bool, cx: &mut Context<Self>) {
        if self.card_editing() {
            return;
        }
        let Some(ix) = self.editing else { return };
        if self.search.is_some() {
            return;
        }
        self.close_slash_as_typing();
        self.text_history_active = false;
        let before = self.history_state();
        self.sync_content();
        let page = &mut self.pages[self.selected];
        let moved = if up {
            page.move_up(ix)
        } else {
            page.move_down(ix)
        };
        if let Some(new_ix) = moved {
            self.record_state(before);
            self.editing = Some(new_ix);
            self.save_page();
            self.reveal(new_ix);
        }
        cx.notify();
    }

    /// A block drag moved over the gap `gap`: show the drop line there,
    /// unless dropping there would put the block inside itself.
    fn set_block_drop(&mut self, dragged: Uuid, gap: Option<DropGap>, cx: &mut Context<Self>) {
        let page = &self.pages[self.selected];
        let gap = gap.filter(
            |gap| match (gap, page.blocks.iter().position(|b| b.id == dragged)) {
                (DropGap::Before(target), Some(from)) => {
                    let end = page.subtree_end(from);
                    !page.blocks[from..end].iter().any(|b| b.id == *target)
                }
                (DropGap::End, Some(_)) => true,
                (_, None) => false,
            },
        );
        if self.block_drop != gap {
            self.block_drop = gap;
            cx.notify();
        }
    }

    /// Drop of a dragged block: move it (with its children) to the gap the
    /// drop line shows. One undo step; the page is saved, so the new order
    /// survives a restart. A block being edited stays in edit mode.
    fn drop_block(&mut self, dragged: Uuid, cx: &mut Context<Self>) {
        let Some(gap) = self.block_drop.take() else {
            return;
        };
        self.close_slash_as_typing();
        self.text_history_active = false;
        let before = self.history_state();
        self.sync_content();
        let page = &self.pages[self.selected];
        let editing_id = self.editing.map(|ix| page.blocks[ix].id);
        let Some(from) = page.blocks.iter().position(|b| b.id == dragged) else {
            return;
        };
        let target = match gap {
            DropGap::Before(id) => match page.blocks.iter().position(|b| b.id == id) {
                Some(ix) => Some(ix),
                None => return,
            },
            DropGap::End => None,
        };
        let page = &mut self.pages[self.selected];
        if let Some(new_ix) = page.move_subtree(from, target) {
            self.editing = editing_id.and_then(|id| page.blocks.iter().position(|b| b.id == id));
            self.record_state(before);
            self.save_page();
            self.reveal(new_ix);
        }
        cx.notify();
    }

    fn backspace(&mut self, _: &Backspace, _: &mut Window, cx: &mut Context<Self>) {
        if self.text_input_open() {
            self.active_editor_mut().backspace();
            self.text_changed();
            cx.notify();
            return;
        }
        let Some(ix) = self.editing else { return };
        if self.slash.is_some() {
            // Edits the filter; deleting the "/" itself closes the menu.
            self.editor.backspace();
            self.refresh_slash();
            cx.notify();
            return;
        }
        if !self.editor.text.is_empty() {
            self.text_history_active = false;
            let before = self.history_state();
            if self.editor.backspace() {
                self.record_state(before);
            }
            cx.notify();
            return;
        }
        if self.card_editing() {
            // An emptied card stays (Delete on the canvas removes it).
            return;
        }
        // Empty block: delete it (never the last remaining block, and never a
        // block that still has children).
        self.text_history_active = false;
        let before = self.history_state();
        // The visible block above (not one hidden in a folded subtree).
        let above = self.visible_neighbor(ix, false).unwrap_or(0);
        let page = &mut self.pages[self.selected];
        if page.blocks.len() > 1 && page.delete_leaf(ix) {
            self.record_state(before);
            self.save_page();
            // Move to the block above (or the new first block if we deleted #0).
            self.load_editor(above, false);
        }
        cx.notify();
    }

    fn delete(&mut self, _: &Delete, _: &mut Window, cx: &mut Context<Self>) {
        // With the menu open, the "/query" becomes text before deleting.
        self.close_slash_as_typing();
        if self.editing.is_some() || self.text_input_open() {
            self.text_history_active = false;
            let before = self.history_state();
            if self.active_editor_mut().delete() && !self.text_input_open() {
                self.record_state(before);
            }
            self.text_changed();
            cx.notify();
        }
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        self.close_slash_as_typing();
        self.active_editor_mut().move_left();
        cx.notify();
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        self.close_slash_as_typing();
        self.active_editor_mut().move_right();
        cx.notify();
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.close_slash_as_typing();
        self.active_editor_mut().move_home();
        cx.notify();
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.close_slash_as_typing();
        self.active_editor_mut().move_end();
        cx.notify();
    }

    /// Shared body of the Shift+arrow actions: extend or shrink the selection.
    fn extend_selection(&mut self, cx: &mut Context<Self>, op: impl FnOnce(&mut EditorState)) {
        self.close_slash_as_typing();
        op(self.active_editor_mut());
        cx.notify();
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.extend_selection(cx, EditorState::select_left);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.extend_selection(cx, EditorState::select_right);
    }

    fn select_home(&mut self, _: &SelectHome, _: &mut Window, cx: &mut Context<Self>) {
        self.extend_selection(cx, EditorState::select_home);
    }

    fn select_end(&mut self, _: &SelectEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.extend_selection(cx, EditorState::select_end);
    }

    // --- mouse selection in the edited block -----------------------------------
    // Same approach as gpui's `examples/input.rs`: mouse-down places the
    // cursor (Shift extends instead), moves while the button is held select,
    // mouse-up ends it. A click without a drag therefore clears the selection.

    /// Byte offset in the edited text under `position`, from the layout
    /// painted last frame.
    fn index_for_mouse(&self, position: gpui::Point<Pixels>) -> usize {
        let len = self.editor.text.len();
        let (Some(bounds), Some(lines)) = (self.last_bounds, self.last_layout.as_ref()) else {
            return self.editor.cursor;
        };
        if position.y < bounds.top() {
            0
        } else if position.y > bounds.bottom() {
            len
        } else {
            lines.closest_index(position - bounds.origin).min(len)
        }
    }

    fn on_text_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.editing.is_none() || self.search.is_some() {
            return;
        }
        self.close_slash_as_typing();
        // A new cursor position starts a new undo step for typing.
        self.text_history_active = false;
        let ix = self.index_for_mouse(event.position);
        if event.modifiers.shift {
            self.editor.select_to(ix);
        } else {
            self.editor.set_cursor(ix);
        }
        self.selecting = true;
        cx.notify();
    }

    fn on_text_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selecting && self.editing.is_some() {
            let ix = self.index_for_mouse(event.position);
            self.editor.select_to(ix);
            cx.notify();
        }
    }

    fn on_text_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.selecting = false;
    }

    fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        if self.search.is_some() {
            self.move_search_selection(-1, cx);
            return;
        }
        if self.ai_move_selection(-1, cx) {
            return;
        }
        if self.slash.is_some() {
            self.move_slash_selection(-1, cx);
            return;
        }
        if self.ref_menu_open() {
            self.move_ref_selection(-1, cx);
            return;
        }
        if self.move_cursor_line(-1, cx) {
            return;
        }
        if self.card_editing() {
            return;
        }
        if let Some(prev) = self.editing.and_then(|ix| self.visible_neighbor(ix, false)) {
            self.move_edit(prev, cx);
        }
    }

    fn down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        if self.search.is_some() {
            self.move_search_selection(1, cx);
            return;
        }
        if self.ai_move_selection(1, cx) {
            return;
        }
        if self.slash.is_some() {
            self.move_slash_selection(1, cx);
            return;
        }
        if self.ref_menu_open() {
            self.move_ref_selection(1, cx);
            return;
        }
        if self.move_cursor_line(1, cx) {
            return;
        }
        if self.card_editing() {
            return;
        }
        if let Some(next) = self.editing.and_then(|ix| self.visible_neighbor(ix, true)) {
            self.move_edit(next, cx);
        }
    }

    /// The copy button on a code block: put its code on the clipboard and
    /// say "Copied" for a moment. A newer copy restarts the moment (the old
    /// timer is dropped, which cancels it).
    fn copy_code(&mut self, block: Uuid, n: usize, code: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(code));
        let which = (block, n);
        self.copied_code = Some(which);
        self.copied_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(COPIED_FOR).await;
            let _ = this.update(cx, |this, cx| {
                if this.copied_code == Some(which) {
                    this.copied_code = None;
                    cx.notify();
                }
            });
        }));
        cx.notify();
    }

    /// An image in reading view: as wide as it is up to the page width and
    /// at most `IMAGE_MAX_HEIGHT` tall, keeping its shape. A file that's
    /// missing or can't be decoded shows a dashed placeholder instead.
    fn render_image(
        &self,
        prefix: &'static str,
        ix: usize,
        n: usize,
        image: &ImageRef,
    ) -> AnyElement {
        let theme = self.theme;
        let placeholder = move |text: String| {
            div()
                .debug_selector(move || format!("{prefix}image-{ix}-{n}-missing"))
                .self_start()
                .px_3()
                .py_2()
                .rounded_md()
                .border_1()
                .border_dashed()
                .border_color(theme.border)
                .text_color(theme.muted)
                .child(text)
                .into_any_element()
        };
        let target = image.target.clone();
        match resolve(self.storage.root(), &image.target).filter(|path| path.is_file()) {
            Some(path) => gpui::img(path)
                .debug_selector(move || format!("{prefix}image-{ix}-{n}"))
                .self_start()
                .max_w_full()
                .max_h(px(IMAGE_MAX_HEIGHT))
                .object_fit(gpui::ObjectFit::Contain)
                .with_fallback(move || placeholder(format!("Image could not be loaded: {target}")))
                .into_any_element(),
            None => placeholder(format!("Image not found: {target}")),
        }
    }

    /// A fenced code block in reading view: monospace on the sidebar colour,
    /// whitespace kept, scrolling sideways when too wide. While the mouse is
    /// over it (or just after a copy) it shows a Copy button in its corner.
    fn render_code(
        &self,
        prefix: &'static str,
        ix: usize,
        block: Uuid,
        n: usize,
        code: &CodeBlock,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = self.theme;
        let font_size = self.ui_size();
        let which = (block, n);
        let copied = self.copied_code == Some(which);
        let show_button = copied || self.hovered_code == Some(which);
        let text = code.code.clone();
        let button = show_button.then(|| {
            div()
                .id("copy")
                .debug_selector(move || format!("{prefix}code-{ix}-{n}-copy"))
                .absolute()
                .top_1()
                .right_1()
                .px_2()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .bg(theme.bg)
                .text_size(px(font_size * 0.8))
                .text_color(if copied { theme.accent } else { theme.muted })
                .cursor_pointer()
                .hover(|d| d.text_color(theme.text))
                // A press here never starts editing the block.
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.copy_code(block, n, text.clone(), cx)
                }))
                .when(copied, |d| {
                    d.debug_selector(move || format!("{prefix}code-{ix}-{n}-copied"))
                })
                .child(if copied { "Copied" } else { "Copy" })
        });
        let lang = (!code.lang.is_empty()).then(|| {
            div()
                .text_size(px(font_size * 0.75))
                .text_color(theme.muted)
                .child(code.lang.clone())
        });
        div()
            .id(SharedString::from(format!("code-{ix}-{n}")))
            .debug_selector(move || format!("{prefix}code-{ix}-{n}"))
            .relative()
            .mt_1()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .bg(theme.sidebar_bg)
            .text_color(theme.text)
            .font_weight(FontWeight::NORMAL)
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                let now = hovered.then_some(which);
                if *hovered || this.hovered_code == Some(which) {
                    if this.hovered_code != now {
                        this.hovered_code = now;
                        cx.notify();
                    }
                }
            }))
            .child(
                div()
                    .id("code-scroll")
                    .px_3()
                    .py_2()
                    .overflow_x_scroll()
                    .flex()
                    .flex_col()
                    .children(lang)
                    .child(
                        div()
                            .whitespace_nowrap()
                            .text_size(px(self.mono_size()))
                            .when_some(self.mono_font.clone(), |d, font| d.font_family(font))
                            .child(code.code.clone()),
                    ),
            )
            .children(button)
            .into_any_element()
    }

    /// In a block with line breaks, move the cursor to the line above
    /// (`-1`) or below (`1`), keeping its horizontal position. False when
    /// there is no such line, so Up and Down move to the next block instead.
    fn move_cursor_line(&mut self, direction: isize, cx: &mut Context<Self>) -> bool {
        if self.editing.is_none() || self.text_input_open() {
            return false;
        }
        let Some(lines) = self.last_layout.as_ref() else {
            return false;
        };
        let cursor = self.editor.cursor;
        let Some(row) = lines.row_of(cursor).checked_add_signed(direction) else {
            return false;
        };
        if row >= lines.lines.len() {
            return false;
        }
        let x = lines.point_for_index(cursor).x;
        let target = lines
            .closest_index_in_row(row, x)
            .min(self.editor.text.len());
        self.text_history_active = false;
        self.editor.set_cursor(target);
        cx.notify();
        true
    }

    /// Shift+Enter: a line break inside the block (Enter starts a new
    /// block). Pipe tables are written this way, one row per line.
    fn new_line(&mut self, _: &NewLine, _: &mut Window, cx: &mut Context<Self>) {
        // The palette and the rename field are single-line (a page name
        // can't hold a line break).
        if self.editing.is_none()
            || self.text_input_open()
            || self.slash.is_some()
            || self.ref_menu_open()
        {
            return;
        }
        let before = self.history_state();
        self.text_history_active = false;
        self.editor.insert_line_break();
        self.record_state(before);
        self.text_changed();
        cx.notify();
    }

    fn escape(&mut self, _: &Escape, _: &mut Window, cx: &mut Context<Self>) {
        if self.vault_dialog_open() {
            self.close_vault_dialog(cx);
        } else if self.import_dialog_open() {
            self.answer_import_clash(None, cx);
        } else if self.trash_confirm.is_some() {
            self.close_trash_confirm(cx);
        } else if self.page_menu.is_some() {
            // Closes the menu, cancels a rename or a delete.
            self.close_page_menu(cx);
        } else if self.shortcuts_open {
            self.close_shortcuts(cx);
        } else if self.ai_escape(cx) {
            // Cancelled a Settings > AI field, or closed the Ask panel.
        } else if self.settings.is_some() {
            self.close_settings(cx);
        } else if self.search.is_some() {
            self.close_search(cx);
        } else if self.capture.is_some() {
            self.close_quick_capture(cx);
        } else if self.slash.is_some() {
            self.dismiss_slash(cx);
        } else if let (Some((range, _)), true) = (self.ref_query(), self.ref_menu_open()) {
            self.ref_dismissed = Some(range.start);
            cx.notify();
        } else if self.editor.selection().is_some() {
            // First Esc only drops the selection; the next one stops editing.
            self.editor.clear_selection();
            cx.notify();
        } else if !self.whiteboard_escape(cx) {
            self.stop_edit(cx);
        }
    }

    fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };
        // An image pasted into a block is saved under `assets/` and the
        // block gets a reference to it (an image beats any text alongside).
        let image = item.entries().iter().find_map(|entry| match entry {
            gpui::ClipboardEntry::Image(image) => Some(image.bytes.clone()),
            _ => None,
        });
        if let (Some(bytes), true) = (image, !self.text_input_open() && self.editing.is_some()) {
            self.add_images(vec![bytes], cx);
            return;
        }
        if let Some(text) = item.text() {
            self.close_slash_as_typing();
            if self.search.is_none() && self.editing.is_some() {
                self.record_edit();
            }
            self.text_history_active = false;
            self.active_editor_mut().insert(&text);
            self.text_changed();
            cx.notify();
        }
    }

    /// Image files dropped onto the page: each is copied into `assets/` like
    /// a pasted image. Other files are ignored, as are files that can't be
    /// read.
    fn drop_files(&mut self, paths: &ExternalPaths, cx: &mut Context<Self>) {
        let images: Vec<Vec<u8>> = paths
            .paths()
            .iter()
            .filter(|path| is_image_path(path))
            .filter_map(|path| match std::fs::read(path) {
                Ok(bytes) => Some(bytes),
                Err(err) => {
                    eprintln!("notesec: could not read {}: {err}", path.display());
                    None
                }
            })
            .collect();
        if !images.is_empty() {
            self.add_images(images, cx);
        }
    }

    /// The mouse was released somewhere. If it ends a drag of outside files
    /// over the page, take the drag and add the images.
    ///
    /// This doesn't use `on_drop`: GPUI only drops onto a hovered element,
    /// and nothing counts as hovered after a keypress until the mouse moves
    /// in the window. Files dragged in from the file manager move only the
    /// drag, so a drop right after typing in a block would be lost.
    fn finish_file_drop(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((paths, inside)) = self.file_drag.take() else {
            return;
        };
        if inside && cx.has_active_drag() {
            cx.stop_active_drag(window);
            self.drop_files(&paths, cx);
        }
        cx.notify();
    }

    /// Save each image as `assets/image-<timestamp>.png` and reference it:
    /// at the cursor of the edited block, or else in a new block at the end
    /// of the page, one per image. One undo step either way (the files stay).
    fn add_images(&mut self, images: Vec<Vec<u8>>, cx: &mut Context<Self>) {
        // The palette or the rename field has the keyboard: no block to
        // put them in, so don't save anything either.
        if self.text_input_open() {
            return;
        }
        let refs: Vec<String> = images
            .iter()
            .filter_map(|bytes| {
                let millis = chrono::Local::now().timestamp_millis();
                match save_image(self.storage.root(), bytes, millis) {
                    Ok(file) => Some(image_markdown(&file)),
                    Err(err) => {
                        eprintln!("notesec: could not save the image: {err}");
                        None
                    }
                }
            })
            .collect();
        if refs.is_empty() {
            return;
        }
        self.close_slash_as_typing();
        self.text_history_active = false;
        self.record_edit();
        if self.editing.is_some() {
            self.editor.insert(&refs.join(" "));
            self.sync_content();
            self.text_changed();
        } else {
            let page = &mut self.pages[self.selected];
            for markdown in refs {
                page.push_block(markdown);
            }
        }
        self.save_page();
        cx.notify();
    }

    /// Ctrl+Enter: next task state for the edited block (TODO -> DOING ->
    /// DONE -> none). One undo step, saved right away like other structural
    /// changes.
    fn cycle_task(&mut self, _: &CycleTask, _: &mut Window, cx: &mut Context<Self>) {
        if self.search.is_some() || self.editing.is_none() {
            return;
        }
        self.close_slash_as_typing();
        self.text_history_active = false;
        let before = self.history_state();
        self.editor.cycle_task();
        self.sync_content();
        self.record_state(before);
        self.save_page();
        cx.notify();
    }

    /// Checkbox click on block `ix` (not being edited): next task state,
    /// without editing it. Any block being edited is committed first. One
    /// undo step, saved right away.
    fn cycle_block_task(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.commit();
        self.text_history_active = false;
        let before = self.history_state();
        let block = &mut self.pages[self.selected].blocks[ix];
        block.content = cycle_task(&block.content);
        self.record_state(before);
        self.save_page();
        cx.notify();
    }

    fn bold(&mut self, _: &Bold, _: &mut Window, cx: &mut Context<Self>) {
        self.toggle_emphasis(Emphasis::Bold, cx);
    }

    fn italic(&mut self, _: &Italic, _: &mut Window, cx: &mut Context<Self>) {
        self.toggle_emphasis(Emphasis::Italic, cx);
    }

    /// Ctrl+B / Ctrl+I in a block (not the search box). One undo step of its
    /// own; saved with the rest of the text when editing ends, like typing.
    fn toggle_emphasis(&mut self, emphasis: Emphasis, cx: &mut Context<Self>) {
        if self.search.is_some() || self.editing.is_none() {
            return;
        }
        self.close_slash_as_typing();
        self.text_history_active = false;
        let before = self.history_state();
        self.editor.toggle_emphasis(emphasis);
        if self.editor.text != before.editor.text {
            self.record_state(before);
        }
        cx.notify();
    }
}

/// A pipe table in reading view. It is laid out column by column, each
/// column as wide as its widest cell, so cells line up whatever their text;
/// every cell is one line high, so rows line up too. The header row is bold
/// on the sidebar colour, and a table wider than the page scrolls sideways.
fn table_grid(
    theme: &Theme,
    name: String,
    rows: Vec<Vec<StyledText>>,
    align: &[Align],
) -> impl IntoElement {
    let row_count = rows.len();
    let mut columns: Vec<Vec<StyledText>> = (0..align.len()).map(|_| Vec::new()).collect();
    for row in rows {
        for (c, cell) in row.into_iter().enumerate() {
            columns[c].push(cell);
        }
    }
    let column_count = columns.len();
    let columns = columns.into_iter().enumerate().map(|(c, cells)| {
        let align = align[c];
        let name = name.clone();
        div()
            .flex()
            .flex_col()
            .flex_none()
            .when(c + 1 < column_count, |col| {
                col.border_r_1().border_color(theme.border)
            })
            .children(cells.into_iter().enumerate().map(move |(r, cell)| {
                let name = name.clone();
                div()
                    .debug_selector(move || format!("{name}-cell-{r}-{c}"))
                    .flex()
                    .flex_row()
                    .px_2()
                    .py(px(2.0))
                    .whitespace_nowrap()
                    .when(align == Align::Center, |d| d.justify_center())
                    .when(align == Align::Right, |d| d.justify_end())
                    .when(r == 0, |d| {
                        d.font_weight(FontWeight::BOLD).bg(theme.sidebar_bg)
                    })
                    .when(r + 1 < row_count, |d| {
                        d.border_b_1().border_color(theme.border)
                    })
                    .child(cell)
            }))
    });
    let columns: Vec<_> = columns.collect();
    div()
        .id(SharedString::from(name.clone()))
        .debug_selector(move || name)
        .overflow_x_scroll()
        .max_w_full()
        .child(
            div()
                .flex()
                .flex_row()
                .flex_none()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .overflow_hidden()
                .children(columns),
        )
}

/// Highlights for a block in reading view: link/tag styling with bold and
/// italic on top. Heading and quote styling come from the row and still apply
/// underneath (`StyledText::with_highlights` resolves against it).
fn reading_highlights(
    display: &DisplayBlock,
    link_style: HighlightStyle,
    tag_style: HighlightStyle,
    ref_style: HighlightStyle,
) -> Vec<(Range<usize>, HighlightStyle)> {
    display
        .segments()
        .into_iter()
        .map(|(range, format)| {
            let mut style = if format.block_ref {
                ref_style
            } else if format.tag {
                tag_style
            } else if format.link {
                link_style
            } else {
                HighlightStyle::default()
            };
            if format.bold {
                style.font_weight = Some(FontWeight::BOLD);
            }
            if format.italic {
                style.font_style = Some(FontStyle::Italic);
            }
            (range, style)
        })
        .collect()
}

/// Create and save an empty journal page for `title` (`YYYY-MM-DD`). Storage
/// turns the title into Logseq's `journals/YYYY_MM_DD.md` file name. A failed
/// save is reported but the page is still returned, so the user can type and
/// the next save can retry.
/// `family` as a font to render with, if it is installed. GPUI would
/// otherwise fall back silently and the user would not know why.
/// Monospace fonts for code blocks, most preferred first.
const MONO_FONTS: [&str; 12] = [
    "JetBrains Mono",
    "Fira Code",
    "Cascadia Code",
    "Source Code Pro",
    "DejaVu Sans Mono",
    "Liberation Mono",
    "Noto Sans Mono",
    "Ubuntu Mono",
    "Hack",
    "Menlo",
    "Consolas",
    "Courier New",
];

/// The first installed font of `MONO_FONTS` among `names`, if any (the editor
/// and code then fall back to the UI font, quietly: it still reads fine).
fn auto_mono_font(names: &[String]) -> Option<SharedString> {
    MONO_FONTS
        .iter()
        .find(|font| names.iter().any(|name| name == *font))
        .map(|font| SharedString::from(*font))
}

/// The editor's font: `chosen` (from Settings) if that family is installed,
/// else the first installed font of `MONO_FONTS`. A chosen family that is
/// missing falls back quietly to the automatic one rather than to a
/// proportional font, so the editor stays monospace.
fn pick_mono_font(chosen: Option<&str>, names: &[String]) -> Option<SharedString> {
    chosen
        .filter(|family| names.iter().any(|name| name == family))
        .map(|family| SharedString::from(family.to_string()))
        .or_else(|| auto_mono_font(names))
}

fn resolve_mono_font(chosen: Option<&str>, cx: &App) -> Option<SharedString> {
    let names = cx.text_system().all_font_names();
    if let Some(family) = chosen {
        if !names.iter().any(|name| name == family) {
            eprintln!("notesec: font {family:?} is not installed; using the default editor font");
        }
    }
    pick_mono_font(chosen, &names)
}

/// The tallest an image in a block is drawn, in pixels.
const IMAGE_MAX_HEIGHT: f32 = 320.0;
/// What "Git backup on" says about where commits go: a new repository in
/// the graph folder, the graph's own repository, or (a repository above
/// the graph) that only the graph folder is committed there.
fn backup_on_message(graph: &std::path::Path, repo: &Repo) -> String {
    let same = |a: &std::path::Path, b: &std::path::Path| {
        a == b
            || a.canonicalize()
                .ok()
                .is_some_and(|a| b.canonicalize().ok() == Some(a))
    };
    if repo.created {
        format!("Git backup on: new repository in {}", graph.display())
    } else if same(&repo.root, graph) {
        format!("Git backup on: {}", graph.display())
    } else {
        format!(
            "Git backup on: committing {} in the repository at {}",
            graph.display(),
            repo.root.display()
        )
    }
}

/// How long a status message (`NoteSec::show_status`) stays on screen.
const STATUS_FOR: std::time::Duration = std::time::Duration::from_secs(5);

/// How long a copy button says "Copied".
const COPIED_FOR: std::time::Duration = std::time::Duration::from_millis(1500);

fn installed_font(family: &str, cx: &App) -> Option<SharedString> {
    if cx
        .text_system()
        .all_font_names()
        .iter()
        .any(|f| f == family)
    {
        Some(SharedString::from(family.to_string()))
    } else {
        eprintln!("notesec: font {family:?} is not installed; using the system font");
        None
    }
}

fn create_journal(storage: &Storage, title: &str) -> Page {
    let mut page = Page::from_markdown(title, true, "- \n");
    page.blocks[0].content.clear();
    if let Err(err) = storage.save(&page) {
        eprintln!("notesec: could not create journal {title}: {err}");
    }
    page
}

/// Sidebar order. Journals first, newest first (`YYYY-MM-DD` sorts
/// lexically); they always keep date order. Then regular pages: those named
/// in `order` (the custom order from `state.toml`, matched ignoring case)
/// in that order, then any others alphabetically. An empty `order` is
/// plain alphabetical; entries without a page are ignored.
fn sort_pages(pages: &mut [Page], order: &[String]) {
    let mut rank = HashMap::new();
    for (i, title) in order.iter().enumerate() {
        rank.entry(title.to_lowercase()).or_insert(i);
    }
    // Listed pages by their position, then the rest alphabetically.
    let key = |p: &Page| {
        let title = p.title.to_lowercase();
        match rank.get(&title) {
            Some(&r) => (0, r, String::new()),
            None => (1, 0, title),
        }
    };
    pages.sort_by(|a, b| match (a.is_journal, b.is_journal) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        (true, true) => b.title.cmp(&a.title),
        (false, false) => key(a).cmp(&key(b)),
    });
}

// ---------------------------------------------------------------------------
// OS text input (typing characters, IME composition)
//
// Plain key presses like "a" don't arrive as actions; GPUI routes them to the
// element registered via `window.handle_input` (see `BlockText::paint`), which
// calls these methods. This mirrors gpui's `examples/input.rs`.
// ---------------------------------------------------------------------------
impl EntityInputHandler for NoteSec {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.active_editor().range_from_utf16(&range_utf16);
        actual_range.replace(self.active_editor().range_to_utf16(&range));
        Some(self.active_editor().text[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let editor = self.active_editor();
        Some(UTF16Selection {
            range: editor.range_to_utf16(&editor.selected_range()),
            reversed: editor.selection_reversed(),
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.active_editor()
            .marked
            .as_ref()
            .map(|r| self.active_editor().range_to_utf16(r))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.active_editor_mut().marked = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.vim_takes_keys() || self.import_dialog_open() {
            return;
        }
        let range = range_utf16
            .as_ref()
            .map(|r| self.active_editor().range_from_utf16(r))
            .or(self.active_editor().marked.clone())
            .unwrap_or(self.active_editor().selected_range());
        let in_block = self.search.is_none() && self.editing.is_some();
        // Typing another "(" or "[" may start a new "((" or "[[": show the
        // picker again.
        if new_text.contains(['(', '[']) {
            self.ref_dismissed = None;
        }
        // "/" typed into an empty block, or over a selection, opens the
        // block-type menu. The "/" is inserted without replacing anything
        // (after the selection), and its typing records no history: the menu
        // settles that when it closes.
        if in_block && self.slash.is_none() {
            if let Some(menu) = SlashMenu::open_for(&self.editor, &range, new_text) {
                let before = self.history_state();
                self.editor.clear_selection();
                self.editor.replace_range(menu.slash..menu.slash, new_text);
                self.slash = Some(SlashState { menu, before });
                cx.notify();
                return;
            }
        }
        if in_block && self.slash.is_some() {
            self.editor.replace_range(range, new_text);
            self.refresh_slash();
            cx.notify();
            return;
        }
        // Replacing a selection always starts its own undo step, so undo
        // brings the selected text (and the selection) back.
        let replaces_selection = self.editor.selection().is_some();
        let before = if in_block {
            if self.text_history_active && !replaces_selection {
                None
            } else {
                self.text_history_active = true;
                Some(self.history_state())
            }
        } else {
            None
        };
        self.active_editor_mut().replace_range(range, new_text);
        if let Some(before) = before {
            self.record_state(before);
        }
        self.text_changed();
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.vim_takes_keys() || self.import_dialog_open() {
            return;
        }
        let range = range_utf16
            .as_ref()
            .map(|r| self.active_editor().range_from_utf16(r))
            .or(self.active_editor().marked.clone())
            .unwrap_or(self.active_editor().selected_range());
        let in_menu = self.slash.is_some();
        let replaces_selection = self.editor.selection().is_some();
        let before = if self.search.is_none() && self.editing.is_some() && !in_menu {
            if self.text_history_active && !replaces_selection {
                None
            } else {
                self.text_history_active = true;
                Some(self.history_state())
            }
        } else {
            None
        };
        self.active_editor_mut()
            .replace_range(range.clone(), new_text);
        let len = self.active_editor().cursor - range.start;
        self.active_editor_mut().marked = (len > 0).then_some(range.start..range.start + len);
        if let Some(sel) = new_selected_range_utf16 {
            // The OS gives the cursor relative to the composition text.
            let end = self.active_editor().offset_from_utf16(sel.end).min(len);
            self.active_editor_mut().cursor = range.start + end;
        }
        if let Some(before) = before {
            self.record_state(before);
        }
        if in_menu {
            self.refresh_slash();
        }
        self.text_changed();
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let lines = self.last_layout.as_ref()?;
        let range = self.active_editor().range_from_utf16(&range_utf16);
        // The range's first line only: enough for the IME to place its window.
        let start = lines.point_for_index(range.start);
        let end = if lines.row_of(range.end) == lines.row_of(range.start) {
            lines.point_for_index(range.end)
        } else {
            point(lines.row_width(lines.row_of(range.start)), start.y)
        };
        Some(Bounds::from_corners(
            bounds.origin + start,
            bounds.origin + point(end.x, end.y + lines.line_height),
        ))
    }

    fn character_index_for_point(
        &mut self,
        pt: gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let local = self.last_bounds?.localize(&pt)?;
        let lines = self.last_layout.as_ref()?;
        let utf8 = lines.index_for_point(local)?;
        Some(self.active_editor().offset_to_utf16(utf8))
    }
}

// ---------------------------------------------------------------------------
// BlockText: a custom GPUI `Element` that draws the editor's text and cursor.
//
// GPUI elements go through three phases each frame:
//   request_layout -> tell the layout engine how big we want to be
//   prepaint       -> compute things that depend on final bounds (shape text)
//   paint          -> draw, and register the input handler
// ---------------------------------------------------------------------------
struct BlockText {
    app: Entity<NoteSec>,
}

/// The edited text shaped one line per `\n`-separated line (GPUI shapes
/// single lines only), stacked `line_height` apart. Positions are relative
/// to the top-left of the text.
#[derive(Default)]
struct TextLines {
    /// Each line's byte offset in the text, and its shaped glyphs.
    lines: Vec<(usize, ShapedLine)>,
    line_height: Pixels,
}

impl TextLines {
    /// The line that byte `ix` is on (a `\n` belongs to the line it ends).
    fn row_of(&self, ix: usize) -> usize {
        self.lines
            .iter()
            .rposition(|(start, _)| *start <= ix)
            .unwrap_or(0)
    }

    fn row_width(&self, row: usize) -> Pixels {
        self.lines.get(row).map_or(px(0.), |(_, line)| line.width())
    }

    /// Top-left corner of the caret before byte `ix`.
    fn point_for_index(&self, ix: usize) -> gpui::Point<Pixels> {
        let row = self.row_of(ix);
        let x = self
            .lines
            .get(row)
            .map_or(px(0.), |(start, line)| line.x_for_index(ix - start));
        point(x, self.line_height * row as f32)
    }

    /// The row under `y`, clamped to the existing lines.
    fn row_at(&self, y: Pixels) -> usize {
        let row = (y / self.line_height).floor().max(0.) as usize;
        row.min(self.lines.len().saturating_sub(1))
    }

    /// The caret position in `row` closest to `x`.
    fn closest_index_in_row(&self, row: usize, x: Pixels) -> usize {
        self.lines
            .get(row)
            .map_or(0, |(start, line)| start + line.closest_index_for_x(x))
    }

    /// The caret position closest to `position`.
    fn closest_index(&self, position: gpui::Point<Pixels>) -> usize {
        self.closest_index_in_row(self.row_at(position.y), position.x)
    }

    /// The byte of the character under `position`, if it is over one.
    fn index_for_point(&self, position: gpui::Point<Pixels>) -> Option<usize> {
        let (start, line) = self.lines.get(self.row_at(position.y))?;
        Some(start + line.index_for_x(position.x)?)
    }
}

/// The runs covering `range` of the text, cut to fit it. Never empty, so
/// an empty line still shapes with the block's font.
fn runs_in(runs: &[TextRun], range: Range<usize>) -> Vec<TextRun> {
    let mut out = Vec::new();
    let mut pos = 0;
    for run in runs {
        let (start, end) = (pos.max(range.start), (pos + run.len).min(range.end));
        if start < end {
            out.push(TextRun {
                len: end - start,
                ..run.clone()
            });
        }
        pos += run.len;
    }
    if out.is_empty() {
        if let Some(run) = runs.first() {
            out.push(TextRun {
                len: 0,
                ..run.clone()
            });
        }
    }
    out
}

/// Data computed in `prepaint` and consumed in `paint`.
struct PrepaintState {
    lines: TextLines,
    cursor: PaintQuad,
    /// Highlight behind the selected text, one quad per line it covers.
    selection: Vec<PaintQuad>,
}

impl IntoElement for BlockText {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for BlockText {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        // Full width, one line tall per line of the block.
        let rows = self.app.read(cx).active_editor().text.split('\n').count();
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = (window.line_height() * rows as f32).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let app = self.app.read(cx);
        let text: SharedString = app.active_editor().text.clone().into();
        let cursor = app.active_editor().cursor;
        let selected = app.active_editor().selection();
        let marked = app.active_editor().marked.clone();
        let accent: Hsla = app.theme.accent.into();
        let selection_color: Hsla = app.theme.selection.into();
        let style = window.text_style();

        let run = TextRun {
            len: text.len(),
            font: style.font(),
            color: style.color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        // Underline the IME composition range, if any, by splitting the text
        // into up to three runs: before / marked / after.
        let runs: Vec<TextRun> = match marked {
            Some(m) => vec![
                TextRun {
                    len: m.start,
                    ..run.clone()
                },
                TextRun {
                    len: m.end - m.start,
                    underline: Some(UnderlineStyle {
                        color: Some(run.color),
                        thickness: px(1.0),
                        wavy: false,
                    }),
                    ..run.clone()
                },
                TextRun {
                    len: text.len() - m.end,
                    ..run
                },
            ]
            .into_iter()
            .filter(|r| r.len > 0)
            .collect(),
            None => vec![run],
        };

        let font_size = style.font_size.to_pixels(window.rem_size());
        let line_height = window.line_height();
        let mut start = 0;
        let lines = text
            .split('\n')
            .map(|line_text| {
                let range = start..start + line_text.len();
                start = range.end + 1;
                let shaped = window.text_system().shape_line(
                    SharedString::from(line_text.to_string()),
                    font_size,
                    &runs_in(&runs, range.clone()),
                    None,
                );
                (range.start, shaped)
            })
            .collect();
        let lines = TextLines { lines, line_height };

        let caret = lines.point_for_index(cursor);
        let cursor = fill(
            Bounds::new(bounds.origin + caret, size(px(1.5), line_height)),
            accent,
        );
        // The selection is translucent quads painted *under* the text, so it
        // never changes the text runs (colours, IME underline) on top of it.
        // Every line but the last also covers its line break.
        let selection = selected.map_or(Vec::new(), |range| {
            let (first, last) = (lines.row_of(range.start), lines.row_of(range.end));
            (first..=last)
                .map(|row| {
                    let from = if row == first {
                        lines.point_for_index(range.start)
                    } else {
                        point(px(0.), line_height * row as f32)
                    };
                    let to_x = if row == last {
                        lines.point_for_index(range.end).x
                    } else {
                        lines.row_width(row) + px(4.)
                    };
                    fill(
                        Bounds::from_corners(
                            bounds.origin + from,
                            bounds.origin + point(to_x, from.y + line_height),
                        ),
                        selection_color,
                    )
                })
                .collect()
        });
        PrepaintState {
            lines,
            cursor,
            selection,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        // Tell GPUI that keyboard text input for this window goes to our entity.
        let focus_handle = self.app.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.app.clone()),
            cx,
        );

        for selection in prepaint.selection.drain(..) {
            window.paint_quad(selection);
        }
        let line_height = prepaint.lines.line_height;
        for (row, (_, line)) in prepaint.lines.lines.iter().enumerate() {
            line.paint(
                bounds.origin + point(px(0.), line_height * row as f32),
                line_height,
                gpui::TextAlign::Left,
                None,
                window,
                cx,
            )
            .expect("failed to paint block text");
        }

        if focus_handle.is_focused(window) {
            window.paint_quad(prepaint.cursor.clone());
        }

        // Remember the layout for the OS input-method callbacks.
        let lines = std::mem::take(&mut prepaint.lines);
        self.app.update(cx, |app, _| {
            app.last_layout = Some(lines);
            app.last_bounds = Some(bounds);
        });
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------
impl NoteSec {
    /// The settings panel (gear, Ctrl-, or "Open settings" in the palette):
    /// a backdrop like the palette's with theme, font size and font family.
    /// Every control applies and saves its change right away.
    fn render_settings(&self, state: &SettingsState, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let config = &self.config;

        // A selectable button; `active` highlights the current choice.
        let button = |id: &'static str, label: SharedString, active: bool| {
            div()
                .id(id)
                .debug_selector(move || id.to_string())
                .px_3()
                .py_1()
                .rounded_md()
                .border_1()
                .border_color(if active { theme.accent } else { theme.border })
                .cursor_pointer()
                .text_color(if active { theme.accent } else { theme.text })
                .when(active, |d| d.bg(theme.selected_bg))
                .hover(|d| d.bg(theme.selected_bg))
                .child(label)
        };
        let label = |text: &'static str| div().text_color(theme.muted).child(text);

        let backup_on = config.git_backup;
        let backup_row = div()
            .flex()
            .flex_row()
            .gap_2()
            .child(
                button("git-backup-off", "Off".into(), !backup_on)
                    .on_click(cx.listener(|this, _e, _window, cx| this.set_git_backup(false, cx))),
            )
            .child(
                button("git-backup-on", "On".into(), backup_on)
                    .on_click(cx.listener(|this, _e, _window, cx| this.set_git_backup(true, cx))),
            );

        let general = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(label("Git auto-backup"))
            .child(backup_row)
            .child(
                div()
                    .debug_selector(|| "git-backup-note".to_string())
                    .text_color(theme.muted)
                    .child(format!(
                        "Local git commits {} s after changes. Never pushes.",
                        backup::BACKUP_AFTER.as_secs()
                    )),
            )
            .child(self.render_vim_settings(cx));

        let section = state.section;
        let tabs = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap_2()
            .child(
                button(
                    "settings-tab-general",
                    "General".into(),
                    section == SettingsSection::General,
                )
                .on_click(cx.listener(|this, _e, _window, cx| {
                    this.show_settings_section(SettingsSection::General, cx)
                })),
            )
            .child(
                button(
                    "settings-tab-appearance",
                    "Appearance".into(),
                    section == SettingsSection::Appearance,
                )
                .on_click(cx.listener(|this, _e, _window, cx| {
                    this.show_settings_section(SettingsSection::Appearance, cx)
                })),
            )
            .child(
                button(
                    "settings-tab-shortcuts",
                    "Shortcuts".into(),
                    section == SettingsSection::Shortcuts,
                )
                .on_click(cx.listener(|this, _e, _window, cx| {
                    this.show_settings_section(SettingsSection::Shortcuts, cx)
                })),
            )
            .child(
                button(
                    "settings-tab-ai",
                    "AI".into(),
                    section == SettingsSection::Ai,
                )
                .on_click(cx.listener(|this, _e, _window, cx| {
                    this.show_settings_section(SettingsSection::Ai, cx)
                })),
            )
            .child(
                button(
                    "settings-tab-clipper",
                    "Web clipper".into(),
                    section == SettingsSection::Clipper,
                )
                .on_click(cx.listener(|this, _e, _window, cx| {
                    this.show_settings_section(SettingsSection::Clipper, cx)
                })),
            )
            .child(
                button(
                    "settings-tab-voice",
                    "Voice notes".into(),
                    section == SettingsSection::Voice,
                )
                .on_click(cx.listener(|this, _e, _window, cx| {
                    this.show_settings_section(SettingsSection::Voice, cx)
                })),
            )
            .child(
                button(
                    "settings-tab-plugins",
                    "Plugins".into(),
                    section == SettingsSection::Plugins,
                )
                .on_click(cx.listener(|this, _e, _window, cx| {
                    this.show_settings_section(SettingsSection::Plugins, cx)
                })),
            );
        let body = match section {
            SettingsSection::General => general.into_any_element(),
            SettingsSection::Appearance => self.render_appearance_settings(state, cx),
            SettingsSection::Shortcuts => self.render_hotkeys(state, cx),
            SettingsSection::Ai => self.render_ai_settings(state, cx),
            SettingsSection::Clipper => self.render_clipper_settings(cx),
            SettingsSection::Voice => self.render_voice_settings(cx),
            SettingsSection::Plugins => self.render_plugin_settings(cx),
        };

        div()
            .id("settings-backdrop")
            .debug_selector(|| "settings-backdrop".to_string())
            .absolute()
            .inset_0()
            .occlude()
            .bg(theme.scrim)
            .flex()
            .flex_col()
            .items_center()
            .pt(px(60.0))
            .pb(px(30.0))
            .on_click(cx.listener(|this, _e, _window, cx| this.close_settings(cx)))
            .child(
                // `occlude` keeps clicks inside the panel from reaching the
                // backdrop (which would close it). The body scrolls when the
                // panel would be taller than the window (large fonts).
                div()
                    .id("settings-panel")
                    .debug_selector(|| "settings-panel".to_string())
                    .occlude()
                    .w(px(500.0))
                    .max_h_full()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_4()
                    .rounded_lg()
                    .bg(theme.sidebar_bg)
                    .border_1()
                    .border_color(theme.border)
                    .shadow_lg()
                    .child(
                        div()
                            .flex_shrink_0()
                            .flex()
                            .flex_row()
                            .justify_between()
                            .child(div().font_weight(FontWeight::BOLD).child("Settings"))
                            .child(label("Esc to close")),
                    )
                    .child(div().flex_shrink_0().child(tabs))
                    .child(
                        div()
                            .id("settings-body")
                            .debug_selector(|| "settings-body".to_string())
                            .min_h_0()
                            .overflow_y_scroll()
                            .child(body),
                    ),
            )
            .into_any_element()
    }

    /// Settings > Shortcuts (decision 41): every palette command with its
    /// keys. Clicking the keys waits for a new key (`capture_keystroke`);
    /// a changed row is marked and has a ↺ back to its default.
    fn render_hotkeys(&self, state: &SettingsState, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let table = self.key_table();
        let rows: Vec<AnyElement> = Command::ALL
            .iter()
            .enumerate()
            .map(|(i, &command)| {
                let name = command.name();
                let capturing = state.capture == Some(command);
                let modified = hotkeys::override_for(&self.state.shortcuts, command).is_some();
                let keys = hotkeys::command_keys(&table, command);
                let (text, muted) = if capturing {
                    ("Press keys\u{2026}".to_string(), false)
                } else if keys.is_empty() {
                    ("Unbound".to_string(), true)
                } else {
                    (keys.join(" / "), false)
                };
                let chip = div()
                    .id(("hotkey-key", i))
                    .debug_selector(move || format!("hotkey-key-{name}"))
                    .flex_shrink_0()
                    .px_2()
                    .rounded_md()
                    .border_1()
                    .border_color(if capturing {
                        theme.accent
                    } else {
                        theme.border
                    })
                    .bg(if capturing {
                        theme.selected_bg
                    } else {
                        theme.bg
                    })
                    .text_color(if muted { theme.muted } else { theme.text })
                    .cursor_pointer()
                    .hover(|d| d.border_color(theme.accent))
                    .on_click(
                        cx.listener(move |this, _e, _window, cx| this.toggle_capture(command, cx)),
                    )
                    .child(text);
                div()
                    .id(("hotkey-row", i))
                    .debug_selector(move || format!("hotkey-row-{name}"))
                    .flex_shrink_0()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py(px(2.0))
                    .rounded_md()
                    .when(capturing, |d| d.bg(theme.selected_bg))
                    .child(div().flex_1().min_w_0().truncate().child(command.label()))
                    .when(modified, |d| {
                        d.child(
                            div()
                                .debug_selector(move || format!("hotkey-modified-{name}"))
                                .flex_shrink_0()
                                .text_color(theme.accent)
                                .child("changed"),
                        )
                    })
                    .child(chip)
                    .when(modified, |d| {
                        d.child(
                            div()
                                .id(("hotkey-reset", i))
                                .debug_selector(move || format!("hotkey-reset-{name}"))
                                .flex_shrink_0()
                                .px_1()
                                .rounded_sm()
                                .cursor_pointer()
                                .text_color(theme.muted)
                                .hover(|d| d.bg(theme.selected_bg).text_color(theme.text))
                                .on_click(cx.listener(move |this, _e, _window, cx| {
                                    this.reset_hotkey(command, cx)
                                }))
                                .child("\u{21ba}"),
                        )
                    })
                    .into_any_element()
            })
            .collect();
        let any_override = !self.state.shortcuts.is_empty();
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .text_color(theme.muted)
                    .child("Click a key, then press the new one. Esc cancels, Backspace unbinds."),
            )
            .child(
                div()
                    .id("hotkey-list")
                    .h(px(HOTKEY_LIST_HEIGHT))
                    .overflow_y_scroll()
                    .track_scroll(&state.hotkey_scroll)
                    .flex()
                    .flex_col()
                    .p_1()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.bg)
                    .children(rows),
            )
            .when_some(state.hotkey_message.clone(), |d, message| {
                d.child(
                    div()
                        .debug_selector(|| "hotkey-message".to_string())
                        .text_color(if message.error {
                            theme.danger
                        } else {
                            theme.muted
                        })
                        .child(message.text),
                )
            })
            .child(
                div().flex().flex_row().child(
                    div()
                        .id("hotkeys-reset-all")
                        .debug_selector(|| "hotkeys-reset-all".to_string())
                        .px_3()
                        .py_1()
                        .rounded_md()
                        .border_1()
                        .border_color(theme.border)
                        .cursor_pointer()
                        .text_color(if any_override {
                            theme.text
                        } else {
                            theme.muted
                        })
                        .hover(|d| d.bg(theme.selected_bg))
                        .on_click(cx.listener(|this, _e, _window, cx| this.reset_all_hotkeys(cx)))
                        .child("Reset to defaults"),
                ),
            )
            .into_any_element()
    }
}

impl NoteSec {
    /// The page menu overlay: the right-click menu (or the rename field it
    /// turns into) at the click position, or the delete confirmation. A
    /// full-window backdrop closes it on any click outside (left or right);
    /// `occlude` keeps clicks inside the panel from reaching the backdrop.
    fn render_page_menu(&self, menu: &PageMenu, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let panel = || {
            div()
                .occlude()
                .flex()
                .flex_col()
                .p_1()
                .rounded_lg()
                .bg(theme.sidebar_bg)
                .border_1()
                .border_color(theme.border)
                .shadow_lg()
        };

        if let MenuStep::ConfirmDelete { error } = &menu.step {
            let confirm = Confirm {
                id: "confirm-delete",
                title: format!("Move \u{201c}{}\u{201d} to the trash?", menu.title),
                body: "You can restore it from Trash in the sidebar, or delete it forever there."
                    .into(),
                error: error.clone(),
                ok_label: "Move to trash",
            };
            return self.render_confirm(confirm, Self::close_page_menu, Self::confirm_delete, cx);
        }

        let content = match &menu.step {
            MenuStep::Rename { error, .. } => panel()
                .w(px(300.0))
                .gap_1()
                .p_2()
                .child(div().px_1().text_color(theme.muted).child("Rename page"))
                .child(
                    div()
                        .debug_selector(|| "rename-input".to_string())
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .border_1()
                        .border_color(theme.accent)
                        .bg(theme.bg)
                        .child(BlockText { app: cx.entity() }),
                )
                .when_some(error.clone(), |d, error| {
                    d.child(
                        div()
                            .debug_selector(|| "rename-error".to_string())
                            .px_1()
                            .text_color(theme.danger)
                            .child(error),
                    )
                })
                .child(
                    div()
                        .px_1()
                        .text_color(theme.muted)
                        .child("Enter to rename, Esc to cancel"),
                )
                .into_any_element(),
            _ => {
                // One menu row; a disabled one is muted and has no handler.
                let item = |id: &'static str, label: &'static str, enabled: bool| {
                    div()
                        .id(id)
                        .debug_selector(move || id.to_string())
                        .px_3()
                        .py_1()
                        .rounded_md()
                        .text_color(if enabled { theme.text } else { theme.muted })
                        .when(enabled, |d| {
                            d.cursor_pointer().hover(|d| d.bg(theme.selected_bg))
                        })
                        .child(label)
                };
                let can_rename = self.can_rename(&menu.title);
                let can_delete = self.can_delete();
                let custom_order = !self.state.page_order.is_empty();
                panel()
                    .id("page-menu")
                    .debug_selector(|| "page-menu".to_string())
                    .w(px(200.0))
                    .child(
                        item("page-menu-rename", "Rename", can_rename).when(can_rename, |d| {
                            d.on_click(cx.listener(|this, _e, _window, cx| this.start_rename(cx)))
                        }),
                    )
                    .child(item("page-menu-delete", "Delete\u{2026}", can_delete).when(
                        can_delete,
                        |d| {
                            d.text_color(theme.danger)
                                .on_click(cx.listener(|this, _e, _window, cx| this.ask_delete(cx)))
                        },
                    ))
                    .child(
                        item("page-menu-copy", "Copy page title", true).on_click(
                            cx.listener(|this, _e, _window, cx| this.copy_page_title(cx)),
                        ),
                    )
                    // Only useful once pages were dragged out of A-Z order.
                    .child(div().my_1().h(px(1.0)).bg(theme.border))
                    .child(
                        item("page-menu-sort", "Sort pages A-Z", custom_order).when(
                            custom_order,
                            |d| {
                                d.on_click(
                                    cx.listener(|this, _e, _window, cx| this.sort_pages_az(cx)),
                                )
                            },
                        ),
                    )
                    .into_any_element()
            }
        };

        div()
            .id("page-menu-backdrop")
            .absolute()
            .inset_0()
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _e, _window, cx| this.close_page_menu(cx)),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _e, _window, cx| this.close_page_menu(cx)),
            )
            // Window coordinates of the click; `anchored` flips or shifts the
            // panel so it stays inside the window.
            .child(
                anchored()
                    .position(menu.position)
                    .snap_to_window_with_margin(px(8.))
                    .child(content),
            )
            .into_any_element()
    }
}

impl NoteSec {
    /// A confirmation before a destructive step (moving a page to the
    /// trash, deleting from the trash): a modal like the settings panel,
    /// with a dimmed backdrop, a centred box, Cancel and a danger-coloured
    /// button. A backdrop click cancels, as does Esc (through the caller's
    /// key context); Enter does nothing, so it always takes a click.
    /// Selectors: `{id}`, `{id}-cancel`, `{id}-ok`, `{id}-backdrop`.
    fn render_confirm(
        &self,
        confirm: Confirm,
        cancel: fn(&mut Self, &mut Context<Self>),
        ok: fn(&mut Self, &mut Context<Self>),
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = self.theme;
        let id = confirm.id;
        div()
            .id(format!("{id}-backdrop"))
            .debug_selector(move || format!("{id}-backdrop"))
            .absolute()
            .inset_0()
            .occlude()
            .bg(theme.scrim)
            .flex()
            .flex_col()
            .items_center()
            .pt(px(160.0))
            .on_click(cx.listener(move |this, _e, _window, cx| cancel(this, cx)))
            .child(
                div()
                    .id(id)
                    .debug_selector(move || id.to_string())
                    .occlude()
                    .flex()
                    .flex_col()
                    .rounded_lg()
                    .bg(theme.sidebar_bg)
                    .border_1()
                    .border_color(theme.border)
                    .shadow_lg()
                    .w(px(420.0))
                    .gap_2()
                    .p_4()
                    .child(div().font_weight(FontWeight::BOLD).child(confirm.title))
                    .child(div().text_color(theme.muted).child(confirm.body))
                    .when_some(confirm.error, |d, error| {
                        d.child(div().text_color(theme.danger).child(error))
                    })
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap_2()
                            .child(
                                div()
                                    .id(format!("{id}-cancel"))
                                    .debug_selector(move || format!("{id}-cancel"))
                                    .px_3()
                                    .py_1()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(theme.border)
                                    .cursor_pointer()
                                    .hover(|d| d.bg(theme.selected_bg))
                                    .on_click(
                                        cx.listener(move |this, _e, _window, cx| cancel(this, cx)),
                                    )
                                    .child("Cancel"),
                            )
                            .child(
                                div()
                                    .id(format!("{id}-ok"))
                                    .debug_selector(move || format!("{id}-ok"))
                                    .px_3()
                                    .py_1()
                                    .rounded_md()
                                    .bg(theme.danger)
                                    .text_color(theme.bg)
                                    .font_weight(FontWeight::BOLD)
                                    .cursor_pointer()
                                    .hover(|d| d.opacity(0.85))
                                    .on_click(
                                        cx.listener(move |this, _e, _window, cx| ok(this, cx)),
                                    )
                                    .child(confirm.ok_label),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// The trash tab: deleted pages, newest first, each with when it was
    /// deleted and Restore / Delete forever buttons (`trash-item-{i}`,
    /// `trash-restore-{i}`, `trash-delete-{i}`), plus Empty trash.
    fn render_trash(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let now = chrono::Local::now();
        let button = |id: ElementId, selector: String, label: &'static str, danger: bool| {
            div()
                .id(id)
                .debug_selector(move || selector)
                .flex_shrink_0()
                .px_2()
                .rounded_md()
                .border_1()
                .border_color(if danger { theme.danger } else { theme.border })
                .text_color(if danger { theme.danger } else { theme.text })
                .cursor_pointer()
                .hover(|d| d.bg(theme.selected_bg))
                .child(label)
        };
        let rows: Vec<AnyElement> = self
            .trash
            .iter()
            .enumerate()
            .map(|(i, entry)| {
                let (restore_id, delete_id) = (entry.id.clone(), entry.id.clone());
                div()
                    .id(("trash-item", i))
                    .debug_selector(move || format!("trash-item-{i}"))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .hover(|d| d.bg(theme.selected_bg))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(theme.text)
                            .child(entry.title.clone()),
                    )
                    .when(entry.is_journal, |d| {
                        d.child(
                            div()
                                .flex_shrink_0()
                                .text_color(theme.muted)
                                .child("Journal"),
                        )
                    })
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_color(theme.muted)
                            .child(deleted_label(entry.deleted_at, now)),
                    )
                    .child(
                        button(
                            ("trash-restore", i).into(),
                            format!("trash-restore-{i}"),
                            "Restore",
                            false,
                        )
                        .on_click(cx.listener(
                            move |this, _e, _window, cx| this.restore_from_trash(&restore_id, cx),
                        )),
                    )
                    .child(
                        button(
                            ("trash-delete", i).into(),
                            format!("trash-delete-{i}"),
                            "Delete forever",
                            true,
                        )
                        .on_click(cx.listener(
                            move |this, _e, _window, cx| this.ask_delete_forever(&delete_id, cx),
                        )),
                    )
                    .into_any_element()
            })
            .collect();
        let has_entries = !rows.is_empty();
        // Disabled (muted, no handler) while there is nothing to empty.
        let empty_button = div()
            .id("trash-empty")
            .debug_selector(|| "trash-empty".to_string())
            .px_3()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(if has_entries {
                theme.danger
            } else {
                theme.border
            })
            .text_color(if has_entries {
                theme.danger
            } else {
                theme.muted
            })
            .when(has_entries, |d| {
                d.cursor_pointer()
                    .hover(|d| d.bg(theme.selected_bg))
                    .on_click(cx.listener(|this, _e, _window, cx| this.ask_empty_trash(cx)))
            })
            .child("Empty trash");

        div()
            .id("trash")
            .debug_selector(|| "trash".to_string())
            .flex_1()
            .h_full()
            .overflow_y_scroll()
            .p_8()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(self.ui_size() * 1.9))
                            .text_color(theme.text)
                            .child("Trash"),
                    )
                    .child(empty_button),
            )
            .child(
                div()
                    .mb_4()
                    .text_color(theme.muted)
                    .child("Deleted pages wait here, in the graph's .trash folder, until you restore them or delete them forever."),
            )
            .when_some(self.trash_error.clone(), |d, error| {
                d.child(
                    div()
                        .debug_selector(|| "trash-error".to_string())
                        .px_3()
                        .py_1()
                        .text_color(theme.danger)
                        .child(error),
                )
            })
            .children(rows)
            .when(!has_entries, |d| {
                d.child(
                    div()
                        .debug_selector(|| "trash-empty-state".to_string())
                        .px_3()
                        .text_color(theme.muted)
                        .child("The trash is empty."),
                )
            })
            .into_any_element()
    }

    /// The trash's confirmation before deleting for good.
    fn render_trash_confirm(&self, confirm: &TrashConfirm, cx: &mut Context<Self>) -> AnyElement {
        let confirm = match confirm {
            TrashConfirm::DeleteForever(entry) => Confirm {
                id: "trash-confirm",
                title: format!("Delete \u{201c}{}\u{201d} forever?", entry.title),
                body: "Its file is removed from the trash. This can't be undone.".into(),
                error: None,
                ok_label: "Delete forever",
            },
            TrashConfirm::Empty => {
                let n = self.trash.len();
                Confirm {
                    id: "trash-confirm",
                    title: "Empty the trash?".into(),
                    body: format!(
                        "{n} deleted {} removed for good. This can't be undone.",
                        if n == 1 { "page is" } else { "pages are" }
                    ),
                    error: None,
                    ok_label: "Empty trash",
                }
            }
        };
        self.render_confirm(confirm, Self::close_trash_confirm, Self::confirm_trash, cx)
    }

    /// The keyboard shortcuts dialog (Ctrl+/ or "Keyboard shortcuts"): the
    /// `shortcuts` table, grouped, in a modal like the settings panel.
    fn render_shortcuts(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let mut children: Vec<AnyElement> = Vec::new();
        let mut n: usize = 0;
        for (group, rows) in cheatsheet(&self.key_table()) {
            children.push(
                div()
                    .mt_2()
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme.accent)
                    .child(group.label())
                    .into_any_element(),
            );
            for (keys, description) in rows {
                let i = n;
                n += 1;
                children.push(
                    div()
                        .debug_selector(move || format!("shortcut-row-{i}"))
                        .flex()
                        .flex_row()
                        .gap_3()
                        .py(px(2.0))
                        .child(
                            div()
                                .w(px(200.0))
                                .flex_shrink_0()
                                .text_color(theme.text)
                                .child(keys),
                        )
                        .child(div().text_color(theme.muted).child(description))
                        .into_any_element(),
                );
            }
        }
        div()
            .id("shortcuts-backdrop")
            .debug_selector(|| "shortcuts-backdrop".to_string())
            .absolute()
            .inset_0()
            .occlude()
            .bg(theme.scrim)
            .flex()
            .flex_col()
            .items_center()
            .pt(px(60.0))
            .on_click(cx.listener(|this, _e, _window, cx| this.close_shortcuts(cx)))
            .child(
                div()
                    .id("shortcuts-dialog")
                    .debug_selector(|| "shortcuts-dialog".to_string())
                    .occlude()
                    .w(px(600.0))
                    .max_h(px(620.0))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .p_4()
                    .rounded_lg()
                    .bg(theme.sidebar_bg)
                    .border_1()
                    .border_color(theme.border)
                    .shadow_lg()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_between()
                            .child(
                                div()
                                    .font_weight(FontWeight::BOLD)
                                    .child("Keyboard shortcuts"),
                            )
                            .child(div().text_color(theme.muted).child("Esc to close")),
                    )
                    .child(
                        div()
                            .id("shortcuts-customize")
                            .debug_selector(|| "shortcuts-customize".to_string())
                            .mt_1()
                            .text_color(theme.accent)
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _e, _window, cx| {
                                this.shortcuts_open = false;
                                this.show_settings_section(SettingsSection::Shortcuts, cx);
                            }))
                            .child(
                                "Change any command's key in Settings \u{203a} Shortcuts \
                                 (or \u{201c}Change keyboard shortcuts\u{201d} in the palette)",
                            ),
                    )
                    .children(children),
            )
            .into_any_element()
    }

    /// The agenda tab: open tasks in Overdue, Today, Upcoming (one header
    /// per day) and Unscheduled sections. Rows are numbered top to bottom
    /// (`agenda-item-{i}`).
    fn render_agenda(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let agenda = self.agenda();
        let today = chrono::Local::now().date_naive();
        let mut children: Vec<AnyElement> = Vec::new();
        let mut next: usize = 0;

        let header = |id: &'static str, label: String, color: gpui::Rgba| {
            div()
                .debug_selector(move || id.to_string())
                .mt_4()
                .px_3()
                .py_1()
                .text_color(color)
                .child(label)
                .into_any_element()
        };
        let mut row =
            |item: &AgendaItem| {
                let i = next;
                next += 1;
                let (page, block) = (item.page.clone(), item.block);
                let started = item.state.is_started();
                let date = |kind: &str, d: Option<crate::agenda::AgendaDate>, late: bool| {
                    d.map(|d| {
                        div()
                            .flex_shrink_0()
                            .text_color(if late { theme.danger } else { theme.muted })
                            .child(format!("{kind} {}", d.label()))
                    })
                };
                div()
                    .id(("agenda-item", i))
                    .debug_selector(move || format!("agenda-item-{i}"))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|d| d.bg(theme.selected_bg))
                    .on_click(cx.listener(move |this, _e, _window, cx| {
                        this.open_agenda_item(&page, block, cx)
                    }))
                    .child(
                        div()
                            .flex_shrink_0()
                            .px_1()
                            .rounded_sm()
                            .border_1()
                            .border_color(if started { theme.accent } else { theme.border })
                            .text_color(if started { theme.accent } else { theme.muted })
                            .child(item.state.keyword()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(theme.text)
                            .child(item.text.clone()),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .max_w(px(180.0))
                            .truncate()
                            .text_color(theme.muted)
                            .child(item.page.clone()),
                    )
                    .children(date(
                        "Scheduled",
                        item.dates.scheduled,
                        item.dates.scheduled.is_some_and(|d| d.date < today),
                    ))
                    .children(date(
                        "Deadline",
                        item.dates.deadline,
                        item.dates.deadline.is_some_and(|d| d.date < today),
                    ))
                    .into_any_element()
            };

        if !agenda.overdue.is_empty() {
            children.push(header(
                "agenda-group-overdue",
                format!("OVERDUE \u{b7} {}", agenda.overdue.len()),
                theme.danger,
            ));
            children.extend(agenda.overdue.iter().map(&mut row));
        }
        if !agenda.today.is_empty() {
            children.push(header(
                "agenda-group-today",
                format!("TODAY \u{b7} {}", day_label(today)),
                theme.accent,
            ));
            children.extend(agenda.today.iter().map(&mut row));
        }
        if !agenda.upcoming.is_empty() {
            children.push(header(
                "agenda-group-upcoming",
                "UPCOMING".to_string(),
                theme.muted,
            ));
            for (day, items) in &agenda.upcoming {
                children.push(
                    div()
                        .px_3()
                        .pt_2()
                        .text_color(theme.text)
                        .child(day_label(*day))
                        .into_any_element(),
                );
                children.extend(items.iter().map(&mut row));
            }
        }
        if !agenda.unscheduled.is_empty() {
            children.push(header(
                "agenda-group-unscheduled",
                format!("UNSCHEDULED \u{b7} {}", agenda.unscheduled.len()),
                theme.muted,
            ));
            children.extend(agenda.unscheduled.iter().map(&mut row));
        }
        if agenda.is_empty() {
            children.push(
                div()
                    .mt_4()
                    .px_3()
                    .text_color(theme.muted)
                    .child(
                        "No open tasks. Start a block with TODO; date it with a line \
                         SCHEDULED: <2026-10-09> or DEADLINE: <2026-10-09> under it.",
                    )
                    .into_any_element(),
            );
        }

        div()
            .id("agenda")
            .debug_selector(|| "agenda".to_string())
            .flex_1()
            .h_full()
            .overflow_y_scroll()
            .p_8()
            .flex()
            .flex_col()
            .child(
                div()
                    .text_size(px(self.ui_size() * 1.9))
                    .text_color(theme.text)
                    .child("Agenda"),
            )
            .children(children)
            .into_any_element()
    }
}

impl NoteSec {
    /// The unfocused pane's page (decision 40).
    fn render_other_page(&mut self, page_ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let view = self.render_page_view(page_ix, false, None, cx);
        #[cfg(test)]
        {
            self.other_layouts = view.layouts;
            self.other_embed_lines = view.embed_lines;
        }
        view.element
    }

    /// One pane's page: its title, blocks and backlinks. Drawn for the
    /// focused pane (`focused`, page `selected`: the one with the editor,
    /// the "/" and "((" menus and drop targets) and, when split, for the
    /// other pane too (read-only until a press focuses it, decision 40). The
    /// other pane's debug selectors start with `OTHER_PANE` so tests can
    /// tell the two apart; its element ids don't clash, since each pane's
    /// container has its own id.
    fn render_page_view(
        &self,
        page_ix: usize,
        focused: bool,
        mut slash_menu: Option<AnyElement>,
        cx: &mut Context<Self>,
    ) -> PageView {
        let theme = self.theme;
        let font_size = self.ui_size();
        // Style for `[[wikilinks]]` in display mode: accent colour + underline.
        let link_style = HighlightStyle {
            color: Some(theme.accent.into()),
            underline: Some(UnderlineStyle {
                color: Some(theme.accent.into()),
                thickness: px(1.0),
                wavy: false,
            }),
            ..Default::default()
        };

        // Tags look like small chips: accent text on a subtle background.
        let tag_style = HighlightStyle {
            color: Some(theme.accent.into()),
            background_color: Some(theme.selected_bg.into()),
            ..Default::default()
        };

        // Block references show the referenced text on a faint accent wash
        // with a wavy muted underline, so they read as quoted, not as your
        // own words.
        let ref_style = HighlightStyle {
            background_color: Some(Hsla::from(theme.accent).opacity(0.10)),
            underline: Some(UnderlineStyle {
                color: Some(theme.muted.into()),
                thickness: px(1.0),
                wavy: true,
            }),
            ..Default::default()
        };

        let prefix = if focused { "" } else { OTHER_PANE };
        let page = &self.pages[page_ix];
        #[cfg(test)]
        let mut reading_layouts = Vec::new();
        #[cfg(test)]
        let mut embed_lines = Vec::new();
        let visible = page.visible_blocks(&self.collapsed);
        // The drop gap below each row's lower half: just before the next
        // visible row, or the end of the page after the last one.
        let gap_below: Vec<DropGap> = (0..page.blocks.len())
            .map(|ix| {
                (ix + 1..page.blocks.len())
                    .find(|&i| visible[i])
                    .map_or(DropGap::End, |i| DropGap::Before(page.blocks[i].id))
            })
            .collect();
        let dragging = cx.has_active_drag();
        let block_drop = if dragging { self.block_drop } else { None };
        let file_over = dragging && self.file_drag.as_ref().is_some_and(|(_, inside)| *inside);
        let rows: Vec<AnyElement> = page
            .blocks
            .iter()
            .enumerate()
            // Blocks inside a folded subtree aren't rendered at all.
            .filter(|&(ix, _)| visible[ix])
            .map(|(ix, block)| {
                let is_editing = focused && self.editing == Some(ix);
                let depth = page.depth_of(ix);
                // Display mode (the reading view) hides the type prefix
                // (`# `, `> `) and paired `**`/`*` markers and styles the row
                // instead; the editor shows the raw markdown. `display` also
                // maps shown offsets back to `content` offsets.
                // The same page in the other pane mirrors the block being
                // edited live, before it is committed.
                let mirrored = !focused && page_ix == self.selected && self.editing == Some(ix);
                let source = if mirrored {
                    &self.editor.text
                } else {
                    &block.content
                };
                let display = (!is_editing).then(|| {
                    Rc::new(DisplayBlock::with_refs(source, |id| {
                        find_block(&self.pages, id)
                            .map(|(p, b)| self.pages[p].blocks[b].content.clone())
                    }))
                });
                let kind = match &display {
                    Some(d) => d.kind,
                    None => BlockKind::parse(&self.editor.text).0,
                };
                // Layout of this row's text, kept so a press can be mapped to
                // the character (and so the link) under the mouse.
                let mut text_layout = None;
                let content: AnyElement = match &display {
                    None => div()
                        .text_size(px(self.mono_size()))
                        .when_some(self.mono_font.clone(), |d, font| d.font_family(font))
                        .on_mouse_down(MouseButton::Left, cx.listener(Self::on_text_mouse_down))
                        .child(BlockText { app: cx.entity() })
                        .into_any_element(),
                    Some(d) => {
                        // Code blocks, pipe tables and images are drawn as
                        // their own boxes between the prose around them. They
                        // aren't mapped back to `content`, so a press on
                        // such a block starts editing at its start.
                        let parts = split_code(&block.content);
                        let rich = parts.iter().any(|part| match part {
                            Part::Code(_) => true,
                            Part::Text(text) => {
                                parse_table(text).is_some() || !parse_images(text).is_empty()
                            }
                        });
                        let text = if rich {
                            let resolve = |id| {
                                find_block(&self.pages, id)
                                    .map(|(p, b)| self.pages[p].blocks[b].content.clone())
                            };
                            let styled = |text: &str, first: bool| {
                                // Only the block's first line can carry its
                                // type prefix and task keyword (hidden).
                                let d = if first {
                                    DisplayBlock::with_refs(text, resolve)
                                } else {
                                    DisplayBlock::inline(text, resolve)
                                };
                                StyledText::new(d.text.clone()).with_highlights(reading_highlights(
                                    &d, link_style, tag_style, ref_style,
                                ))
                            };
                            let mut pieces: Vec<AnyElement> = Vec::new();
                            let (mut tables, mut codes, mut images) = (0, 0, 0);
                            // Prose with images in it: each image is drawn on
                            // its own between the text around it, and the line
                            // breaks next to an image are dropped.
                            let voice_entity = cx.entity();
                            let mut prose =
                                |pieces: &mut Vec<AnyElement>, text: &str, first: bool| {
                                    let mut pos = 0;
                                    let mut first = first;
                                    let text_piece =
                                        |pieces: &mut Vec<AnyElement>,
                                         t: &str,
                                         first: &mut bool| {
                                            let t = t.trim_matches('\n');
                                            if !t.trim().is_empty() {
                                                pieces.push(styled(t, *first).into_any_element());
                                            }
                                            *first = false;
                                        };
                                    for image in parse_images(text) {
                                        text_piece(
                                            pieces,
                                            &text[pos..image.range.start],
                                            &mut first,
                                        );
                                        pieces.push(
                                            if crate::voice::is_audio_target(&image.target) {
                                                self.render_audio(
                                                    prefix,
                                                    ix,
                                                    images,
                                                    &image,
                                                    block.id,
                                                    &voice_entity,
                                                )
                                            } else {
                                                self.render_image(prefix, ix, images, &image)
                                            },
                                        );
                                        images += 1;
                                        pos = image.range.end;
                                    }
                                    if pos == 0 {
                                        pieces.push(styled(text, first).into_any_element());
                                    } else {
                                        text_piece(pieces, &text[pos..], &mut first);
                                    }
                                };
                            for (p, part) in parts.iter().enumerate() {
                                let mut rest = match part {
                                    Part::Code(code) => {
                                        pieces.push(
                                            self.render_code(prefix, ix, block.id, codes, code, cx),
                                        );
                                        codes += 1;
                                        continue;
                                    }
                                    Part::Text(text) => text.clone(),
                                };
                                let mut first = p == 0;
                                while let Some(table) = parse_table(&rest) {
                                    if !table.before.is_empty() {
                                        prose(&mut pieces, &table.before, first);
                                    }
                                    let cells = table
                                        .rows
                                        .iter()
                                        .map(|row| {
                                            row.iter().map(|cell| styled(cell, false)).collect()
                                        })
                                        .collect();
                                    let name = match tables {
                                        0 => format!("{prefix}table-{ix}"),
                                        k => format!("{prefix}table-{ix}-{k}"),
                                    };
                                    pieces.push(
                                        table_grid(&theme, name, cells, &table.align)
                                            .into_any_element(),
                                    );
                                    tables += 1;
                                    first = false;
                                    rest = table.after;
                                }
                                if !rest.is_empty() {
                                    prose(&mut pieces, &rest, first);
                                }
                            }
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .children(pieces)
                                .into_any_element()
                        } else {
                            let highlights =
                                reading_highlights(d, link_style, tag_style, ref_style);
                            // `with_highlights` resolves against the inherited
                            // text style, so heading size/weight and quote
                            // styling (and the theme's text colour) apply to
                            // the unhighlighted parts.
                            let text = StyledText::new(d.text.clone()).with_highlights(highlights);
                            text_layout = Some(text.layout().clone());
                            #[cfg(test)]
                            reading_layouts.push((ix, text.layout().clone()));
                            text.into_any_element()
                        };
                        // DONE text is dimmed and struck through.
                        let text = div()
                            .flex_1()
                            .min_w_0()
                            .when(d.task == Some(TaskState::Done), |t| {
                                t.text_color(theme.muted)
                                    .line_through()
                                    .debug_selector(move || format!("{prefix}done-text-{ix}"))
                            })
                            .child(text);
                        match d.task {
                            None => text.into_any_element(),
                            // The keyword is drawn as a checkbox; a press on
                            // it cycles the state and never starts editing
                            // (it stops the row's mouse-down handler).
                            Some(state) => div()
                                .flex()
                                .flex_row()
                                .items_start()
                                .gap(px(6.0))
                                .child(
                                    task_checkbox(&theme, font_size, kind, state)
                                        .debug_selector(move || {
                                            format!(
                                                "{prefix}task-{ix}-{}",
                                                state.keyword().to_lowercase()
                                            )
                                        })
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                                                cx.stop_propagation();
                                                this.cycle_block_task(ix, cx);
                                            }),
                                        ),
                                )
                                .child(text)
                                .into_any_element(),
                        }
                    }
                };
                // A `{{query #tag}}` block lists every block tagged #tag
                // under its text. Computed each frame from the pages in
                // memory, so it follows every save and navigation.
                let query_list = display
                    .as_ref()
                    .and_then(|_| parse_query(&block.content))
                    .map(|(_, tag)| {
                        let hits = tag_query(&self.pages, &tag);
                        let header = match hits.len() {
                            0 => format!("No blocks tagged #{tag}"),
                            1 => format!("1 block tagged #{tag}"),
                            n => format!("{n} blocks tagged #{tag}"),
                        };
                        let items: Vec<AnyElement> = hits
                            .iter()
                            .enumerate()
                            .map(|(n, &(p, b))| {
                                let title = self.pages[p].title.clone();
                                let text = DisplayBlock::new(&self.pages[p].blocks[b].content)
                                    .text
                                    .replace('\n', " ");
                                div()
                                    .id(("query-hit", n))
                                    .debug_selector(move || format!("{prefix}query-{ix}-hit-{n}"))
                                    .px_2()
                                    .py(px(2.0))
                                    .rounded_md()
                                    .flex()
                                    .flex_row()
                                    .gap_3()
                                    .cursor_pointer()
                                    .hover(|d| d.bg(theme.selected_bg))
                                    // A press here never starts editing the
                                    // query block; the click opens the page.
                                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                        cx.stop_propagation()
                                    })
                                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                        this.open_page(&title, cx);
                                    }))
                                    .child(
                                        div()
                                            .flex_none()
                                            .text_color(theme.accent)
                                            .child(self.pages[p].title.clone()),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .text_color(theme.text)
                                            .child(text),
                                    )
                                    .into_any_element()
                            })
                            .collect();
                        div()
                            .id(("query", ix))
                            .debug_selector(move || format!("{prefix}query-{ix}"))
                            .mt_1()
                            .p_2()
                            .rounded_md()
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.sidebar_bg)
                            .flex()
                            .flex_col()
                            .text_size(px(font_size * 0.9))
                            .child(div().px_2().pb_1().text_color(theme.muted).child(header))
                            .children(items)
                    });
                // Embedded pages and blocks, live (decision 47).
                let embeds = display
                    .as_ref()
                    .and_then(|_| self.render_embeds(prefix, page_ix, ix, source, cx));
                #[cfg(test)]
                if let Some(embeds) = &embeds {
                    embed_lines.extend(embeds.lines.iter().cloned());
                }
                // Plugin render hooks: `{{macro}}` boxes (decision 55).
                let plugin_boxes = display
                    .as_ref()
                    .and_then(|_| self.render_plugin_boxes(source));
                let extras: Vec<AnyElement> = query_list
                    .map(IntoElement::into_any_element)
                    .into_iter()
                    .chain(embeds.map(|e| e.element))
                    .chain(plugin_boxes)
                    .collect();
                let content = if extras.is_empty() {
                    content
                } else {
                    div()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .child(content)
                        .children(extras)
                        .into_any_element()
                };
                // A press on a row that isn't being edited starts editing it
                // with the cursor under the mouse, and a drag from there
                // selects (the root's mouse-move handler extends it). A press
                // on a link does nothing here: the click handler navigates.
                let link_at = {
                    let (layout, display) = (text_layout.clone(), display.clone());
                    move |position: gpui::Point<Pixels>| {
                        let (layout, display) = (layout.as_ref()?, display.as_ref()?);
                        // `Ok` only when the mouse is over an actual glyph.
                        let char_ix = layout.index_for_position(position).ok()?;
                        display
                            .links
                            .iter()
                            .find(|l| l.range.contains(&char_ix))
                            .cloned()
                    }
                };
                let block_ref_at = {
                    let (layout, display) = (text_layout.clone(), display.clone());
                    move |position: gpui::Point<Pixels>| {
                        let (layout, display) = (layout.as_ref()?, display.as_ref()?);
                        let char_ix = layout.index_for_position(position).ok()?;
                        display
                            .block_refs
                            .iter()
                            .find(|r| r.range.contains(&char_ix))
                            .map(|r| r.id)
                    }
                };
                let on_press = {
                    let (layout, display, link_at, block_ref_at) = (
                        text_layout.clone(),
                        display.clone(),
                        link_at.clone(),
                        block_ref_at.clone(),
                    );
                    cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                        if link_at(event.position).is_some()
                            || block_ref_at(event.position).is_some()
                        {
                            return;
                        }
                        let offset = layout.as_ref().map_or(0, |layout| {
                            match layout.index_for_position(event.position) {
                                Ok(i) | Err(i) => i,
                            }
                        });
                        // Hidden prefix and markers are accounted for here;
                        // see `DisplayBlock::to_source` for the boundary rule.
                        let source = display.as_ref().map_or(0, |d| d.to_source(offset));
                        this.start_edit(ix, window, cx);
                        this.editor.set_cursor(source);
                        this.selecting = true;
                        cx.notify();
                    })
                };
                let on_click = cx.listener(move |this, event: &ClickEvent, window, cx| {
                    // The press already started editing unless it was on a
                    // link or a block reference; only those act on click.
                    if this.editing == Some(ix) {
                        return;
                    }
                    if let Some(link) = link_at(event.position()) {
                        this.open_page(&link.target, cx);
                    } else if let Some(id) = block_ref_at(event.position()) {
                        this.open_block_ref(id, window, cx);
                    }
                });
                // Blocks with children get a fold arrow; a folded one also
                // shows how many blocks it hides. A press on the arrow stops
                // there, so it never starts editing.
                let hidden_count = page.descendant_count(ix);
                let folded = hidden_count > 0 && self.collapsed.contains(&block.id);
                let fold = (hidden_count > 0).then(|| {
                    fold_arrow(&theme, folded)
                        .debug_selector(move || format!("{prefix}fold-{ix}"))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                                cx.stop_propagation();
                                this.toggle_fold(ix, cx);
                            }),
                        )
                        .into_any_element()
                });
                // Dragging the bullet moves the block. The press on the handle
                // stops there, so it never starts editing the row.
                let block_id = block.id;
                let handle = {
                    let preview_text: SharedString = block
                        .content
                        .lines()
                        .next()
                        .unwrap_or("")
                        .to_string()
                        .into();
                    drag_handle()
                        .id(("handle", ix))
                        .debug_selector(move || format!("{prefix}handle-{ix}"))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_drag(DraggedBlock { id: block_id }, move |_, _, _, cx| {
                            cx.new(|_| BlockDragPreview {
                                text: preview_text.clone(),
                                theme,
                            })
                        })
                        .into_any_element()
                };
                let below = gap_below[ix];
                let on_drag_move =
                    cx.listener(move |this, event: &DragMoveEvent<DraggedBlock>, _, cx| {
                        let position = event.event.position;
                        if !event.bounds.contains(&position) {
                            return;
                        }
                        let gap = if position.y < event.bounds.center().y {
                            DropGap::Before(block_id)
                        } else {
                            below
                        };
                        let dragged = event.drag(cx).id;
                        this.set_block_drop(dragged, Some(gap), cx);
                    });
                let drop_here = block_drop == Some(DropGap::Before(block_id));
                let row = block_row(&theme, depth, font_size, kind, content, fold, handle)
                    .relative()
                    .when(drop_here, |d| {
                        d.child(
                            drop_line(&theme, depth)
                                .debug_selector(move || format!("{prefix}drop-before-{ix}")),
                        )
                    })
                    .when(focused, |d| d.on_drag_move(on_drag_move))
                    .when(folded, |d| {
                        d.child(fold_badge(&theme, font_size, hidden_count).debug_selector(
                            move || format!("{prefix}fold-badge-{ix}-{hidden_count}"),
                        ))
                    })
                    .id(("block", ix))
                    // Lets tests find this row's on-screen bounds; a no-op in
                    // normal builds.
                    .debug_selector(|| format!("{prefix}block-{ix}"))
                    .cursor_text()
                    .when(!is_editing, |d| {
                        d.on_mouse_down(MouseButton::Left, on_press)
                            .on_click(on_click)
                    });
                let menu = if is_editing { slash_menu.take() } else { None };
                match menu {
                    // The menu hangs off a zero-height strip right under the
                    // edited row, lined up with its text; `deferred` paints it
                    // above the rows below.
                    Some(menu) => div()
                        .flex()
                        .flex_col()
                        .child(row)
                        .child(
                            div().h(px(0.)).child(deferred(
                                anchored()
                                    .offset(point(px(24.0 * depth as f32 + 14.0), px(2.0)))
                                    .snap_to_window_with_margin(px(8.))
                                    .child(menu),
                            )),
                        )
                        .into_any_element(),
                    None => row.into_any_element(),
                }
            })
            .collect();

        // --- Linked from: blocks on other pages that link here ---------------
        // Recomputed every frame. That is a scan of every block, which is fine
        // for a personal graph; an index can replace it if it ever shows up in
        // a profile.
        let groups = backlinks(&self.pages, page_ix);
        let total: usize = groups.iter().map(|g| g.blocks.len()).sum();
        // Blocks are shown as in reading view, so a block reference reads as
        // the text it points at rather than `((uuid))`.
        let pages = &self.pages;
        let resolve =
            |id: Uuid| find_block(pages, id).map(|(p, b)| pages[p].blocks[b].content.clone());
        let mut backlink_items: Vec<AnyElement> = Vec::new();
        #[cfg(test)]
        let mut backlink_texts = Vec::new();
        let mut n: usize = 0; // running index over all references, for ids
        for (g, group) in groups.iter().enumerate() {
            let source = &self.pages[group.page];
            let source_title = source.title.clone();
            backlink_items.push(
                div()
                    .id(("backlink-page", group.page))
                    .debug_selector(move || format!("{prefix}linked-page-{g}"))
                    .mt_2()
                    .text_color(theme.accent)
                    .cursor_pointer()
                    .on_click(cx.listener({
                        let title = source_title.clone();
                        move |this, _e, _window, cx| this.open_page(&title, cx)
                    }))
                    .child(source_title.clone())
                    .into_any_element(),
            );
            for &block_ix in &group.blocks {
                let title = source_title.clone();
                let text = DisplayBlock::with_refs(&source.blocks[block_ix].content, &resolve).text;
                #[cfg(test)]
                backlink_texts.push(text.clone());
                backlink_items.push(
                    div()
                        .id(("backlink", n))
                        .debug_selector(|| format!("{prefix}backlink-{n}"))
                        .pl_4()
                        .py_1()
                        .cursor_pointer()
                        .text_color(theme.text)
                        .hover(|d| d.bg(theme.selected_bg))
                        .on_click(cx.listener(move |this, _e, _window, cx| {
                            this.open_page(&title, cx);
                            // Unfold whatever hides the referencing block.
                            if block_ix < this.pages[this.selected].blocks.len() {
                                this.reveal(block_ix);
                            }
                        }))
                        .child(text)
                        .into_any_element(),
                );
                n += 1;
            }
        }
        // Every page has the section; with nothing linking here it says so.
        let pages_count = groups.len();
        let backlinks_panel = div()
            .id("backlinks")
            .debug_selector(|| format!("{prefix}linked-from"))
            .mt_8()
            .pt_4()
            .border_t_1()
            .border_color(theme.border)
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(div().text_color(theme.text).child("Linked from"))
                    .when(total > 0, |d| {
                        d.child(div().text_color(theme.muted).child(format!(
                            "{pages_count} page{}, {total} reference{}",
                            if pages_count == 1 { "" } else { "s" },
                            if total == 1 { "" } else { "s" }
                        )))
                    }),
            )
            .when(total == 0, |d| {
                d.child(
                    div()
                        .debug_selector(|| format!("{prefix}linked-from-empty"))
                        .mt_1()
                        .text_color(theme.muted)
                        .child("No other page links here yet."),
                )
            })
            .children(backlink_items);

        let element = div()
            .id("main")
            .flex_1()
            .h_full()
            .overflow_y_scroll()
            .p_8()
            .flex()
            .flex_col()
            .child(
                div()
                    .mb_4()
                    .text_size(px(font_size * 1.9))
                    .text_color(theme.text)
                    .child(page.title.clone()),
            )
            .children(self.render_tag_suggestions(page_ix, focused, cx))
            .children(rows)
            // The drop line for "after the last block".
            .when(block_drop == Some(DropGap::End), |d| {
                d.child(
                    div()
                        .relative()
                        .h(px(0.))
                        .child(drop_line(&theme, 0).debug_selector(|| format!("{prefix}drop-end"))),
                )
            })
            // Leaving the page area hides the drop line; dropping anywhere on
            // the page moves the block to where the line is. Only the
            // focused pane takes drops: a press in the other one focuses it
            // first, and the drag then continues over the redrawn pane.
            .when(focused, |d| {
                d.on_drag_move(
                    cx.listener(|this, event: &DragMoveEvent<DraggedBlock>, _, cx| {
                        if !event.bounds.contains(&event.event.position) {
                            let dragged = event.drag(cx).id;
                            this.set_block_drop(dragged, None, cx);
                        }
                    }),
                )
                .on_drop(cx.listener(|this, dragged: &DraggedBlock, _, cx| {
                    this.drop_block(dragged.id, cx)
                }))
                // Image files dragged in from outside: a faint wash while
                // they are over the page; releasing them adds them
                // (`finish_file_drop`).
                .on_drag_move(
                    cx.listener(|this, event: &DragMoveEvent<ExternalPaths>, _, cx| {
                        let inside = event.bounds.contains(&event.event.position);
                        let was_inside = this.file_drag.as_ref().map(|(_, inside)| *inside);
                        this.file_drag = Some((event.drag(cx).clone(), inside));
                        if was_inside != Some(inside) {
                            cx.notify();
                        }
                    }),
                )
                .when(file_over, |d| d.bg(theme.selected_bg))
                .child({
                    let app = cx.entity();
                    gpui::canvas(
                        |_, _, _| {},
                        move |_, _, window, _| {
                            let on_up = app.clone();
                            window.on_mouse_event(move |_: &MouseUpEvent, phase, window, cx| {
                                if phase == gpui::DispatchPhase::Bubble {
                                    on_up.update(cx, |this, cx| this.finish_file_drop(window, cx));
                                }
                            });
                            // Outside files never press the mouse in this
                            // window, so a press means any earlier file drag
                            // is over.
                            window.on_mouse_event(move |_: &MouseDownEvent, phase, _, cx| {
                                if phase == gpui::DispatchPhase::Capture {
                                    app.update(cx, |this, _| this.file_drag = None);
                                }
                            });
                        },
                    )
                    .absolute()
                    .size_0()
                })
            })
            .children(self.render_page_ai(page_ix, focused, cx))
            .child(backlinks_panel)
            .children(self.render_mentions(page_ix, focused, cx))
            // Empty space below the blocks: clicking it leaves edit mode.
            .child(
                div()
                    .id("filler")
                    .flex_1()
                    .min_h(px(120.0))
                    .on_click(cx.listener(|this, _e, _window, cx| this.stop_edit(cx))),
            );
        PageView {
            element: element.into_any_element(),
            #[cfg(test)]
            layouts: reading_layouts,
            #[cfg(test)]
            backlink_texts,
            #[cfg(test)]
            embed_lines,
        }
    }
}

impl Render for NoteSec {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Plugins found failing while drawing the last frame are turned off.
        self.apply_plugin_disables(cx);
        // A drag released outside the sidebar just ends (GPUI drops it), so
        // forget where it would have landed.
        if !cx.has_active_drag() {
            self.page_drop = None;
        }
        let theme = self.theme;
        let font_size = self.ui_size();
        // Shortcut hints are read from the keymap (`bind_keys`), so they
        // always show the real binding.
        let keymap = cx.key_bindings();
        let hint = move |action: &dyn gpui::Action| -> String {
            binding_hint(&keymap.borrow(), action).unwrap_or_default()
        };
        // --- Sidebar: one clickable row per page ---------------------------
        // The accent line showing where a dragged page would land: above the
        // row it is drawn in, in the gap between rows.
        let page_drop_line = move || {
            div()
                .debug_selector(|| "page-drop-indicator".to_string())
                .absolute()
                .left_0()
                .right_0()
                .top(px(-3.0))
                .h(px(2.0))
                .rounded_sm()
                .bg(theme.accent)
        };
        let drop_before = self.page_drop.as_ref().map(|d| d.before.clone());
        let preview_font = self.font_family.clone();
        let saved_searches = self.render_saved_searches(cx);
        // "Record voice note" / "Stop recording" (decision 52).
        let voice_item = self.render_voice_sidebar_item(cx);
        let sidebar_items = self.pages.iter().enumerate().map(|(ix, page)| {
            let is_selected = self.mode == Mode::Notes && ix == self.selected;
            let is_favorite = self.state.is_favorite(&page.title);
            let title = page.title.clone();
            let star_title = page.title.clone();
            let menu_title = page.title.clone();
            let drop_here = drop_before == Some(Some(page.title.clone()));
            // Regular pages can be dragged to reorder them; journals keep
            // their date order. Journals come first, so whatever follows a
            // regular page is regular too.
            let drag = (!page.is_journal).then(|| {
                let next = self.pages.get(ix + 1).map(|p| p.title.clone());
                (page.title.clone(), next, preview_font.clone())
            });
            div()
                // Interactive elements need a stable id; (name, index) is the idiom.
                .id(("page", ix))
                .debug_selector(|| format!("page-{ix}"))
                // Every row uses the same group name (Zed's idiom): the star's
                // `group_hover` resolves to the row it sits in.
                .group("page-row")
                .px_3()
                .py_1()
                .rounded_md()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap_2()
                .cursor_pointer()
                .text_color(if is_selected {
                    theme.accent
                } else {
                    theme.text
                })
                .when(is_selected, |d| d.bg(theme.selected_bg))
                .hover(|d| d.bg(theme.selected_bg))
                // `cx.listener` turns a closure over `&mut Self` into a GPUI handler.
                // Opens the page in its tab, or a new tab.
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    // Save the block being edited first. Saving can add pages
                    // and re-sort, shifting indices, so find the row's page again.
                    this.stop_edit(cx);
                    let ix = match this.pages.get(ix) {
                        Some(p) if p.title == title => ix,
                        _ => this.find_page(&title).unwrap_or(0),
                    };
                    this.show_page(ix, cx);
                }))
                // Right-click: Rename / Delete / Copy page title.
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, event: &MouseDownEvent, _window, cx| {
                        this.open_page_menu(menu_title.clone(), event.position, cx)
                    }),
                )
                .when_some(drag, |d, (zone, next, font_family)| {
                    let dragged = DraggedPage {
                        title: zone.clone(),
                    };
                    d.on_drag(dragged, move |page: &DraggedPage, _offset, _window, cx| {
                        let font_family = font_family.clone();
                        cx.new(|_| PageDragPreview {
                            title: page.title.clone().into(),
                            theme,
                            font_size,
                            font_family,
                        })
                    })
                    .on_drag_move(cx.listener(
                        move |this, event: &DragMoveEvent<DraggedPage>, _window, cx| {
                            let zone = Some(zone.clone());
                            this.drag_over_zone(event, zone.clone(), zone, next.clone(), cx)
                        },
                    ))
                })
                .relative()
                .when(drop_here, |d| d.child(page_drop_line()))
                .child(div().flex_1().overflow_hidden().child(page.title.clone()))
                .child(
                    favorite_star(&theme, is_favorite)
                        .id(("star", ix))
                        .debug_selector(|| format!("star-{ix}"))
                        // Outline stars only show while the row is hovered.
                        .when(!is_favorite, |d| {
                            d.invisible().group_hover("page-row", |s| s.visible())
                        })
                        .on_click(cx.listener(move |this, _e, _window, cx| {
                            // Starring must not also open the page.
                            cx.stop_propagation();
                            this.toggle_favorite(&star_title, cx);
                        })),
                )
        });

        // FAVORITES and RECENT: pages by title, skipping titles whose page no
        // longer exists (they stay in `state.toml`). Clicking opens the page.
        let current_title = (self.mode == Mode::Notes)
            .then(|| {
                self.pages
                    .get(self.selected)
                    .map(|p| p.title.to_lowercase())
            })
            .flatten();
        let shortcut_row = |id: ElementId, selector: String, title: &str| {
            let is_current = current_title.as_deref() == Some(title.to_lowercase().as_str());
            let target = title.to_string();
            let menu_title = title.to_string();
            div()
                .id(id)
                .debug_selector(move || selector)
                .group("page-row")
                .px_3()
                .py_1()
                .rounded_md()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap_2()
                .cursor_pointer()
                .text_color(if is_current { theme.accent } else { theme.text })
                .when(is_current, |d| d.bg(theme.selected_bg))
                .hover(|d| d.bg(theme.selected_bg))
                .on_click(cx.listener(move |this, _e, _window, cx| {
                    this.open_page(&target, cx);
                }))
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, event: &MouseDownEvent, _window, cx| {
                        this.open_page_menu(menu_title.clone(), event.position, cx)
                    }),
                )
                .child(div().flex_1().overflow_hidden().child(title.to_string()))
        };
        let existing = |titles: &[String]| -> Vec<(usize, String)> {
            titles
                .iter()
                .filter_map(|t| self.find_page(t).map(|ix| self.pages[ix].title.clone()))
                .enumerate()
                .collect()
        };
        let favorite_rows: Vec<AnyElement> = existing(&self.state.favorites)
            .into_iter()
            .map(|(i, title)| {
                let star_title = title.clone();
                shortcut_row(ElementId::from(("fav", i)), format!("fav-{i}"), &title)
                    .child(
                        favorite_star(&theme, true)
                            .id(("fav-star", i))
                            .debug_selector(|| format!("fav-star-{i}"))
                            .invisible()
                            .group_hover("page-row", |s| s.visible())
                            .on_click(cx.listener(move |this, _e, _window, cx| {
                                cx.stop_propagation();
                                this.toggle_favorite(&star_title, cx);
                            })),
                    )
                    .into_any_element()
            })
            .collect();
        let recent_rows: Vec<AnyElement> = existing(&self.state.recent)
            .into_iter()
            .map(|(i, title)| {
                shortcut_row(
                    ElementId::from(("recent", i)),
                    format!("recent-{i}"),
                    &title,
                )
                .into_any_element()
            })
            .collect();
        let section_header = |label: &'static str| {
            div()
                .mt_2()
                .px_3()
                .py_2()
                .text_color(theme.muted)
                .child(label)
        };

        // Tag index: every tag in the graph with how many blocks use it.
        // Clicking one opens its page, whose backlinks panel lists the uses.
        let tags = tag_counts(&self.pages);
        let tag_rows: Vec<AnyElement> = tags
            .into_iter()
            .enumerate()
            .map(|(i, (name, count))| {
                let target = name.clone();
                div()
                    .id(("tag", i))
                    .debug_selector(|| format!("tag-{i}"))
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .flex()
                    .flex_row()
                    .justify_between()
                    .cursor_pointer()
                    .text_color(theme.text)
                    .hover(|d| d.bg(theme.selected_bg))
                    .on_click(cx.listener(move |this, _e, _window, cx| {
                        this.navigate(&target, Nav::Tab, cx);
                    }))
                    .child(format!("#{name}"))
                    .child(div().text_color(theme.muted).child(count.to_string()))
                    .into_any_element()
            })
            .collect();
        let has_tags = !tag_rows.is_empty();

        // "Today" button at the very top: one click to today's journal.
        // Highlighted while that journal is the page on screen.
        let today = today_title();
        let on_today = self.mode == Mode::Notes
            && self
                .pages
                .get(self.selected)
                .is_some_and(|p| p.is_journal && p.title == today);
        let today_item = div()
            .id("today")
            .debug_selector(|| "today".to_string())
            .px_3()
            .py_1()
            .rounded_md()
            .flex()
            .flex_row()
            .justify_between()
            .cursor_pointer()
            .text_color(if on_today { theme.accent } else { theme.text })
            .when(on_today, |d| d.bg(theme.selected_bg))
            .hover(|d| d.bg(theme.selected_bg))
            .on_click(cx.listener(|this, _e, _window, cx| this.open_today(cx)))
            .child("Today")
            .child(div().text_color(theme.muted).child(hint(&OpenToday)));

        // "Graph view" entry above the page list; highlighted while open.
        let in_graph = self.mode == Mode::Graph;
        let graph_item = div()
            .id("graph-item")
            .debug_selector(|| "graph-item".to_string())
            .px_3()
            .py_1()
            .rounded_md()
            .cursor_pointer()
            .text_color(if in_graph { theme.accent } else { theme.text })
            .when(in_graph, |d| d.bg(theme.selected_bg))
            .hover(|d| d.bg(theme.selected_bg))
            .on_click(cx.listener(|this, _e, _window, cx| this.toggle_graph(cx)))
            .child("Graph view");

        // "Agenda" entry: open tasks by date; highlighted while open.
        let in_agenda = self.mode == Mode::Agenda;
        let agenda_item = div()
            .id("sidebar-agenda")
            .debug_selector(|| "sidebar-agenda".to_string())
            .px_3()
            .py_1()
            .rounded_md()
            .cursor_pointer()
            .text_color(if in_agenda { theme.accent } else { theme.text })
            .when(in_agenda, |d| d.bg(theme.selected_bg))
            .hover(|d| d.bg(theme.selected_bg))
            .on_click(cx.listener(|this, _e, _window, cx| this.show_agenda(cx)))
            .child("Agenda");

        // "Trash" entry: deleted pages, with how many; highlighted while open.
        let in_trash = self.mode == Mode::Trash;
        let trash_count = self.trash.len();
        let trash_item = div()
            .id("sidebar-trash")
            .debug_selector(|| "sidebar-trash".to_string())
            .px_3()
            .py_1()
            .rounded_md()
            .flex()
            .flex_row()
            .justify_between()
            .cursor_pointer()
            .text_color(if in_trash { theme.accent } else { theme.text })
            .when(in_trash, |d| d.bg(theme.selected_bg))
            .hover(|d| d.bg(theme.selected_bg))
            .on_click(cx.listener(|this, _e, _window, cx| this.show_trash(cx)))
            .child("Trash")
            .when(trash_count > 0, |d| {
                d.child(
                    div()
                        .debug_selector(|| "sidebar-trash-count".to_string())
                        .text_color(theme.muted)
                        .child(trash_count.to_string()),
                )
            });

        let pages_header = div()
            .px_3()
            .py_2()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .text_color(theme.muted)
            .child("PAGES")
            .child(
                div()
                    .id("new-page")
                    .debug_selector(|| "new-page".to_string())
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .text_color(theme.accent)
                    .hover(|d| d.bg(theme.selected_bg))
                    .on_click(cx.listener(|this, _event, window, cx| {
                        this.new_page(window, cx);
                    }))
                    .child("+ New page"),
            );

        // Settings entry, pinned under the scrolling list.
        let settings_item = div()
            .id("settings-gear")
            .debug_selector(|| "settings-gear".to_string())
            .m_2()
            .px_3()
            .py_1()
            .rounded_md()
            .flex()
            .flex_row()
            .justify_between()
            .cursor_pointer()
            .text_color(theme.text)
            .hover(|d| d.bg(theme.selected_bg))
            .on_click(cx.listener(|this, _e, _window, cx| this.open_settings(cx)))
            .child("Settings")
            .child(div().text_color(theme.muted).child(hint(&OpenSettings)));

        // Below the last page: dropping here moves a page to the very end.
        let drop_end = div()
            .id("page-drop-end")
            .debug_selector(|| "page-drop-end".to_string())
            .relative()
            .h(px(12.0))
            .on_drag_move(
                cx.listener(|this, event: &DragMoveEvent<DraggedPage>, _window, cx| {
                    this.drag_over_zone(event, None, None, None, cx)
                }),
            )
            .when(drop_before == Some(None), |d| d.child(page_drop_line()));

        let sidebar_list = div()
            .id("sidebar")
            .debug_selector(|| "sidebar".to_string())
            // A page dragged anywhere in the list lands where the indicator
            // is (nowhere if none is shown).
            .on_drop(cx.listener(|this, dragged: &DraggedPage, _window, cx| {
                this.drop_page(&dragged.title, cx)
            }))
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .overflow_y_scroll()
            .child(today_item)
            .child(graph_item)
            .child(agenda_item)
            .child(trash_item)
            .child(voice_item)
            .when(!favorite_rows.is_empty(), |d| {
                d.child(section_header("FAVORITES")).children(favorite_rows)
            })
            .when(!recent_rows.is_empty(), |d| {
                d.child(section_header("RECENT")).children(recent_rows)
            })
            .children(saved_searches)
            .child(pages_header)
            .children(sidebar_items)
            .child(drop_end)
            .when(has_tags, |d| {
                d.child(
                    div()
                        .mt_2()
                        .px_3()
                        .py_2()
                        .text_color(theme.muted)
                        .child("TAGS"),
                )
                .children(tag_rows)
            });
        let sidebar = div()
            .w(px(240.0))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .bg(theme.sidebar_bg)
            .border_r_1()
            .border_color(theme.border)
            .child(sidebar_list)
            .child(
                div()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(settings_item),
            );

        // --- "/" block-type menu (shown under the edited block) -------------
        let mut slash_menu = self.slash.as_ref().map(|state| {
            let selected = state.menu.selected;
            let items: Vec<AnyElement> = self
                .slash_matches()
                .into_iter()
                .enumerate()
                .map(|(i, kind)| {
                    div()
                        .id(("slash-item", i))
                        .debug_selector(|| format!("slash-item-{i}"))
                        .px_3()
                        .py_1()
                        .rounded_md()
                        .flex()
                        .flex_row()
                        .justify_between()
                        .gap_4()
                        .cursor_pointer()
                        .text_color(theme.text)
                        .when(i == selected, |d| d.bg(theme.selected_bg))
                        .hover(|d| d.bg(theme.selected_bg))
                        .on_click(cx.listener(move |this, _e, _window, cx| {
                            this.apply_slash(kind, cx);
                        }))
                        .child(kind.label())
                        .child(
                            div()
                                .text_color(theme.muted)
                                .child(kind.prefix().trim_end().to_string()),
                        )
                        .into_any_element()
                })
                .collect();
            div()
                .id("slash-menu")
                .debug_selector(|| "slash-menu".to_string())
                // Clicks on the menu must not reach the blocks underneath.
                .occlude()
                .w(px(220.0))
                .flex()
                .flex_col()
                .p_1()
                .rounded_lg()
                .bg(theme.sidebar_bg)
                .border_1()
                .border_color(theme.border)
                .shadow_lg()
                .text_size(px(font_size))
                .children(items)
        });

        // --- "((" and "[[" pickers (same place as the "/" menu) -------------
        let ref_matches = self.ref_matches();
        if slash_menu.is_none() && !ref_matches.is_empty() {
            let selected = self.ref_highlight();
            let items: Vec<AnyElement> = ref_matches
                .into_iter()
                .enumerate()
                .map(|(i, item)| {
                    // Main text, and the muted text on the right.
                    let (text, side) = match &item {
                        RefItem::Block(page, block) => {
                            let page = &self.pages[*page];
                            let content = &page.blocks[*block].content;
                            let text = DisplayBlock::new(content).text.replace('\n', " ");
                            (text, page.title.clone())
                        }
                        RefItem::Page(page, alias) => (
                            self.pages[*page].title.clone(),
                            match alias {
                                Some(alias) => format!("alias: {alias}"),
                                None => "page".to_string(),
                            },
                        ),
                    };
                    div()
                        .id(("ref-item", i))
                        .debug_selector(move || format!("ref-item-{i}"))
                        .px_3()
                        .py_1()
                        .rounded_md()
                        .flex()
                        .flex_row()
                        .justify_between()
                        .gap_4()
                        .cursor_pointer()
                        .text_color(theme.text)
                        .when(i == selected, |d| d.bg(theme.selected_bg))
                        .hover(|d| d.bg(theme.selected_bg))
                        .on_click(cx.listener(move |this, _e, _window, cx| {
                            this.insert_ref(item.clone(), cx);
                        }))
                        .child(
                            div()
                                .flex_1()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .child(text),
                        )
                        .child(div().flex_none().text_color(theme.muted).child(side))
                        .into_any_element()
                })
                .collect();
            slash_menu = Some(
                div()
                    .id("ref-menu")
                    .debug_selector(|| "ref-menu".to_string())
                    .occlude()
                    .w(px(420.0))
                    .flex()
                    .flex_col()
                    .p_1()
                    .rounded_lg()
                    .bg(theme.sidebar_bg)
                    .border_1()
                    .border_color(theme.border)
                    .shadow_lg()
                    .text_size(px(font_size))
                    .children(items),
            );
        }

        // --- The focused pane's page (decision 40: the other pane, if any,
        // is drawn further down) ---------------------------------------------
        self.sync_page_ai(cx);
        self.sync_mentions(cx);
        let page_view = self.render_page_view(
            self.selected,
            true,
            slash_menu.map(IntoElement::into_any_element),
            cx,
        );
        #[cfg(test)]
        {
            self.reading_layouts = page_view.layouts;
        }
        #[cfg(test)]
        {
            self.backlink_texts = page_view.backlink_texts;
            self.embed_lines = page_view.embed_lines;
        }
        let main = page_view.element;

        // --- Search overlay (Ctrl-K) -----------------------------------------
        let overlay = self.search.as_ref().map(|state| {
            let hits = self.search_results();
            let plugin_labels: Vec<String> = self
                .plugin_commands()
                .into_iter()
                .map(|(_, _, l)| l)
                .collect();
            let selected = state.selected;
            let templates = state.templates.as_deref();
            let headers: Vec<Option<&'static str>> =
                (0..hits.len()).map(|i| palette_header(&hits, i)).collect();
            let rows: Vec<AnyElement> =
                hits.into_iter()
                    .enumerate()
                    .flat_map(|(i, hit)| {
                        // Page hit: just the title. Block hit: the text, then the
                        // page it lives on. Command: its label and a muted tag.
                        // (Muted text is the secondary column in each case.)
                        let row =
                            |main: gpui::Div, side: String| {
                                div().flex().flex_row().gap_2().child(main).child(
                                    div().flex_shrink_0().text_color(theme.muted).child(side),
                                )
                            };
                        let label = match hit.target {
                            Target::Page(p) => row(
                                div()
                                    .text_color(theme.accent)
                                    .child(self.pages[p].title.clone()),
                                String::new(),
                            ),
                            Target::Block(p, b) => row(
                                div()
                                    .truncate()
                                    .text_color(theme.text)
                                    .child(self.pages[p].blocks[b].content.clone()),
                                self.pages[p].title.clone(),
                            ),
                            // The shortcut hint, right-aligned and muted.
                            Target::Command(c) => div()
                                .debug_selector(move || format!("command-{}", c.name()))
                                .flex()
                                .flex_row()
                                .justify_between()
                                .gap_2()
                                .child(div().text_color(theme.text).child(c.label()))
                                .child(
                                    div()
                                        .flex_shrink_0()
                                        .text_color(theme.muted)
                                        .child(hint(c.action().as_ref())),
                                ),
                            Target::Plugin(i) => div()
                                .debug_selector(move || format!("plugin-command-{i}"))
                                .flex()
                                .flex_row()
                                .justify_between()
                                .gap_2()
                                .child(
                                    div()
                                        .text_color(theme.text)
                                        .child(plugin_labels.get(i).cloned().unwrap_or_default()),
                                )
                                .child(
                                    div()
                                        .flex_shrink_0()
                                        .text_color(theme.muted)
                                        .child("plugin"),
                                ),
                            Target::Template(t) => row(
                                div().text_color(theme.accent).child(
                                    templates.map_or(String::new(), |ts| ts[t].name.clone()),
                                ),
                                "template".into(),
                            ),
                            // Global search: the page, then the matching line
                            // with the match in bold accent.
                            Target::Match {
                                page,
                                block,
                                start,
                                end,
                            } => {
                                let content = &self.pages[page].blocks[block].content;
                                let (text, range) = snippet(content, start..end, SNIPPET_CHARS);
                                let bold = HighlightStyle {
                                    color: Some(theme.accent.into()),
                                    font_weight: Some(FontWeight::BOLD),
                                    ..Default::default()
                                };
                                div()
                                    .flex()
                                    .flex_col()
                                    .child(
                                        div()
                                            .text_color(theme.muted)
                                            .child(self.pages[page].title.clone()),
                                    )
                                    .child(
                                        div()
                                            .debug_selector(move || format!("snippet-{i}"))
                                            .truncate()
                                            .text_color(theme.text)
                                            .child(
                                                StyledText::new(text)
                                                    .with_highlights([(range, bold)]),
                                            ),
                                    )
                            }
                        };
                        let header = headers[i].map(|title| {
                            div()
                                .debug_selector(move || {
                                    format!("palette-{}-header", title.to_lowercase())
                                })
                                .px_3()
                                .pt_2()
                                .text_color(theme.muted)
                                .child(title)
                                .into_any_element()
                        });
                        let row = div()
                            .id(("search-result", i))
                            .debug_selector(|| format!("search-result-{i}"))
                            .flex_shrink_0()
                            .px_3()
                            .py_1()
                            .rounded_md()
                            .cursor_pointer()
                            .when(i == selected, |d| d.bg(theme.selected_bg))
                            .hover(|d| d.bg(theme.selected_bg))
                            .on_click(cx.listener(move |this, _e, window, cx| {
                                this.open_hit(&hit, window, cx);
                            }))
                            .child(label)
                            .into_any_element();
                        header.into_iter().chain(std::iter::once(row))
                    })
                    .collect();
            let no_results = rows.is_empty();
            // In template mode: a heading, and a hint if there are no templates.
            let picking_templates = templates.is_some();
            let global = state.global;
            let empty_message = match templates {
                Some([]) => format!(
                    "No templates yet: add .md files to {}",
                    self.storage.templates_dir().display()
                ),
                _ if global && state.query.text.trim().is_empty() => {
                    "Type to search the text of every page and journal".to_string()
                }
                _ => "No results".to_string(),
            };

            // Backdrop: dims the app, swallows mouse events, closes on click.
            div()
                .id("search-backdrop")
                .debug_selector(|| "search-backdrop".to_string())
                .absolute()
                .inset_0()
                .occlude()
                .bg(theme.scrim)
                .flex()
                .flex_col()
                .items_center()
                .pt(px(90.0))
                .on_click(cx.listener(|this, _e, _window, cx| this.close_search(cx)))
                .child(
                    // The palette itself. `occlude` keeps clicks inside it from
                    // reaching the backdrop (which would close the overlay).
                    div()
                        .id("search-panel")
                        .occlude()
                        .w(px(620.0))
                        .flex()
                        .flex_col()
                        .gap_1()
                        .p_2()
                        .rounded_lg()
                        .bg(theme.sidebar_bg)
                        .border_1()
                        .border_color(theme.border)
                        .shadow_lg()
                        .when(picking_templates, |d| {
                            d.child(
                                div()
                                    .px_3()
                                    .pt_1()
                                    .text_color(theme.muted)
                                    .child("Insert template"),
                            )
                        })
                        .when(global, |d| {
                            d.child(
                                div()
                                    .debug_selector(|| "global-search-heading".to_string())
                                    .px_3()
                                    .pt_1()
                                    .text_color(theme.muted)
                                    .child("Search all pages"),
                            )
                        })
                        .children(self.save_search_button(SearchKind::Global, cx))
                        .child(
                            div()
                                .debug_selector(|| "search-input".to_string())
                                .px_3()
                                .py_2()
                                .rounded_md()
                                .bg(theme.bg)
                                .child(BlockText { app: cx.entity() }),
                        )
                        // Scrolls: an empty query lists every command.
                        .child(
                            div()
                                .id("search-results")
                                .max_h(px(PALETTE_LIST_HEIGHT))
                                .overflow_y_scroll()
                                .track_scroll(&state.scroll)
                                .flex()
                                .flex_col()
                                .gap_1()
                                .children(rows),
                        )
                        .when(no_results, |d| {
                            d.child(
                                div()
                                    .px_3()
                                    .py_1()
                                    .text_color(theme.muted)
                                    .child(empty_message),
                            )
                        }),
                )
        });

        // --- Tab bar: one tab per open page (or the graph) -------------------
        let tab_items: Vec<AnyElement> = self
            .tabs
            .tabs
            .iter()
            .enumerate()
            .map(|(i, tab)| {
                let active = self.tabs.active == Some(i);
                let label = match tab {
                    TabTarget::Page(title) => title.clone(),
                    TabTarget::Graph => "Graph".to_string(),
                    TabTarget::Agenda => "Agenda".to_string(),
                    TabTarget::Trash => "Trash".to_string(),
                };
                div()
                    .id(("tab", i))
                    .debug_selector(move || format!("tab-{i}"))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .pl_3()
                    .pr_1()
                    .h_full()
                    .flex_shrink_0()
                    .border_r_1()
                    .border_color(theme.border)
                    .cursor_pointer()
                    .text_color(if active { theme.accent } else { theme.muted })
                    .when(active, |d| d.bg(theme.bg))
                    .hover(|d| d.text_color(theme.text))
                    .on_click(cx.listener(move |this, _e, _window, cx| this.activate_tab(i, cx)))
                    .child(div().max_w(px(200.0)).truncate().child(label))
                    .child(
                        // The ×: closes on press, before the tab's own click
                        // could focus it.
                        div()
                            .debug_selector(move || format!("tab-close-{i}"))
                            .px_1()
                            .rounded_sm()
                            .text_color(theme.muted)
                            .hover(|d| d.bg(theme.selected_bg).text_color(theme.text))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                                    cx.stop_propagation();
                                    this.close_tab(i, cx);
                                }),
                            )
                            .child("×"),
                    )
                    .into_any_element()
            })
            .collect();
        let tab_bar = div()
            .id("tab-bar")
            .flex_shrink_0()
            .h(px(font_size * 2.2))
            .flex()
            .flex_row()
            .overflow_x_scroll()
            .bg(theme.sidebar_bg)
            .border_b_1()
            .border_color(theme.border)
            .children(tab_items);

        // No tabs: say so and offer the usual ways to open one.
        let empty_state = |cx: &mut Context<Self>| {
            let hint_row = |keys: String, what: &'static str| {
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(div().w(px(70.0)).text_color(theme.accent).child(keys))
                    .child(what)
            };
            div()
                .id("empty-state")
                .debug_selector(|| "empty-state".to_string())
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_3()
                .text_color(theme.muted)
                .child(
                    div()
                        .text_size(px(font_size * 1.4))
                        .text_color(theme.text)
                        .child("No open tabs"),
                )
                .child(
                    div()
                        .id("empty-today")
                        .debug_selector(|| "empty-today".to_string())
                        .px_4()
                        .py_1()
                        .rounded_md()
                        .cursor_pointer()
                        .bg(theme.selected_bg)
                        .text_color(theme.accent)
                        .on_click(cx.listener(|this, _e, _window, cx| this.open_today(cx)))
                        .child("Open today's journal"),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(hint_row(hint(&OpenToday), "today's journal"))
                        .child(hint_row(
                            hint(&ToggleSearch),
                            "search pages, blocks and commands",
                        ))
                        .child(hint_row(
                            hint(&SearchAllPages),
                            "search the text of every page",
                        ))
                        .child(hint_row(hint(&NewPage), "new page"))
                        .child(hint_row(hint(&ToggleGraph), "graph view"))
                        .child(hint_row(hint(&ShowShortcuts), "keyboard shortcuts"))
                        .child(hint_row(String::new(), "or pick a page in the sidebar")),
                )
        };

        // The main area: the tab bar, then the page, the graph or the empty
        // state.
        let whiteboard_shown = self.whiteboard_shown();
        let view: AnyElement = match (&self.mode, &self.graph) {
            (Mode::Graph, Some(graph)) => graph.clone().into_any_element(),
            (Mode::Agenda, _) => self.render_agenda(cx),
            (Mode::Trash, _) => self.render_trash(cx),
            (Mode::Empty, _) => empty_state(cx).into_any_element(),
            _ if whiteboard_shown => self.render_whiteboard(cx),
            _ => main.into_any_element(),
        };

        // Split (decision 40): the focused pane shows `view`; the other one
        // what it holds, drawn read-only. A press anywhere in the other pane
        // focuses it first (capture phase), so the click then acts there.
        let split = self.split.clone();
        #[cfg(test)]
        {
            self.other_layouts.clear();
            self.other_embed_lines.clear();
        }
        let other: Option<AnyElement> = match &split {
            None => None,
            // The left pane: its active tab.
            Some(s) if s.right_focused => Some(match self.tabs.active_target().cloned() {
                Some(TabTarget::Page(title)) => match self.find_page(&title) {
                    Some(ix) => self.render_other_page(ix, cx),
                    None => div().into_any_element(),
                },
                Some(TabTarget::Graph) => match &self.graph {
                    Some(graph) => graph.clone().into_any_element(),
                    None => div().into_any_element(),
                },
                Some(TabTarget::Agenda) => self.render_agenda(cx),
                Some(TabTarget::Trash) => self.render_trash(cx),
                None => empty_state(cx).into_any_element(),
            }),
            // The right pane: its page.
            Some(_) => Some(match self.right_page() {
                Some(ix) => self.render_other_page(ix, cx),
                None => div().into_any_element(),
            }),
        };
        let content =
            match (split, other) {
                (Some(split), Some(other)) => {
                    let right_focused = split.right_focused;
                    // A pane's × (each pane has one while split).
                    let close_button = |right: bool| {
                        let side = if right { "right" } else { "left" };
                        div()
                            .id(if right {
                                "pane-close-right"
                            } else {
                                "pane-close-left"
                            })
                            .debug_selector(move || format!("pane-close-{side}"))
                            .flex_shrink_0()
                            .px_2()
                            .mx_1()
                            .rounded_sm()
                            .cursor_pointer()
                            .text_color(theme.muted)
                            .hover(|d| d.bg(theme.selected_bg).text_color(theme.text))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                                    cx.stop_propagation();
                                    this.close_pane(right, cx);
                                }),
                            )
                            .child("×")
                    };
                    // The focused pane gets an accent line along its top.
                    let focus_line = |focused: bool, side: &'static str| {
                        div()
                            .flex_shrink_0()
                            .h(px(2.0))
                            .bg(if focused { theme.accent } else { theme.border })
                            .when(focused, |d| {
                                d.debug_selector(move || format!("pane-focus-{side}"))
                            })
                    };
                    let left_header = div()
                        .flex_shrink_0()
                        .flex()
                        .flex_row()
                        .items_center()
                        .bg(theme.sidebar_bg)
                        .child(div().flex_1().min_w_0().child(tab_bar))
                        .child(close_button(false));
                    let right_title = div()
                        .debug_selector(|| "pane-right-title".to_string())
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(if right_focused {
                            theme.accent
                        } else {
                            theme.muted
                        })
                        .child(split.right.clone());
                    let right_header = div()
                        .flex_shrink_0()
                        .h(px(font_size * 2.2))
                        .flex()
                        .flex_row()
                        .items_center()
                        .pl_3()
                        .bg(theme.sidebar_bg)
                        .border_b_1()
                        .border_color(theme.border)
                        .child(right_title)
                        .child(close_button(true));
                    let (left_view, right_view) = if right_focused {
                        (other, view)
                    } else {
                        (view, other)
                    };
                    let left = div()
                        .id("pane-left")
                        .debug_selector(|| "pane-left".to_string())
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .flex()
                        .flex_col()
                        .when(right_focused, |d| {
                            d.capture_any_mouse_down(cx.listener(
                                |this, _: &MouseDownEvent, _, cx| this.focus_pane(false, cx),
                            ))
                        })
                        .child(focus_line(!right_focused, "left"))
                        .child(left_header)
                        .child(left_view);
                    let right = div()
                        .id("pane-right")
                        .debug_selector(|| "pane-right".to_string())
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .flex()
                        .flex_col()
                        .border_l_1()
                        .border_color(theme.border)
                        .when(!right_focused, |d| {
                            d.capture_any_mouse_down(cx.listener(
                                |this, _: &MouseDownEvent, _, cx| this.focus_pane(true, cx),
                            ))
                        })
                        .child(focus_line(right_focused, "right"))
                        .child(right_header)
                        .child(right_view);
                    div()
                        .flex_1()
                        .h_full()
                        .min_w_0()
                        .flex()
                        .flex_row()
                        .child(left)
                        .child(right)
                }
                _ => div().flex_1().h_full().min_w_0().flex().flex_row().child(
                    div()
                        .id("pane-left")
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .flex()
                        .flex_col()
                        .child(tab_bar)
                        .child(view),
                ),
            };

        let settings_overlay = self
            .settings
            .as_ref()
            .map(|state| self.render_settings(state, cx));

        let page_menu_overlay = self
            .page_menu
            .as_ref()
            .map(|menu| self.render_page_menu(menu, cx));

        let shortcuts_overlay = self.shortcuts_open.then(|| self.render_shortcuts(cx));

        let ask_overlay = self.render_ai_overlay(cx);

        let trash_confirm_overlay = self
            .trash_confirm
            .as_ref()
            .map(|confirm| self.render_trash_confirm(confirm, cx));

        // The status message: a small box at the bottom right, over the
        // page but under any dialog.
        let publish_actions = self.render_publish_actions(cx);
        let clipper_actions = self.render_clipper_actions(cx);
        let status_toast = self.status.as_ref().map(|status| {
            div()
                .debug_selector(|| "status-toast".to_string())
                .absolute()
                .bottom(px(16.0))
                .right(px(16.0))
                .max_w(px(560.0))
                .px_3()
                .py_2()
                .rounded_md()
                .bg(theme.sidebar_bg)
                .border_1()
                .border_color(if status.error {
                    theme.danger
                } else {
                    theme.border
                })
                .text_color(if status.error {
                    theme.danger
                } else {
                    theme.text
                })
                .shadow_md()
                .child(status.text.clone())
                .children(publish_actions)
                .children(clipper_actions)
        });

        let is_editing = self.editing.is_some() || self.text_input_open();
        let whiteboard_focused = self.whiteboard_focused();
        let shortcuts_open = self.shortcuts_open;
        // While a Settings > AI field is edited it types, so the root takes
        // "BlockEditor" (below) instead of "Settings".
        let settings_open =
            self.settings.is_some() && !shortcuts_open && !self.ai_settings_editing();
        let page_menu_open = self.page_menu.is_some() && !is_editing && !shortcuts_open;
        let trash_confirm_open = self.trash_confirm.is_some()
            && !is_editing
            && !shortcuts_open
            && !settings_open
            && !page_menu_open;
        div()
            .size_full()
            .relative()
            .flex()
            .flex_row()
            .bg(theme.bg)
            .text_color(theme.text)
            // Font settings are set on the root so every descendant inherits them.
            .text_size(px(font_size))
            .when_some(self.font_family.clone(), |d, family| d.font_family(family))
            .track_focus(&self.focus_handle)
            // The key context only exists while editing, which is what makes
            // the "BlockEditor" key bindings conditional. The settings panel
            // takes over the keyboard context while it is open.
            .when(shortcuts_open, |d| d.key_context("Shortcuts"))
            .when(settings_open, |d| d.key_context("Settings"))
            .when(!shortcuts_open && !settings_open && is_editing, |d| {
                d.key_context("BlockEditor")
            })
            .when(!settings_open && page_menu_open, |d| {
                d.key_context("PageMenu")
            })
            .when(trash_confirm_open, |d| d.key_context("TrashDialog"))
            .when(whiteboard_focused, |d| d.key_context("Whiteboard"))
            .on_key_down(cx.listener(|this, e: &KeyDownEvent, _, _| {
                if e.keystroke.key == "space" {
                    this.set_whiteboard_space(true);
                }
            }))
            .on_key_up(cx.listener(|this, e: &KeyUpEvent, _, _| {
                if e.keystroke.key == "space" {
                    this.set_whiteboard_space(false);
                }
            }))
            .on_action(cx.listener(Self::enter))
            .on_action(cx.listener(Self::tab))
            .on_action(cx.listener(Self::shift_tab))
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::new_line))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::escape))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_home))
            .on_action(cx.listener(Self::select_end))
            // Drag-selection keeps tracking when the mouse leaves the block,
            // so these live on the root rather than the block.
            .on_mouse_move(cx.listener(Self::on_text_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_text_mouse_up))
            // Whiteboard drags, likewise (written on release).
            .on_mouse_move(cx.listener(Self::on_board_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_board_mouse_up))
            .on_mouse_up(MouseButton::Middle, cx.listener(Self::on_board_mouse_up))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::bold))
            .on_action(cx.listener(Self::italic))
            .on_action(cx.listener(Self::cycle_task))
            .on_action(cx.listener(Self::on_move_block_up))
            .on_action(cx.listener(Self::on_move_block_down))
            .on_action(cx.listener(Self::toggle_search))
            .on_action(cx.listener(Self::search_all_pages))
            .on_action(cx.listener(Self::on_quick_capture))
            .on_action(cx.listener(Self::on_new_page))
            .on_action(cx.listener(Self::on_open_today))
            .on_action(cx.listener(Self::on_undo))
            .on_action(cx.listener(Self::on_redo))
            .on_action(cx.listener(Self::on_toggle_theme))
            .on_action(cx.listener(Self::on_toggle_graph))
            .on_action(cx.listener(Self::on_close_tab))
            .on_action(cx.listener(Self::on_next_tab))
            .on_action(cx.listener(Self::on_prev_tab))
            .on_action(cx.listener(Self::on_increase_font))
            .on_action(cx.listener(Self::on_decrease_font))
            .on_action(cx.listener(Self::on_reset_font))
            .on_action(cx.listener(Self::on_open_settings))
            .on_action(cx.listener(Self::on_toggle_git_backup))
            .on_action(cx.listener(Self::on_customize_shortcuts))
            .on_action(cx.listener(Self::on_split_right))
            .on_action(cx.listener(Self::on_close_pane))
            .on_action(cx.listener(Self::on_focus_other_pane))
            .on_action(cx.listener(Self::on_open_agenda))
            .on_action(cx.listener(Self::on_open_trash))
            .on_action(cx.listener(Self::on_export_html))
            .on_action(cx.listener(Self::on_rename_page))
            .on_action(cx.listener(Self::on_delete_page))
            .on_action(cx.listener(Self::on_copy_page_title))
            .on_action(cx.listener(Self::on_toggle_favorite))
            .on_action(cx.listener(Self::on_sort_pages_az))
            .on_action(cx.listener(Self::on_insert_template))
            .on_action(cx.listener(Self::on_collapse_all))
            .on_action(cx.listener(Self::on_expand_all))
            .on_action(cx.listener(Self::on_toggle_local_graph))
            .on_action(cx.listener(Self::on_fit_graph))
            .on_action(cx.listener(Self::on_toggle_graph_journals))
            .on_action(cx.listener(Self::on_show_shortcuts))
            .on_action(cx.listener(Self::on_ask_my_notes))
            .on_action(cx.listener(Self::on_semantic_search))
            .on_action(cx.listener(Self::on_suggest_tags))
            .on_action(cx.listener(Self::on_save_search))
            .on_action(cx.listener(Self::on_toggle_vim_mode))
            .on_action(cx.listener(Self::on_publish_page))
            .on_action(cx.listener(Self::on_publish_page_with_links))
            .on_action(cx.listener(Self::on_import_obsidian))
            .on_action(cx.listener(Self::on_import_logseq))
            .on_action(cx.listener(Self::on_import_notion))
            .on_action(cx.listener(Self::on_record_voice_note))
            .on_action(cx.listener(Self::on_stop_recording))
            .on_action(cx.listener(Self::on_cancel_recording))
            .on_action(cx.listener(Self::on_transcribe_voice_notes))
            .on_action(cx.listener(Self::on_new_whiteboard))
            .on_action(cx.listener(Self::on_export_vault))
            .on_action(cx.listener(Self::on_import_vault))
            .on_action(cx.listener(Self::on_whiteboard_fit))
            .on_action(cx.listener(Self::on_whiteboard_zoom_reset))
            .on_action(cx.listener(Self::on_whiteboard_add_page))
            .on_action(cx.listener(Self::on_toggle_whiteboard_outline))
            .on_action(cx.listener(Self::on_whiteboard_delete))
            .child(sidebar)
            .child(content)
            .children(status_toast)
            .children(self.render_vim_pill())
            .children(self.render_voice_pill(cx))
            .children(ask_overlay)
            .children(overlay)
            .children(self.render_capture_overlay(cx))
            .children(settings_overlay)
            .children(page_menu_overlay)
            .children(shortcuts_overlay)
            .children(self.render_import_dialog(cx))
            .children(self.render_vault_dialog(cx))
            .children(trash_confirm_overlay)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ThemeKind;
    use crate::graph_view::Scope;
    use crate::state::MAX_RECENT;
    use gpui::{
        Modifiers, MouseButton, Point, ScrollDelta, ScrollWheelEvent, TestAppContext, TouchPhase,
        VisualTestContext,
    };
    use std::path::PathBuf;

    /// Build a window with a graph containing one page "Test" with `markdown`,
    /// selected and rendered. Returns the view, a test context and the graph dir.
    fn setup<'a>(
        cx: &'a mut TestAppContext,
        name: &str,
        markdown: &str,
    ) -> (Entity<NoteSec>, &'a mut VisualTestContext, PathBuf) {
        setup_pages(cx, name, &[("Test", markdown)], "Test")
    }

    /// Like `setup`, but seeds several `(title, markdown)` pages and selects
    /// the one called `selected`.
    fn setup_pages<'a>(
        cx: &'a mut TestAppContext,
        name: &str,
        pages: &[(&str, &str)],
        selected: &str,
    ) -> (Entity<NoteSec>, &'a mut VisualTestContext, PathBuf) {
        let dir = std::env::temp_dir().join(format!("notesec-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let storage = Storage::open(dir.clone()).unwrap();
        for (title, markdown) in pages {
            std::fs::write(dir.join(format!("pages/{title}.md")), markdown).unwrap();
        }

        cx.update(bind_keys);
        let (view, cx) =
            cx.add_window_view(|window, cx| NoteSec::new(storage, Config::default(), window, cx));
        view.update(cx, |app, cx| {
            app.selected = app.pages.iter().position(|p| p.title == selected).unwrap();
            // As if the app had started on that page: one tab showing it.
            app.tabs = Tabs::new(TabTarget::Page(selected.to_string()));
            cx.notify();
        });
        cx.run_until_parked();
        (view, cx, dir)
    }

    fn click_block(cx: &mut VisualTestContext, ix: usize) {
        // `debug_bounds` needs 'static selectors, so leak a tiny string in tests.
        let selector: &'static str = Box::leak(format!("block-{ix}").into_boxed_str());
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} was not rendered"));
        cx.simulate_click(bounds.center(), Modifiers::none());
    }

    fn file(dir: &std::path::Path) -> String {
        std::fs::read_to_string(dir.join("pages/Test.md")).unwrap()
    }

    #[gpui::test]
    fn ctrl_n_creates_and_focuses_the_first_block(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(
            cx,
            "new-page-shortcut",
            &[
                ("Test", "- existing\n"),
                ("Untitled", "- used\n"),
                ("Untitled 1", "- used\n"),
            ],
            "Test",
        );

        cx.simulate_keystrokes("ctrl-n");

        view.update(cx, |app, _| {
            assert_eq!(app.pages[app.selected].title, "Untitled 2");
            assert_eq!(app.editing, Some(0));
            assert_eq!(app.editor.text, "");
            assert!(app.pages.iter().any(|page| page.title == "Untitled 2"));
        });
        assert_eq!(
            std::fs::read_to_string(dir.join("pages/Untitled 2.md")).unwrap(),
            "- \n"
        );
        assert!(cx.debug_bounds("new-page").is_some());
    }

    #[gpui::test]
    fn new_page_button_creates_the_first_free_name_and_shows_it(cx: &mut TestAppContext) {
        let (view, cx, dir) =
            setup_pages(cx, "new-page-button", &[("Test", "- existing\n")], "Test");
        let button = cx
            .debug_bounds("new-page")
            .expect("new page button rendered");

        cx.simulate_click(button.center(), Modifiers::none());

        view.update(cx, |app, _| {
            assert_eq!(app.pages[app.selected].title, "Untitled");
            assert_eq!(app.editing, Some(0));
            assert!(app.pages.iter().any(|page| page.title == "Untitled"));
        });
        assert_eq!(
            std::fs::read_to_string(dir.join("pages/Untitled.md")).unwrap(),
            "- \n"
        );
    }

    /// Path of today's journal file in `dir` (Logseq naming: `YYYY_MM_DD.md`).
    fn todays_journal_file(dir: &std::path::Path) -> PathBuf {
        dir.join(format!("journals/{}.md", today_title().replace('-', "_")))
    }

    /// Like `setup`, but today's journal already exists on disk with
    /// `journal_markdown` before the app starts.
    fn setup_with_journal<'a>(
        cx: &'a mut TestAppContext,
        name: &str,
        journal_markdown: &str,
    ) -> (Entity<NoteSec>, &'a mut VisualTestContext, PathBuf) {
        let dir = std::env::temp_dir().join(format!("notesec-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let storage = Storage::open(dir.clone()).unwrap();
        std::fs::write(dir.join("pages/Test.md"), "- hello\n").unwrap();
        std::fs::write(todays_journal_file(&dir), journal_markdown).unwrap();

        cx.update(bind_keys);
        let (view, cx) =
            cx.add_window_view(|window, cx| NoteSec::new(storage, Config::default(), window, cx));
        view.update(cx, |app, cx| {
            app.selected = app.pages.iter().position(|p| p.title == "Test").unwrap();
            cx.notify();
        });
        cx.run_until_parked();
        (view, cx, dir)
    }

    #[gpui::test]
    fn today_button_opens_the_existing_journal(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_with_journal(cx, "today-existing", "- morning notes\n");
        let button = cx.debug_bounds("today").expect("Today button rendered");

        cx.simulate_click(button.center(), Modifiers::none());

        let today = today_title();
        view.update(cx, |app, _| {
            let page = &app.pages[app.selected];
            assert!(page.is_journal);
            assert_eq!(page.title, today);
            assert_eq!(page.blocks[0].content, "morning notes");
            assert_eq!(
                app.pages
                    .iter()
                    .filter(|p| p.is_journal && p.title == today)
                    .count(),
                1,
                "no duplicate journal created"
            );
        });
        assert_eq!(
            std::fs::read_to_string(todays_journal_file(&dir)).unwrap(),
            "- morning notes\n",
            "existing journal left untouched"
        );
    }

    #[gpui::test]
    fn today_button_creates_a_missing_journal(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "today-missing", "- hello\n");
        // Simulate the journal not existing (e.g. the app was left open past
        // midnight, or the file was deleted): drop it from memory and disk.
        let today = today_title();
        view.update(cx, |app, cx| {
            app.pages.retain(|p| !(p.is_journal && p.title == today));
            app.selected = app.find_page("Test").unwrap();
            cx.notify();
        });
        std::fs::remove_file(todays_journal_file(&dir)).unwrap();
        cx.run_until_parked();

        let button = cx.debug_bounds("today").expect("Today button rendered");
        cx.simulate_click(button.center(), Modifiers::none());

        view.update(cx, |app, _| {
            let page = &app.pages[app.selected];
            assert!(page.is_journal);
            assert_eq!(page.title, today);
            assert_eq!(app.mode, Mode::Notes);
            // Journals sort first, so it is back at the top of the sidebar.
            assert_eq!(app.selected, 0);
        });
        assert_eq!(
            std::fs::read_to_string(todays_journal_file(&dir)).unwrap(),
            "- \n",
            "journal file created with Logseq naming"
        );
    }

    #[gpui::test]
    fn ctrl_j_opens_today_and_saves_the_edited_block(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "today-ctrl-j", "- hello\n");
        click_block(cx, 0);
        cx.simulate_input(" world");

        cx.simulate_keystrokes("ctrl-j");

        view.update(cx, |app, _| {
            let page = &app.pages[app.selected];
            assert!(page.is_journal);
            assert_eq!(page.title, today_title());
            assert_eq!(app.editing, None);
        });
        assert_eq!(file(&dir), "- hello world\n");
    }

    #[gpui::test]
    fn quick_capture_hotkey_appends_to_todays_journal(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_with_journal(cx, "quick-capture", "- morning notes\n");

        cx.simulate_keystrokes("ctrl-shift-c");
        view.update(cx, |app, _| {
            assert!(app.capture.is_some(), "capture box opened");
        });

        cx.simulate_input("captured thought");
        cx.simulate_keystrokes("enter");

        view.update(cx, |app, _| {
            assert!(app.capture.is_none(), "capture box closed");
            let journal = app
                .pages
                .iter()
                .find(|p| p.is_journal && p.title == today_title())
                .expect("today's journal");
            assert!(journal
                .blocks
                .iter()
                .any(|b| b.content == "captured thought"));
        });
        let text = std::fs::read_to_string(todays_journal_file(&dir)).unwrap();
        assert!(text.contains("- captured thought\n"), "{text}");
    }

    #[gpui::test]
    fn quick_capture_escape_cancels_without_writing(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_with_journal(cx, "quick-capture-esc", "- morning notes\n");

        cx.simulate_keystrokes("ctrl-shift-c");
        cx.simulate_input("never mind");
        cx.simulate_keystrokes("escape");

        view.update(cx, |app, _| {
            assert!(app.capture.is_none(), "capture box closed");
        });
        assert_eq!(
            std::fs::read_to_string(todays_journal_file(&dir)).unwrap(),
            "- morning notes\n"
        );
    }

    #[gpui::test]
    fn ctrl_j_leaves_the_graph_view(cx: &mut TestAppContext) {
        let (view, cx, _dir) = setup(cx, "today-from-graph", "- hello\n");
        cx.simulate_keystrokes("ctrl-g");
        view.update(cx, |app, _| assert_eq!(app.mode, Mode::Graph));

        cx.simulate_keystrokes("ctrl-j");

        view.update(cx, |app, _| {
            assert_eq!(app.mode, Mode::Notes);
            assert_eq!(app.pages[app.selected].title, today_title());
        });
    }

    /// Open the palette, choose "Insert template", and return the template
    /// names the picker lists.
    fn open_template_picker(view: &Entity<NoteSec>, cx: &mut VisualTestContext) -> Vec<String> {
        cx.simulate_keystrokes("ctrl-k");
        cx.simulate_input("insert template");
        cx.simulate_keystrokes("enter");
        view.update(cx, |app, _| {
            let state = app.search.as_ref().expect("palette stays open");
            let templates = state.templates.as_ref().expect("in template mode");
            assert_eq!(state.query.text, "", "query cleared for the picker");
            app.search_results()
                .iter()
                .map(|hit| match hit.target {
                    Target::Template(ix) => templates[ix].name.clone(),
                    other => panic!("unexpected hit {other:?}"),
                })
                .collect()
        })
    }

    #[gpui::test]
    fn insert_template_lists_templates_and_inserts_after_the_edited_block(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "template-insert", "- one\n- two\n");
        std::fs::write(
            dir.join("templates/Meeting.md"),
            "- Agenda\n  - item\n- Notes\n",
        )
        .unwrap();
        click_block(cx, 0);

        let names = open_template_picker(&view, cx);
        assert_eq!(names, ["Daily review", "Meeting"]);

        cx.simulate_input("meet");
        cx.simulate_keystrokes("enter");

        view.update(cx, |app, _| {
            assert!(app.search.is_none(), "palette closes after inserting");
            let contents: Vec<&str> = app.pages[app.selected]
                .blocks
                .iter()
                .map(|b| b.content.as_str())
                .collect();
            assert_eq!(contents, ["one", "Agenda", "item", "Notes", "two"]);
        });
        assert_eq!(file(&dir), "- one\n- Agenda\n  - item\n- Notes\n- two\n");

        // The whole insert is one undo step.
        cx.simulate_keystrokes("ctrl-z");
        view.update(cx, |app, _| {
            assert_eq!(app.pages[app.selected].blocks.len(), 2);
        });
        assert_eq!(file(&dir), "- one\n- two\n");
    }

    #[gpui::test]
    fn insert_template_without_an_edited_block_appends_to_the_page(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "template-append", "- one\n  - child\n");
        // Not editing: the template goes at the end, at the top level.
        open_template_picker(&view, cx);
        cx.simulate_keystrokes("enter"); // first (only) template: Daily review
        assert_eq!(
            file(&dir),
            "- one\n  - child\n- Wins\n  - \n- Lessons\n  - \n- Plan for tomorrow\n  - \n"
        );
    }

    #[gpui::test]
    fn insert_template_replaces_a_blank_page(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "template-blank", "- \n");
        open_template_picker(&view, cx);
        cx.simulate_keystrokes("enter");
        assert_eq!(
            file(&dir),
            "- Wins\n  - \n- Lessons\n  - \n- Plan for tomorrow\n  - \n"
        );
    }

    #[gpui::test]
    fn template_picker_with_no_templates_inserts_nothing(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "template-none", "- one\n");
        std::fs::remove_file(dir.join("templates/Daily review.md")).unwrap();

        assert!(open_template_picker(&view, cx).is_empty());
        cx.simulate_keystrokes("enter");

        view.update(cx, |app, _| {
            assert!(app.search.is_some(), "nothing to pick, palette stays open");
        });
        assert_eq!(file(&dir), "- one\n");
    }

    #[gpui::test]
    fn undo_and_redo_restore_block_text(cx: &mut TestAppContext) {
        let (view, cx, _dir) = setup(cx, "undo-text", "- one\n");
        click_block(cx, 0);
        cx.simulate_input(" changed");

        view.update(cx, |app, _| assert_eq!(app.editor.text, "one changed"));
        cx.simulate_keystrokes("ctrl-z");
        view.update(cx, |app, _| {
            assert_eq!(app.editor.text, "one");
            assert_eq!(app.pages[app.selected].blocks[0].content, "one");
        });

        cx.simulate_keystrokes("ctrl-shift-z");
        view.update(cx, |app, _| assert_eq!(app.editor.text, "one changed"));
        cx.simulate_keystrokes("ctrl-z ctrl-y");
        view.update(cx, |app, _| assert_eq!(app.editor.text, "one changed"));
    }

    #[gpui::test]
    fn undo_restores_structural_block_changes(cx: &mut TestAppContext) {
        let (view, cx, _dir) = setup(cx, "undo-structure", "- one\n- two\n");
        click_block(cx, 0);
        cx.simulate_keystrokes("end enter");

        view.update(cx, |app, _| {
            assert_eq!(app.pages[app.selected].blocks.len(), 3)
        });
        cx.simulate_keystrokes("ctrl-z");
        view.update(cx, |app, _| {
            assert_eq!(app.pages[app.selected].blocks.len(), 2);
            assert_eq!(app.pages[app.selected].blocks[0].content, "one");
        });
        cx.simulate_keystrokes("ctrl-y");
        view.update(cx, |app, _| {
            assert_eq!(app.pages[app.selected].blocks.len(), 3)
        });

        cx.simulate_keystrokes("escape");
        click_block(cx, 1);
        cx.simulate_keystrokes("tab");
        view.update(cx, |app, _| {
            assert_eq!(app.pages[app.selected].depth_of(1), 1);
        });
        cx.simulate_keystrokes("ctrl-z");
        view.update(cx, |app, _| {
            assert_eq!(app.pages[app.selected].depth_of(1), 0);
        });
        cx.simulate_keystrokes("ctrl-y");
        view.update(cx, |app, _| {
            assert_eq!(app.pages[app.selected].depth_of(1), 1);
        });
        cx.simulate_keystrokes("shift-tab");
        view.update(cx, |app, _| {
            assert_eq!(app.pages[app.selected].depth_of(1), 0);
        });
    }

    #[gpui::test]
    fn new_edit_clears_redo_history(cx: &mut TestAppContext) {
        let (view, cx, _dir) = setup(cx, "undo-redo-clear", "- one\n");
        click_block(cx, 0);
        cx.simulate_input(" two");
        cx.simulate_keystrokes("ctrl-z");
        cx.simulate_input(" three");
        cx.simulate_keystrokes("ctrl-y");
        view.update(cx, |app, _| assert_eq!(app.editor.text, "one three"));
    }

    #[gpui::test]
    fn click_type_enter_tab_backspace(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "flow", "- one\n- two\n");

        // Click block 0 -> edit mode, text loaded, cursor at end.
        click_block(cx, 0);
        view.update(cx, |app, _| {
            assert_eq!(app.editing, Some(0));
            assert_eq!(app.editor.text, "one");
            assert_eq!(app.editor.cursor, 3);
        });

        // Typing goes through the OS input handler.
        cx.simulate_input("!");
        view.update(cx, |app, _| assert_eq!(app.editor.text, "one!"));
        // Nothing hits disk on keystrokes.
        assert_eq!(file(&dir), "- one\n- two\n");

        // Enter at end -> new empty block below, now being edited.
        cx.simulate_keystrokes("enter");
        view.update(cx, |app, _| {
            assert_eq!(app.editing, Some(1));
            assert_eq!(app.pages[app.selected].blocks.len(), 3);
            assert_eq!(app.pages[app.selected].blocks[0].content, "one!");
        });
        assert_eq!(file(&dir), "- one!\n- \n- two\n");

        cx.simulate_input("new");

        // Tab nests it under "one!"; the typed text is saved with it.
        cx.simulate_keystrokes("tab");
        assert_eq!(file(&dir), "- one!\n  - new\n- two\n");

        // Shift-Tab brings it back out.
        cx.simulate_keystrokes("shift-tab");
        assert_eq!(file(&dir), "- one!\n- new\n- two\n");

        // Backspace erases text first...
        cx.simulate_keystrokes("backspace backspace backspace");
        view.update(cx, |app, _| {
            assert_eq!(app.editor.text, "");
            assert_eq!(app.pages[app.selected].blocks.len(), 3);
        });
        // ...and on the now-empty block deletes it, moving to the block above.
        cx.simulate_keystrokes("backspace");
        view.update(cx, |app, _| {
            assert_eq!(app.editing, Some(0));
            assert_eq!(app.editor.text, "one!");
            assert_eq!(app.pages[app.selected].blocks.len(), 2);
        });
        assert_eq!(file(&dir), "- one!\n- two\n");

        // Escape leaves edit mode.
        cx.simulate_keystrokes("escape");
        view.update(cx, |app, _| assert_eq!(app.editing, None));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn enter_in_the_middle_splits_the_block(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "split", "- hello world\n");
        click_block(cx, 0);
        cx.simulate_keystrokes("home right right right right right");
        cx.simulate_keystrokes("enter");
        view.update(cx, |app, _| {
            assert_eq!(app.editing, Some(1));
            assert_eq!(app.editor.cursor, 0);
            assert_eq!(app.editor.text, " world");
        });
        assert_eq!(file(&dir), "- hello\n-  world\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    fn page_file(dir: &std::path::Path, title: &str) -> PathBuf {
        dir.join("pages").join(format!("{title}.md"))
    }

    #[gpui::test]
    fn clicking_a_link_navigates_and_creates_the_page(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "linknav", "- [[Foo]] tail\n");
        let row = cx.debug_bounds("block-0").expect("row rendered");

        // The row is: bullet (6px) + gap (8px) + text, so x + 24 is on `[[Foo]]`.
        cx.simulate_click(
            point(row.left() + px(24.), row.center().y),
            Modifiers::none(),
        );
        view.update(cx, |app, _| {
            assert_eq!(app.editing, None, "link click must not start editing");
            assert_eq!(app.pages[app.selected].title, "Foo");
        });
        assert!(page_file(&dir, "Foo").exists(), "Foo.md should be created");
        // The page we came from is unchanged.
        assert_eq!(file(&dir), "- [[Foo]] tail\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn clicking_outside_the_link_still_edits(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "linkedit", "- [[Foo]] tail\n");
        let row = cx.debug_bounds("block-0").expect("row rendered");
        // Far right of the row: past the end of the text.
        cx.simulate_click(
            point(row.right() - px(10.), row.center().y),
            Modifiers::none(),
        );
        view.update(cx, |app, _| {
            assert_eq!(app.editing, Some(0));
            assert_eq!(app.pages[app.selected].title, "Test");
        });
        assert!(!page_file(&dir, "Foo").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn typed_links_create_pages_only_on_commit(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "linktype", "- hi\n");
        click_block(cx, 0);

        // Half-typed link: nothing should be created.
        cx.simulate_input(" [[Ba");
        view.update(cx, |app, _| assert!(app.find_page("Ba").is_none()));
        assert!(!page_file(&dir, "Ba").exists());

        // Finish it, and add a differently-cased link to the existing page.
        cx.simulate_input("r]] [[test]]");
        let page_count = view.update(cx, |app, _| app.pages.len());
        assert!(!page_file(&dir, "Bar").exists(), "not created until commit");

        cx.simulate_keystrokes("escape");
        assert!(page_file(&dir, "Bar").exists(), "created on commit");
        assert_eq!(file(&dir), "- hi [[Bar]] [[test]]\n");
        view.update(cx, |app, _| {
            // Bar was added; `[[test]]` matched `Test` case-insensitively.
            assert_eq!(app.pages.len(), page_count + 1);
            assert!(app.find_page("Bar").is_some());
            // Sorting moved pages around but we are still looking at Test.
            assert_eq!(app.pages[app.selected].title, "Test");
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn backlinks_panel_lists_and_navigates(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(
            cx,
            "backlinks",
            &[
                ("Test", "- hi [[Test]]\n"), // self-link: must not count
                ("Alpha", "- see [[Test]]\n- unrelated\n"),
                ("Beta", "- x [[test]] y\n  - nested [[Other]]\n"),
            ],
            "Test",
        );

        // Two references (Alpha's first block, Beta's first block), in order.
        assert!(cx.debug_bounds("backlink-0").is_some());
        assert!(cx.debug_bounds("backlink-1").is_some());
        assert!(cx.debug_bounds("backlink-2").is_none());

        // Clicking the first reference opens its source page, Alpha.
        let r = cx.debug_bounds("backlink-0").unwrap();
        cx.simulate_click(r.center(), Modifiers::none());
        view.update(cx, |app, _| {
            assert_eq!(app.pages[app.selected].title, "Alpha")
        });

        // Nothing links to Alpha, so its panel is gone.
        assert!(cx.debug_bounds("backlink-0").is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn backlinks_update_after_editing(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(
            cx,
            "backlinks-live",
            &[("Test", "- one\n"), ("Alpha", "- plain\n")],
            "Alpha",
        );
        assert!(cx.debug_bounds("backlink-0").is_none());

        // Add a link to Alpha from the Test page and commit it.
        view.update(cx, |app, cx| {
            app.selected = app.pages.iter().position(|p| p.title == "Test").unwrap();
            cx.notify();
        });
        cx.run_until_parked();
        click_block(cx, 0);
        cx.simulate_input(" [[Alpha]]");
        cx.simulate_keystrokes("escape");

        // Back on Alpha, the new reference shows up.
        view.update(cx, |app, cx| {
            app.selected = app.pages.iter().position(|p| p.title == "Alpha").unwrap();
            cx.notify();
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("backlink-0").is_some());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn tag_index_in_sidebar_opens_tag_page_with_its_uses(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "tags-index", "- a #idea\n- b #idea #todo\n- plain\n");

        // Sidebar lists tags by usage: idea (2 blocks), then todo (1).
        assert!(cx.debug_bounds("tag-0").is_some());
        assert!(cx.debug_bounds("tag-1").is_some());
        assert!(cx.debug_bounds("tag-2").is_none());
        view.update(cx, |app, _| {
            let tags = tag_counts(&app.pages);
            assert_eq!(tags, vec![("idea".to_string(), 2), ("todo".to_string(), 1)]);
        });

        // Clicking the first tag opens (and creates) the `idea` page...
        let row = cx.debug_bounds("tag-0").unwrap();
        cx.simulate_click(row.center(), Modifiers::none());
        view.update(cx, |app, _| {
            assert_eq!(app.pages[app.selected].title, "idea")
        });
        assert!(page_file(&dir, "idea").exists());

        // ...whose backlinks panel is the tag's index: both tagged blocks.
        assert!(cx.debug_bounds("backlink-0").is_some());
        assert!(cx.debug_bounds("backlink-1").is_some());
        assert!(cx.debug_bounds("backlink-2").is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn clicking_a_tag_in_a_block_navigates(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "tags-click", "- #idea tail\n");
        let row = cx.debug_bounds("block-0").expect("row rendered");

        // Same geometry as the wikilink test: x + 24 lands on the tag text.
        cx.simulate_click(
            point(row.left() + px(24.), row.center().y),
            Modifiers::none(),
        );
        view.update(cx, |app, _| {
            assert_eq!(app.editing, None, "tag click must not start editing");
            assert_eq!(app.pages[app.selected].title, "idea");
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn typed_tags_create_pages_only_on_commit(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "tags-type", "- hi\n");
        click_block(cx, 0);

        cx.simulate_input(" #ne");
        assert!(!page_file(&dir, "ne").exists(), "nothing while typing");
        cx.simulate_input("wtag and #[[two words]]");
        assert!(!page_file(&dir, "newtag").exists());

        cx.simulate_keystrokes("escape");
        assert!(page_file(&dir, "newtag").exists());
        assert!(page_file(&dir, "two words").exists());
        assert!(
            !page_file(&dir, "ne").exists(),
            "partial tag never became a page"
        );
        assert_eq!(file(&dir), "- hi #newtag and #[[two words]]\n");
        view.update(cx, |app, _| {
            assert_eq!(app.pages[app.selected].title, "Test");
            assert_eq!(tag_counts(&app.pages).len(), 2);
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    fn saved_config(dir: &std::path::Path) -> Config {
        Config::load(dir)
    }

    #[gpui::test]
    fn theme_shortcut_toggles_and_persists(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "theme-key", "- hi\n");
        let dark = view.update(cx, |app, _| app.theme.bg);
        assert_eq!(
            view.update(cx, |app, _| app.theme_kind()),
            ThemeKind::TokyoNight,
            "Tokyo Night is the default"
        );

        cx.simulate_keystrokes("ctrl-shift-t");
        view.update(cx, |app, _| {
            assert_eq!(app.theme_kind(), ThemeKind::Light);
            assert_ne!(app.theme.bg, dark, "colours actually changed");
        });
        assert_eq!(
            UiState::load(&dir).theme,
            Some(ThemeKind::Light),
            "written to state.toml"
        );

        cx.simulate_keystrokes("ctrl-shift-t");
        view.update(cx, |app, _| assert_eq!(app.theme.bg, dark));
        assert_eq!(UiState::load(&dir).theme, Some(ThemeKind::TokyoNight));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn font_size_shortcuts_change_layout_and_persist(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "font-keys", "- hi\n");
        let row_height =
            |cx: &mut VisualTestContext| cx.debug_bounds("block-0").unwrap().size.height;
        let base = row_height(cx);

        cx.simulate_keystrokes("ctrl-= ctrl-= ctrl-=");
        view.update(cx, |app, _| {
            assert_eq!(app.ui_size(), 19.0);
            assert_eq!(app.mono_size(), 17.0);
        });
        assert_eq!(UiState::load(&dir).ui_size, Some(19.0));
        assert!(row_height(cx) > base, "bigger font gives taller rows");

        cx.simulate_keystrokes("ctrl--");
        view.update(cx, |app, _| assert_eq!(app.ui_size(), 18.0));

        cx.simulate_keystrokes("ctrl-0");
        view.update(cx, |app, _| {
            assert_eq!(app.ui_size(), 16.0);
            assert_eq!(app.mono_size(), 14.0);
        });
        assert_eq!(row_height(cx), base, "reset restores the original layout");

        // Limits: it cannot shrink or grow without bound.
        for _ in 0..40 {
            cx.simulate_keystrokes("ctrl--");
        }
        view.update(cx, |app, _| {
            assert_eq!(app.ui_size(), crate::config::MIN_FONT_SIZE)
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn palette_commands_run(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "palette-cmd", "- hi\n");

        cx.simulate_keystrokes("ctrl-k");
        cx.simulate_input("toggle theme");
        view.update(cx, |app, _| {
            assert_eq!(
                app.search_results()[0].target,
                Target::Command(Command::ToggleTheme),
                "the command is the top result"
            );
        });
        cx.simulate_keystrokes("enter");
        view.update(cx, |app, _| {
            assert!(
                app.search.is_none(),
                "palette closes after running a command"
            );
            assert_eq!(app.theme_kind(), ThemeKind::Light);
        });
        assert_eq!(UiState::load(&dir).theme, Some(ThemeKind::Light));

        cx.simulate_keystrokes("ctrl-k");
        cx.simulate_input("increase font");
        cx.simulate_keystrokes("enter");
        view.update(cx, |app, _| assert_eq!(app.ui_size(), 17.0));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn unknown_font_is_ignored_but_not_erased(cx: &mut TestAppContext) {
        let dir = std::env::temp_dir().join(format!("notesec-test-badfont-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let storage = Storage::open(dir.clone()).unwrap();
        // A font name that isn't installed: kept in state.toml, system font used.
        let mut state = UiState::load(&dir);
        state.ui_font = Some("Definitely Not A Font 12345".into());
        state.save(&dir).unwrap();
        let config = Config::default();

        cx.update(bind_keys);
        let (view, cx) = cx.add_window_view(|window, cx| NoteSec::new(storage, config, window, cx));
        view.update(cx, |app, _| {
            assert_eq!(app.font_family, None, "falls back to the system font");
            assert_eq!(
                app.state.ui_font.as_deref(),
                Some("Definitely Not A Font 12345"),
                "the setting itself is kept"
            );
        });
        // Changing another setting rewrites the file; the user's font name stays.
        cx.simulate_keystrokes("ctrl-=");
        assert_eq!(
            UiState::load(&dir).ui_font.as_deref(),
            Some("Definitely Not A Font 12345")
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn settings_loaded_from_config_are_applied(cx: &mut TestAppContext) {
        let dir = std::env::temp_dir().join(format!("notesec-test-loadcfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let storage = Storage::open(dir.clone()).unwrap();
        std::fs::write(Config::path(&dir), "theme = \"light\"\nfont_size = 22.0\n").unwrap();
        let config = Config::load(&dir);

        let (view, cx) = cx.add_window_view(|window, cx| NoteSec::new(storage, config, window, cx));
        view.update(cx, |app, _| {
            assert_eq!(app.theme.bg, Theme::light().bg);
            assert_eq!(app.ui_size(), 22.0);
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Open the graph via the sidebar entry and return its entity plus the
    /// on-screen bounds of the drawing area.
    fn open_graph(
        view: &Entity<NoteSec>,
        cx: &mut VisualTestContext,
    ) -> (Entity<GraphView>, Bounds<Pixels>) {
        let item = cx
            .debug_bounds("graph-item")
            .expect("sidebar has a Graph view entry");
        cx.simulate_click(item.center(), Modifiers::none());
        cx.run_until_parked();
        let graph = view
            .update(cx, |app, _| app.graph.clone())
            .expect("graph created");
        let canvas = cx.debug_bounds("graph-canvas").expect("graph canvas drawn");
        (graph, canvas)
    }

    /// Absolute window position of a node, found from the graph's own layout.
    fn node_pos(
        graph: &Entity<GraphView>,
        canvas: Bounds<Pixels>,
        title: &str,
        cx: &mut VisualTestContext,
    ) -> Point<Pixels> {
        let (x, y) = graph
            .update(cx, |g, _| g.node_screen_pos(title))
            .unwrap_or_else(|| panic!("no node {title}"));
        point(canvas.origin.x + px(x), canvas.origin.y + px(y))
    }

    fn graph_pages() -> [(&'static str, &'static str); 3] {
        [
            ("Test", "- links to [[Alpha]]\n"),
            ("Alpha", "- back to [[Test]] and [[Zed]]\n"),
            ("Zed", "- leaf\n"),
        ]
    }

    #[gpui::test]
    fn graph_opens_from_sidebar_and_toggles_with_ctrl_g(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "graph-open", &graph_pages(), "Test");
        assert!(
            cx.debug_bounds("graph-canvas").is_none(),
            "notes view first"
        );

        let (graph, canvas) = open_graph(&view, cx);
        view.update(cx, |app, _| assert_eq!(app.mode, Mode::Graph));
        assert!(canvas.size.width > px(300.) && canvas.size.height > px(200.));
        // 3 pages + today's journal.
        assert_eq!(
            graph.update(cx, |g, _| g.node_screen_pos("Alpha").is_some()),
            true
        );

        cx.simulate_keystrokes("ctrl-g");
        view.update(cx, |app, _| assert_eq!(app.mode, Mode::Notes));
        assert!(cx.debug_bounds("graph-canvas").is_none());
        cx.simulate_keystrokes("ctrl-g");
        view.update(cx, |app, _| assert_eq!(app.mode, Mode::Graph));

        // Choosing a page in the sidebar leaves the graph.
        cx.simulate_keystrokes("ctrl-g");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn clicking_a_node_opens_that_page(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "graph-click", &graph_pages(), "Test");
        let (graph, canvas) = open_graph(&view, cx);

        let alpha = node_pos(&graph, canvas, "Alpha", cx);
        cx.simulate_click(alpha, Modifiers::none());
        view.update(cx, |app, _| {
            assert_eq!(app.mode, Mode::Notes, "clicking a node leaves the graph");
            assert_eq!(app.pages[app.selected].title, "Alpha");
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn hovering_a_node_highlights_it_and_leaving_clears_it(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "graph-hover", &graph_pages(), "Test");
        let (graph, canvas) = open_graph(&view, cx);

        let zed = node_pos(&graph, canvas, "Zed", cx);
        cx.simulate_mouse_move(zed, None, Modifiers::none());
        assert_eq!(
            graph.update(cx, |g, _| g.hovered_title()),
            Some("Zed".to_string())
        );

        // Empty corner of the pane: nothing hovered.
        let corner = point(canvas.origin.x + px(4.), canvas.origin.y + px(4.));
        cx.simulate_mouse_move(corner, None, Modifiers::none());
        assert_eq!(graph.update(cx, |g, _| g.hovered_title()), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn dragging_a_node_pins_it_without_leaving_the_graph(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "graph-drag", &graph_pages(), "Test");
        let (graph, canvas) = open_graph(&view, cx);

        let from = node_pos(&graph, canvas, "Zed", cx);
        let to = point(from.x + px(70.), from.y + px(45.));
        cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(
            point(from.x + px(20.), from.y + px(10.)),
            Some(MouseButton::Left),
            Modifiers::none(),
        );
        cx.simulate_mouse_move(to, Some(MouseButton::Left), Modifiers::none());
        cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::none());

        view.update(cx, |app, _| {
            assert_eq!(app.mode, Mode::Graph, "a drag is not a click")
        });
        assert!(graph.update(cx, |g, _| g.is_pinned("Zed")));
        let now = node_pos(&graph, canvas, "Zed", cx);
        assert!(
            (now.x - to.x).abs() < px(1.) && (now.y - to.y).abs() < px(1.),
            "node sits where it was dropped"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn scroll_wheel_zooms_the_graph(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "graph-zoom", &graph_pages(), "Test");
        let (graph, canvas) = open_graph(&view, cx);

        let before = graph.update(cx, |g, _| g.zoom());
        cx.simulate_event(ScrollWheelEvent {
            position: canvas.center(),
            delta: ScrollDelta::Pixels(point(px(0.), px(120.))),
            modifiers: Modifiers::none(),
            touch_phase: TouchPhase::Moved,
        });
        let zoomed_in = graph.update(cx, |g, _| g.zoom());
        assert!(
            zoomed_in > before,
            "scroll up zooms in ({before} -> {zoomed_in})"
        );

        cx.simulate_event(ScrollWheelEvent {
            position: canvas.center(),
            delta: ScrollDelta::Pixels(point(px(0.), px(-300.))),
            modifiers: Modifiers::none(),
            touch_phase: TouchPhase::Moved,
        });
        assert!(
            graph.update(cx, |g, _| g.zoom()) < zoomed_in,
            "scroll down zooms out"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn journals_toggle_button_changes_the_node_count(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "graph-journals", &graph_pages(), "Test");
        let (graph, _canvas) = open_graph(&view, cx);
        let todays_journal = today_title();
        assert!(graph.update(cx, |g, _| g.node_screen_pos(&todays_journal).is_some()));

        let button = cx.debug_bounds("graph-journals").expect("toggle button");
        cx.simulate_click(button.center(), Modifiers::none());
        assert!(
            graph.update(cx, |g, _| g.node_screen_pos(&todays_journal).is_none()),
            "journal hidden"
        );
        cx.simulate_click(button.center(), Modifiers::none());
        assert!(
            graph.update(cx, |g, _| g.node_screen_pos(&todays_journal).is_some()),
            "journal back"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn graph_follows_theme_changes_and_palette_command(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "graph-theme", &graph_pages(), "Test");
        let (graph, _) = open_graph(&view, cx);
        let dark = graph.update(cx, |g, _| g.theme_bg());
        cx.simulate_keystrokes("ctrl-shift-t");
        assert_ne!(
            graph.update(cx, |g, _| g.theme_bg()),
            dark,
            "graph switches to the light theme"
        );

        // The palette command toggles the view too.
        cx.simulate_keystrokes("ctrl-k");
        cx.simulate_input("toggle graph view");
        cx.simulate_keystrokes("enter");
        view.update(cx, |app, _| assert_eq!(app.mode, Mode::Notes));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn graph_shows_edited_links_when_reopened_and_keeps_pins(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "graph-reopen", &graph_pages(), "Test");
        let (graph, canvas) = open_graph(&view, cx);

        // Drag Zed somewhere and leave the graph.
        let from = node_pos(&graph, canvas, "Zed", cx);
        let to = point(from.x + px(60.), from.y + px(60.));
        cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(to, Some(MouseButton::Left), Modifiers::none());
        cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::none());
        cx.simulate_keystrokes("ctrl-g");

        // Add a link to a brand-new page from the Test page.
        click_block(cx, 0);
        cx.simulate_input(" [[Fresh]]");
        cx.simulate_keystrokes("escape");

        let (graph, _) = open_graph_again(&view, cx);
        assert!(
            graph.update(cx, |g, _| g.node_screen_pos("Fresh").is_some()),
            "new page appears"
        );
        assert!(
            graph.update(cx, |g, _| g.is_pinned("Zed")),
            "dragged node is still pinned"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    fn graph_titles(graph: &Entity<GraphView>, cx: &mut VisualTestContext) -> Vec<String> {
        let mut titles = graph.update(cx, |g, _| g.node_titles());
        titles.sort();
        titles
    }

    #[gpui::test]
    fn local_graph_toggle_shows_neighbours_and_nodes_still_open_pages(cx: &mut TestAppContext) {
        let pages = [
            ("Test", "- links to [[Alpha]] about #topic\n"),
            ("Alpha", "- back to [[Test]] and [[Zed]]\n"),
            ("Zed", "- leaf\n"),
            ("topic", "- tag page\n"),
            ("Island", "- alone\n"),
        ];
        let (view, cx, dir) = setup_pages(cx, "graph-local", &pages, "Test");
        let today = today_title();
        let mut all = vec!["Alpha", "Island", "Test", "Zed", "topic", today.as_str()]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>();
        all.sort();
        let (graph, _) = open_graph(&view, cx);
        assert_eq!(graph_titles(&graph, cx), all, "global by default");

        // Local: the open page, the page it links to and its tag.
        click_on(cx, "graph-mode-local");
        assert_eq!(graph.update(cx, |g, _| g.scope()), Scope::Local);
        assert_eq!(graph_titles(&graph, cx), vec!["Alpha", "Test", "topic"]);
        // Two hops reach Zed (via Alpha); one hop again drops it.
        click_on(cx, "graph-local-depth");
        assert_eq!(
            graph_titles(&graph, cx),
            vec!["Alpha", "Test", "Zed", "topic"]
        );
        click_on(cx, "graph-local-depth");
        assert_eq!(graph_titles(&graph, cx), vec!["Alpha", "Test", "topic"]);

        // Clicking a node still opens its page (in a page tab).
        let canvas = cx.debug_bounds("graph-canvas").expect("canvas drawn");
        let alpha = node_pos(&graph, canvas, "Alpha", cx);
        cx.simulate_click(alpha, Modifiers::none());
        view.update(cx, |app, _| {
            assert_eq!(app.mode, Mode::Notes);
            assert_eq!(app.pages[app.selected].title, "Alpha");
        });

        // Back in the graph: still local, now around Alpha (incoming link
        // from Test, outgoing to Zed).
        let (graph, _) = open_graph_again(&view, cx);
        assert_eq!(graph.update(cx, |g, _| g.scope()), Scope::Local);
        assert_eq!(graph_titles(&graph, cx), vec!["Alpha", "Test", "Zed"]);

        click_on(cx, "graph-mode-global");
        assert_eq!(graph_titles(&graph, cx), all, "Global shows everything");

        // The palette command shows the graph and switches it to local.
        cx.simulate_keystrokes("ctrl-g");
        view.update(cx, |app, _| assert_eq!(app.mode, Mode::Notes));
        cx.simulate_keystrokes("ctrl-k");
        cx.simulate_input("local graph");
        view.update(cx, |app, _| {
            assert_eq!(
                app.search_results()[0].target,
                Target::Command(Command::ToggleLocalGraph)
            );
        });
        cx.simulate_keystrokes("enter");
        view.update(cx, |app, _| assert_eq!(app.mode, Mode::Graph));
        assert_eq!(graph.update(cx, |g, _| g.scope()), Scope::Local);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Re-open the graph with Ctrl-G (the sidebar entry was used the first time).
    fn open_graph_again(
        view: &Entity<NoteSec>,
        cx: &mut VisualTestContext,
    ) -> (Entity<GraphView>, Bounds<Pixels>) {
        cx.simulate_keystrokes("ctrl-g");
        cx.run_until_parked();
        let graph = view
            .update(cx, |app, _| app.graph.clone())
            .expect("graph exists");
        (
            graph,
            cx.debug_bounds("graph-canvas").expect("canvas drawn"),
        )
    }

    fn search_pages() -> [(&'static str, &'static str); 3] {
        [
            ("Test", "- hello\n"),
            ("Alpha", "- first\n- needle in haystack\n"),
            ("Zed", "- other\n"),
        ]
    }

    #[gpui::test]
    fn ctrl_k_saves_edit_and_finds_a_page(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "search-page", &search_pages(), "Test");

        // Start editing and type, then open the palette without pressing Escape.
        click_block(cx, 0);
        cx.simulate_input("!");
        cx.simulate_keystrokes("ctrl-k");
        view.update(cx, |app, _| {
            assert!(app.search.is_some());
            assert_eq!(app.editing, None, "opening search leaves block editing");
        });
        assert_eq!(file(&dir), "- hello!\n", "the pending edit was saved");
        assert!(cx.debug_bounds("search-input").is_some());

        // Empty query lists pages.
        assert!(cx.debug_bounds("search-result-0").is_some());

        // Typing filters; the Zed page is the only match for "zed".
        cx.simulate_input("zed");
        view.update(cx, |app, _| {
            assert_eq!(app.search.as_ref().unwrap().query.text, "zed");
            assert_eq!(app.search_results().len(), 1);
        });
        cx.simulate_keystrokes("enter");
        view.update(cx, |app, _| {
            assert!(app.search.is_none(), "palette closes after choosing");
            assert_eq!(app.pages[app.selected].title, "Zed");
            assert_eq!(app.editing, None, "a page hit does not start editing");
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn search_block_hit_opens_page_and_edits_block(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "search-block", &search_pages(), "Test");
        cx.simulate_keystrokes("ctrl-k");
        cx.simulate_input("needle");
        cx.simulate_keystrokes("enter");
        view.update(cx, |app, _| {
            assert_eq!(app.pages[app.selected].title, "Alpha");
            assert_eq!(app.editing, Some(1));
            assert_eq!(app.editor.text, "needle in haystack");
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn search_keyboard_navigation_and_editing_the_query(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "search-keys", &search_pages(), "Test");
        cx.simulate_keystrokes("ctrl-k");

        // Down/up move the highlight and clamp at both ends.
        cx.simulate_keystrokes("down down");
        view.update(cx, |app, _| {
            assert_eq!(app.search.as_ref().unwrap().selected, 2)
        });
        // (An empty query lists the pages, then every command.)
        for _ in 0..Command::ALL.len() + 12 {
            cx.simulate_keystrokes("down");
        }
        view.update(cx, |app, _| {
            let last = app.search_results().len() - 1;
            assert_eq!(app.search.as_ref().unwrap().selected, last);
        });
        cx.simulate_keystrokes("up");

        // Changing the query resets the highlight to the top result; backspace
        // edits the query, not any block.
        cx.simulate_input("zz");
        cx.simulate_keystrokes("backspace");
        view.update(cx, |app, _| {
            let s = app.search.as_ref().unwrap();
            assert_eq!(s.query.text, "z");
            assert_eq!(s.selected, 0);
        });
        assert_eq!(file(&dir), "- hello\n", "search typing never touches pages");

        // Escape closes; Ctrl-K toggles open and closed.
        cx.simulate_keystrokes("escape");
        view.update(cx, |app, _| assert!(app.search.is_none()));
        cx.simulate_keystrokes("ctrl-k ctrl-k");
        view.update(cx, |app, _| assert!(app.search.is_none()));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn search_mouse_interaction(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "search-mouse", &search_pages(), "Test");

        // Clicking a result opens it. With an empty query the order is the
        // sidebar order: today's journal, Alpha, Test, Zed.
        cx.simulate_keystrokes("ctrl-k");
        let target = view.update(cx, |app, _| {
            app.pages.iter().position(|p| p.title == "Zed").unwrap()
        });
        let key: &'static str = Box::leak(format!("search-result-{target}").into_boxed_str());
        let row = cx.debug_bounds(key).expect("result row rendered");
        cx.simulate_click(row.center(), Modifiers::none());
        view.update(cx, |app, _| {
            assert!(app.search.is_none());
            assert_eq!(app.pages[app.selected].title, "Zed");
        });

        // Clicking inside the panel keeps it open; clicking the backdrop closes it.
        cx.simulate_keystrokes("ctrl-k");
        let input = cx.debug_bounds("search-input").unwrap();
        cx.simulate_click(input.center(), Modifiers::none());
        view.update(cx, |app, _| assert!(app.search.is_some()));
        cx.simulate_click(point(px(5.), px(5.)), Modifiers::none());
        view.update(cx, |app, _| assert!(app.search.is_none()));
        let _ = std::fs::remove_dir_all(dir);
    }

    fn slash_kinds(view: &Entity<NoteSec>, cx: &mut VisualTestContext) -> Vec<BlockKind> {
        view.update(cx, |app, _| app.slash_matches())
    }

    #[gpui::test]
    fn slash_menu_opens_on_slash_in_an_empty_block(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "slash-open", "- one\n- \n");

        // "/" inside text is just a character.
        click_block(cx, 0);
        cx.simulate_input("/");
        view.update(cx, |app, _| {
            assert_eq!(app.editor.text, "one/");
            assert!(app.slash.is_none());
        });
        assert!(cx.debug_bounds("slash-menu").is_none());

        // "/" in the empty block opens the menu listing every block kind.
        click_block(cx, 1);
        cx.simulate_input("/");
        view.update(cx, |app, _| {
            assert!(app.slash.is_some());
            assert_eq!(app.editor.text, "/");
            assert_eq!(app.editing, Some(1));
        });
        assert_eq!(slash_kinds(&view, cx), BlockKind::ALL.to_vec());
        assert!(cx.debug_bounds("slash-menu").is_some());
        assert!(cx.debug_bounds("slash-item-4").is_some());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn slash_filter_narrows_results(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "slash-filter", "- \n");
        click_block(cx, 0);
        cx.simulate_input("/hea");
        assert_eq!(
            slash_kinds(&view, cx),
            vec![
                BlockKind::Heading1,
                BlockKind::Heading2,
                BlockKind::Heading3
            ]
        );
        assert!(cx.debug_bounds("slash-item-2").is_some());
        assert!(cx.debug_bounds("slash-item-3").is_none());

        cx.simulate_input("ding 3");
        assert_eq!(slash_kinds(&view, cx), vec![BlockKind::Heading3]);

        // Backspace widens the filter again...
        cx.simulate_keystrokes("backspace backspace backspace backspace backspace backspace");
        assert_eq!(slash_kinds(&view, cx).len(), 3);
        // ...and a query nothing matches closes the menu, keeping the text.
        cx.simulate_input("zz");
        view.update(cx, |app, _| {
            assert!(app.slash.is_none());
            assert_eq!(app.editor.text, "/heazz");
        });
        assert!(cx.debug_bounds("slash-menu").is_none());
        // That literal text is one undo step.
        cx.simulate_keystrokes("ctrl-z");
        view.update(cx, |app, _| assert_eq!(app.editor.text, ""));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn slash_enter_converts_block_type(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "slash-enter", "- \n- after\n");
        click_block(cx, 0);

        // Down/Up move the highlight (clamped); Enter applies it.
        cx.simulate_input("/head");
        cx.simulate_keystrokes("up down down down down up enter");
        view.update(cx, |app, _| {
            assert!(app.slash.is_none());
            assert_eq!(app.editing, Some(0), "still editing the same block");
            assert_eq!(app.editor.text, "## ", "the /query is gone");
            assert_eq!(
                app.pages[app.selected].blocks.len(),
                2,
                "Enter did not split"
            );
        });
        // A type change is saved straight away, as Logseq markdown.
        assert_eq!(file(&dir), "- ## \n- after\n");

        cx.simulate_input("Title");
        cx.simulate_keystrokes("escape");
        assert_eq!(file(&dir), "- ## Title\n- after\n");

        // An empty heading can be retyped; the new type replaces the prefix,
        // here by clicking the menu entry.
        click_block(cx, 1);
        cx.simulate_keystrokes("backspace backspace backspace backspace backspace");
        cx.simulate_input("# ");
        cx.simulate_input("/quo");
        let item = cx
            .debug_bounds("slash-item-0")
            .expect("menu entry rendered");
        cx.simulate_click(item.center(), Modifiers::none());
        view.update(cx, |app, _| {
            assert!(app.slash.is_none());
            assert_eq!(app.editor.text, "> ");
        });
        cx.simulate_input("quoted");
        cx.simulate_keystrokes("escape");
        assert_eq!(file(&dir), "- ## Title\n- > quoted\n");

        // Changing the type is one undo step back to before the "/".
        click_block(cx, 1);
        cx.simulate_keystrokes("end enter");
        cx.simulate_input("/h1");
        cx.simulate_keystrokes("enter");
        view.update(cx, |app, _| assert_eq!(app.editor.text, "# "));
        cx.simulate_keystrokes("ctrl-z");
        view.update(cx, |app, _| {
            assert_eq!(app.editing, Some(2));
            assert_eq!(app.editor.text, "");
            assert!(app.slash.is_none());
        });
        cx.simulate_keystrokes("ctrl-y");
        view.update(cx, |app, _| assert_eq!(app.editor.text, "# "));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn slash_escape_closes_without_changes(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "slash-esc", "- # \n");
        click_block(cx, 0);
        let undo_depth = view.update(cx, |app, _| app.undo_stack.len());

        cx.simulate_input("/quo");
        view.update(cx, |app, _| assert_eq!(app.editor.text, "# /quo"));
        cx.simulate_keystrokes("escape");
        view.update(cx, |app, _| {
            assert!(app.slash.is_none());
            assert_eq!(app.editing, Some(0), "Esc only closes the menu");
            assert_eq!(app.editor.text, "# ");
            assert_eq!(app.editor.cursor, 2);
            assert_eq!(app.pages[app.selected].blocks[0].content, "# ");
            assert_eq!(app.undo_stack.len(), undo_depth, "nothing to undo");
        });
        assert!(cx.debug_bounds("slash-menu").is_none());

        // A second Esc leaves editing as usual; the file never changed.
        cx.simulate_keystrokes("escape");
        view.update(cx, |app, _| assert_eq!(app.editing, None));
        assert_eq!(file(&dir), "- # \n");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn headings_render_bigger_than_text(cx: &mut TestAppContext) {
        let (_view, cx, dir) = setup(cx, "kinds", "- # Big\n- ### Small\n- plain\n- > quoted\n");
        let height = |cx: &mut VisualTestContext, ix: usize| {
            let selector: &'static str = Box::leak(format!("block-{ix}").into_boxed_str());
            cx.debug_bounds(selector).unwrap().size.height
        };
        let (h1, h3, plain, quote) = (height(cx, 0), height(cx, 1), height(cx, 2), height(cx, 3));
        assert!(h1 > h3, "{h1:?} > {h3:?}");
        assert!(h3 > plain, "{h3:?} > {plain:?}");
        assert_eq!(quote, plain);
        // Rendering never rewrites the stored markdown.
        assert_eq!(file(&dir), "- # Big\n- ### Small\n- plain\n- > quoted\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Window position of byte `ix` in the edited block, from the last paint.
    fn text_point(view: &Entity<NoteSec>, cx: &mut VisualTestContext, ix: usize) -> Point<Pixels> {
        view.update(cx, |app, _| {
            let bounds = app.last_bounds.expect("edited block painted");
            let lines = app.last_layout.as_ref().unwrap();
            let at = lines.point_for_index(ix);
            bounds.origin + point(at.x, at.y + lines.line_height / 2.)
        })
    }

    fn drag(cx: &mut VisualTestContext, from: Point<Pixels>, to: Point<Pixels>) {
        cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(to, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::none());
    }

    fn selection(view: &Entity<NoteSec>, cx: &mut VisualTestContext) -> Option<Range<usize>> {
        view.update(cx, |app, _| app.editor.selection())
    }

    /// Click block `ix` and select its last `n` bytes with Shift+Left.
    fn select_tail(cx: &mut VisualTestContext, ix: usize, n: usize) {
        click_block(cx, ix);
        cx.simulate_keystrokes(&vec!["shift-left"; n].join(" "));
    }

    #[gpui::test]
    fn drag_selects_and_click_clears(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "sel-drag", "- hello world\n");
        click_block(cx, 0);

        // Drag from before "h" to after "hello".
        let (start, end) = (text_point(&view, cx, 0), text_point(&view, cx, 5));
        drag(cx, start, end);
        assert_eq!(selection(&view, cx), Some(0..5));
        view.update(cx, |app, _| assert_eq!(app.editor.cursor, 5));

        // The OS input handler sees the real selection, in UTF-16 units.
        let reported = cx.update(|window, cx| {
            view.update(cx, |app, cx| app.selected_text_range(false, window, cx))
        });
        let reported = reported.unwrap();
        assert_eq!((reported.range, reported.reversed), (0..5, false));

        // Dragging leftwards selects the other way round.
        let (start, end) = (text_point(&view, cx, 11), text_point(&view, cx, 6));
        drag(cx, start, end);
        assert_eq!(selection(&view, cx), Some(6..11));
        view.update(cx, |app, _| assert!(app.editor.selection_reversed()));

        // A click without a drag clears it and places the cursor.
        let at = text_point(&view, cx, 2);
        cx.simulate_click(at, Modifiers::none());
        assert_eq!(selection(&view, cx), None);
        view.update(cx, |app, _| {
            assert_eq!(app.editor.cursor, 2);
            assert_eq!(app.editing, Some(0));
            assert_eq!(app.editor.text, "hello world");
        });
        assert_eq!(file(&dir), "- hello world\n", "selecting never edits");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn shift_arrows_extend_and_shrink_the_selection(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "sel-keys", "- hello world\n");
        select_tail(cx, 0, 3);
        assert_eq!(selection(&view, cx), Some(8..11));
        cx.simulate_keystrokes("shift-right");
        assert_eq!(selection(&view, cx), Some(9..11));
        // Plain Left collapses to the start of the selection.
        cx.simulate_keystrokes("left");
        assert_eq!(selection(&view, cx), None);
        view.update(cx, |app, _| assert_eq!(app.editor.cursor, 9));
        cx.simulate_keystrokes("shift-home");
        assert_eq!(selection(&view, cx), Some(0..9));
        // Plain Right collapses to its end.
        cx.simulate_keystrokes("right");
        view.update(cx, |app, _| assert_eq!(app.editor.cursor, 9));
        cx.simulate_keystrokes("shift-end");
        assert_eq!(selection(&view, cx), Some(9..11));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn typing_replaces_the_selection_in_one_undo_step(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "sel-type", "- hello world\n");
        select_tail(cx, 0, 5);
        cx.simulate_input("there");
        view.update(cx, |app, _| {
            assert_eq!(app.editor.text, "hello there");
            assert_eq!(app.editor.selection(), None);
        });

        // One undo brings back the text and the selection; redo re-applies.
        cx.simulate_keystrokes("ctrl-z");
        view.update(cx, |app, _| assert_eq!(app.editor.text, "hello world"));
        assert_eq!(selection(&view, cx), Some(6..11));
        cx.simulate_keystrokes("ctrl-y");
        view.update(cx, |app, _| assert_eq!(app.editor.text, "hello there"));

        // Backspace deletes a selection; paste replaces one.
        cx.simulate_keystrokes("shift-left shift-left shift-left shift-left shift-left backspace");
        view.update(cx, |app, _| assert_eq!(app.editor.text, "hello "));
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("you".into()));
        cx.simulate_keystrokes("shift-home ctrl-v");
        view.update(cx, |app, _| assert_eq!(app.editor.text, "you"));
        cx.simulate_keystrokes("ctrl-z");
        view.update(cx, |app, _| assert_eq!(app.editor.text, "hello "));
        assert_eq!(selection(&view, cx), Some(0..6));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn escape_clears_the_selection_before_leaving_the_block(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "sel-esc", "- hello world\n");
        select_tail(cx, 0, 5);
        cx.simulate_keystrokes("escape");
        view.update(cx, |app, _| {
            assert_eq!(app.editor.selection(), None);
            assert_eq!(app.editing, Some(0), "still editing");
            assert_eq!(app.editor.text, "hello world");
        });
        cx.simulate_keystrokes("escape");
        view.update(cx, |app, _| assert_eq!(app.editing, None));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn slash_over_a_selection_converts_and_keeps_the_text(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "sel-slash", "- hello world\n");
        select_tail(cx, 0, 5);

        // "/" does not replace the selection: it goes after it.
        cx.simulate_input("/");
        view.update(cx, |app, _| {
            assert!(app.slash.is_some());
            assert_eq!(app.editor.text, "hello world/");
        });
        assert!(cx.debug_bounds("slash-menu").is_some());

        // Esc puts back the text and the selection exactly.
        cx.simulate_input("he");
        cx.simulate_keystrokes("escape");
        view.update(cx, |app, _| {
            assert!(app.slash.is_none());
            assert_eq!(app.editor.text, "hello world");
            assert_eq!(app.editing, Some(0));
        });
        assert_eq!(selection(&view, cx), Some(6..11));

        // Enter converts the block, keeping all of its text.
        cx.simulate_input("/quo");
        cx.simulate_keystrokes("enter");
        view.update(cx, |app, _| {
            assert!(app.slash.is_none());
            assert_eq!(app.editor.text, "> hello world");
        });
        assert_eq!(file(&dir), "- > hello world\n");

        // Undo goes back to before the "/", selection included.
        cx.simulate_keystrokes("ctrl-z");
        view.update(cx, |app, _| assert_eq!(app.editor.text, "hello world"));
        assert_eq!(selection(&view, cx), Some(6..11));

        // A query nothing matches is plain typing: it replaces the selection.
        cx.simulate_input("/zz");
        view.update(cx, |app, _| {
            assert!(app.slash.is_none());
            assert_eq!(app.editor.text, "hello /zz");
        });
        cx.simulate_keystrokes("ctrl-z");
        view.update(cx, |app, _| assert_eq!(app.editor.text, "hello world"));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A point just inside the start of block `ix`'s text (past the bullet).
    fn row_text_start(cx: &mut VisualTestContext, ix: usize) -> Point<Pixels> {
        let selector: &'static str = Box::leak(format!("block-{ix}").into_boxed_str());
        let row = cx.debug_bounds(selector).expect("row rendered");
        // The row is: bullet (6px) + gap (8px) + text.
        point(row.left() + px(15.), row.center().y)
    }

    #[gpui::test]
    fn drag_from_unedited_block_starts_editing_and_selects(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "drag-unedited", "- one\n- hello world\n");
        click_block(cx, 0);

        // Press at the start of block 1, which is not being edited.
        let start = row_text_start(cx, 1);
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
        view.update(cx, |app, _| {
            assert_eq!(app.editing, Some(1));
            assert_eq!(app.editor.cursor, 0);
            assert_eq!(app.editor.selection(), None);
        });
        // Dragging selects, following the mouse.
        let to = text_point(&view, cx, 5);
        cx.simulate_mouse_move(to, MouseButton::Left, Modifiers::none());
        assert_eq!(selection(&view, cx), Some(0..5));
        let to = text_point(&view, cx, 8);
        cx.simulate_mouse_move(to, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::none());
        assert_eq!(selection(&view, cx), Some(0..8));
        view.update(cx, |app, _| assert_eq!(app.editing, Some(1)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn click_on_unedited_block_places_the_cursor(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "click-unedited", "- # Big\n- plain\n");
        // Clicking the start of the text puts the cursor there; past the end
        // of the text puts it at the end.
        let at = row_text_start(cx, 1);
        cx.simulate_click(at, Modifiers::none());
        view.update(cx, |app, _| {
            assert_eq!(app.editing, Some(1));
            assert_eq!(app.editor.cursor, 0);
            assert_eq!(app.editor.selection(), None);
        });
        // On a heading the hidden `# ` prefix is accounted for.
        let at = row_text_start(cx, 0);
        cx.simulate_click(at, Modifiers::none());
        view.update(cx, |app, _| {
            assert_eq!(app.editing, Some(0));
            assert_eq!(app.editor.text, "# Big");
            assert_eq!(app.editor.cursor, 2);
        });
        click_block(cx, 1);
        view.update(cx, |app, _| assert_eq!(app.editor.cursor, 5));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn link_click_navigates_without_editing_or_selecting(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "link-press", "- [[Foo]] tail\n- other\n");
        click_block(cx, 1);
        let row = cx.debug_bounds("block-0").expect("row rendered");
        // x + 24 is on `[[Foo]]` (see `clicking_a_link_navigates...`).
        let on_link = point(row.left() + px(24.), row.center().y);
        cx.simulate_mouse_down(on_link, MouseButton::Left, Modifiers::none());
        view.update(cx, |app, _| {
            assert_eq!(app.editing, Some(1), "a press on a link doesn't edit");
            assert!(!app.selecting);
        });
        cx.simulate_mouse_up(on_link, MouseButton::Left, Modifiers::none());
        view.update(cx, |app, _| {
            assert_eq!(app.pages[app.selected].title, "Foo");
            assert_eq!(app.editing, None);
            assert!(!app.selecting);
        });
        assert_eq!(file(&dir), "- [[Foo]] tail\n- other\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn cannot_delete_last_block_or_block_with_children(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "guard", "- parent\n  - child\n");
        // Empty the parent, then backspace: it has children, so it must stay.
        click_block(cx, 0);
        cx.simulate_keystrokes("backspace backspace backspace backspace backspace backspace");
        cx.simulate_keystrokes("backspace");
        view.update(cx, |app, _| {
            assert_eq!(app.pages[app.selected].blocks.len(), 2);
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn ctrl_b_and_ctrl_i_toggle_the_selection_in_one_undo_step_each(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "fmt-sel", "- hello world\n");
        select_tail(cx, 0, 5);
        cx.simulate_keystrokes("ctrl-b");
        view.update(cx, |app, _| assert_eq!(app.editor.text, "hello **world**"));
        assert_eq!(selection(&view, cx), Some(8..13), "still on \"world\"");
        cx.simulate_keystrokes("ctrl-i");
        view.update(cx, |app, _| {
            assert_eq!(app.editor.text, "hello ***world***")
        });
        assert_eq!(selection(&view, cx), Some(9..14));

        // Each shortcut is its own undo step, selection included.
        cx.simulate_keystrokes("ctrl-z");
        view.update(cx, |app, _| assert_eq!(app.editor.text, "hello **world**"));
        assert_eq!(selection(&view, cx), Some(8..13));
        cx.simulate_keystrokes("ctrl-z");
        view.update(cx, |app, _| assert_eq!(app.editor.text, "hello world"));
        assert_eq!(selection(&view, cx), Some(6..11));
        cx.simulate_keystrokes("ctrl-y");
        view.update(cx, |app, _| assert_eq!(app.editor.text, "hello **world**"));

        // Bold again toggles it off; the result is saved on leaving the block.
        cx.simulate_keystrokes("ctrl-b");
        view.update(cx, |app, _| assert_eq!(app.editor.text, "hello world"));
        assert_eq!(selection(&view, cx), Some(6..11));
        cx.simulate_keystrokes("ctrl-i escape escape");
        view.update(cx, |app, _| assert_eq!(app.editing, None));
        assert_eq!(file(&dir), "- hello *world*\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn ctrl_b_wraps_the_word_or_inserts_empty_markers(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "fmt-word", "- héllo wörld\n");
        click_block(cx, 0);
        cx.simulate_keystrokes("end ctrl-b");
        view.update(cx, |app, _| {
            assert_eq!(app.editor.text, "héllo **wörld**");
            assert_eq!(app.editor.cursor, "héllo **wörld".len());
        });

        // Outside any word: empty markers, typing goes between them.
        cx.simulate_keystrokes("end");
        cx.simulate_input(" ");
        cx.simulate_keystrokes("ctrl-i");
        cx.simulate_input("ünï");
        view.update(cx, |app, _| {
            assert_eq!(app.editor.text, "héllo **wörld** *ünï*")
        });

        // Typing after a shortcut is a separate undo step.
        cx.simulate_keystrokes("ctrl-z");
        view.update(cx, |app, _| {
            assert_eq!(app.editor.text, "héllo **wörld** **")
        });
        cx.simulate_keystrokes("ctrl-z");
        view.update(cx, |app, _| assert_eq!(app.editor.text, "héllo **wörld** "));

        // The search box ignores the shortcuts.
        cx.simulate_keystrokes("ctrl-k");
        cx.simulate_input("ab");
        cx.simulate_keystrokes("ctrl-b");
        view.update(cx, |app, _| {
            assert_eq!(app.search.as_ref().unwrap().query.text, "ab");
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Where display offset `ix` of reading-view row `row` is on screen
    /// (1px into the character that starts there, vertically centred).
    fn reading_point(
        view: &Entity<NoteSec>,
        cx: &mut VisualTestContext,
        row: usize,
        ix: usize,
    ) -> Point<Pixels> {
        view.update(cx, |app, _| {
            let (_, layout) = app
                .reading_layouts
                .iter()
                .find(|(r, _)| *r == row)
                .expect("row is in reading view");
            let p = layout.position_for_index(ix).expect("index on the line");
            point(p.x + px(1.), p.y + layout.line_height() / 2.)
        })
    }

    /// The text shown for reading-view row `row`, if it is in reading view.
    fn reading_text(
        view: &Entity<NoteSec>,
        cx: &mut VisualTestContext,
        row: usize,
    ) -> Option<String> {
        view.update(cx, |app, _| {
            app.reading_layouts
                .iter()
                .find(|(r, _)| *r == row)
                .map(|(_, l)| l.text())
        })
    }

    #[gpui::test]
    fn unedited_block_hides_markers_and_renders_formatting(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(
            cx,
            "read-format",
            "- a **bold** and *it* 2 * 3\n- ## ***Big*** [[Page]]\n- **see [[Page]] #tag**\n",
        );
        assert_eq!(
            reading_text(&view, cx, 0).as_deref(),
            Some("a bold and it 2 * 3")
        );
        assert_eq!(reading_text(&view, cx, 1).as_deref(), Some("Big [[Page]]"));
        assert_eq!(
            reading_text(&view, cx, 2).as_deref(),
            Some("see [[Page]] #tag")
        );

        // The highlights handed to the text: bold/italic, combined with the
        // link and tag styles.
        let link = HighlightStyle {
            color: Some(gpui::red()),
            ..Default::default()
        };
        let tag = HighlightStyle {
            color: Some(gpui::blue()),
            ..Default::default()
        };
        let bold = Some(FontWeight::BOLD);
        let italic = Some(FontStyle::Italic);
        let h = reading_highlights(&DisplayBlock::new("a **bold** and *it*"), link, tag, link);
        assert_eq!(h.len(), 2);
        assert_eq!((h[0].0.clone(), h[0].1.font_weight), (2..6, bold));
        assert_eq!((h[1].0.clone(), h[1].1.font_style), (11..13, italic));
        let h = reading_highlights(&DisplayBlock::new("## ***Big*** [[Page]]"), link, tag, link);
        assert_eq!(h[0].0, 0..3);
        assert_eq!((h[0].1.font_weight, h[0].1.font_style), (bold, italic));
        assert_eq!((h[1].0.clone(), h[1].1), (4..12, link));
        let h = reading_highlights(&DisplayBlock::new("**see [[Page]] #tag**"), link, tag, link);
        let ranges: Vec<_> = h.iter().map(|(r, _)| r.clone()).collect();
        assert_eq!(ranges, vec![0..4, 4..12, 12..13, 13..17]);
        assert!(h.iter().all(|(_, s)| s.font_weight == bold));
        assert_eq!((h[1].1.color, h[3].1.color), (link.color, tag.color));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn click_on_formatted_block_maps_to_the_source_offset(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "read-click", "- ab **cd** ef\n- # é**😀ü**x\n");
        // Shown as "ab cd ef". Between "c" and "d": inside the bold.
        let at = reading_point(&view, cx, 0, 4);
        cx.simulate_click(at, Modifiers::none());
        view.update(cx, |app, _| {
            assert_eq!(app.editing, Some(0));
            assert_eq!(app.editor.text, "ab **cd** ef");
            assert_eq!(app.editor.cursor, "ab **c".len());
        });
        cx.simulate_keystrokes("escape");
        // Just after "cd": before the closing "**", still inside the bold.
        let at = reading_point(&view, cx, 0, 5);
        cx.simulate_click(at, Modifiers::none());
        view.update(cx, |app, _| assert_eq!(app.editor.cursor, "ab **cd".len()));
        cx.simulate_keystrokes("escape");
        // Before "e": after the closing markers and the space.
        let at = reading_point(&view, cx, 0, 6);
        cx.simulate_click(at, Modifiers::none());
        view.update(cx, |app, _| {
            assert_eq!(app.editor.cursor, "ab **cd** ".len())
        });
        cx.simulate_keystrokes("escape");

        // Heading prefix, accents and emoji next to markers: shown as
        // "é😀üx"; after "ü" lands before the closing "**".
        assert_eq!(reading_text(&view, cx, 1).as_deref(), Some("é😀üx"));
        let at = reading_point(&view, cx, 1, "é😀ü".len());
        cx.simulate_click(at, Modifiers::none());
        view.update(cx, |app, _| {
            assert_eq!(app.editing, Some(1));
            assert_eq!(app.editor.cursor, "# é**😀ü".len());
        });
        cx.simulate_keystrokes("escape");
        let at = reading_point(&view, cx, 1, "é".len());
        cx.simulate_click(at, Modifiers::none());
        view.update(cx, |app, _| assert_eq!(app.editor.cursor, "# é".len()));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn edit_mode_shows_raw_markers(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "read-edit", "- **bold** *it*\n- other\n");
        assert_eq!(reading_text(&view, cx, 0).as_deref(), Some("bold it"));
        let at = reading_point(&view, cx, 0, 0);
        cx.simulate_click(at, Modifiers::none());
        view.update(cx, |app, _| {
            assert_eq!(app.editing, Some(0));
            assert_eq!(app.editor.text, "**bold** *it*");
            assert_eq!(app.editor.cursor, 2, "start of the text, inside the bold");
            // The edited row is drawn by the editor with the raw text.
            let layout = app.last_layout.as_ref().expect("edited block painted");
            assert_eq!(layout.lines.len(), 1);
            assert_eq!(layout.lines[0].1.text.as_ref(), "**bold** *it*");
        });
        assert_eq!(reading_text(&view, cx, 0), None);
        // Leaving the block goes back to the reading view.
        cx.simulate_keystrokes("escape escape");
        assert_eq!(reading_text(&view, cx, 0).as_deref(), Some("bold it"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn link_inside_bold_navigates(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "read-link", "- **see [[Foo]]** *x*\n- other\n");
        // Shown as "see [[Foo]] x"; "F" is at display offset 6.
        let at = reading_point(&view, cx, 0, 6);
        cx.simulate_click(at, Modifiers::none());
        view.update(cx, |app, _| {
            assert_eq!(app.pages[app.selected].title, "Foo");
            assert_eq!(app.editing, None);
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn drag_from_formatted_unedited_block_maps_the_start(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "read-drag", "- one\n- **ab** cd\n");
        click_block(cx, 0);
        // Press just after "ab" (display 2): source 4, before the closing
        // "**". Then drag to the end of the now-raw text.
        let start = reading_point(&view, cx, 1, 2);
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
        view.update(cx, |app, _| {
            assert_eq!(app.editing, Some(1));
            assert_eq!(app.editor.cursor, 4);
        });
        let to = text_point(&view, cx, 9);
        cx.simulate_mouse_move(to, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::none());
        assert_eq!(selection(&view, cx), Some(4..9));
        view.update(cx, |app, _| assert_eq!(&app.editor.text[4..9], "** cd"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn reading_view_leaves_storage_unchanged(cx: &mut TestAppContext) {
        let original = "- # **Big** title\n- ***both*** and 2 * 3 **\n- *é😀* [[A*b]]\n- edit me\n";
        let (view, cx, dir) = setup(cx, "read-store", original);
        // Model round trip keeps the raw markers.
        let page = Page::from_markdown("Test", false, original);
        assert_eq!(page.to_markdown(), original);
        // Rendering in reading view and saving (by editing another block)
        // writes the formatted blocks back byte for byte.
        assert_eq!(
            reading_text(&view, cx, 1).as_deref(),
            Some("both and 2 * 3 **")
        );
        click_block(cx, 3);
        cx.simulate_keystrokes("end");
        cx.simulate_input("!");
        cx.simulate_keystrokes("escape escape");
        view.update(cx, |app, _| {
            assert_eq!(app.pages[app.selected].blocks[0].content, "# **Big** title");
        });
        assert_eq!(file(&dir), original.replace("edit me", "edit me!"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn ctrl_enter_cycles_the_task_state_and_saves(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "task-keys", "- buy milk\n");
        click_block(cx, 0);
        let mut seen = Vec::new();
        for _ in 0..4 {
            cx.simulate_keystrokes("ctrl-enter");
            let text = view.update(cx, |app, _| app.editor.text.clone());
            // Saved right away, not only when editing ends.
            assert_eq!(file(&dir), format!("- {text}\n"));
            seen.push(text);
        }
        assert_eq!(
            seen,
            [
                "TODO buy milk",
                "DOING buy milk",
                "DONE buy milk",
                "buy milk"
            ]
        );
        view.update(cx, |app, _| {
            assert_eq!(app.editing, Some(0));
            assert_eq!(
                app.editor.cursor,
                "buy milk".len(),
                "cursor stays at the end"
            );
        });
        // Each change is one undo step.
        cx.simulate_keystrokes("ctrl-z");
        view.update(cx, |app, _| assert_eq!(app.editor.text, "DONE buy milk"));
        cx.simulate_keystrokes("ctrl-z ctrl-z");
        view.update(cx, |app, _| assert_eq!(app.editor.text, "TODO buy milk"));
        // Plain Enter still splits.
        cx.simulate_keystrokes("end enter");
        view.update(cx, |app, _| {
            assert_eq!(app.pages[app.selected].blocks.len(), 2);
            assert_eq!(app.editing, Some(1));
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    fn has(cx: &mut VisualTestContext, selector: &str) -> bool {
        let selector: &'static str = Box::leak(selector.to_string().into_boxed_str());
        cx.debug_bounds(selector).is_some()
    }

    #[gpui::test]
    fn each_task_state_renders_its_own_checkbox(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(
            cx,
            "task-render",
            "- TODO a\n- DOING b\n- DONE c\n- LATER d\n- NOW e\n- ## TODO head\n- plain TODO\n",
        );
        for (ix, state) in ["todo", "doing", "done", "later", "now", "todo"]
            .iter()
            .enumerate()
        {
            assert!(
                has(cx, &format!("task-{ix}-{state}")),
                "row {ix} is {state}"
            );
        }
        assert!(!has(cx, "task-6-todo"), "a keyword mid-text is not a task");
        // Only DONE text is dimmed and struck through.
        assert!(has(cx, "done-text-2"));
        assert!((0..7)
            .filter(|&ix| ix != 2)
            .all(|ix| !has(cx, &format!("done-text-{ix}"))));
        // The keyword itself is hidden; the heading prefix comes first.
        assert_eq!(reading_text(&view, cx, 0).as_deref(), Some("a"));
        assert_eq!(reading_text(&view, cx, 5).as_deref(), Some("head"));
        assert_eq!(reading_text(&view, cx, 6).as_deref(), Some("plain TODO"));
        // Clicking the text still edits, cursor mapped past the keyword; the
        // editor shows the raw keyword and no checkbox.
        let at = reading_point(&view, cx, 5, 0);
        cx.simulate_click(at, Modifiers::none());
        view.update(cx, |app, _| {
            assert_eq!(app.editing, Some(5));
            assert_eq!(app.editor.cursor, "## TODO ".len());
        });
        assert!(!has(cx, "task-5-todo"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn clicking_the_checkbox_cycles_without_editing(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "task-click", "- TODO a\n- b\n");
        let click = |cx: &mut VisualTestContext, selector: &'static str| {
            let bounds = cx.debug_bounds(selector).expect(selector);
            cx.simulate_click(bounds.center(), Modifiers::none());
        };
        click(cx, "task-0-todo");
        view.update(cx, |app, _| {
            assert_eq!(app.editing, None, "the checkbox never starts editing");
            assert_eq!(app.pages[app.selected].blocks[0].content, "DOING a");
        });
        assert_eq!(file(&dir), "- DOING a\n- b\n");
        click(cx, "task-0-doing");
        assert_eq!(file(&dir), "- DONE a\n- b\n");

        // While another block is being edited: that block is committed and
        // stays in edit mode.
        click_block(cx, 1);
        cx.simulate_input("x");
        click(cx, "task-0-done");
        view.update(cx, |app, _| {
            assert_eq!(app.editing, Some(1));
            assert_eq!(app.pages[app.selected].blocks[0].content, "a");
        });
        assert_eq!(file(&dir), "- a\n- bx\n");
        assert!(!has(cx, "task-0-todo"), "plain again: no checkbox");

        // One undo step brings DONE back.
        cx.simulate_keystrokes("escape ctrl-z");
        view.update(cx, |app, _| {
            assert_eq!(app.pages[app.selected].blocks[0].content, "DONE a");
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    fn click_on(cx: &mut VisualTestContext, selector: &str) {
        let selector: &'static str = Box::leak(selector.to_string().into_boxed_str());
        let bounds = cx.debug_bounds(selector).expect(selector);
        cx.simulate_click(bounds.center(), Modifiers::none());
    }

    /// Indices of the rendered block rows, out of the first `n` blocks.
    fn shown(cx: &mut VisualTestContext, n: usize) -> Vec<usize> {
        (0..n)
            .filter(|&ix| has(cx, &format!("block-{ix}")))
            .collect()
    }

    #[gpui::test]
    fn fold_arrow_collapses_and_expands_with_a_count(cx: &mut TestAppContext) {
        let original = "- a\n  - b\n    - c\n  - d\n- e\n";
        let (view, cx, dir) = setup(cx, "fold", original);
        // Only blocks with children have an arrow.
        assert!(has(cx, "fold-0") && has(cx, "fold-1"));
        assert!(!has(cx, "fold-2") && !has(cx, "fold-3") && !has(cx, "fold-4"));

        // Folding hides every descendant and counts them all.
        click_on(cx, "fold-0");
        assert_eq!(shown(cx, 5), [0, 4]);
        assert!(has(cx, "fold-badge-0-3"));
        view.update(cx, |app, _| {
            assert_eq!(app.editing, None, "the arrow never edits")
        });
        // Unfolding restores them.
        click_on(cx, "fold-0");
        assert_eq!(shown(cx, 5), [0, 1, 2, 3, 4]);
        assert!(!has(cx, "fold-badge-0-3"));

        // Nested folds: the inner one is remembered across the outer one.
        click_on(cx, "fold-1");
        assert_eq!(shown(cx, 5), [0, 1, 3, 4]);
        assert!(has(cx, "fold-badge-1-1"));
        click_on(cx, "fold-0");
        assert_eq!(shown(cx, 5), [0, 4]);
        click_on(cx, "fold-0");
        assert_eq!(shown(cx, 5), [0, 1, 3, 4]);

        // UI-only: the file is untouched.
        assert_eq!(file(&dir), original);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn editing_around_folded_blocks(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "fold-edit", "- a\n  - b\n- c\n");
        // Folding the parent of the edited block leaves edit mode, saving it.
        click_block(cx, 1);
        cx.simulate_input("!");
        click_on(cx, "fold-0");
        view.update(cx, |app, _| assert_eq!(app.editing, None));
        assert_eq!(file(&dir), "- a\n  - b!\n- c\n");

        // Up/Down skip hidden blocks.
        click_block(cx, 0);
        cx.simulate_keystrokes("down");
        view.update(cx, |app, _| assert_eq!(app.editing, Some(2)));
        cx.simulate_keystrokes("up");
        view.update(cx, |app, _| assert_eq!(app.editing, Some(0)));

        // Enter on a folded block adds a sibling after its subtree.
        cx.simulate_keystrokes("end enter");
        view.update(cx, |app, _| {
            assert_eq!(app.editing, Some(2));
            assert_eq!(app.pages[app.selected].depth_of(2), 0);
        });
        assert!(has(cx, "fold-badge-0-1"), "still folded");
        // Backspace in that empty block goes back to the folded block, not
        // into its hidden child.
        cx.simulate_keystrokes("backspace");
        view.update(cx, |app, _| {
            assert_eq!(app.editing, Some(0));
            assert_eq!(app.pages[app.selected].blocks.len(), 3);
        });
        assert_eq!(shown(cx, 3), [0, 2]);

        // Indenting "c" under the folded "a" unfolds it.
        click_block(cx, 2);
        cx.simulate_keystrokes("tab");
        view.update(cx, |app, _| {
            assert_eq!(app.editing, Some(2));
            assert_eq!(app.pages[app.selected].depth_of(2), 1);
        });
        assert_eq!(shown(cx, 3), [0, 1, 2]);
        assert_eq!(file(&dir), "- a\n  - b!\n  - c\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn search_and_backlinks_unfold_to_a_hidden_block(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(
            cx,
            "fold-nav",
            &[
                ("Test", "- top\n  - deep zqneedle [[Hub]]\n- after\n"),
                ("Hub", "- hub\n"),
            ],
            "Test",
        );
        click_on(cx, "fold-0");
        assert_eq!(shown(cx, 3), [0, 2]);
        // A search hit in a folded subtree unfolds it and edits the block.
        cx.simulate_keystrokes("ctrl-k");
        cx.simulate_input("zqneedle");
        cx.simulate_keystrokes("enter");
        view.update(cx, |app, _| assert_eq!(app.editing, Some(1)));
        assert_eq!(shown(cx, 3), [0, 1, 2]);

        // Fold again, go to Hub and follow the backlink back.
        click_on(cx, "fold-0");
        view.update(cx, |app, cx| app.open_page("Hub", cx));
        cx.run_until_parked();
        click_on(cx, "backlink-0");
        view.update(cx, |app, _| {
            assert_eq!(app.pages[app.selected].title, "Test")
        });
        assert_eq!(shown(cx, 3), [0, 1, 2]);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The open tabs as labels, and the active index.
    fn tab_state(
        view: &Entity<NoteSec>,
        cx: &mut VisualTestContext,
    ) -> (Vec<String>, Option<usize>) {
        view.update(cx, |app, _| {
            let labels = app
                .tabs
                .tabs
                .iter()
                .map(|t| match t {
                    TabTarget::Page(title) => title.clone(),
                    TabTarget::Graph => "Graph".to_string(),
                    TabTarget::Agenda => "Agenda".to_string(),
                    TabTarget::Trash => "Trash".to_string(),
                })
                .collect();
            (labels, app.tabs.active)
        })
    }

    fn tabs_are(
        view: &Entity<NoteSec>,
        cx: &mut VisualTestContext,
        labels: &[&str],
        active: usize,
    ) {
        let (got, got_active) = tab_state(view, cx);
        assert_eq!(
            (got, got_active),
            (labels.iter().map(|l| l.to_string()).collect(), Some(active))
        );
        // The screen agrees: one tab element per tab, and the active page.
        for i in 0..labels.len() {
            assert!(has(cx, &format!("tab-{i}")), "tab-{i} drawn");
        }
        assert!(!has(cx, &format!("tab-{}", labels.len())));
        view.update(cx, |app, _| match labels[active] {
            "Graph" => assert_eq!(app.mode, Mode::Graph),
            "Agenda" => assert_eq!(app.mode, Mode::Agenda),
            "Trash" => assert_eq!(app.mode, Mode::Trash),
            title => {
                assert_eq!(app.mode, Mode::Notes);
                assert_eq!(app.pages[app.selected].title, title);
            }
        });
    }

    fn click_sidebar_page(view: &Entity<NoteSec>, cx: &mut VisualTestContext, title: &str) {
        let ix = view.update(cx, |app, _| app.find_page(title).unwrap());
        click_on(cx, &format!("page-{ix}"));
    }

    fn tab_pages() -> [(&'static str, &'static str); 3] {
        [
            ("Test", "- see [[Alpha]]\n"),
            ("Alpha", "- alpha\n"),
            ("Beta", "- beta\n"),
        ]
    }

    #[gpui::test]
    fn sidebar_opens_focuses_and_x_closes_tabs(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "tabs-open", &tab_pages(), "Test");
        tabs_are(&view, cx, &["Test"], 0);
        // A sidebar click opens a new tab...
        click_sidebar_page(&view, cx, "Alpha");
        tabs_are(&view, cx, &["Test", "Alpha"], 1);
        // ...or focuses the tab already showing that page.
        click_sidebar_page(&view, cx, "Test");
        tabs_are(&view, cx, &["Test", "Alpha"], 0);
        // Clicking a tab focuses it.
        click_on(cx, "tab-1");
        tabs_are(&view, cx, &["Test", "Alpha"], 1);
        // The x of an inactive tab closes it and keeps the active one.
        click_on(cx, "tab-close-0");
        tabs_are(&view, cx, &["Alpha"], 0);
        // The x of the last tab leaves the empty state.
        click_on(cx, "tab-close-0");
        assert_eq!(tab_state(&view, cx), (vec![], None));
        assert!(has(cx, "empty-state"));
        view.update(cx, |app, _| assert_eq!(app.mode, Mode::Empty));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn ctrl_w_saves_the_edit_and_closes_the_tab(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "tabs-ctrl-w", &tab_pages(), "Test");
        click_sidebar_page(&view, cx, "Alpha");
        click_block(cx, 0);
        cx.simulate_keystrokes("end");
        cx.simulate_input("!");
        // Ctrl+W while editing: saves, ends editing, closes the tab; it
        // never edits the text.
        cx.simulate_keystrokes("ctrl-w");
        view.update(cx, |app, _| assert_eq!(app.editing, None));
        let alpha = view.update(cx, |app, _| app.find_page("Alpha").unwrap());
        view.update(cx, |app, _| {
            assert_eq!(app.pages[alpha].blocks[0].content, "alpha!");
        });
        assert_eq!(
            std::fs::read_to_string(dir.join("pages/Alpha.md")).unwrap(),
            "- alpha!\n"
        );
        tabs_are(&view, cx, &["Test"], 0);

        // Closing the last tab shows the empty state; Ctrl+W there is a no-op.
        cx.simulate_keystrokes("ctrl-w");
        assert!(has(cx, "empty-state"));
        cx.simulate_keystrokes("ctrl-w");
        assert_eq!(tab_state(&view, cx), (vec![], None));
        // The empty state offers today's journal.
        click_on(cx, "empty-today");
        let today = today_title();
        tabs_are(&view, cx, &[today.as_str()], 0);
        // Ctrl+N from the empty state also opens a tab.
        cx.simulate_keystrokes("ctrl-w ctrl-n");
        tabs_are(&view, cx, &["Untitled"], 0);
        view.update(cx, |app, _| assert_eq!(app.editing, Some(0)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn ctrl_tab_cycles_tabs(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "tabs-cycle", &tab_pages(), "Test");
        click_sidebar_page(&view, cx, "Alpha");
        click_sidebar_page(&view, cx, "Beta");
        tabs_are(&view, cx, &["Test", "Alpha", "Beta"], 2);
        cx.simulate_keystrokes("ctrl-tab");
        tabs_are(&view, cx, &["Test", "Alpha", "Beta"], 0);
        cx.simulate_keystrokes("ctrl-tab");
        tabs_are(&view, cx, &["Test", "Alpha", "Beta"], 1);
        cx.simulate_keystrokes("ctrl-shift-tab ctrl-shift-tab");
        tabs_are(&view, cx, &["Test", "Alpha", "Beta"], 2);

        // While editing: the edit is saved and editing ends.
        click_block(cx, 0);
        cx.simulate_input("x");
        cx.simulate_keystrokes("ctrl-tab");
        view.update(cx, |app, _| assert_eq!(app.editing, None));
        tabs_are(&view, cx, &["Test", "Alpha", "Beta"], 0);
        assert_eq!(
            std::fs::read_to_string(dir.join("pages/Beta.md")).unwrap(),
            "- betax\n"
        );
        // Ctrl-J opens or focuses today's journal in a tab, like the sidebar.
        cx.simulate_keystrokes("ctrl-j");
        let today = today_title();
        tabs_are(&view, cx, &["Test", "Alpha", "Beta", today.as_str()], 3);
        cx.simulate_keystrokes("ctrl-tab ctrl-j");
        tabs_are(&view, cx, &["Test", "Alpha", "Beta", today.as_str()], 3);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn links_replace_the_tab_and_search_opens_one(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "tabs-links", &tab_pages(), "Test");
        // An in-page link replaces the current tab's page, like a browser.
        // Test shows "see [[Alpha]]"; "A" is at display offset 6.
        let at = reading_point(&view, cx, 0, 6);
        cx.simulate_click(at, Modifiers::none());
        tabs_are(&view, cx, &["Alpha"], 0);
        // A search hit opens (or focuses) a tab.
        cx.simulate_keystrokes("ctrl-k");
        cx.simulate_input("beta");
        cx.simulate_keystrokes("enter");
        tabs_are(&view, cx, &["Alpha", "Beta"], 1);
        cx.simulate_keystrokes("ctrl-k");
        cx.simulate_input("alpha");
        cx.simulate_keystrokes("enter");
        tabs_are(&view, cx, &["Alpha", "Beta"], 0);
        // Tab shortcuts do nothing while the palette is open.
        cx.simulate_keystrokes("ctrl-k ctrl-w ctrl-tab");
        view.update(cx, |app, _| assert!(app.search.is_some()));
        cx.simulate_keystrokes("escape");
        tabs_are(&view, cx, &["Alpha", "Beta"], 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn graph_lives_in_a_tab(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "tabs-graph", &graph_pages(), "Test");
        let (graph, canvas) = open_graph(&view, cx);
        tabs_are(&view, cx, &["Test", "Graph"], 1);
        // A node click from the graph tab opens a page tab; the graph stays.
        let zed = node_pos(&graph, canvas, "Zed", cx);
        cx.simulate_click(zed, Modifiers::none());
        tabs_are(&view, cx, &["Test", "Graph", "Zed"], 2);
        // Ctrl-G focuses the existing graph tab, and from it closes it.
        cx.simulate_keystrokes("ctrl-g");
        tabs_are(&view, cx, &["Test", "Graph", "Zed"], 1);
        assert!(has(cx, "graph-canvas"));
        cx.simulate_keystrokes("ctrl-g");
        tabs_are(&view, cx, &["Test", "Zed"], 1);
        // Cycling back onto a graph tab shows the graph.
        cx.simulate_keystrokes("ctrl-g ctrl-tab ctrl-shift-tab");
        tabs_are(&view, cx, &["Test", "Zed", "Graph"], 2);
        assert!(has(cx, "graph-canvas"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn undo_closes_tabs_of_pages_it_removes(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "tabs-undo", &tab_pages(), "Test");
        // An edit on Test (undo point), then a new page in its own tab.
        click_block(cx, 0);
        cx.simulate_input("x");
        cx.simulate_keystrokes("escape ctrl-n");
        tabs_are(&view, cx, &["Test", "Untitled"], 1);
        // Undo goes back to before the page existed: its tab is closed and
        // the edited page is shown in its tab.
        cx.simulate_keystrokes("escape ctrl-z");
        view.update(cx, |app, _| assert!(app.find_page("Untitled").is_none()));
        tabs_are(&view, cx, &["Test"], 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    // --- favorites and recent pages ----------------------------------------

    /// Selector of the sidebar row (or, with `prefix = "star"`, its star) for
    /// the page called `title`.
    fn page_row(
        view: &Entity<NoteSec>,
        cx: &mut VisualTestContext,
        prefix: &str,
        title: &str,
    ) -> String {
        let ix = view.update(cx, |app, _| app.find_page(title).expect(title));
        format!("{prefix}-{ix}")
    }

    fn selected_title(view: &Entity<NoteSec>, cx: &mut VisualTestContext) -> String {
        view.update(cx, |app, _| app.pages[app.selected].title.clone())
    }

    /// Move the mouse onto `selector`'s element, as a real pointer does
    /// before a click. Outline stars are only painted (and so only clickable)
    /// while their row is hovered.
    fn hover(cx: &mut VisualTestContext, selector: &str) {
        let selector: &'static str = Box::leak(selector.to_string().into_boxed_str());
        let bounds = cx.debug_bounds(selector).expect(selector);
        cx.simulate_mouse_move(bounds.center(), None, Modifiers::none());
        cx.run_until_parked();
    }

    /// Click `selector` in the sidebar, first scrolling the list (as a user
    /// would) if the row is below its visible part.
    fn click_in_sidebar(cx: &mut VisualTestContext, selector: &str) {
        let selector: &'static str = Box::leak(selector.to_string().into_boxed_str());
        let list = cx.debug_bounds("sidebar").expect("sidebar rendered");
        let row = cx.debug_bounds(selector).expect(selector);
        if row.bottom() > list.bottom() {
            cx.simulate_event(ScrollWheelEvent {
                position: list.center(),
                delta: ScrollDelta::Pixels(point(px(0.), list.bottom() - row.bottom() - px(8.))),
                modifiers: Modifiers::none(),
                touch_phase: TouchPhase::Moved,
            });
            cx.run_until_parked();
        }
        let row = cx.debug_bounds(selector).expect(selector);
        assert!(
            row.bottom() <= list.bottom(),
            "{selector} scrolled into view"
        );
        cx.simulate_click(row.center(), Modifiers::none());
    }

    #[gpui::test]
    fn clicking_a_page_row_records_it_in_recent_and_persists(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(
            cx,
            "recent-click",
            &[("Alpha", "- a\n"), ("Beta", "- b\n")],
            "Alpha",
        );
        // Startup counts as opening today's journal.
        let today = today_title();
        view.update(cx, |app, _| {
            assert_eq!(app.state.recent, vec![today.clone()])
        });

        let row = page_row(&view, cx, "page", "Beta");
        click_on(cx, &row);

        assert_eq!(selected_title(&view, cx), "Beta");
        view.update(cx, |app, _| {
            assert_eq!(app.state.recent, vec!["Beta".to_string(), today.clone()]);
            assert_eq!(UiState::load(&dir), app.state);
        });
        // config.toml is not touched by navigation.
        assert!(!Config::path(&dir).exists());

        // The RECENT section lists them, most recent first; a row opens its page.
        assert!(has(cx, "recent-0") && has(cx, "recent-1") && !has(cx, "recent-2"));
        click_on(cx, "recent-1");
        assert_eq!(selected_title(&view, cx), today);
        view.update(cx, |app, _| {
            assert_eq!(app.state.recent, vec![today.clone(), "Beta".to_string()])
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn recent_is_most_recent_first_deduped_and_capped(cx: &mut TestAppContext) {
        let titles: Vec<String> = (0..12).map(|i| format!("P{i:02}")).collect();
        let pages: Vec<(&str, &str)> = titles.iter().map(|t| (t.as_str(), "- x\n")).collect();
        let (view, cx, dir) = setup_pages(cx, "recent-cap", &pages, "P00");

        for title in &titles {
            let row = page_row(&view, cx, "page", title);
            click_in_sidebar(cx, &row);
        }
        // Reopening a page moves it to the front without duplicating it,
        // whichever route opened it (here: a search for it, via `open_page`).
        view.update(cx, |app, cx| app.open_page("p05", cx));

        let expected: Vec<String> = [
            "P05", "P11", "P10", "P09", "P08", "P07", "P06", "P04", "P03", "P02",
        ]
        .map(String::from)
        .to_vec();
        view.update(cx, |app, _| assert_eq!(app.state.recent, expected));
        assert_eq!(UiState::load(&dir).recent, expected);
        cx.run_until_parked();
        assert!(has(cx, &format!("recent-{}", MAX_RECENT - 1)));
        assert!(!has(cx, &format!("recent-{MAX_RECENT}")));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn star_favorites_without_navigating_and_favorites_open_the_page(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(
            cx,
            "favorite-star",
            &[("Alpha", "- a\n"), ("Beta", "- b\n")],
            "Alpha",
        );
        assert!(!has(cx, "fav-0"));

        let row = page_row(&view, cx, "page", "Beta");
        let star = page_row(&view, cx, "star", "Beta");
        hover(cx, &row);
        click_on(cx, &star);

        // Starred, but still on Alpha (the row's own click did not run).
        assert_eq!(selected_title(&view, cx), "Alpha");
        view.update(cx, |app, _| {
            assert_eq!(app.state.favorites, vec!["Beta".to_string()]);
            assert!(!app.state.recent.contains(&"Beta".to_string()));
        });
        assert_eq!(UiState::load(&dir).favorites, vec!["Beta".to_string()]);

        // The FAVORITES section appears, and its row opens the page.
        assert!(has(cx, "fav-0") && !has(cx, "fav-1"));
        click_on(cx, "fav-0");
        assert_eq!(selected_title(&view, cx), "Beta");
        view.update(cx, |app, _| assert_eq!(app.state.recent[0], "Beta"));

        // A favorite's star in the PAGES list is always shown; clicking it
        // unstars without navigating.
        cx.simulate_mouse_move(point(px(900.), px(900.)), None, Modifiers::none());
        let alpha = page_row(&view, cx, "page", "Alpha");
        click_on(cx, &alpha);
        let star = page_row(&view, cx, "star", "Beta");
        click_on(cx, &star);
        assert_eq!(selected_title(&view, cx), "Alpha");
        view.update(cx, |app, _| assert!(app.state.favorites.is_empty()));
        assert!(!has(cx, "fav-0"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn favorites_star_unfavorites_and_missing_pages_are_skipped(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(
            cx,
            "favorite-unstar",
            &[("Alpha", "- a\n"), ("Beta", "- b\n")],
            "Alpha",
        );
        view.update(cx, |app, cx| {
            app.toggle_favorite("Gone", cx); // no such page
            app.toggle_favorite("beta", cx); // case differs from the page
        });
        cx.run_until_parked();
        // Only the existing page is listed, under its real title.
        assert!(has(cx, "fav-0") && !has(cx, "fav-1"));

        hover(cx, "fav-0");
        click_on(cx, "fav-star-0");

        assert_eq!(selected_title(&view, cx), "Alpha");
        view.update(cx, |app, _| {
            // The missing page's entry is kept, not cleaned up.
            assert_eq!(app.state.favorites, vec!["Gone".to_string()]);
        });
        assert_eq!(UiState::load(&dir).favorites, vec!["Gone".to_string()]);
        assert!(!has(cx, "fav-0"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn state_reloads_into_a_fresh_window(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(
            cx,
            "state-reload",
            &[("Alpha", "- a\n"), ("Beta", "- b\n")],
            "Alpha",
        );
        view.update(cx, |app, cx| {
            app.toggle_favorite("Alpha", cx);
            app.open_page("Beta", cx);
        });
        let saved = view.update(cx, |app, _| app.state.clone());

        let storage = Storage::open(dir.clone()).unwrap();
        let (view2, cx2) = cx
            .cx
            .add_window_view(|window, cx| NoteSec::new(storage, Config::default(), window, cx));
        cx2.run_until_parked();
        view2.update(cx2, |app, _| {
            assert_eq!(app.state.favorites, saved.favorites);
            // Startup opens today's journal, which moves to the front.
            assert_eq!(app.state.recent, vec![today_title(), "Beta".to_string()]);
        });
        assert!(has(cx2, "fav-0") && has(cx2, "recent-1"));
        let _ = std::fs::remove_dir_all(dir);
    }

    // --- settings panel ----------------------------------------------------

    fn settings_open(view: &Entity<NoteSec>, cx: &mut VisualTestContext) -> bool {
        let open = view.update(cx, |app, _| app.settings.is_some());
        assert_eq!(open, has(cx, "settings-panel"), "state and rendering agree");
        open
    }

    fn config_text(dir: &std::path::Path) -> String {
        std::fs::read_to_string(Config::path(dir)).unwrap()
    }

    fn state_text(dir: &std::path::Path) -> String {
        std::fs::read_to_string(UiState::path(dir)).unwrap()
    }

    #[gpui::test]
    fn gear_and_ctrl_comma_open_settings_and_escape_closes_them(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "settings-open", "- hello\n");
        assert!(!settings_open(&view, cx));

        click_on(cx, "settings-gear");
        assert!(settings_open(&view, cx));
        cx.simulate_keystrokes("escape");
        assert!(!settings_open(&view, cx));

        // Ctrl-, saves the block being edited first, and toggles.
        click_block(cx, 0);
        cx.simulate_input(" world");
        cx.simulate_keystrokes("ctrl-,");
        assert!(settings_open(&view, cx));
        view.update(cx, |app, _| assert_eq!(app.editing, None));
        assert_eq!(file(&dir), "- hello world\n");
        cx.simulate_keystrokes("ctrl-,");
        assert!(!settings_open(&view, cx));

        // Starting to edit (Ctrl-N) closes the panel rather than editing under it.
        cx.simulate_keystrokes("ctrl-, ctrl-n");
        assert!(!settings_open(&view, cx));
        view.update(cx, |app, _| assert_eq!(app.editing, Some(0)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn palette_command_opens_settings(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "settings-palette", "- hi\n");
        for query in ["settings", "preferences"] {
            cx.simulate_keystrokes("ctrl-k");
            cx.simulate_input(query);
            view.update(cx, |app, _| {
                assert_eq!(
                    app.search_results()[0].target,
                    Target::Command(Command::OpenSettings),
                    "{query}"
                );
            });
            cx.simulate_keystrokes("enter");
            view.update(cx, |app, _| assert!(app.search.is_none()));
            assert!(settings_open(&view, cx));
            cx.simulate_keystrokes("escape");
            assert!(!settings_open(&view, cx));
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn backdrop_click_closes_settings_but_panel_clicks_do_not(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "settings-backdrop", "- hi\n");
        click_on(cx, "settings-gear");

        let panel = cx.debug_bounds("settings-panel").unwrap();
        cx.simulate_click(panel.origin + point(px(8.), px(8.)), Modifiers::none());
        assert!(
            settings_open(&view, cx),
            "a click inside the panel keeps it open"
        );

        let backdrop = cx.debug_bounds("settings-backdrop").unwrap();
        cx.simulate_click(backdrop.origin + point(px(20.), px(20.)), Modifiers::none());
        assert!(!settings_open(&view, cx));
        // The click didn't reach the sidebar underneath.
        assert_eq!(
            view.update(cx, |app, _| app.pages[app.selected].title.clone()),
            "Test"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn theme_buttons_apply_immediately_and_persist(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "settings-theme", "- hi\n");
        click_on(cx, "settings-gear");
        click_on(cx, "settings-tab-appearance");

        click_on(cx, "theme-light");
        view.update(cx, |app, _| {
            assert_eq!(app.theme_kind(), ThemeKind::Light);
            assert_eq!(app.theme.bg, Theme::light().bg);
        });
        assert_eq!(UiState::load(&dir).theme, Some(ThemeKind::Light));
        assert!(state_text(&dir).contains("theme = \"light\""));
        assert!(
            !Config::path(&dir).exists(),
            "choosing a theme does not write config.toml"
        );
        assert!(settings_open(&view, cx), "the panel stays open");

        click_on(cx, "theme-catppuccin-mocha");
        view.update(cx, |app, _| {
            assert_eq!(app.theme.bg, Theme::catppuccin_mocha().bg)
        });
        assert_eq!(UiState::load(&dir).theme, Some(ThemeKind::CatppuccinMocha));

        click_on(cx, "theme-tokyo-night");
        view.update(cx, |app, _| {
            assert_eq!(app.theme.bg, Theme::tokyo_night().bg)
        });
        assert_eq!(UiState::load(&dir).theme, Some(ThemeKind::TokyoNight));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn font_size_buttons_change_persist_and_clamp(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "settings-size", "- hi\n");
        click_on(cx, "settings-gear");
        click_on(cx, "settings-tab-appearance");
        let size = |view: &Entity<NoteSec>, cx: &mut VisualTestContext| {
            view.update(cx, |app, _| app.ui_size())
        };
        assert!(has(cx, "ui-size-value"));

        click_on(cx, "ui-size-inc");
        click_on(cx, "ui-size-inc");
        assert_eq!(size(&view, cx), 18.0);
        assert_eq!(UiState::load(&dir).ui_size, Some(18.0));
        click_on(cx, "ui-size-dec");
        assert_eq!(size(&view, cx), 17.0);
        click_on(cx, "ui-size-reset");
        assert_eq!(size(&view, cx), crate::config::DEFAULT_FONT_SIZE);
        assert_eq!(UiState::load(&dir).ui_size, None);

        for _ in 0..10 {
            click_on(cx, "ui-size-dec");
        }
        assert_eq!(size(&view, cx), crate::config::MIN_FONT_SIZE);
        assert_eq!(
            UiState::load(&dir).ui_size,
            Some(crate::config::MIN_FONT_SIZE)
        );
        for _ in 0..30 {
            click_on(cx, "ui-size-inc");
        }
        assert_eq!(size(&view, cx), crate::config::MAX_FONT_SIZE);
        assert_eq!(
            UiState::load(&dir).ui_size,
            Some(crate::config::MAX_FONT_SIZE)
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn font_family_choice_persists_and_system_default_removes_it(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "settings-family", "- hi\n");
        click_on(cx, "settings-gear");
        click_on(cx, "settings-tab-appearance");
        // The test platform's text system reports no installed fonts, so the
        // list only has "System default"...
        assert!(has(cx, "ui-font-default"));
        assert!(!has(cx, "ui-font-0"));
        // ...so give the open panel a known list, as the real text system would.
        view.update(cx, |app, cx| {
            app.settings.as_mut().unwrap().fonts = vec!["Test Sans".into(), "Test Serif".into()];
            cx.notify();
        });
        cx.run_until_parked();
        assert!(has(cx, "ui-font-1") && !has(cx, "ui-font-2"));

        click_on(cx, "ui-font-1");
        view.update(cx, |app, _| {
            assert_eq!(app.state.ui_font.as_deref(), Some("Test Serif"));
            // Not really installed here, so rendering keeps the system font.
            assert_eq!(app.font_family, None);
        });
        assert_eq!(UiState::load(&dir).ui_font.as_deref(), Some("Test Serif"));

        click_on(cx, "ui-font-default");
        view.update(cx, |app, _| assert_eq!(app.state.ui_font, None));
        assert_eq!(UiState::load(&dir).ui_font, None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn settings_from_the_panel_load_in_a_fresh_window(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "settings-reload", "- hi\n");
        click_on(cx, "settings-gear");
        click_on(cx, "settings-tab-appearance");
        click_on(cx, "theme-light");
        for _ in 0..4 {
            click_on(cx, "ui-size-inc");
        }
        view.update(cx, |app, cx| app.set_ui_font(Some("Test Serif".into()), cx));

        let storage = Storage::open(dir.clone()).unwrap();
        let config = Config::load(&dir);
        let (view2, cx2) = cx
            .cx
            .add_window_view(|window, cx| NoteSec::new(storage, config, window, cx));
        view2.update(cx2, |app, _| {
            assert_eq!(app.theme_kind(), ThemeKind::Light);
            assert_eq!(app.theme.bg, Theme::light().bg);
            assert_eq!(app.ui_size(), 20.0);
            assert_eq!(app.state.ui_font.as_deref(), Some("Test Serif"));
        });
        let _ = std::fs::remove_dir_all(dir);
    }
    // --- tabs together with favorites/recent and settings -------------------

    fn recent(view: &Entity<NoteSec>, cx: &mut VisualTestContext) -> Vec<String> {
        view.update(cx, |app, _| app.state.recent.clone())
    }

    fn recent_starts_with(view: &Entity<NoteSec>, cx: &mut VisualTestContext, expected: &[&str]) {
        let got = recent(view, cx);
        assert!(
            got.len() >= expected.len() && got[..expected.len()] == *expected,
            "RECENT is {got:?}, expected it to start with {expected:?}"
        );
    }

    #[gpui::test]
    fn focusing_page_tabs_records_recent_but_graph_and_empty_do_not(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "tabs-recent", &tab_pages(), "Test");
        // Opening tabs from the sidebar records each page, most recent first.
        click_sidebar_page(&view, cx, "Alpha");
        click_sidebar_page(&view, cx, "Beta");
        tabs_are(&view, cx, &["Test", "Alpha", "Beta"], 2);
        recent_starts_with(&view, cx, &["Beta", "Alpha"]);
        // Clicking a tab counts as opening its page.
        click_on(cx, "tab-0");
        tabs_are(&view, cx, &["Test", "Alpha", "Beta"], 0);
        recent_starts_with(&view, cx, &["Test", "Beta", "Alpha"]);
        // So do Ctrl+Tab and Ctrl+Shift+Tab.
        cx.simulate_keystrokes("ctrl-tab");
        tabs_are(&view, cx, &["Test", "Alpha", "Beta"], 1);
        recent_starts_with(&view, cx, &["Alpha", "Test", "Beta"]);
        cx.simulate_keystrokes("ctrl-shift-tab");
        tabs_are(&view, cx, &["Test", "Alpha", "Beta"], 0);
        recent_starts_with(&view, cx, &["Test", "Alpha", "Beta"]);
        // The graph tab is not a page: RECENT doesn't change.
        let before = recent(&view, cx);
        cx.simulate_keystrokes("ctrl-g");
        tabs_are(&view, cx, &["Test", "Alpha", "Beta", "Graph"], 3);
        assert_eq!(recent(&view, cx), before);
        // Closing it hands focus to its neighbour, which counts.
        cx.simulate_keystrokes("ctrl-w");
        tabs_are(&view, cx, &["Test", "Alpha", "Beta"], 2);
        recent_starts_with(&view, cx, &["Beta", "Test", "Alpha"]);
        cx.simulate_keystrokes("ctrl-w");
        tabs_are(&view, cx, &["Test", "Alpha"], 1);
        recent_starts_with(&view, cx, &["Alpha", "Beta", "Test"]);
        cx.simulate_keystrokes("ctrl-w");
        tabs_are(&view, cx, &["Test"], 0);
        recent_starts_with(&view, cx, &["Test", "Alpha", "Beta"]);
        // The empty state records nothing.
        let before = recent(&view, cx);
        cx.simulate_keystrokes("ctrl-w");
        assert_eq!(tab_state(&view, cx), (vec![], None));
        assert_eq!(recent(&view, cx), before);
        // And the list is on disk.
        assert_eq!(UiState::load(&dir).recent, before);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn tab_keys_do_nothing_while_settings_are_open(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "tabs-settings", &tab_pages(), "Test");
        click_sidebar_page(&view, cx, "Alpha");
        tabs_are(&view, cx, &["Test", "Alpha"], 1);
        cx.simulate_keystrokes("ctrl-,");
        view.update(cx, |app, _| assert!(app.settings.is_some()));
        cx.simulate_keystrokes("ctrl-tab ctrl-shift-tab ctrl-w");
        view.update(cx, |app, _| {
            assert!(app.settings.is_some());
            assert_eq!(app.tabs.tabs.len(), 2);
            assert_eq!(app.tabs.active, Some(1));
        });
        cx.simulate_keystrokes("escape");
        view.update(cx, |app, _| assert!(app.settings.is_none()));
        tabs_are(&view, cx, &["Test", "Alpha"], 1);
        // With the panel closed the keys work again.
        cx.simulate_keystrokes("ctrl-tab");
        tabs_are(&view, cx, &["Test", "Alpha"], 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn sidebar_click_that_ends_an_edit_opens_the_right_tab(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "tabs-stale-row", &tab_pages(), "Test");
        // Edit a block so it links to a page that doesn't exist yet. Saving
        // it (when the sidebar click ends the edit) creates "Aaa", which
        // sorts before "Beta" and shifts the row indices under the click.
        click_block(cx, 0);
        cx.simulate_input(" [[Aaa]]");
        let before = view.update(cx, |app, _| app.find_page("Beta").unwrap());
        click_sidebar_page(&view, cx, "Beta");
        let after = view.update(cx, |app, _| {
            assert!(
                app.find_page("Aaa").is_some(),
                "the link target was created"
            );
            app.find_page("Beta").unwrap()
        });
        assert_ne!(before, after, "the edit shifted the sidebar rows");
        tabs_are(&view, cx, &["Test", "Beta"], 1);
        recent_starts_with(&view, cx, &["Beta"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    fn bounds_of(cx: &mut VisualTestContext, selector: &str) -> Bounds<Pixels> {
        let selector: &'static str = Box::leak(selector.to_string().into_boxed_str());
        cx.debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} was not rendered"))
    }

    /// Press on block `from`'s bullet handle and drag (without releasing)
    /// to `to`.
    fn start_block_drag(cx: &mut VisualTestContext, from: usize, to: Point<Pixels>) {
        let handle = bounds_of(cx, &format!("handle-{from}")).center();
        cx.simulate_mouse_down(handle, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        // Past GPUI's drag threshold, then to the target.
        let nudge = point(handle.x, handle.y + px(6.));
        cx.simulate_mouse_move(nudge, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        cx.simulate_mouse_move(to, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
    }

    fn release_block_drag(cx: &mut VisualTestContext, at: Point<Pixels>) {
        cx.simulate_mouse_up(at, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
    }

    /// A point in the upper (`upper == true`) or lower half of block `ix`.
    fn row_half(cx: &mut VisualTestContext, ix: usize, upper: bool) -> Point<Pixels> {
        let b = bounds_of(cx, &format!("block-{ix}"));
        let y = if upper {
            b.top() + px(2.)
        } else {
            b.bottom() - px(2.)
        };
        point(b.center().x, y)
    }

    #[gpui::test]
    fn dragging_a_block_down_moves_it_with_its_children(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "drag-down", "- a\n  - a1\n- b\n- c\n");

        // Lower half of "b": the line shows just before "c".
        let target = row_half(cx, 2, false);
        start_block_drag(cx, 0, target);
        assert!(has(cx, "drop-before-3"), "drop line before c");
        assert!(!has(cx, "drop-before-2"));
        release_block_drag(cx, target);

        assert_eq!(file(&dir), "- b\n- a\n  - a1\n- c\n");
        assert!(!has(cx, "drop-before-3"), "line gone after the drop");
        view.update(cx, |app, _| {
            // Pressing the handle never started editing.
            assert_eq!(app.editing, None);
            assert_eq!(app.block_drop, None);
        });

        // The whole move is one undo step.
        cx.simulate_keystrokes("ctrl-z");
        cx.run_until_parked();
        assert_eq!(file(&dir), "- a\n  - a1\n- b\n- c\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn dragging_a_block_up_moves_it_before_the_target(cx: &mut TestAppContext) {
        let (_view, cx, dir) = setup(cx, "drag-up", "- a\n  - a1\n- b\n- c\n");

        // Upper half of "a1": "c" becomes a1's sibling, before it.
        let target = row_half(cx, 1, true);
        start_block_drag(cx, 3, target);
        assert!(has(cx, "drop-before-1"));
        release_block_drag(cx, target);
        assert_eq!(file(&dir), "- a\n  - c\n  - a1\n- b\n");

        // Upper half of the first block: to the very top.
        let target = row_half(cx, 0, true);
        start_block_drag(cx, 3, target);
        release_block_drag(cx, target);
        assert_eq!(file(&dir), "- b\n- a\n  - c\n  - a1\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn a_block_cannot_be_dropped_into_itself_and_can_go_to_the_end(cx: &mut TestAppContext) {
        let (_view, cx, dir) = setup(cx, "drag-self", "- a\n  - a1\n- b\n");

        // Over its own child: no line, and dropping changes nothing.
        let target = row_half(cx, 1, true);
        start_block_drag(cx, 0, target);
        assert!(!has(cx, "drop-before-1"));
        assert!(!has(cx, "drop-end"));
        release_block_drag(cx, target);
        assert_eq!(file(&dir), "- a\n  - a1\n- b\n");

        // Lower half of the last row: the end of the page, at the top level.
        let target = row_half(cx, 2, false);
        start_block_drag(cx, 1, target);
        assert!(has(cx, "drop-end"));
        release_block_drag(cx, target);
        assert_eq!(file(&dir), "- a\n- b\n- a1\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn alt_arrows_move_the_edited_block_and_keep_editing_it(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "alt-arrows", "- a\n  - a1\n- b\n- c\n");
        click_block(cx, 0);
        cx.simulate_input("!");

        cx.simulate_keystrokes("alt-down");
        assert_eq!(file(&dir), "- b\n- a!\n  - a1\n- c\n");
        view.update(cx, |app, _| {
            assert_eq!(app.editing, Some(1));
            assert_eq!(app.editor.text, "a!");
        });

        cx.simulate_keystrokes("alt-down");
        assert_eq!(file(&dir), "- b\n- c\n- a!\n  - a1\n");
        // Already last: nothing happens.
        cx.simulate_keystrokes("alt-down");
        assert_eq!(file(&dir), "- b\n- c\n- a!\n  - a1\n");

        cx.simulate_keystrokes("alt-up alt-up");
        assert_eq!(file(&dir), "- a!\n  - a1\n- b\n- c\n");
        view.update(cx, |app, _| assert_eq!(app.editing, Some(0)));
        // Already first: nothing happens.
        cx.simulate_keystrokes("alt-up");
        assert_eq!(file(&dir), "- a!\n  - a1\n- b\n- c\n");

        // A child only moves among its siblings.
        click_block(cx, 1);
        cx.simulate_keystrokes("alt-up");
        assert_eq!(file(&dir), "- a!\n  - a1\n- b\n- c\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn moved_block_order_survives_a_restart(cx: &mut TestAppContext) {
        let (_view, cx, dir) = setup(cx, "drag-persist", "- a\n  - a1\n- b\n- c\n");
        let target = row_half(cx, 3, false);
        start_block_drag(cx, 0, target);
        release_block_drag(cx, target);

        // A fresh load from disk, as on the next start.
        let pages = Storage::open(dir.clone()).unwrap().load_all();
        let page = pages.iter().find(|p| p.title == "Test").unwrap();
        let contents: Vec<&str> = page.blocks.iter().map(|b| b.content.as_str()).collect();
        assert_eq!(contents, ["b", "c", "a", "a1"]);
        assert_eq!(page.blocks[3].parent_id, Some(page.blocks[2].id));
        assert_eq!(page.to_markdown(), "- b\n- c\n- a\n  - a1\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn double_paren_picks_a_block_and_inserts_its_reference(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(
            cx,
            "block-ref-insert",
            &[
                ("Test", "- alpha one\n- \n"),
                ("Other", "- beta source\n- beta two\n"),
            ],
            "Test",
        );
        click_block(cx, 1);
        cx.simulate_input("((bet");
        assert!(has(cx, "ref-menu"));
        assert!(has(cx, "ref-item-0") && has(cx, "ref-item-1"));
        // The edited block itself is never offered; "alpha" doesn't match.
        assert!(!has(cx, "ref-item-2"));

        cx.simulate_keystrokes("down enter");
        let (id, other) = view.update(cx, |app, _| {
            let other = app.find_page("Other").unwrap();
            (app.pages[other].blocks[1].id, other)
        });
        view.update(cx, |app, _| {
            assert_eq!(app.editor.text, format!("(({id}))"));
            assert_eq!(app.editor.cursor, app.editor.text.len());
            assert!(app.pages[other].saved_ids.contains(&id));
        });
        assert!(!has(cx, "ref-menu"));
        assert_eq!(file(&dir), format!("- alpha one\n- (({id}))\n"));
        assert_eq!(
            std::fs::read_to_string(dir.join("pages/Other.md")).unwrap(),
            format!("- beta source\n- beta two\n  id:: {id}\n")
        );

        // One undo step back to the typed query.
        cx.simulate_keystrokes("ctrl-z");
        view.update(cx, |app, _| assert_eq!(app.editor.text, "((bet"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn escape_closes_the_block_ref_picker_and_keeps_the_text(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "block-ref-esc", "- source\n- \n");
        click_block(cx, 1);
        cx.simulate_input("((so");
        assert!(has(cx, "ref-menu"));
        cx.simulate_keystrokes("escape");
        assert!(!has(cx, "ref-menu"));
        view.update(cx, |app, _| {
            assert_eq!(app.editing, Some(1));
            assert_eq!(app.editor.text, "((so");
        });
        // Still closed while typing on; Enter is a normal Enter again.
        cx.simulate_input("u");
        assert!(!has(cx, "ref-menu"));
        // A new "((" opens it again.
        cx.simulate_input(" ((");
        assert!(has(cx, "ref-menu"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn block_reference_shows_the_source_text_and_click_jumps_there(cx: &mut TestAppContext) {
        let id = Uuid::new_v4();
        let (view, cx, dir) = setup_pages(
            cx,
            "block-ref-render",
            &[
                ("Test", &format!("- see (({id})) here\n")),
                (
                    "Other",
                    &format!("- first\n- the **source** text\n  id:: {id}\n"),
                ),
            ],
            "Test",
        );
        let shown = view.update(cx, |app, _| {
            let (_, layout) = app.reading_layouts.iter().find(|(r, _)| *r == 0).unwrap();
            layout.text()
        });
        assert_eq!(shown, "see the source text here");
        // The file still holds the reference, not the copied text.
        assert_eq!(file(&dir), format!("- see (({id})) here\n"));

        // Clicking the referenced text opens the source block for editing.
        let at = reading_point(&view, cx, 0, "see the so".len());
        cx.simulate_click(at, Modifiers::none());
        view.update(cx, |app, _| {
            assert_eq!(app.pages[app.selected].title, "Other");
            assert_eq!(app.editing, Some(1));
            assert_eq!(app.editor.text, "the **source** text");
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn missing_block_reference_shows_as_written(cx: &mut TestAppContext) {
        let id = Uuid::new_v4();
        let (view, cx, dir) = setup(cx, "block-ref-missing", &format!("- see (({id}))\n"));
        let shown = view.update(cx, |app, _| {
            let (_, layout) = app.reading_layouts.iter().find(|(r, _)| *r == 0).unwrap();
            layout.text()
        });
        assert_eq!(shown, format!("see (({id}))"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn block_reference_survives_save_and_load(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(
            cx,
            "block-ref-persist",
            &[("Test", "- \n"), ("Other", "- keep me\n")],
            "Test",
        );
        click_block(cx, 0);
        cx.simulate_input("((keep");
        cx.simulate_keystrokes("enter escape");
        let id = view.update(cx, |app, _| {
            app.pages[app.find_page("Other").unwrap()].blocks[0].id
        });

        // As on the next start: everything read back from disk.
        let pages = Storage::open(dir.clone()).unwrap().load_all();
        let test = pages.iter().find(|p| p.title == "Test").unwrap();
        assert_eq!(test.blocks[0].content, format!("(({id}))"));
        let (p, b) = find_block(&pages, id).expect("the referenced block keeps its id");
        assert_eq!(pages[p].title, "Other");
        assert_eq!(pages[p].blocks[b].content, "keep me");
        let resolved = DisplayBlock::with_refs(&test.blocks[0].content, |id| {
            find_block(&pages, id).map(|(p, b)| pages[p].blocks[b].content.clone())
        });
        assert_eq!(resolved.text, "keep me");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn tag_query_lists_tagged_blocks_from_every_page(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(
            cx,
            "tag-query",
            &[
                ("Test", "- {{query #proj}}\n- {{query #[[big plan]]}}\n"),
                ("Alpha", "- buy milk #proj\n- nothing here\n"),
                ("Beta", "- ship it #[[Proj]]\n- x #[[big plan]]\n"),
            ],
            "Test",
        );
        assert!(has(cx, "query-0") && has(cx, "query-1"));
        // Two hits for #proj (one per page), one for the multi-word tag.
        assert!(has(cx, "query-0-hit-0") && has(cx, "query-0-hit-1"));
        assert!(!has(cx, "query-0-hit-2"));
        assert!(has(cx, "query-1-hit-0") && !has(cx, "query-1-hit-1"));
        // Showing results doesn't touch the file.
        assert_eq!(file(&dir), "- {{query #proj}}\n- {{query #[[big plan]]}}\n");

        // Clicking a hit goes to that block's page, without editing the query.
        let at = bounds_of(cx, "query-1-hit-0").center();
        cx.simulate_click(at, Modifiers::none());
        view.update(cx, |app, _| {
            assert_eq!(app.pages[app.selected].title, "Beta");
            assert_eq!(app.editing, None);
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn tag_query_results_follow_edits_on_other_pages(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(
            cx,
            "tag-query-live",
            &[("Test", "- {{query #todo}}\n"), ("Other", "- one\n- two\n")],
            "Test",
        );
        assert!(has(cx, "query-0") && !has(cx, "query-0-hit-0"));

        // Tag a block on another page, then come back.
        let other = view.update(cx, |app, _| app.find_page("Other").unwrap());
        view.update(cx, |app, cx| app.show_page(other, cx));
        cx.run_until_parked();
        click_block(cx, 1);
        cx.simulate_input(" #todo");
        cx.simulate_keystrokes("escape");
        let test = view.update(cx, |app, _| app.find_page("Test").unwrap());
        view.update(cx, |app, cx| app.show_page(test, cx));
        cx.run_until_parked();
        assert!(has(cx, "query-0-hit-0") && !has(cx, "query-0-hit-1"));

        // Clicking the hit lands on the page that has it.
        let at = bounds_of(cx, "query-0-hit-0").center();
        cx.simulate_click(at, Modifiers::none());
        view.update(cx, |app, _| {
            assert_eq!(app.pages[app.selected].title, "Other")
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn table_columns_line_up(cx: &mut TestAppContext) {
        let md = "- Prices **now**\n  | Item | Qty | Note |\n  | :--- | :-: | ---: |\n  | a | 1000000 | x |\n  | a much longer item | 2 | [[P]] |\n- after\n";
        let (_view, cx, dir) = setup_pages(cx, "table-align", &[("Test", md)], "Test");
        assert!(has(cx, "table-0"));
        let cell = |cx: &mut VisualTestContext, r: usize, c: usize| {
            bounds_of(cx, &format!("table-0-cell-{r}-{c}"))
        };
        for c in 0..3 {
            let first = cell(cx, 0, c);
            for r in 1..3 {
                let b = cell(cx, r, c);
                assert_eq!(
                    (b.origin.x, b.size.width),
                    (first.origin.x, first.size.width)
                );
            }
            if c > 0 {
                assert!(first.origin.x >= cell(cx, 0, c - 1).right());
            }
        }
        for r in 0..3 {
            let first = cell(cx, r, 0);
            for c in 1..3 {
                let b = cell(cx, r, c);
                assert_eq!(
                    (b.origin.y, b.size.height),
                    (first.origin.y, first.size.height)
                );
            }
            if r > 0 {
                assert!(first.origin.y >= cell(cx, r - 1, 0).bottom());
            }
        }
        // The widest cell sets the column's width.
        assert!(cell(cx, 0, 0).size.width > cell(cx, 0, 2).size.width);
        // Text before the table is above it, and the next block still renders.
        assert!(has(cx, "block-1"));
        assert!(bounds_of(cx, "block-1").top() >= bounds_of(cx, "table-0").bottom());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn ragged_table_rows_render_without_crashing(cx: &mut TestAppContext) {
        let md =
            "- | a | b |\n  |---|---|\n  | 1 |\n  | 1 | 2 | 3 | 4 |\n  ||\n- | only |\n  | --- |\n";
        let (_view, cx, dir) = setup_pages(cx, "table-ragged", &[("Test", md)], "Test");
        // Four columns in every row, short rows padded with empty cells.
        for r in 0..4 {
            for c in 0..4 {
                assert!(has(cx, &format!("table-0-cell-{r}-{c}")), "cell {r},{c}");
            }
        }
        assert!(!has(cx, "table-0-cell-0-4") && !has(cx, "table-0-cell-4-0"));
        // A header with no body rows is still a table.
        assert!(has(cx, "table-1-cell-0-0") && !has(cx, "table-1-cell-1-0"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn tables_survive_save_and_load_byte_for_byte(cx: &mut TestAppContext) {
        // Odd padding, trailing spaces, an escaped pipe, alignment colons and
        // a table nested two levels down.
        let md = "- Groceries\n  - | Item   |  Qty |\n    | :---   | --:  |\n    |  tea   | 2    |  \n    | a \\| b |\n    after the table\n  - other\n- Prices\n  a|b\n  -|:-:\n  1|2\n";
        let (view, cx, dir) = setup_pages(cx, "table-roundtrip", &[("Test", md)], "Test");
        assert!(has(cx, "table-1") && has(cx, "table-3"));
        // Storage alone gives back exactly what it read.
        let storage = Storage::open(dir.clone()).unwrap();
        let page = storage
            .load_all()
            .into_iter()
            .find(|p| p.title == "Test")
            .unwrap();
        storage.save(&page).unwrap();
        assert_eq!(file(&dir), md);
        // So does opening the table for editing and leaving again.
        click_block(cx, 1);
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        // Editing another block rewrites the file with the tables untouched.
        click_block(cx, 2);
        cx.simulate_keystrokes("end");
        cx.simulate_input("!");
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        view.update(cx, |app, _| assert_eq!(app.editing, None));
        assert_eq!(file(&dir), md.replace("  - other\n", "  - other!\n"));
        assert!(has(cx, "table-1") && has(cx, "table-3"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn editing_a_multi_line_block_shows_every_line(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(
            cx,
            "multi-line-edit",
            "- first\n  second\n  third\n- next\n",
        );
        click_block(cx, 0);
        let line_height = view.update(cx, |app, _| {
            assert_eq!(app.editing, Some(0));
            assert_eq!(app.editor.text, "first\nsecond\nthird");
            let lines = app.last_layout.as_ref().expect("edited block painted");
            let texts: Vec<&str> = lines.lines.iter().map(|(_, l)| l.text.as_ref()).collect();
            assert_eq!(texts, ["first", "second", "third"]);
            assert_eq!(lines.lines[2].0, "first\nsecond\n".len());
            let bounds = app.last_bounds.unwrap();
            assert_eq!(bounds.size.height, lines.line_height * 3.);
            lines.line_height
        });
        // A click on the second line puts the cursor there.
        let at = text_point(&view, cx, "first\nsec".len());
        cx.simulate_click(at, Modifiers::none());
        view.update(cx, |app, _| {
            assert_eq!(app.editor.cursor, "first\nsec".len())
        });
        // The next block sits below all three lines.
        cx.simulate_keystrokes("escape");
        assert!(
            bounds_of(cx, "block-1").top() >= bounds_of(cx, "block-0").top() + line_height * 3.
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn up_and_down_move_between_lines_before_blocks(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "multi-line-keys", "- above\n- abc\n  abcdef\n- below\n");
        click_block(cx, 1);
        view.update(cx, |app, cx| {
            app.editor.set_cursor(2);
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("down");
        view.update(cx, |app, _| {
            assert_eq!(app.editing, Some(1));
            assert_eq!(app.editor.cursor, "abc\nab".len(), "same column, next line");
        });
        cx.simulate_keystrokes("down");
        view.update(cx, |app, _| assert_eq!(app.editing, Some(2)));
        cx.simulate_keystrokes("up");
        view.update(cx, |app, cx| {
            assert_eq!(app.editing, Some(1));
            app.editor.set_cursor("abc\nabcdef".len());
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("up");
        view.update(cx, |app, _| {
            assert_eq!(app.editing, Some(1));
            assert_eq!(app.editor.cursor, 3, "end of the shorter line above");
        });
        cx.simulate_keystrokes("up");
        view.update(cx, |app, _| assert_eq!(app.editing, Some(0)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn shift_enter_writes_a_table_that_renders(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "shift-enter-table", "- \n");
        click_block(cx, 0);
        cx.simulate_input("| a | b |");
        cx.simulate_keystrokes("shift-enter");
        cx.simulate_input("|---|--:|");
        cx.simulate_keystrokes("shift-enter");
        cx.simulate_input("| 1 | 2 |");
        view.update(cx, |app, _| {
            assert_eq!(app.editing, Some(0), "still the same block");
            assert_eq!(app.editor.text, "| a | b |\n|---|--:|\n| 1 | 2 |");
            assert_eq!(app.last_layout.as_ref().unwrap().lines.len(), 3);
        });
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert_eq!(file(&dir), "- | a | b |\n  |---|--:|\n  | 1 | 2 |\n");
        assert!(has(cx, "table-0-cell-1-1"));
        // Undo takes the last line break back out.
        click_block(cx, 0);
        cx.simulate_keystrokes("end");
        cx.simulate_keystrokes("shift-enter");
        view.update(cx, |app, _| {
            assert!(app.editor.text.ends_with("| 1 | 2 |\n"))
        });
        cx.simulate_keystrokes("ctrl-z");
        view.update(cx, |app, _| assert!(app.editor.text.ends_with("| 1 | 2 |")));
        let _ = std::fs::remove_dir_all(dir);
    }

    const CODE_PAGE: &str =
        "- Run:\n  ```sh\n  ls -la  \n    echo  #hi [[x]]\n  ```\n  then done\n- next\n";

    /// Hover code block `n` of block 0, click its copy button and check the
    /// clipboard, the "Copied" label and that it goes away again.
    fn copy_first_code_block(view: &Entity<NoteSec>, cx: &mut VisualTestContext) {
        assert!(has(cx, "code-0-0") && !has(cx, "code-0-0-copy"));
        let code = bounds_of(cx, "code-0-0");
        cx.simulate_mouse_move(code.center(), None, Modifiers::none());
        cx.run_until_parked();
        assert!(has(cx, "code-0-0-copy") && !has(cx, "code-0-0-copied"));
        let button = bounds_of(cx, "code-0-0-copy");
        assert!(
            code.contains(&button.center()),
            "the button sits inside the block"
        );
        cx.simulate_click(button.center(), Modifiers::none());
        cx.run_until_parked();
        let copied = cx.read_from_clipboard().and_then(|item| item.text());
        assert_eq!(copied.as_deref(), Some("ls -la  \n  echo  #hi [[x]]"));
        assert!(has(cx, "code-0-0-copied"));
        view.update(cx, |app, _| {
            assert_eq!(app.editing, None, "copying doesn't edit")
        });
        // The label goes back after a moment, with the mouse elsewhere.
        let away = bounds_of(cx, "block-1").center();
        cx.simulate_mouse_move(away, None, Modifiers::none());
        cx.executor()
            .advance_clock(COPIED_FOR + std::time::Duration::from_millis(100));
        cx.run_until_parked();
        assert!(!has(cx, "code-0-0-copied") && !has(cx, "code-0-0-copy"));
    }

    #[gpui::test]
    fn code_blocks_render_with_a_copy_button_on_hover(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "code-copy", CODE_PAGE);
        assert!(has(cx, "code-0-0"));
        // Prose around the code still renders; the code is not a link or tag.
        let code = bounds_of(cx, "code-0-0");
        assert!(bounds_of(cx, "block-1").top() >= code.bottom());
        view.update(cx, |app, _| {
            assert!(app.pages[app.selected].blocks[0].content.contains("```sh"));
            assert!(crate::model::tag_counts(&app.pages).is_empty());
        });
        copy_first_code_block(&view, cx);
        // A press on the code itself (not the button) edits the raw block.
        let at = bounds_of(cx, "code-0-0").center();
        cx.simulate_click(at, Modifiers::none());
        view.update(cx, |app, _| {
            assert_eq!(app.editing, Some(0));
            assert_eq!(
                app.editor.text,
                "Run:\n```sh\nls -la  \n  echo  #hi [[x]]\n```\nthen done"
            );
        });
        cx.simulate_keystrokes("escape");
        assert_eq!(file(&dir), CODE_PAGE, "nothing rewritten");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Renders the real app with each theme (notes view and Settings) to
    /// PNGs in `target/shots/`, to check the look by eye. Not a regression
    /// test: `cargo test --offline render_theme_screenshots -- --ignored`.
    #[test]
    #[ignore]
    fn render_theme_screenshots() {
        use gpui::{AppContext as _, HeadlessAppContext};
        use std::sync::Arc;

        std::fs::create_dir_all("target/shots").unwrap();
        let text_system = Arc::new(gpui_wgpu::CosmicTextSystem::new("DejaVu Sans"));
        let mut hcx = HeadlessAppContext::with_platform(text_system, Arc::new(()), || {
            gpui_platform::current_headless_renderer()
        });

        for kind in ThemeKind::ALL {
            let name = format!("{kind:?}").to_lowercase();
            let dir =
                std::env::temp_dir().join(format!("notesec-shot-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let storage = Storage::open(dir.clone()).unwrap();
            let journal = format!("journals/{}.md", today_title().replace('-', "_"));
            std::fs::write(
                dir.join(journal),
                "- # Design pass\n\
                 - Trying the new **themes**: see [[Projects]] and #design\n\
                 \x20 - A nested note with `inline code` and a [[Reading list]] link\n\
                 \x20 - TODO check contrast on #light\n\
                 \x20 - DONE ship the settings picker\n\
                 - Plain text, to compare against *italic* and the muted colours\n",
            )
            .unwrap();
            for page in ["Projects", "Reading list", "Ideas", "People"] {
                std::fs::write(
                    dir.join(format!("pages/{page}.md")),
                    "- notes about [[Design pass]]\n",
                )
                .unwrap();
            }
            UiState {
                theme: Some(kind),
                favorites: vec!["Projects".into()],
                ..UiState::default()
            }
            .save(&dir)
            .unwrap();
            let config = Config::load(&dir);

            let window = hcx
                .open_window(size(px(1200.), px(760.)), |window, cx| {
                    cx.new(|cx| NoteSec::new(storage, config, window, cx))
                })
                .unwrap();
            let view = window.root(&mut hcx).unwrap();
            for (shot, settings) in [("notes", false), ("settings", true)] {
                if settings {
                    hcx.update(|cx| view.update(cx, |app, cx| app.open_settings(cx)));
                }
                hcx.update_window(window.into(), |_, window, cx| {
                    let _ = window.draw(cx);
                })
                .unwrap();
                let image = hcx.capture_screenshot(window.into()).expect("screenshot");
                image
                    .save(format!("target/shots/{shot}-{name}.png"))
                    .unwrap();
                println!("wrote target/shots/{shot}-{name}.png");
            }
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[gpui::test]
    fn the_default_theme_is_tokyo_night_and_nothing_is_saved_until_chosen(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "theme-default", "- hi\n");
        view.update(cx, |app, _| {
            assert_eq!(app.theme_kind(), ThemeKind::TokyoNight);
            assert_eq!(app.theme.bg, gpui::rgb(0x16161e));
            assert_eq!(app.theme.accent, gpui::rgb(0x7aa2f7));
        });
        assert_eq!(
            UiState::load(&dir).theme,
            None,
            "not chosen, so not written"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn settings_lists_the_three_themes_and_marks_the_current_one(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "theme-picker", "- hi\n");
        click_on(cx, "settings-gear");
        click_on(cx, "settings-tab-appearance");
        for id in ["theme-tokyo-night", "theme-catppuccin-mocha", "theme-light"] {
            assert!(has(cx, id), "Settings has a {id} button");
        }
        // Clicking a button moves to that theme and the choice sticks.
        click_on(cx, "theme-catppuccin-mocha");
        view.update(cx, |app, _| {
            assert_eq!(app.theme_kind(), ThemeKind::CatppuccinMocha)
        });
        click_on(cx, "theme-light");
        view.update(cx, |app, _| assert_eq!(app.theme_kind(), ThemeKind::Light));
        // Clicking the one already in use changes nothing (and writes nothing new).
        let before = state_text(&dir);
        click_on(cx, "theme-light");
        assert_eq!(state_text(&dir), before);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Start a second app over the same graph folder, as a restart would.
    fn restart<'a>(
        cx: &'a mut VisualTestContext,
        dir: &std::path::Path,
    ) -> (Entity<NoteSec>, &'a mut VisualTestContext) {
        let storage = Storage::open(dir.to_path_buf()).unwrap();
        let config = Config::load(dir);
        cx.cx
            .add_window_view(|window, cx| NoteSec::new(storage, config, window, cx))
    }

    #[gpui::test]
    fn a_chosen_theme_survives_a_restart(cx: &mut TestAppContext) {
        let (_view, cx, dir) = setup(cx, "theme-restart", "- hi\n");
        click_on(cx, "settings-gear");
        click_on(cx, "settings-tab-appearance");
        click_on(cx, "theme-catppuccin-mocha");

        let (view2, cx2) = restart(cx, &dir);
        view2.update(cx2, |app, _| {
            assert_eq!(app.theme_kind(), ThemeKind::CatppuccinMocha);
            assert_eq!(app.theme.bg, Theme::catppuccin_mocha().bg);
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A graph folder whose `config.toml` still has the old `theme` key.
    fn legacy_graph(name: &str, config_toml: &str, state_toml: Option<&str>) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("notesec-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Storage::open(dir.clone()).unwrap();
        std::fs::write(Config::path(&dir), config_toml).unwrap();
        if let Some(state) = state_toml {
            std::fs::write(UiState::path(&dir), state).unwrap();
        }
        dir
    }

    #[gpui::test]
    fn an_old_config_theme_carries_over_to_state_toml(cx: &mut TestAppContext) {
        // "dark" was the one dark palette, now Catppuccin Mocha: same look.
        let dir = legacy_graph(
            "theme-legacy-dark",
            "theme = \"dark\"\nfont_size = 18.0\n",
            None,
        );
        let storage = Storage::open(dir.clone()).unwrap();
        let config = Config::load(&dir);
        let (view, cx) = cx.add_window_view(|w, cx| NoteSec::new(storage, config, w, cx));
        view.update(cx, |app, _| {
            assert_eq!(app.theme_kind(), ThemeKind::CatppuccinMocha);
            assert_eq!(
                app.theme.bg,
                Theme::catppuccin_mocha().bg,
                "same look as before"
            );
        });
        assert_eq!(
            UiState::load(&dir).theme,
            Some(ThemeKind::CatppuccinMocha),
            "migrated"
        );

        // The old keys stay in config.toml until a config save drops them;
        // the values themselves carried over to state.toml and aren't lost.
        cx.simulate_keystrokes("ctrl-=");
        assert_eq!(UiState::load(&dir).ui_size, Some(19.0));
        let (view2, cx2) = restart(cx, &dir);
        view2.update(cx2, |app, _| {
            assert_eq!(app.theme_kind(), ThemeKind::CatppuccinMocha)
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn an_old_light_theme_stays_light(cx: &mut TestAppContext) {
        let dir = legacy_graph("theme-legacy-light", "theme = \"light\"\n", None);
        let storage = Storage::open(dir.clone()).unwrap();
        let config = Config::load(&dir);
        let (view, cx) = cx.add_window_view(|w, cx| NoteSec::new(storage, config, w, cx));
        view.update(cx, |app, _| assert_eq!(app.theme_kind(), ThemeKind::Light));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn a_newer_choice_beats_the_old_config_value(cx: &mut TestAppContext) {
        let dir = legacy_graph(
            "theme-legacy-loses",
            "theme = \"light\"\n",
            Some("theme = \"tokyo-night\"\n"),
        );
        let storage = Storage::open(dir.clone()).unwrap();
        let config = Config::load(&dir);
        let (view, cx) = cx.add_window_view(|w, cx| NoteSec::new(storage, config, w, cx));
        view.update(cx, |app, _| {
            assert_eq!(app.theme_kind(), ThemeKind::TokyoNight)
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn the_graph_and_modal_scrims_follow_the_theme(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "theme-scrim", &graph_pages(), "Test");
        let (graph, _) = open_graph(&view, cx);
        for kind in ThemeKind::ALL {
            view.update(cx, |app, cx| app.set_theme(kind, cx));
            cx.run_until_parked();
            assert_eq!(
                graph.update(cx, |g, _| g.theme_bg()),
                Theme::from_kind(kind).bg,
                "the open graph recolours with {kind:?}"
            );
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The spec: UI colours all come from `Theme`. This scans the UI sources
    /// (not their tests) for colour literals so one can't sneak back in.
    #[test]
    fn ui_sources_contain_no_hardcoded_colours() {
        const FORBIDDEN: [&str; 9] = [
            "rgb(0x",
            "rgba(0x",
            "hsla(",
            "opaque_grey(",
            "gpui::black()",
            "gpui::white()",
            "gpui::red()",
            "gpui::green()",
            "gpui::blue()",
        ];
        fn collect(path: &std::path::Path, files: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(path).unwrap().flatten() {
                let p = entry.path();
                if p.is_dir() {
                    collect(&p, files);
                } else if p.extension().is_some_and(|e| e == "rs") {
                    files.push(p);
                }
            }
        }
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = vec![src.join("app.rs"), src.join("graph_view.rs")];
        collect(&src.join("app"), &mut files);
        assert!(files.len() > 10, "found the UI sources");

        let mut hits = Vec::new();
        for file in files {
            let text = std::fs::read_to_string(&file).unwrap();
            // Test code (after the first `#[cfg(test)]`) may use any colour.
            let code = text.split("#[cfg(test)]").next().unwrap();
            for (n, line) in code.lines().enumerate() {
                if line.trim_start().starts_with("//") {
                    continue;
                }
                if let Some(bad) = FORBIDDEN.iter().find(|f| line.contains(**f)) {
                    hits.push(format!("{}:{}: {bad}", file.display(), n + 1));
                }
            }
        }
        assert!(hits.is_empty(), "hardcoded colours:\n{}", hits.join("\n"));
    }

    #[gpui::test]
    fn copy_button_works_in_the_light_theme(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "code-copy-light", CODE_PAGE);
        view.update(cx, |app, cx| app.set_theme(ThemeKind::Light, cx));
        cx.run_until_parked();
        view.update(cx, |app, _| assert_eq!(app.theme_kind(), ThemeKind::Light));
        copy_first_code_block(&view, cx);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn copying_again_restarts_the_copied_label(cx: &mut TestAppContext) {
        let md = "- ```\n  one\n  ```\n  ```\n  two\n  ```\n- next\n";
        let (_view, cx, dir) = setup(cx, "code-copy-two", md);
        let click_copy = |cx: &mut VisualTestContext, n: usize| {
            let code = bounds_of(cx, &format!("code-0-{n}"));
            cx.simulate_mouse_move(code.center(), None, Modifiers::none());
            cx.run_until_parked();
            let button = bounds_of(cx, &format!("code-0-{n}-copy")).center();
            cx.simulate_click(button, Modifiers::none());
            cx.run_until_parked();
        };
        click_copy(cx, 0);
        cx.executor().advance_clock(COPIED_FOR / 2);
        click_copy(cx, 1);
        let text = cx.read_from_clipboard().and_then(|item| item.text());
        assert_eq!(text.as_deref(), Some("two"));
        // Only the latest copy says "Copied", for its full moment.
        assert!(has(cx, "code-0-1-copied") && !has(cx, "code-0-0-copied"));
        cx.executor().advance_clock(COPIED_FOR * 3 / 4);
        cx.run_until_parked();
        assert!(has(cx, "code-0-1-copied"));
        cx.executor().advance_clock(COPIED_FOR / 2);
        cx.run_until_parked();
        assert!(!has(cx, "code-0-1-copied"));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The `.png` files in `assets/`, sorted.
    fn assets(dir: &std::path::Path) -> Vec<String> {
        let mut files: Vec<String> = std::fs::read_dir(dir.join("assets"))
            .map(|entries| {
                entries
                    .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        files.sort();
        files
    }

    /// The asset file a block's image reference points at.
    fn referenced_file(content: &str) -> String {
        let images = crate::assets::parse_images(content);
        assert_eq!(images.len(), 1, "one image in {content:?}");
        let target = &images[0].target;
        let file = target.strip_prefix("../assets/").unwrap();
        assert!(
            file.starts_with("image-") && file.ends_with(".png"),
            "{file}"
        );
        file.to_string()
    }

    #[gpui::test]
    fn pasting_an_image_saves_it_and_references_it(cx: &mut TestAppContext) {
        let png = crate::assets::tests::tiny_png();
        let (view, cx, dir) = setup(cx, "paste-image", "- hello\n- next\n");
        assert!(!dir.join("assets").exists());
        click_block(cx, 0);
        cx.simulate_keystrokes("end");
        cx.write_to_clipboard(gpui::ClipboardItem::new_image(&gpui::Image::from_bytes(
            gpui::ImageFormat::Png,
            png.clone(),
        )));
        cx.simulate_keystrokes("ctrl-v");
        let content = view.update(cx, |app, _| app.editor.text.clone());
        assert!(
            content.starts_with("hello![image](../assets/image-"),
            "{content}"
        );
        let name = referenced_file(&content);
        // `assets/` was created and holds exactly the pasted bytes.
        assert_eq!(assets(&dir), vec![name.clone()]);
        assert_eq!(std::fs::read(dir.join("assets").join(&name)).unwrap(), png);
        assert_eq!(file(&dir), format!("- {content}\n- next\n"));

        // Reading view draws the image under the prose, within the cap.
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(has(cx, "image-0-0"));
        assert!(!has(cx, "image-0-0-missing"));
        let image = bounds_of(cx, "image-0-0");
        assert!(image.size.height <= px(IMAGE_MAX_HEIGHT));
        assert!(bounds_of(cx, "block-1").top() >= image.bottom());

        // One undo takes the reference back out; the file stays.
        click_block(cx, 1);
        cx.simulate_keystrokes("escape ctrl-z");
        view.update(cx, |app, _| {
            assert_eq!(app.pages[app.selected].blocks[0].content, "hello")
        });
        assert_eq!(assets(&dir), vec![name]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn pasting_text_still_pastes_text(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "paste-text-not-image", "- a\n");
        click_block(cx, 0);
        cx.simulate_keystrokes("end");
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("bc".into()));
        cx.simulate_keystrokes("ctrl-v");
        view.update(cx, |app, _| assert_eq!(app.editor.text, "abc"));
        assert!(!dir.join("assets").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn missing_or_broken_images_render_a_placeholder(cx: &mut TestAppContext) {
        let md =
            "- before\n  ![cat](../assets/gone.png)\n  after\n- ![x](../assets/bad.png)\n- next\n";
        let (view, cx, dir) = setup(cx, "image-missing", md);
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        std::fs::write(dir.join("assets/bad.png"), b"not an image").unwrap();
        view.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        // Missing: a placeholder, with the text on both sides still shown.
        assert!(has(cx, "image-0-0-missing"));
        assert!(!has(cx, "image-0-0"));
        // Present but undecodable: the fallback placeholder.
        assert!(has(cx, "image-1-0-missing"));
        assert!(has(cx, "block-2"));
        assert!(bounds_of(cx, "block-2").top() >= bounds_of(cx, "image-1-0-missing").bottom());
        // Editing shows the raw reference, and nothing is rewritten.
        click_block(cx, 0);
        view.update(cx, |app, _| {
            assert_eq!(app.editor.text, "before\n![cat](../assets/gone.png)\nafter")
        });
        cx.simulate_keystrokes("escape");
        assert_eq!(file(&dir), md);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn images_inside_code_blocks_are_not_drawn(cx: &mut TestAppContext) {
        let md = "- ```\n  ![x](../assets/gone.png)\n  ```\n";
        let (_view, cx, dir) = setup(cx, "image-in-code", md);
        assert!(has(cx, "code-0-0"));
        assert!(!has(cx, "image-0-0-missing") && !has(cx, "image-0-0"));
        let _ = std::fs::remove_dir_all(dir);
    }

    fn drop_files(cx: &mut VisualTestContext, at: gpui::Point<Pixels>, paths: Vec<PathBuf>) {
        cx.simulate_event(gpui::FileDropEvent::Entered {
            position: at,
            paths: ExternalPaths(paths.into_iter().collect()),
        });
        cx.simulate_event(gpui::FileDropEvent::Submit { position: at });
        cx.simulate_event(gpui::FileDropEvent::Exited);
        cx.run_until_parked();
    }

    #[gpui::test]
    fn dropping_image_files_adds_them_as_blocks(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "drop-image", "- a\n  - child\n");
        let outside = dir.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let png = crate::assets::tests::tiny_png();
        std::fs::write(outside.join("pic.PNG"), &png).unwrap();
        let mut bmp = std::io::Cursor::new(Vec::new());
        image::RgbImage::new(3, 2)
            .write_to(&mut bmp, image::ImageFormat::Bmp)
            .unwrap();
        std::fs::write(outside.join("photo.bmp"), bmp.into_inner()).unwrap();
        std::fs::write(outside.join("notes.txt"), "hi").unwrap();

        let at = bounds_of(cx, "block-0").center();
        drop_files(
            cx,
            at,
            vec![
                outside.join("pic.PNG"),
                outside.join("notes.txt"),
                outside.join("photo.bmp"),
            ],
        );
        // Two images, two new top-level blocks at the end; the text file is
        // ignored. Both are PNGs now, with different names.
        let files = assets(&dir);
        assert_eq!(files.len(), 2, "{files:?}");
        let blocks = view.update(cx, |app, _| {
            let page = &app.pages[app.selected];
            assert_eq!(page.blocks.len(), 4);
            assert!(page.blocks[2..].iter().all(|b| b.parent_id.is_none()));
            page.blocks[2..]
                .iter()
                .map(|b| b.content.clone())
                .collect::<Vec<_>>()
        });
        let named: Vec<String> = blocks.iter().map(|b| referenced_file(b)).collect();
        let mut sorted = named.clone();
        sorted.sort();
        assert_eq!(sorted, files);
        assert_eq!(
            std::fs::read(dir.join("assets").join(&named[0])).unwrap(),
            png
        );
        let converted = std::fs::read(dir.join("assets").join(&named[1])).unwrap();
        assert_eq!(
            image::guess_format(&converted).unwrap(),
            image::ImageFormat::Png
        );
        assert_eq!(
            file(&dir),
            format!("- a\n  - child\n- {}\n- {}\n", blocks[0], blocks[1])
        );
        assert!(has(cx, "image-2-0") && has(cx, "image-3-0"));

        // Dropped while editing: the reference goes in at the cursor.
        // A keypress just before matters: GPUI then treats nothing as
        // hovered until the mouse moves in the window.
        click_block(cx, 0);
        cx.simulate_keystrokes("end");
        drop_files(cx, at, vec![outside.join("pic.PNG")]);
        let content = view.update(cx, |app, _| app.editor.text.clone());
        assert!(
            content.starts_with("a![image](../assets/image-"),
            "{content}"
        );
        assert_eq!(assets(&dir).len(), 3);

        // Only non-image files: nothing happens.
        cx.simulate_keystrokes("escape");
        let before = file(&dir);
        drop_files(cx, at, vec![outside.join("notes.txt")]);
        assert_eq!(file(&dir), before);
        assert_eq!(assets(&dir).len(), 3);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn file_drags_that_leave_or_miss_the_page_add_nothing(cx: &mut TestAppContext) {
        let md = "- a\n- b\n";
        let (_view, cx, dir) = setup(cx, "drop-miss", md);
        let pic = dir.join("pic.png");
        std::fs::write(&pic, crate::assets::tests::tiny_png()).unwrap();
        let at = bounds_of(cx, "block-0").center();
        // Dragged over the page, then back out of the window.
        cx.simulate_event(gpui::FileDropEvent::Entered {
            position: at,
            paths: ExternalPaths([pic.clone()].into_iter().collect()),
        });
        cx.simulate_event(gpui::FileDropEvent::Exited);
        // A later click (or block drag) doesn't pick the files up.
        click_block(cx, 1);
        cx.simulate_keystrokes("escape");
        // Dropped on the sidebar, not the page.
        let sidebar = gpui::point(bounds_of(cx, "sidebar").center().x, at.y);
        drop_files(cx, sidebar, vec![pic]);
        assert!(!dir.join("assets").exists());
        assert_eq!(file(&dir), md);
        let _ = std::fs::remove_dir_all(dir);
    }

    // --- page context menu -------------------------------------------------

    /// Right-click (press and release) the middle of `selector`'s element.
    fn right_click(cx: &mut VisualTestContext, selector: &str) -> Point<Pixels> {
        let selector: &'static str = Box::leak(selector.to_string().into_boxed_str());
        let at = cx.debug_bounds(selector).expect(selector).center();
        cx.simulate_mouse_down(at, MouseButton::Right, Modifiers::none());
        cx.simulate_mouse_up(at, MouseButton::Right, Modifiers::none());
        at
    }

    fn right_click_page(view: &Entity<NoteSec>, cx: &mut VisualTestContext, title: &str) {
        let row = page_row(view, cx, "page", title);
        right_click(cx, &row);
    }

    fn menu_title(view: &Entity<NoteSec>, cx: &mut VisualTestContext) -> Option<String> {
        view.update(cx, |app, _| app.page_menu.as_ref().map(|m| m.title.clone()))
    }

    fn non_journal_titles(view: &Entity<NoteSec>, cx: &mut VisualTestContext) -> Vec<String> {
        view.update(cx, |app, _| {
            app.pages
                .iter()
                .filter(|p| !p.is_journal)
                .map(|p| p.title.clone())
                .collect()
        })
    }

    #[gpui::test]
    fn right_click_opens_the_page_menu_and_esc_or_outside_click_closes_it(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "menu-open", &tab_pages(), "Test");
        assert!(!has(cx, "page-menu"));

        let row = page_row(&view, cx, "page", "Alpha");
        let at = right_click(cx, &row);
        assert_eq!(menu_title(&view, cx).as_deref(), Some("Alpha"));
        for item in [
            "page-menu",
            "page-menu-rename",
            "page-menu-delete",
            "page-menu-copy",
        ] {
            assert!(has(cx, item), "{item}");
        }
        // It opens where the click was, and the click didn't open the page.
        let menu = cx.debug_bounds("page-menu").unwrap();
        assert!((menu.origin.x - at.x).abs() < px(2.) && (menu.origin.y - at.y).abs() < px(2.));
        assert_eq!(selected_title(&view, cx), "Test");

        cx.simulate_keystrokes("escape");
        assert!(menu_title(&view, cx).is_none() && !has(cx, "page-menu"));

        // A click outside closes it without reaching what is underneath.
        right_click_page(&view, cx, "Alpha");
        let beta = page_row(&view, cx, "page", "Beta");
        click_on(cx, &beta);
        assert!(menu_title(&view, cx).is_none());
        assert_eq!(selected_title(&view, cx), "Test");
        // So does a right-click outside.
        right_click_page(&view, cx, "Alpha");
        cx.simulate_mouse_down(
            point(px(900.), px(600.)),
            MouseButton::Right,
            Modifiers::none(),
        );
        assert!(menu_title(&view, cx).is_none());

        // FAVORITES rows have the same menu.
        view.update(cx, |app, cx| app.toggle_favorite("Beta", cx));
        cx.run_until_parked();
        right_click(cx, "fav-0");
        assert_eq!(menu_title(&view, cx).as_deref(), Some("Beta"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn rename_moves_the_file_and_selection_tabs_and_lists_follow(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "menu-rename", &tab_pages(), "Test");
        click_sidebar_page(&view, cx, "Alpha");
        view.update(cx, |app, cx| app.toggle_favorite("Alpha", cx));
        // An unsaved edit on the page is saved before the file moves.
        click_block(cx, 0);
        cx.simulate_input(" edited");

        right_click_page(&view, cx, "Alpha");
        view.update(cx, |app, _| assert_eq!(app.editing, None));
        click_on(cx, "page-menu-rename");
        assert!(has(cx, "rename-input") && !has(cx, "page-menu"));
        // The old name is selected, so typing replaces it.
        cx.simulate_input("Gamma");
        cx.simulate_keystrokes("enter");

        assert!(menu_title(&view, cx).is_none() && !has(cx, "rename-input"));
        assert!(!page_file(&dir, "Alpha").exists());
        assert_eq!(
            std::fs::read_to_string(page_file(&dir, "Gamma")).unwrap(),
            "- alpha edited\n"
        );
        // Still sorted, and the selection and the open tab follow.
        assert_eq!(non_journal_titles(&view, cx), ["Beta", "Gamma", "Test"]);
        let gamma = page_row(&view, cx, "page", "Gamma");
        assert!(has(cx, &gamma));
        assert_eq!(selected_title(&view, cx), "Gamma");
        tabs_are(&view, cx, &["Test", "Gamma"], 1);
        view.update(cx, |app, _| {
            assert_eq!(app.state.favorites, vec!["Gamma".to_string()]);
            assert_eq!(app.state.recent[0], "Gamma");
            assert!(!app.state.recent.iter().any(|t| t == "Alpha"));
            assert_eq!(UiState::load(&dir), app.state);
        });
        // Rename isn't undoable, and undo can't bring the old file back.
        cx.simulate_keystrokes("ctrl-z");
        assert!(!page_file(&dir, "Alpha").exists());
        assert_eq!(selected_title(&view, cx), "Gamma");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn rename_refuses_taken_empty_and_unmappable_names(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "menu-rename-bad", &tab_pages(), "Test");
        right_click_page(&view, cx, "Alpha");
        click_on(cx, "page-menu-rename");

        let error = |view: &Entity<NoteSec>, cx: &mut VisualTestContext| {
            view.update(cx, |app, _| match &app.page_menu {
                Some(PageMenu {
                    step: MenuStep::Rename { error, .. },
                    ..
                }) => error.clone(),
                _ => panic!("rename field closed"),
            })
        };
        // Another page's name, in any case.
        cx.simulate_input("beta");
        cx.simulate_keystrokes("enter");
        assert!(error(&view, cx).unwrap().contains("already exists"));
        assert!(has(cx, "rename-error"));
        // Editing the name clears the message.
        cx.simulate_keystrokes("shift-home backspace");
        assert_eq!(error(&view, cx), None);
        cx.simulate_input("   ");
        cx.simulate_keystrokes("enter");
        assert!(error(&view, cx).unwrap().contains("empty"));
        cx.simulate_keystrokes("shift-home backspace");
        cx.simulate_input("a___b");
        cx.simulate_keystrokes("enter");
        assert!(error(&view, cx).unwrap().contains("___"));

        cx.simulate_keystrokes("escape");
        assert!(menu_title(&view, cx).is_none());
        assert_eq!(non_journal_titles(&view, cx), ["Alpha", "Beta", "Test"]);
        assert!(page_file(&dir, "Alpha").exists() && page_file(&dir, "Beta").exists());

        // Journals are named by their date: Rename is disabled for them.
        let today = view.update(cx, |app, _| app.find_journal(&today_title()).unwrap());
        right_click(cx, &format!("page-{today}"));
        click_on(cx, "page-menu-rename");
        assert!(has(cx, "page-menu") && !has(cx, "rename-input"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn delete_after_confirm_trashes_file_and_removes_page_and_tab(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "menu-delete", &tab_pages(), "Test");
        click_sidebar_page(&view, cx, "Alpha");
        click_sidebar_page(&view, cx, "Beta");
        click_on(cx, "tab-1");
        tabs_are(&view, cx, &["Test", "Alpha", "Beta"], 1);
        view.update(cx, |app, cx| app.toggle_favorite("Alpha", cx));

        right_click_page(&view, cx, "Alpha");
        click_on(cx, "page-menu-delete");
        assert!(has(cx, "confirm-delete") && !has(cx, "page-menu"));
        // Enter doesn't confirm a destructive action.
        cx.simulate_keystrokes("enter");
        assert!(has(cx, "confirm-delete") && page_file(&dir, "Alpha").exists());

        click_on(cx, "confirm-delete-ok");
        assert!(menu_title(&view, cx).is_none() && !has(cx, "confirm-delete"));
        assert!(!page_file(&dir, "Alpha").exists());
        let trashed = view.update(cx, |app, _| app.trash[0].clone());
        assert_eq!(trashed.title, "Alpha");
        assert!(dir
            .join(".trash")
            .join(&trashed.id)
            .join("pages/Alpha.md")
            .exists());
        assert_eq!(non_journal_titles(&view, cx), ["Beta", "Test"]);
        // Its tab closed and the neighbour that took its place has focus.
        tabs_are(&view, cx, &["Test", "Beta"], 1);
        view.update(cx, |app, _| {
            assert!(!app.state.favorites.iter().any(|t| t == "Alpha"));
            assert!(!app.state.recent.iter().any(|t| t == "Alpha"));
            assert_eq!(UiState::load(&dir), app.state);
        });
        // Deleting a page with no tab leaves the tabs alone.
        right_click_page(&view, cx, "Test");
        click_on(cx, "page-menu-delete");
        click_on(cx, "confirm-delete-ok");
        tabs_are(&view, cx, &["Beta"], 0);
        assert!(!page_file(&dir, "Test").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn cancelling_a_delete_keeps_the_file(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "menu-delete-cancel", &tab_pages(), "Test");
        let ask = |view: &Entity<NoteSec>, cx: &mut VisualTestContext| {
            right_click_page(view, cx, "Beta");
            click_on(cx, "page-menu-delete");
            assert!(has(cx, "confirm-delete"));
        };
        ask(&view, cx);
        click_on(cx, "confirm-delete-cancel");
        assert!(!has(cx, "confirm-delete"));
        ask(&view, cx);
        cx.simulate_keystrokes("escape");
        assert!(!has(cx, "confirm-delete"));
        ask(&view, cx);
        let dialog = cx.debug_bounds("confirm-delete").unwrap();
        cx.simulate_click(dialog.origin - point(px(20.), px(20.)), Modifiers::none());
        assert!(menu_title(&view, cx).is_none());

        assert!(page_file(&dir, "Beta").exists());
        assert_eq!(non_journal_titles(&view, cx), ["Alpha", "Beta", "Test"]);
        // Nothing went to the trash either.
        view.update(cx, |app, _| assert!(app.trash.is_empty()));
        assert!(!dir.join(".trash").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn todays_journal_can_be_deleted_but_not_the_last_page(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "menu-delete-journal", "- hi\n");
        let today = view.update(cx, |app, _| app.find_journal(&today_title()).unwrap());
        right_click(cx, &format!("page-{today}"));
        click_on(cx, "page-menu-delete");
        click_on(cx, "confirm-delete-ok");
        assert!(!todays_journal_file(&dir).exists());
        view.update(cx, |app, _| {
            assert_eq!(app.pages.len(), 1);
            assert_eq!(app.pages[app.selected].title, "Test");
            // It went to the trash, as a journal.
            assert_eq!(app.trash[0].title, today_title());
            assert!(app.trash[0].is_journal);
        });

        // The last page: Delete is disabled.
        right_click_page(&view, cx, "Test");
        click_on(cx, "page-menu-delete");
        assert!(has(cx, "page-menu") && !has(cx, "confirm-delete"));
        cx.simulate_keystrokes("escape");
        assert!(file(&dir).contains("hi"));

        // Ctrl-J brings today's journal back.
        cx.simulate_keystrokes("ctrl-j");
        assert!(todays_journal_file(&dir).exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn copy_page_title_puts_it_on_the_clipboard(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "menu-copy", &tab_pages(), "Test");
        right_click_page(&view, cx, "Alpha");
        click_on(cx, "page-menu-copy");
        assert!(menu_title(&view, cx).is_none());
        assert_eq!(
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .as_deref(),
            Some("Alpha")
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    // --- drag to reorder pages ---------------------------------------------

    fn reorder_pages() -> Vec<(&'static str, &'static str)> {
        vec![
            ("Alpha", "- a\n"),
            ("Beta", "- b\n"),
            ("Gamma", "- g\n"),
            ("Delta", "- d\n"),
        ]
    }

    /// Press on `from`'s element and drag (left button held) to `to`
    /// without releasing. The first move, past GPUI's 2px threshold, starts
    /// the drag; the drop zones hear the moves after it.
    fn start_drag(cx: &mut VisualTestContext, from: &str, to: Point<Pixels>) {
        let start = bounds_of(cx, from).center();
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(
            start + point(px(0.), px(5.)),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.simulate_mouse_move(to, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
    }

    fn release(cx: &mut VisualTestContext, at: Point<Pixels>) {
        cx.simulate_mouse_up(at, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
    }

    /// Just inside the top (`upper`) or bottom edge of `selector`'s element.
    fn edge_of(cx: &mut VisualTestContext, selector: &str, upper: bool) -> Point<Pixels> {
        let b = bounds_of(cx, selector);
        if upper {
            point(b.center().x, b.top() + px(3.))
        } else {
            point(b.center().x, b.bottom() - px(3.))
        }
    }

    fn drag_page_to(
        view: &Entity<NoteSec>,
        cx: &mut VisualTestContext,
        title: &str,
        to: Point<Pixels>,
    ) {
        let row = page_row(view, cx, "page", title);
        start_drag(cx, &row, to);
        release(cx, to);
    }

    fn titles(list: &[&str]) -> Vec<String> {
        list.iter().map(|t| t.to_string()).collect()
    }

    #[test]
    fn sort_pages_puts_journals_first_then_the_custom_order_then_the_rest() {
        let mut pages: Vec<Page> = [
            ("beta", false),
            ("2026-10-07", true),
            ("Mid", false),
            ("Alpha", false),
            ("2026-10-08", true),
            ("Zed", false),
        ]
        .into_iter()
        .map(|(title, journal)| Page::new(title, journal))
        .collect();
        let order_of =
            |pages: &[Page]| -> Vec<String> { pages.iter().map(|p| p.title.clone()).collect() };

        // No custom order: alphabetical, ignoring case.
        sort_pages(&mut pages, &[]);
        assert_eq!(
            order_of(&pages),
            titles(&["2026-10-08", "2026-10-07", "Alpha", "beta", "Mid", "Zed"])
        );

        // Listed pages first, matched ignoring case (the first entry wins);
        // a stale entry is skipped; unlisted pages follow alphabetically.
        let order = titles(&["zed", "Gone", "alpha", "ZED"]);
        sort_pages(&mut pages, &order);
        assert_eq!(
            order_of(&pages),
            titles(&["2026-10-08", "2026-10-07", "Zed", "Alpha", "beta", "Mid"])
        );

        // Journals never take part, even if listed.
        sort_pages(&mut pages, &titles(&["2026-10-07", "Mid"]));
        assert_eq!(
            order_of(&pages),
            titles(&["2026-10-08", "2026-10-07", "Mid", "Alpha", "beta", "Zed"])
        );
    }

    #[gpui::test]
    fn dragging_a_page_reorders_it_and_the_order_survives_a_restart(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "drag-reorder", &reorder_pages(), "Beta");
        assert_eq!(
            non_journal_titles(&view, cx),
            titles(&["Alpha", "Beta", "Delta", "Gamma"])
        );

        // Drag Gamma over the upper half of Alpha: the indicator shows in
        // the gap above Alpha.
        let gamma = page_row(&view, cx, "page", "Gamma");
        let alpha = page_row(&view, cx, "page", "Alpha");
        let to = edge_of(cx, &alpha, true);
        start_drag(cx, &gamma, to);
        let line = bounds_of(cx, "page-drop-indicator");
        let alpha_top = bounds_of(cx, &alpha).top();
        assert!(line.bottom() <= alpha_top && line.top() >= alpha_top - px(4.));
        release(cx, to);

        assert!(!has(cx, "page-drop-indicator"), "gone after the drop");
        let order = titles(&["Gamma", "Alpha", "Beta", "Delta"]);
        assert_eq!(non_journal_titles(&view, cx), order);
        view.update(cx, |app, _| {
            assert_eq!(app.state.page_order, order);
            assert_eq!(UiState::load(&dir).page_order, order);
            // The selection and the tab still show Beta (indices moved);
            // the drag did not click Gamma open.
            assert_eq!(app.pages[app.selected].title, "Beta");
            assert_eq!(app.tabs.tabs, vec![TabTarget::Page("Beta".into())]);
            // Journals stay first.
            assert!(app.pages[0].is_journal);
        });
        // The row now rendered first among the pages is Gamma.
        let first = page_row(&view, cx, "page", "Gamma");
        let second = page_row(&view, cx, "page", "Alpha");
        assert!(bounds_of(cx, &first).top() < bounds_of(cx, &second).top());

        let storage = Storage::open(dir.clone()).unwrap();
        let (view2, cx2) = cx
            .cx
            .add_window_view(|window, cx| NoteSec::new(storage, Config::default(), window, cx));
        cx2.run_until_parked();
        assert_eq!(non_journal_titles(&view2, cx2), order);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn pages_drop_below_a_row_at_the_very_end_but_not_onto_journals(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "drag-end", &reorder_pages(), "Beta");

        // Lower half of Beta: after it.
        let beta = page_row(&view, cx, "page", "Beta");
        let to = edge_of(cx, &beta, false);
        drag_page_to(&view, cx, "Alpha", to);
        assert_eq!(
            non_journal_titles(&view, cx),
            titles(&["Beta", "Alpha", "Delta", "Gamma"])
        );

        // The end zone below the last page: to the very end, with the
        // indicator drawn there.
        let beta = page_row(&view, cx, "page", "Beta");
        let end = bounds_of(cx, "page-drop-end").center();
        start_drag(cx, &beta, end);
        let line = bounds_of(cx, "page-drop-indicator");
        assert!(line.bottom() <= bounds_of(cx, "page-drop-end").top());
        release(cx, end);
        assert_eq!(
            non_journal_titles(&view, cx),
            titles(&["Alpha", "Delta", "Gamma", "Beta"])
        );

        // Lower half of the last row (Beta): also the end.
        let beta = page_row(&view, cx, "page", "Beta");
        let to = edge_of(cx, &beta, false);
        drag_page_to(&view, cx, "Alpha", to);
        let order = titles(&["Delta", "Gamma", "Beta", "Alpha"]);
        assert_eq!(non_journal_titles(&view, cx), order);

        // Dropping a page where it already is shows no indicator and
        // changes nothing.
        let delta = page_row(&view, cx, "page", "Delta");
        let to = edge_of(cx, &delta, false);
        start_drag(cx, &delta, to);
        assert!(!has(cx, "page-drop-indicator"));
        release(cx, to);
        assert_eq!(non_journal_titles(&view, cx), order);

        // Journals can't be dragged, and aren't drop targets.
        let journal = page_row(&view, cx, "page", &today_title());
        let gamma = page_row(&view, cx, "page", "Gamma");
        let to = edge_of(cx, &gamma, true);
        start_drag(cx, &journal, to);
        assert!(!has(cx, "page-drop-indicator"));
        release(cx, to);
        let to = edge_of(cx, &journal, false);
        drag_page_to(&view, cx, "Alpha", to);
        assert_eq!(non_journal_titles(&view, cx), order);
        view.update(cx, |app, _| {
            assert!(app.pages[0].is_journal);
            assert_eq!(UiState::load(&dir).page_order, order);
        });

        // Released outside the sidebar (no move there first, as when the
        // pointer leaves the window): the drag just ends.
        let main = bounds_of(cx, "block-0").center();
        let row = page_row(&view, cx, "page", "Beta");
        let delta = page_row(&view, cx, "page", "Delta");
        let to = edge_of(cx, &delta, true);
        start_drag(cx, &row, to);
        assert!(has(cx, "page-drop-indicator"));
        release(cx, main);
        assert!(!has(cx, "page-drop-indicator"));
        assert_eq!(non_journal_titles(&view, cx), order);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn new_pages_go_at_the_end_of_a_custom_order(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "drag-new", &reorder_pages(), "Beta");
        // Without a custom order a new page is placed alphabetically.
        view.update(cx, |app, cx| app.open_page("Aardvark", cx));
        assert_eq!(
            non_journal_titles(&view, cx),
            titles(&["Aardvark", "Alpha", "Beta", "Delta", "Gamma"])
        );
        view.update(cx, |app, _| assert!(app.state.page_order.is_empty()));

        let alpha = page_row(&view, cx, "page", "Alpha");
        let to = edge_of(cx, &alpha, true);
        drag_page_to(&view, cx, "Gamma", to);
        let order = titles(&["Aardvark", "Gamma", "Alpha", "Beta", "Delta"]);
        assert_eq!(non_journal_titles(&view, cx), order);

        // Ctrl-N, then a page created by following a link: each at the end.
        cx.simulate_keystrokes("ctrl-n");
        cx.simulate_keystrokes("escape");
        view.update(cx, |app, cx| app.open_page("Apple", cx));
        let mut expected = order.clone();
        expected.extend(titles(&["Untitled", "Apple"]));
        assert_eq!(non_journal_titles(&view, cx), expected);
        view.update(cx, |app, _| {
            assert_eq!(app.pages[app.selected].title, "Apple");
            assert_eq!(app.state.page_order, expected);
            assert_eq!(UiState::load(&dir).page_order, expected);
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn sort_a_z_from_the_palette_or_page_menu_restores_alphabetical_order(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "drag-sort", &reorder_pages(), "Beta");
        let alphabetical = titles(&["Alpha", "Beta", "Delta", "Gamma"]);
        let reorder = |view: &Entity<NoteSec>, cx: &mut VisualTestContext| {
            let alpha = page_row(view, cx, "page", "Alpha");
            let to = edge_of(cx, &alpha, true);
            drag_page_to(view, cx, "Delta", to);
            assert_eq!(
                non_journal_titles(view, cx),
                titles(&["Delta", "Alpha", "Beta", "Gamma"])
            );
        };

        reorder(&view, cx);
        cx.simulate_keystrokes("ctrl-k");
        cx.simulate_input("sort pages");
        view.update(cx, |app, _| {
            assert_eq!(
                app.search_results()[0].target,
                Target::Command(Command::SortPagesAz)
            );
        });
        cx.simulate_keystrokes("enter");
        assert_eq!(non_journal_titles(&view, cx), alphabetical);
        view.update(cx, |app, _| {
            assert!(app.state.page_order.is_empty());
            assert!(UiState::load(&dir).page_order.is_empty());
            assert_eq!(app.pages[app.selected].title, "Beta");
        });

        // From the page menu; it is disabled while already alphabetical.
        right_click_page(&view, cx, "Gamma");
        click_on(cx, "page-menu-sort");
        assert_eq!(menu_title(&view, cx).as_deref(), Some("Gamma"), "disabled");
        cx.simulate_keystrokes("escape");
        reorder(&view, cx);
        right_click_page(&view, cx, "Gamma");
        click_on(cx, "page-menu-sort");
        assert_eq!(menu_title(&view, cx), None);
        assert_eq!(non_journal_titles(&view, cx), alphabetical);
        assert!(UiState::load(&dir).page_order.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn rename_delete_and_undo_keep_the_custom_order(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "drag-rename", &reorder_pages(), "Beta");
        // An edit before the reorder leaves an undo snapshot in A-Z order.
        click_block(cx, 0);
        cx.simulate_input("!");
        cx.simulate_keystrokes("escape");
        let alpha = page_row(&view, cx, "page", "Alpha");
        let to = edge_of(cx, &alpha, true);
        drag_page_to(&view, cx, "Gamma", to);
        let order = titles(&["Gamma", "Alpha", "Beta", "Delta"]);
        assert_eq!(non_journal_titles(&view, cx), order);

        // Undo restores the text but not the old order.
        cx.simulate_keystrokes("ctrl-z");
        view.update(cx, |app, _| {
            assert_eq!(app.pages[app.selected].title, "Beta");
            assert_eq!(app.pages[app.selected].blocks[0].content, "b");
        });
        assert_eq!(non_journal_titles(&view, cx), order);

        // A renamed page keeps its place (no longer alphabetical).
        view.update(cx, |app, cx| app.rename_page("Alpha", "Zulu", cx))
            .unwrap();
        let order = titles(&["Gamma", "Zulu", "Beta", "Delta"]);
        assert_eq!(non_journal_titles(&view, cx), order);
        view.update(cx, |app, cx| app.delete_page("Beta", cx))
            .unwrap();
        let order = titles(&["Gamma", "Zulu", "Delta"]);
        assert_eq!(non_journal_titles(&view, cx), order);
        view.update(cx, |app, _| {
            assert_eq!(app.state.page_order, order);
            assert_eq!(UiState::load(&dir).page_order, order);
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    // --- agenda --------------------------------------------------------------

    #[gpui::test]
    fn agenda_groups_dated_tasks_and_clicking_one_opens_its_page(cx: &mut TestAppContext) {
        let today = chrono::Local::now().date_naive();
        let fmt = |d: chrono::NaiveDate| d.format("%Y-%m-%d").to_string();
        let (yesterday, tomorrow) = (
            fmt(today - chrono::Days::new(1)),
            fmt(today + chrono::Days::new(1)),
        );
        let today = fmt(today);
        let work = format!(
            "- TODO late report\n  DEADLINE: <{yesterday}>\n\
             - TODO due today\n  SCHEDULED: <{today}>\n\
             - DONE finished\n  SCHEDULED: <{today}>\n\
             - TODO next up\n  SCHEDULED: <{tomorrow} 09:00>\n"
        );
        let home =
            format!("- DOING someday\n- parent\n  - TODO nested today\n    SCHEDULED: <{today}>\n");
        let pages = [
            ("Test", "- hello\n"),
            ("Work", work.as_str()),
            ("Home", home.as_str()),
        ];
        let (view, cx, dir) = setup_pages(cx, "agenda", &pages, "Test");
        assert!(!has(cx, "agenda"));

        click_on(cx, "sidebar-agenda");
        tabs_are(&view, cx, &["Test", "Agenda"], 1);
        view.update(cx, |app, _| assert_eq!(app.mode, Mode::Agenda));

        // Top to bottom: each group header, then its items. Within Today the
        // pages sort by title (Home before Work); DONE is left out.
        let top = |cx: &mut VisualTestContext, s: &str| bounds_of(cx, s).top();
        let order = [
            "agenda-group-overdue",
            "agenda-item-0",
            "agenda-group-today",
            "agenda-item-1",
            "agenda-item-2",
            "agenda-group-upcoming",
            "agenda-item-3",
            "agenda-group-unscheduled",
            "agenda-item-4",
        ];
        for pair in order.windows(2) {
            assert!(top(cx, pair[0]) < top(cx, pair[1]), "{pair:?}");
        }
        assert!(!has(cx, "agenda-item-5"), "the DONE task is not listed");
        view.update(cx, |app, _| {
            let agenda = app.agenda();
            assert_eq!(agenda.overdue[0].text, "late report");
            let today_items: Vec<&str> = agenda.today.iter().map(|i| i.text.as_str()).collect();
            assert_eq!(today_items, vec!["nested today", "due today"]);
            assert_eq!(agenda.upcoming[0].1[0].text, "next up");
            assert_eq!(agenda.unscheduled[0].text, "someday");
        });

        // Clicking an item opens its page in a new tab (the agenda tab
        // stays) and unfolds the task's block.
        view.update(cx, |app, _| {
            let home = app.find_page("Home").unwrap();
            let parent = app.pages[home].blocks[1].id;
            app.collapsed.insert(parent);
        });
        click_on(cx, "agenda-item-1");
        tabs_are(&view, cx, &["Test", "Agenda", "Home"], 2);
        view.update(cx, |app, _| {
            assert_eq!(app.mode, Mode::Notes);
            assert_eq!(app.pages[app.selected].title, "Home");
            assert!(app.collapsed.is_empty(), "the task's parent is unfolded");
            assert_eq!(app.editing, None);
        });
        // The page with its two-line task renders in the reading view.
        assert!(has(cx, "block-2"));

        // The palette command focuses the agenda again; it is rebuilt from
        // the pages, so a task finished meanwhile is gone.
        view.update(cx, |app, _| {
            let work = app.find_page("Work").unwrap();
            let block = &mut app.pages[work].blocks[0];
            block.content = block.content.replacen("TODO", "DONE", 1);
        });
        cx.simulate_keystrokes("ctrl-k");
        cx.simulate_input("open agenda");
        view.update(cx, |app, _| {
            assert_eq!(
                app.search_results()[0].target,
                Target::Command(Command::OpenAgenda)
            );
        });
        cx.simulate_keystrokes("enter");
        tabs_are(&view, cx, &["Test", "Agenda", "Home"], 1);
        assert!(!has(cx, "agenda-group-overdue"));
        assert!(has(cx, "agenda-group-today"));

        click_on(cx, "agenda-item-1");
        view.update(cx, |app, _| {
            assert_eq!(app.pages[app.selected].title, "Work")
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn empty_agenda_shows_no_groups(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "agenda-empty", "- hello\n");
        view.update(cx, |app, cx| app.show_agenda(cx));
        assert!(has(cx, "agenda"));
        assert!(!has(cx, "agenda-item-0"));
        for group in ["overdue", "today", "upcoming", "unscheduled"] {
            assert!(!has(cx, &format!("agenda-group-{group}")));
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    // --- command palette for everything, shortcuts dialog -----------------------

    /// Open the palette, type `query` and run the top result with Enter.
    fn run_in_palette(cx: &mut VisualTestContext, query: &str) {
        cx.simulate_keystrokes("ctrl-k");
        cx.simulate_input(query);
        cx.simulate_keystrokes("enter");
    }

    fn top_hit(view: &Entity<NoteSec>, cx: &mut VisualTestContext, query: &str) -> Target {
        cx.simulate_keystrokes("ctrl-k");
        cx.simulate_input(query);
        let top = view.update(cx, |app, _| app.search_results()[0].target);
        cx.simulate_keystrokes("escape");
        top
    }

    fn command_pages() -> [(&'static str, &'static str); 3] {
        [
            ("Test", "- a\n  - b\n- c\n  - d\n    - e\n"),
            ("Beta", "- other\n"),
            ("Alpha", "- more\n"),
        ]
    }

    #[gpui::test]
    fn every_command_is_listed_with_an_empty_query(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "cmd-all", &command_pages(), "Test");
        // Split, so the pane commands are offered, and opened while editing
        // a block, so editor commands are offered too.
        cx.simulate_keystrokes("ctrl-\\");
        click_block(cx, 0);
        cx.simulate_keystrokes("ctrl-k");
        assert!(has(cx, "palette-commands-header"));
        assert!(has(cx, "palette-pages-header"));
        let commands: Vec<Command> = view.update(cx, |app, _| {
            app.search_results()
                .iter()
                .filter_map(|h| match h.target {
                    Target::Command(c) => Some(c),
                    _ => None,
                })
                .collect()
        });
        // All but the ones for a recording in progress (decision 52) and
        // the ones for a whiteboard on screen (decision 53).
        let all: Vec<Command> = Command::ALL
            .iter()
            .copied()
            .filter(|c| {
                !matches!(
                    c,
                    Command::StopRecording
                        | Command::CancelRecording
                        | Command::WhiteboardFit
                        | Command::WhiteboardZoomReset
                        | Command::WhiteboardAddPage
                        | Command::ToggleWhiteboardOutline
                )
            })
            .collect();
        assert_eq!(commands, all);
        // Every row is rendered (the list scrolls; arrows reach the last).
        for c in &all {
            assert!(has(cx, &format!("command-{}", c.name())), "{c:?}");
        }
        for _ in 0..all.len() + 12 {
            cx.simulate_keystrokes("down");
        }
        let last = view.update(cx, |app, _| {
            let s = app.search.as_ref().unwrap();
            app.search_results()[s.selected].target
        });
        assert_eq!(last, Target::Command(*Command::ALL.last().unwrap()));
        cx.simulate_keystrokes("escape");

        // Not editing: no editor commands. From the graph tab: no page
        // commands either.
        cx.simulate_keystrokes("escape ctrl-k");
        let offered = view.update(cx, |app, _| app.available_commands());
        assert!(!offered.contains(&Command::CycleTask));
        assert!(offered.contains(&Command::CollapseAll));
        cx.simulate_keystrokes("escape ctrl-g ctrl-k");
        let offered = view.update(cx, |app, _| app.available_commands());
        for c in &all {
            assert_eq!(
                offered.contains(c),
                c.needs() == Needs::Nothing,
                "{c:?} on the graph tab"
            );
        }
        assert!(!has(cx, "command-CollapseAll"));
        assert!(has(cx, "command-FitGraph"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn fuzzy_queries_find_commands(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "cmd-fuzzy", &command_pages(), "Test");
        for (query, command) in [
            ("shrt", Command::ShowShortcuts),
            ("agnd", Command::OpenAgenda),
            ("trash", Command::OpenTrash),
            ("recycle", Command::OpenTrash),
            ("undelete", Command::OpenTrash),
            ("export html", Command::ExportHtml),
            ("git backup", Command::ToggleGitBackup),
            ("autosave", Command::ToggleGitBackup),
            ("side by side", Command::SplitRight),
            ("split", Command::SplitRight),
            ("rebind", Command::CustomizeShortcuts),
            ("hotkeys", Command::CustomizeShortcuts),
            ("html", Command::ExportHtml),
            ("col all", Command::CollapseAll),
            ("exp all", Command::ExpandAll),
            ("new page", Command::NewPage),
            ("fav", Command::ToggleFavorite),
            ("fit", Command::FitGraph),
            ("journals in graph", Command::ToggleGraphJournals),
            ("rename", Command::RenamePage),
            ("prev tab", Command::PrevTab),
        ] {
            assert_eq!(
                top_hit(&view, cx, query),
                Target::Command(command),
                "{query}"
            );
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn palette_commands_do_what_they_say(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "cmd-run", &command_pages(), "Test");

        // Collapse all folds every block with children; Expand all undoes it.
        run_in_palette(cx, "collapse all");
        assert_eq!(shown(cx, 5), vec![0, 2]);
        view.update(cx, |app, _| {
            assert!(app.search.is_none(), "the palette closed");
            assert!(app.undo_stack.is_empty(), "folding is not an undo step");
        });
        run_in_palette(cx, "expand all");
        assert_eq!(shown(cx, 5), vec![0, 1, 2, 3, 4]);

        // Toggle favorite stars the current page, and again unstars it.
        run_in_palette(cx, "toggle favorite");
        assert!(UiState::load(&dir).favorites.contains(&"Test".to_string()));
        run_in_palette(cx, "toggle favorite");
        assert!(UiState::load(&dir).favorites.is_empty());

        // Copy page title.
        run_in_palette(cx, "copy page title");
        assert_eq!(
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .as_deref(),
            Some("Test")
        );

        // Rename opens the rename field, Delete the confirm dialog.
        run_in_palette(cx, "rename current page");
        assert!(has(cx, "rename-input"));
        assert_eq!(menu_title(&view, cx).as_deref(), Some("Test"));
        cx.simulate_keystrokes("escape");
        run_in_palette(cx, "delete current page");
        assert!(has(cx, "confirm-delete"));
        cx.simulate_keystrokes("escape");
        assert!(dir.join("pages/Test.md").exists());

        // Fit graph and Toggle journals open the graph first.
        run_in_palette(cx, "toggle journals in graph");
        view.update(cx, |app, _| assert_eq!(app.mode, Mode::Graph));
        let graph = view.update(cx, |app, _| app.graph.clone().unwrap());
        assert!(!graph.update(cx, |g, _| g.includes_journals()));
        cx.simulate_keystrokes("ctrl-w");
        run_in_palette(cx, "fit graph");
        view.update(cx, |app, _| assert_eq!(app.mode, Mode::Graph));

        // New page creates a page and opens it.
        run_in_palette(cx, "new page");
        view.update(cx, |app, _| {
            assert_eq!(app.pages[app.selected].title, "Untitled");
            assert_eq!(app.mode, Mode::Notes);
        });
        assert!(dir.join("pages/Untitled.md").exists());

        // Tabs: previous / close.
        let tabs = view.update(cx, |app, _| app.tabs.tabs.len());
        run_in_palette(cx, "close tab");
        view.update(cx, |app, _| assert_eq!(app.tabs.tabs.len(), tabs - 1));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn editor_commands_resume_the_block_the_palette_was_opened_from(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "cmd-editor", "- one\n- two\n");
        click_block(cx, 1);
        cx.simulate_keystrokes("ctrl-k");
        view.update(cx, |app, _| assert_eq!(app.editing, None));
        cx.simulate_input("cycle task");
        cx.simulate_keystrokes("enter");
        view.update(cx, |app, _| {
            assert_eq!(app.editing, Some(1));
            assert_eq!(app.editor.text, "TODO two");
        });
        assert_eq!(file(&dir), "- one\n- TODO two\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn shortcuts_dialog_opens_from_palette_and_key_and_esc_closes_it(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "cmd-shortcuts", "- hi\n");
        run_in_palette(cx, "keyboard shortcuts");
        assert!(has(cx, "shortcuts-dialog"));
        view.update(cx, |app, _| assert!(app.search.is_none()));
        // Modal: tab keys do nothing, Esc closes.
        cx.simulate_keystrokes("ctrl-w");
        view.update(cx, |app, _| assert_eq!(app.tabs.tabs.len(), 1));
        cx.simulate_keystrokes("escape");
        assert!(!has(cx, "shortcuts-dialog"));

        // Ctrl+/ toggles it, even while editing (which it ends).
        click_block(cx, 0);
        cx.simulate_keystrokes("ctrl-/");
        assert!(has(cx, "shortcuts-dialog"));
        view.update(cx, |app, _| assert_eq!(app.editing, None));
        cx.simulate_keystrokes("ctrl-/");
        assert!(!has(cx, "shortcuts-dialog"));

        // A click on the backdrop closes it; one inside does not.
        cx.simulate_keystrokes("ctrl-/");
        click_on(cx, "shortcut-row-0");
        assert!(has(cx, "shortcuts-dialog"));
        let backdrop = cx.debug_bounds("shortcuts-backdrop").unwrap();
        cx.simulate_click(
            backdrop.bottom_right() - point(px(5.0), px(5.0)),
            Modifiers::none(),
        );
        assert!(!has(cx, "shortcuts-dialog"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn hints_and_cheatsheet_come_from_the_registered_bindings(cx: &mut TestAppContext) {
        let (_view, cx, dir) = setup(cx, "cmd-hints", "- hi\n");
        let table = shortcuts();
        let keymap = cx.update(|_, cx| cx.key_bindings());
        let keymap = keymap.borrow();

        // Each command's hint is its action's first context-free binding in
        // the table, else its first binding; no binding, no hint.
        for c in Command::ALL {
            let action = c.action();
            let mine = |s: &&Shortcut| s.binding.action().partial_eq(action.as_ref());
            let expected = table
                .iter()
                .filter(mine)
                .find(|s| s.binding.predicate().is_none())
                .or_else(|| table.iter().find(mine))
                .map(|s| format_keystrokes(s.binding.keystrokes()));
            assert_eq!(binding_hint(&keymap, action.as_ref()), expected, "{c:?}");
        }
        let hint = |c: Command| binding_hint(&keymap, c.action().as_ref());
        assert_eq!(hint(Command::NewPage).as_deref(), Some("Ctrl+N"));
        assert_eq!(hint(Command::Redo).as_deref(), Some("Ctrl+Shift+Z"));
        assert_eq!(hint(Command::IncreaseFont).as_deref(), Some("Ctrl+="));
        assert_eq!(hint(Command::ShowShortcuts).as_deref(), Some("Ctrl+/"));
        assert_eq!(hint(Command::CycleTask).as_deref(), Some("Ctrl+Enter"));
        assert_eq!(hint(Command::OpenAgenda), None);
        assert_eq!(hint(Command::OpenTrash), None);
        assert_eq!(hint(Command::ExportHtml), None);
        assert_eq!(hint(Command::ToggleGitBackup), None);
        assert_eq!(hint(Command::MoveBlockUp).as_deref(), Some("Alt+Up"));
        assert_eq!(hint(Command::MoveBlockDown).as_deref(), Some("Alt+Down"));
        assert_eq!(hint(Command::Paste).as_deref(), Some("Ctrl+V"));

        // The cheatsheet lists every registered binding.
        let sheet = cheatsheet(&table);
        let listed: Vec<String> = sheet
            .iter()
            .flat_map(|(_, rows)| rows.iter())
            .flat_map(|(keys, _)| keys.split(" / ").map(str::to_string))
            .collect();
        assert_eq!(keymap.bindings().len(), table.len());
        for binding in keymap.bindings() {
            let keys = format_keystrokes(binding.keystrokes());
            assert!(listed.contains(&keys), "{keys} missing from the cheatsheet");
        }
        assert_eq!(
            sheet.iter().map(|(g, _)| *g).collect::<Vec<_>>(),
            KeyGroup::ALL
        );
        drop(keymap);

        // The dialog shows every row.
        let rows: usize = sheet.iter().map(|(_, rows)| rows.len()).sum();
        cx.simulate_keystrokes("ctrl-/");
        assert!(has(cx, &format!("shortcut-row-{}", rows - 1)));
        assert!(!has(cx, &format!("shortcut-row-{rows}")));
        let _ = std::fs::remove_dir_all(dir);
    }

    // --- merge of the blocks and app tracks ---------------------------------

    #[gpui::test]
    fn rename_field_stays_one_line_and_enter_still_confirms(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "merge-rename-keys", &tab_pages(), "Test");
        let test_before = page_file(&dir, "Test");
        let test_before = std::fs::read_to_string(test_before).unwrap();
        // Editing a block first: opening the menu ends that.
        click_block(cx, 0);
        right_click_page(&view, cx, "Alpha");
        click_on(cx, "page-menu-rename");
        cx.simulate_keystrokes("shift-home backspace");
        cx.simulate_input("Gamma");
        cx.write_to_clipboard(gpui::ClipboardItem::new_image(&gpui::Image::from_bytes(
            gpui::ImageFormat::Png,
            crate::assets::tests::tiny_png(),
        )));
        cx.simulate_keystrokes("shift-enter up down alt-up alt-down ctrl-v");
        view.update(cx, |app, _| {
            assert!(app.renaming());
            assert_eq!(app.editing, None);
            assert_eq!(app.active_editor().text, "Gamma");
        });
        assert!(!dir.join("assets").exists(), "no image saved");
        cx.simulate_keystrokes("enter");
        assert!(menu_title(&view, cx).is_none());
        assert!(page_file(&dir, "Gamma").exists() && !page_file(&dir, "Alpha").exists());
        assert_eq!(
            std::fs::read_to_string(page_file(&dir, "Test")).unwrap(),
            test_before,
            "links are not rewritten (v1) and no key reached the block"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn palette_inserts_a_clipboard_image_into_the_block(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "merge-palette-image", "- one\n");
        click_block(cx, 0);
        cx.simulate_keystrokes("end");
        cx.write_to_clipboard(gpui::ClipboardItem::new_image(&gpui::Image::from_bytes(
            gpui::ImageFormat::Png,
            crate::assets::tests::tiny_png(),
        )));
        cx.simulate_keystrokes("ctrl-k");
        cx.simulate_input("insert image");
        cx.simulate_keystrokes("enter");
        let content = view.update(cx, |app, _| {
            assert!(app.search.is_none());
            assert_eq!(app.editing, Some(0));
            app.editor.text.clone()
        });
        assert!(
            content.starts_with("one![image](../assets/image-"),
            "{content}"
        );
        assert_eq!(assets(&dir), vec![referenced_file(&content)]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn a_scheduled_line_typed_with_shift_enter_reaches_the_agenda(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "merge-scheduled", "- TODO pay rent\n");
        click_block(cx, 0);
        cx.simulate_keystrokes("end shift-enter");
        cx.simulate_input("SCHEDULED: <2026-10-09 Fri>");
        cx.simulate_keystrokes("escape");
        assert_eq!(
            file(&dir),
            "- TODO pay rent\n  SCHEDULED: <2026-10-09 Fri>\n"
        );
        let agenda = view.update(cx, |app, _| {
            Agenda::build(
                &app.pages,
                chrono::NaiveDate::from_ymd_opt(2026, 10, 8).unwrap(),
            )
        });
        assert!(agenda.unscheduled.is_empty());
        assert_eq!(agenda.upcoming.len(), 1);
        let (day, items) = &agenda.upcoming[0];
        assert_eq!(*day, chrono::NaiveDate::from_ymd_opt(2026, 10, 9).unwrap());
        assert_eq!(items[0].text, "pay rent");
        let _ = std::fs::remove_dir_all(dir);
    }

    // --- trash ---------------------------------------------------------------

    fn trash_pages() -> [(&'static str, &'static str); 3] {
        [
            ("Test", "- see [[Alpha]]\n"),
            ("Alpha", "- alpha\n"),
            ("Beta", "- TODO beta task zqgone\n  - child\n"),
        ]
    }

    /// Titles in the trash list, newest first.
    fn trash_titles(view: &Entity<NoteSec>, cx: &mut VisualTestContext) -> Vec<String> {
        view.update(cx, |app, _| {
            app.trash.iter().map(|e| e.title.clone()).collect()
        })
    }

    /// Whether the palette finds a page or a block for `query`.
    fn search_finds(view: &Entity<NoteSec>, cx: &mut VisualTestContext, query: &str) -> bool {
        cx.simulate_keystrokes("ctrl-k");
        cx.simulate_input(query);
        let found = view.update(cx, |app, _| {
            app.search_results()
                .iter()
                .any(|h| matches!(h.target, Target::Page(_) | Target::Block(..)))
        });
        cx.simulate_keystrokes("escape");
        found
    }

    #[gpui::test]
    fn deleting_moves_the_page_to_the_trash_and_restore_brings_it_back(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "trash-flow", &trash_pages(), "Test");
        let (graph, _) = open_graph(&view, cx);
        assert!(graph_titles(&graph, cx).contains(&"Beta".to_string()));
        click_sidebar_page(&view, cx, "Beta");
        view.update(cx, |app, cx| app.toggle_favorite("Beta", cx));
        // An unsaved edit is saved before the file moves.
        click_block(cx, 1);
        cx.simulate_input(" edited");
        assert!(search_finds(&view, cx, "zqgone"));
        view.update(cx, |app, _| assert_eq!(app.agenda().unscheduled.len(), 1));
        assert!(!has(cx, "sidebar-trash-count"));

        right_click_page(&view, cx, "Beta");
        click_on(cx, "page-menu-delete");
        assert!(has(cx, "confirm-delete"));
        click_on(cx, "confirm-delete-ok");
        assert!(menu_title(&view, cx).is_none() && !has(cx, "confirm-delete"));

        // The file moved into the trash, under its old relative path.
        assert!(!page_file(&dir, "Beta").exists());
        let entry = view.update(cx, |app, _| app.trash[0].clone());
        assert_eq!(entry.relative, std::path::Path::new("pages/Beta.md"));
        let trashed = dir.join(".trash").join(&entry.id).join("pages/Beta.md");
        assert_eq!(
            std::fs::read_to_string(&trashed).unwrap(),
            "- TODO beta task zqgone\n  - child edited\n"
        );
        // Gone from the list, the tabs, favorites/recent, search, the
        // agenda and the graph.
        assert_eq!(non_journal_titles(&view, cx), ["Alpha", "Test"]);
        tabs_are(&view, cx, &["Test", "Graph"], 1);
        view.update(cx, |app, _| {
            assert!(!app.state.is_favorite("Beta"));
            assert!(!app.state.recent.iter().any(|t| t == "Beta"));
            assert!(app.agenda().is_empty());
        });
        assert!(!search_finds(&view, cx, "zqgone"));
        assert!(!graph_titles(&graph, cx).contains(&"Beta".to_string()));
        assert!(has(cx, "sidebar-trash-count"));

        // The trash tab lists it.
        click_on(cx, "sidebar-trash");
        tabs_are(&view, cx, &["Test", "Graph", "Trash"], 2);
        assert!(has(cx, "trash") && has(cx, "trash-item-0"));
        assert!(!has(cx, "trash-item-1") && !has(cx, "trash-empty-state"));
        assert_eq!(trash_titles(&view, cx), ["Beta"]);

        // Restore: back in the list with its content, trash empty again.
        click_on(cx, "trash-restore-0");
        assert_eq!(non_journal_titles(&view, cx), ["Alpha", "Beta", "Test"]);
        assert_eq!(
            std::fs::read_to_string(page_file(&dir, "Beta")).unwrap(),
            "- TODO beta task zqgone\n  - child edited\n"
        );
        assert!(!dir.join(".trash").join(&entry.id).exists());
        assert!(has(cx, "trash-empty-state") && !has(cx, "trash-item-0"));
        assert!(!has(cx, "sidebar-trash-count") && !has(cx, "trash-error"));
        tabs_are(&view, cx, &["Test", "Graph", "Trash"], 2);
        view.update(cx, |app, _| {
            let beta = &app.pages[app.find_page("Beta").unwrap()];
            assert_eq!(beta.blocks[1].content, "child edited");
            assert_eq!(app.agenda().unscheduled.len(), 1);
            // It comes back like a new page: not a favorite any more.
            assert!(!app.state.is_favorite("Beta"));
        });
        assert!(search_finds(&view, cx, "zqgone"));
        click_on(cx, "tab-1");
        assert!(graph_titles(&graph, cx).contains(&"Beta".to_string()));
        click_sidebar_page(&view, cx, "Beta");
        assert_eq!(selected_title(&view, cx), "Beta");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn restore_is_refused_while_the_name_is_taken(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "trash-clash", &trash_pages(), "Test");
        view.update(cx, |app, cx| app.delete_page("Beta", cx))
            .unwrap();
        // A new page with the same name, in another case.
        view.update(cx, |app, cx| app.navigate("beta", Nav::Tab, cx));
        cx.run_until_parked();
        click_on(cx, "sidebar-trash");
        click_on(cx, "trash-restore-0");
        let error = view.update(cx, |app, _| app.trash_error.clone()).unwrap();
        assert!(
            error.contains("Beta") && error.contains("exists"),
            "{error}"
        );
        assert!(has(cx, "trash-error") && has(cx, "trash-item-0"));
        assert_eq!(non_journal_titles(&view, cx), ["Alpha", "beta", "Test"]);
        assert_eq!(
            std::fs::read_to_string(page_file(&dir, "beta")).unwrap(),
            "- \n"
        );
        assert_eq!(trash_titles(&view, cx), ["Beta"]);

        // Deleting the new one makes room: both are in the trash, newest
        // first, and the old one restores.
        view.update(cx, |app, cx| app.delete_page("beta", cx))
            .unwrap();
        cx.run_until_parked();
        assert_eq!(trash_titles(&view, cx), ["beta", "Beta"]);
        click_on(cx, "trash-restore-1");
        assert!(!has(cx, "trash-error"));
        assert_eq!(non_journal_titles(&view, cx), ["Alpha", "Beta", "Test"]);
        assert_eq!(trash_titles(&view, cx), ["beta"]);
        assert!(std::fs::read_to_string(page_file(&dir, "Beta"))
            .unwrap()
            .contains("zqgone"));

        // Journals: today's comes back on Ctrl-J, so the trashed one waits.
        let today = today_title();
        view.update(cx, |app, cx| app.delete_page(&today, cx))
            .unwrap();
        cx.simulate_keystrokes("ctrl-j");
        assert!(todays_journal_file(&dir).exists());
        click_on(cx, "sidebar-trash");
        assert_eq!(trash_titles(&view, cx), [today.clone(), "beta".to_string()]);
        click_on(cx, "trash-restore-0");
        assert!(has(cx, "trash-error"));
        assert_eq!(trash_titles(&view, cx), [today, "beta".to_string()]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn delete_forever_and_empty_trash_ask_first(cx: &mut TestAppContext) {
        // A graph that already has pages in its trash when the app starts.
        let dir =
            std::env::temp_dir().join(format!("notesec-test-trash-forever-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let storage = Storage::open(dir.clone()).unwrap();
        for (i, title) in ["Old", "Older", "Test"].into_iter().enumerate() {
            let page = Page::from_markdown(title, false, "- text\n");
            storage.save(&page).unwrap();
            if title != "Test" {
                storage.trash_at(&page, 1_000 - i as i64).unwrap();
            }
        }
        cx.update(bind_keys);
        let (view, cx) =
            cx.add_window_view(|window, cx| NoteSec::new(storage, Config::default(), window, cx));
        cx.run_until_parked();
        assert!(has(cx, "sidebar-trash-count"));
        assert_eq!(trash_titles(&view, cx), ["Old", "Older"]);

        // The palette command opens the trash tab.
        run_in_palette(cx, "open trash");
        view.update(cx, |app, _| assert_eq!(app.mode, Mode::Trash));
        assert!(has(cx, "trash-item-1"));

        let ask = |cx: &mut VisualTestContext| {
            click_on(cx, "trash-delete-0");
            assert!(has(cx, "trash-confirm"));
        };
        // Enter doesn't confirm; Esc, Cancel and a backdrop click cancel.
        ask(cx);
        cx.simulate_keystrokes("enter");
        assert!(has(cx, "trash-confirm"));
        // Modal: the tab keys do nothing.
        cx.simulate_keystrokes("ctrl-w");
        view.update(cx, |app, _| assert_eq!(app.mode, Mode::Trash));
        cx.simulate_keystrokes("escape");
        assert!(!has(cx, "trash-confirm"));
        ask(cx);
        click_on(cx, "trash-confirm-cancel");
        assert!(!has(cx, "trash-confirm"));
        ask(cx);
        let dialog = cx.debug_bounds("trash-confirm").unwrap();
        cx.simulate_click(dialog.origin - point(px(20.), px(20.)), Modifiers::none());
        assert!(!has(cx, "trash-confirm"));
        assert_eq!(trash_titles(&view, cx), ["Old", "Older"]);
        assert!(dir.join(".trash/1000/pages/Old.md").exists());

        // Confirmed: that entry is gone for good, the other stays.
        ask(cx);
        click_on(cx, "trash-confirm-ok");
        assert!(!has(cx, "trash-confirm"));
        assert_eq!(trash_titles(&view, cx), ["Older"]);
        assert!(!dir.join(".trash/1000").exists());
        assert!(dir.join(".trash/999/pages/Older.md").exists());
        assert!(!has(cx, "trash-item-1"));

        // Empty trash asks too; cancelling keeps everything.
        click_on(cx, "trash-empty");
        assert!(has(cx, "trash-confirm"));
        cx.simulate_keystrokes("escape");
        assert_eq!(trash_titles(&view, cx), ["Older"]);
        click_on(cx, "trash-empty");
        click_on(cx, "trash-confirm-ok");
        assert!(view.update(cx, |app, _| app.trash.is_empty()));
        assert!(Storage::open(dir.clone()).unwrap().list_trash().is_empty());
        assert!(has(cx, "trash-empty-state") && !has(cx, "sidebar-trash-count"));
        // Nothing left to empty: the button does nothing.
        click_on(cx, "trash-empty");
        assert!(!has(cx, "trash-confirm"));
        // The loaded pages were never touched.
        assert!(page_file(&dir, "Test").exists());
        assert_eq!(non_journal_titles(&view, cx), ["Test"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn leaving_the_trash_tab_closes_its_confirmation(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "trash-leave", &trash_pages(), "Test");
        view.update(cx, |app, cx| app.delete_page("Alpha", cx))
            .unwrap();
        view.update(cx, |app, cx| app.show_trash(cx));
        cx.run_until_parked();
        click_on(cx, "trash-delete-0");
        assert!(has(cx, "trash-confirm"));
        // Ctrl-J (global) shows a page; the question goes with the tab.
        cx.simulate_keystrokes("ctrl-j");
        assert!(!has(cx, "trash-confirm"));
        view.update(cx, |app, _| assert!(app.trash_confirm.is_none()));
        // Ctrl-K closes it too.
        view.update(cx, |app, cx| app.show_trash(cx));
        cx.run_until_parked();
        click_on(cx, "trash-delete-0");
        cx.simulate_keystrokes("ctrl-k");
        assert!(!has(cx, "trash-confirm"));
        cx.simulate_keystrokes("escape");
        assert_eq!(trash_titles(&view, cx), ["Alpha"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn deleted_times_read_relative_to_today() {
        use chrono::TimeZone;
        let at = chrono::Local
            .with_ymd_and_hms(2026, 10, 9, 14, 5, 0)
            .single()
            .unwrap();
        let millis = at.timestamp_millis();
        let later = |d: u32, h: u32| {
            chrono::Local
                .with_ymd_and_hms(2026, 10, d, h, 0, 0)
                .single()
                .unwrap()
        };
        assert_eq!(deleted_label(millis, later(9, 23)), "Deleted today 14:05");
        assert_eq!(
            deleted_label(millis, later(10, 1)),
            "Deleted yesterday 14:05"
        );
        assert_eq!(
            deleted_label(millis, later(12, 9)),
            "Deleted 2026-10-09 14:05"
        );
    }

    // --- export --------------------------------------------------------------

    const EXPORT_REF: &str = "6f1c2a3b-0000-4000-8000-00000000abcd";

    fn export_pages() -> [(&'static str, &'static str); 2] {
        [
            (
                "Test",
                "- TODO see [[Alpha]] & <b>\n  - hidden child ((6f1c2a3b-0000-4000-8000-00000000abcd))\n- ![pic](../assets/p.png)\n",
            ),
            ("Alpha", "- quoted text\n  id:: 6f1c2a3b-0000-4000-8000-00000000abcd\n"),
        ]
    }

    fn status_text(view: &Entity<NoteSec>, cx: &mut VisualTestContext) -> Option<String> {
        view.update(cx, |app, _| app.status.as_ref().map(|s| s.text.clone()))
    }

    #[gpui::test]
    fn export_writes_the_page_as_html_and_says_where(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "export", &export_pages(), "Test");
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        std::fs::write(dir.join("assets/p.png"), b"\x89PNG\r\n\x1a\nfake").unwrap();
        // A folded block is exported unfolded.
        click_on(cx, "fold-0");
        assert!(!has(cx, "block-1"));
        assert!(!has(cx, "status-toast"));

        run_in_palette(cx, "export page to html");
        let path = dir.join("exports/Test.html");
        let html = std::fs::read_to_string(&path).unwrap();
        assert!(html.starts_with("<!DOCTYPE html>"));
        assert!(html.contains("<h1 class=\"page-title\">Test</h1>"));
        assert!(html.contains("<span class=\"task task-todo\">TODO</span>"));
        assert!(html.contains("[[Alpha]]</span> &amp; &lt;b&gt;"));
        assert!(html.contains("hidden child"));
        // The block reference shows the referenced block's text.
        assert!(html.contains("<span class=\"ref\">quoted text</span>"));
        assert!(!html.contains(EXPORT_REF));
        // The image is embedded.
        assert!(html.contains("src=\"data:image/png;base64,"));
        assert!(!html.contains("p.png\""));

        // The status says where, then goes away.
        assert!(has(cx, "status-toast"));
        let expected = format!("Exported to {}", path.display());
        assert_eq!(status_text(&view, cx), Some(expected));
        cx.executor().advance_clock(STATUS_FOR);
        cx.run_until_parked();
        assert!(!has(cx, "status-toast") && status_text(&view, cx).is_none());

        // Exporting again (after an edit) replaces the file; the export is
        // never loaded as a page and the page file is untouched.
        click_on(cx, "fold-0");
        click_block(cx, 1);
        cx.simulate_input(" zqnew");
        run_in_palette(cx, "export page to html");
        let html = std::fs::read_to_string(&path).unwrap();
        assert!(html.contains("zqnew"));
        assert_eq!(std::fs::read_dir(dir.join("exports")).unwrap().count(), 1);
        assert!(file(&dir).contains("zqnew"));
        assert!(!non_journal_titles(&view, cx)
            .iter()
            .any(|t| t.contains("html")));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn export_is_offered_only_with_a_page_on_screen(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "export-hidden", &export_pages(), "Test");
        let offered = view.update(cx, |app, _| app.available_commands());
        assert!(offered.contains(&Command::ExportHtml));
        // The journal on screen exports under its file name.
        view.update(cx, |app, cx| app.open_today(cx));
        cx.run_until_parked();
        run_in_palette(cx, "export page to html");
        let journal = todays_journal_file(&dir).with_extension("html");
        let name = journal.file_name().unwrap();
        assert!(dir.join("exports").join(name).exists());

        // On the graph or trash tab there is no page: no command, and the
        // action does nothing.
        for tab in ["graph", "trash"] {
            match tab {
                "graph" => cx.simulate_keystrokes("ctrl-g"),
                _ => click_on(cx, "sidebar-trash"),
            }
            let offered = view.update(cx, |app, _| app.available_commands());
            assert!(!offered.contains(&Command::ExportHtml), "{tab}");
            cx.simulate_keystrokes("ctrl-k");
            cx.simulate_input("export page to html");
            assert!(!has(cx, "command-ExportHtml"));
            cx.simulate_keystrokes("escape");
            cx.dispatch_action(ExportHtml);
            assert_eq!(std::fs::read_dir(dir.join("exports")).unwrap().count(), 1);
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    // --- git auto-backup --------------------------------------------------------

    /// Commit subjects in `dir`'s repository, newest first (none without one).
    fn git_log(dir: &std::path::Path) -> Vec<String> {
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["log", "--format=%s"])
            .env("GIT_CEILING_DIRECTORIES", std::env::temp_dir())
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| {
                String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// `setup`, with git run like `backup`'s own tests (no user config).
    fn setup_backup<'a>(
        cx: &'a mut TestAppContext,
        name: &str,
    ) -> (Entity<NoteSec>, &'a mut VisualTestContext, PathBuf) {
        let (view, cx, dir) = setup(cx, name, "- one\n- two\n");
        view.update(cx, |app, _| app.git = backup::isolated_git());
        (view, cx, dir)
    }

    /// Edit block `ix` and leave it, which saves the page.
    fn edit_and_leave(cx: &mut VisualTestContext, ix: usize, text: &str) {
        click_block(cx, ix);
        cx.simulate_input(text);
        cx.simulate_keystrokes("escape");
    }

    fn wait(cx: &mut VisualTestContext, secs: u64) {
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(secs));
        cx.run_until_parked();
    }

    #[gpui::test]
    fn the_settings_toggle_turns_git_backup_on_and_off_and_persists(cx: &mut TestAppContext) {
        if !backup::git_available() {
            return;
        }
        let (view, cx, dir) = setup_backup(cx, "backup-settings");
        assert!(!saved_config(&dir).git_backup, "off by default");
        click_on(cx, "settings-gear");
        assert!(has(cx, "git-backup-off") && has(cx, "git-backup-note"));

        click_on(cx, "git-backup-on");
        cx.run_until_parked();
        assert!(saved_config(&dir).git_backup);
        assert!(config_text(&dir).contains("git_backup = true"));
        // A repository with the .gitignore, and everything committed.
        assert!(dir.join(".git").is_dir());
        let ignore = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
        assert!(ignore.contains(".trash/") && ignore.contains("exports/"));
        let log = git_log(&dir);
        assert_eq!(log.len(), 1);
        assert!(log[0].starts_with("notesec autosave"), "{log:?}");
        let expected = format!("Git backup on: new repository in {}", dir.display());
        assert_eq!(status_text(&view, cx), Some(expected));

        click_on(cx, "git-backup-off");
        assert!(!saved_config(&dir).git_backup);
        assert_eq!(status_text(&view, cx).as_deref(), Some("Git backup off"));
        // Off: changes are not committed.
        cx.simulate_keystrokes("escape");
        edit_and_leave(cx, 0, " zqoff");
        wait(cx, 30);
        assert!(file(&dir).contains("zqoff"));
        assert_eq!(git_log(&dir).len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn saves_are_committed_after_a_quiet_spell(cx: &mut TestAppContext) {
        if !backup::git_available() {
            return;
        }
        let (view, cx, dir) = setup_backup(cx, "backup-debounce");
        run_in_palette(cx, "toggle git auto-backup");
        cx.run_until_parked();
        assert!(view.update(cx, |app, _| app.config.git_backup));
        assert_eq!(git_log(&dir).len(), 1);

        // Each save restarts the wait.
        edit_and_leave(cx, 0, " first");
        wait(cx, 4);
        assert_eq!(git_log(&dir).len(), 1);
        edit_and_leave(cx, 1, " second");
        wait(cx, 4);
        assert_eq!(git_log(&dir).len(), 1, "4 s after the last save");
        wait(cx, 1);
        let log = git_log(&dir);
        assert_eq!(log.len(), 2);
        assert_eq!(log[0], "notesec autosave: 1 file changed");
        assert!(!view.update(cx, |app, _| app.backup_pending));

        // Nothing changed: no empty commit, however long it waits.
        wait(cx, 60);
        assert_eq!(git_log(&dir).len(), 2);
        // A rename (the old file goes, the new one comes) is one commit.
        view.update(cx, |app, cx| {
            app.rename_page("Test", "Renamed", cx).unwrap()
        });
        cx.run_until_parked();
        wait(cx, 5);
        assert_eq!(git_log(&dir)[0], "notesec autosave: 2 files changed");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn quitting_commits_what_waits_for_the_timer(cx: &mut TestAppContext) {
        if !backup::git_available() {
            return;
        }
        let (view, cx, dir) = setup_backup(cx, "backup-quit");
        view.update(cx, |app, cx| app.set_git_backup(true, cx));
        cx.run_until_parked();
        edit_and_leave(cx, 0, " zqquit");
        assert!(view.update(cx, |app, _| app.backup_pending));
        assert_eq!(git_log(&dir).len(), 1);
        cx.cx.quit();
        assert_eq!(git_log(&dir).len(), 2, "committed at quit, without waiting");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn closing_the_window_commits_what_waits_for_the_timer(cx: &mut TestAppContext) {
        if !backup::git_available() {
            return;
        }
        let (view, cx, dir) = setup_backup(cx, "backup-close");
        view.update(cx, |app, cx| app.set_git_backup(true, cx));
        cx.run_until_parked();
        edit_and_leave(cx, 0, " zqclose");
        assert_eq!(git_log(&dir).len(), 1);
        // The window owns the view: closing it releases the view.
        drop(view);
        cx.update(|window, _| window.remove_window());
        cx.run_until_parked();
        assert_eq!(git_log(&dir).len(), 2, "committed when the view went");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn backup_on_at_startup_commits_what_changed_meanwhile(cx: &mut TestAppContext) {
        if !backup::git_available() {
            return;
        }
        let dir =
            std::env::temp_dir().join(format!("notesec-test-backup-start-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let storage = Storage::open(dir.clone()).unwrap();
        std::fs::write(dir.join("pages/Outside.md"), "- edited elsewhere\n").unwrap();
        let config = Config {
            git_backup: true,
            ..Config::default()
        };
        cx.update(bind_keys);
        let (view, cx) = cx.add_window_view(|window, cx| NoteSec::new(storage, config, window, cx));
        view.update(cx, |app, _| app.git = backup::isolated_git());
        cx.run_until_parked();
        assert_eq!(git_log(&dir).len(), 1);
        assert!(dir.join(".gitignore").exists());
        // Silent when it works: no status at startup.
        assert_eq!(status_text(&view, cx), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn without_git_backup_stays_off_and_says_why(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "backup-nogit", "- one\n");
        view.update(cx, |app, _| {
            app.git = backup::Git::default().with_program("notesec-no-such-git")
        });
        click_on(cx, "settings-gear");
        click_on(cx, "git-backup-on");
        cx.run_until_parked();
        view.update(cx, |app, _| {
            assert!(!app.config.git_backup);
            assert!(app.status.as_ref().is_some_and(|s| s.error));
        });
        assert_eq!(
            status_text(&view, cx).as_deref(),
            Some("Git backup is off: git is not installed")
        );
        assert!(has(cx, "status-toast") && settings_open(&view, cx));
        assert!(!saved_config(&dir).git_backup);
        assert!(!dir.join(".git").exists());
        // The app keeps working.
        cx.simulate_keystrokes("escape");
        edit_and_leave(cx, 0, " still");
        assert!(file(&dir).contains("still"));
        let _ = std::fs::remove_dir_all(dir);
    }

    // --- split panes (decision 40) -------------------------------------------

    fn split_of(view: &Entity<NoteSec>, cx: &mut VisualTestContext) -> Option<Split> {
        view.update(cx, |app, _| app.split.clone())
    }

    fn split_is(right: &str, right_focused: bool) -> Option<Split> {
        Some(Split {
            right: right.to_string(),
            right_focused,
        })
    }

    /// Whether element `selector` is drawn inside pane `side` ("left" or
    /// "right").
    fn in_pane(cx: &mut VisualTestContext, selector: &str, side: &str) -> bool {
        let selector: &'static str = Box::leak(selector.to_string().into_boxed_str());
        let pane: &'static str = Box::leak(format!("pane-{side}").into_boxed_str());
        let element = cx.debug_bounds(selector).expect(selector);
        let pane = cx.debug_bounds(pane).expect(pane);
        pane.contains(&element.center())
    }

    /// The text the unfocused pane shows for reading-view row `row`.
    fn other_text(
        view: &Entity<NoteSec>,
        cx: &mut VisualTestContext,
        row: usize,
    ) -> Option<String> {
        view.update(cx, |app, _| {
            app.other_layouts
                .iter()
                .find(|(r, _)| *r == row)
                .map(|(_, l)| l.text())
        })
    }

    fn tab_labels(
        view: &Entity<NoteSec>,
        cx: &mut VisualTestContext,
    ) -> (Vec<String>, Option<usize>) {
        tab_state(view, cx)
    }

    fn labels(l: &[&str]) -> Vec<String> {
        l.iter().map(|s| s.to_string()).collect()
    }

    #[gpui::test]
    fn split_right_key_opens_the_current_page_on_the_right_and_focuses_it(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "split-open", &tab_pages(), "Test");
        assert!(has(cx, "block-0"));
        for selector in [
            "pane-right",
            "pane-focus-left",
            "pane-close-left",
            "pane-close-right",
        ] {
            assert!(!has(cx, selector), "{selector} before splitting");
        }

        cx.simulate_keystrokes("ctrl-\\");
        assert_eq!(split_of(&view, cx), split_is("Test", true));
        for selector in [
            "pane-left",
            "pane-right",
            "pane-focus-right",
            "pane-close-left",
        ] {
            assert!(has(cx, selector), "{selector}");
        }
        assert!(has(cx, "pane-close-right") && has(cx, "pane-right-title"));
        assert!(!has(cx, "pane-focus-left"));
        // The focused (right) pane has the plain selectors, the other pane
        // the `other-` ones; both show the page.
        assert!(in_pane(cx, "block-0", "right"));
        assert!(in_pane(cx, "other-block-0", "left"));
        assert!(in_pane(cx, "tab-0", "left"));
        assert_eq!(reading_text(&view, cx, 0).as_deref(), Some("see [[Alpha]]"));
        assert_eq!(other_text(&view, cx, 0).as_deref(), Some("see [[Alpha]]"));
        // The tab bar (the left pane's) is unchanged.
        tabs_are(&view, cx, &["Test"], 0);

        // Never a third pane: splitting again only focuses the right pane.
        cx.simulate_keystrokes("ctrl-|");
        assert_eq!(split_of(&view, cx), split_is("Test", false));
        cx.simulate_keystrokes("ctrl-\\");
        assert_eq!(split_of(&view, cx), split_is("Test", true));

        // From the graph tab, the page last shown opens on the right.
        cx.simulate_keystrokes("ctrl-shift-w ctrl-g");
        assert_eq!(split_of(&view, cx), None);
        view.update(cx, |app, _| assert_eq!(app.mode, Mode::Graph));
        cx.simulate_keystrokes("ctrl-\\");
        assert_eq!(split_of(&view, cx), split_is("Test", true));
        view.update(cx, |app, _| {
            assert_eq!(app.mode, Mode::Notes);
            assert_eq!(app.tabs.active_target(), Some(&TabTarget::Graph));
        });
        assert!(in_pane(cx, "block-0", "right"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn navigation_goes_to_the_focused_pane(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "split-nav", &tab_pages(), "Test");
        cx.simulate_keystrokes("ctrl-\\");

        // Right pane focused: the sidebar, links and the palette replace its
        // page, and the tabs stay as they are.
        click_sidebar_page(&view, cx, "Alpha");
        assert_eq!(split_of(&view, cx), split_is("Alpha", true));
        assert_eq!(tab_labels(&view, cx), (labels(&["Test"]), Some(0)));
        assert_eq!(selected_title(&view, cx), "Alpha");
        assert_eq!(reading_text(&view, cx, 0).as_deref(), Some("alpha"));
        assert_eq!(other_text(&view, cx, 0).as_deref(), Some("see [[Alpha]]"));
        assert!(in_pane(cx, "block-0", "right"));
        view.update(cx, |app, cx| app.open_page("Beta", cx));
        assert_eq!(split_of(&view, cx), split_is("Beta", true));
        assert_eq!(tab_labels(&view, cx), (labels(&["Test"]), Some(0)));
        run_in_palette(cx, "Alpha");
        assert_eq!(split_of(&view, cx), split_is("Alpha", true));
        assert_eq!(tab_labels(&view, cx), (labels(&["Test"]), Some(0)));
        // A link in the left (unfocused) pane: the press focuses the left
        // pane, then the link opens there, in the tab bar.
        let link = view.update(cx, |app, _| {
            let (_, layout) = app.other_layouts.iter().find(|(r, _)| *r == 0).unwrap();
            let p = layout.position_for_index(7).unwrap();
            point(p.x + px(1.), p.y + layout.line_height() / 2.)
        });
        cx.simulate_click(link, Modifiers::none());
        assert_eq!(split_of(&view, cx), split_is("Alpha", false));
        tabs_are(&view, cx, &["Alpha"], 0);

        // Left pane focused: navigation uses the tabs as before.
        click_sidebar_page(&view, cx, "Beta");
        tabs_are(&view, cx, &["Alpha", "Beta"], 1);
        assert_eq!(split_of(&view, cx), split_is("Alpha", false));
        assert!(in_pane(cx, "block-0", "left"));
        assert_eq!(other_text(&view, cx, 0).as_deref(), Some("alpha"));

        // Tab actions belong to the left pane and focus it.
        cx.simulate_keystrokes("ctrl-\\");
        assert_eq!(split_of(&view, cx), split_is("Alpha", true));
        click_on(cx, "tab-0");
        assert_eq!(split_of(&view, cx), split_is("Alpha", false));
        tabs_are(&view, cx, &["Alpha", "Beta"], 0);
        cx.simulate_keystrokes("ctrl-\\ ctrl-tab");
        assert_eq!(split_of(&view, cx), split_is("Alpha", false));
        tabs_are(&view, cx, &["Alpha", "Beta"], 1);
        // Agenda, trash and graph open as left tabs, and the right pane
        // keeps its page.
        for (query, mode) in [
            ("open agenda", Mode::Agenda),
            ("open trash", Mode::Trash),
            ("toggle graph view", Mode::Graph),
        ] {
            cx.simulate_keystrokes("ctrl-\\");
            run_in_palette(cx, query);
            assert_eq!(split_of(&view, cx), split_is("Alpha", false), "{query}");
            view.update(cx, |app, _| assert_eq!(app.mode, mode));
            assert_eq!(other_text(&view, cx, 0).as_deref(), Some("alpha"));
        }
        // A press in the right pane focuses it again, the graph stays left.
        click_on(cx, "pane-right-title");
        assert_eq!(split_of(&view, cx), split_is("Alpha", true));
        view.update(cx, |app, _| {
            assert_eq!(app.mode, Mode::Notes);
            assert_eq!(app.tabs.active_target(), Some(&TabTarget::Graph));
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn focus_other_pane_moves_the_indicator_and_the_editor(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "split-focus", &tab_pages(), "Test");
        cx.simulate_keystrokes("ctrl-\\");
        click_sidebar_page(&view, cx, "Alpha");
        click_block(cx, 0);
        cx.simulate_input(" one");
        view.update(cx, |app, _| assert_eq!(app.editing, Some(0)));

        // The key: the edit is saved, editing stops, the indicator moves.
        cx.simulate_keystrokes("ctrl-|");
        assert_eq!(split_of(&view, cx), split_is("Alpha", false));
        view.update(cx, |app, _| assert_eq!(app.editing, None));
        assert_eq!(
            std::fs::read_to_string(page_file(&dir, "Alpha")).unwrap(),
            "- alpha one\n"
        );
        assert!(has(cx, "pane-focus-left") && !has(cx, "pane-focus-right"));
        assert_eq!(selected_title(&view, cx), "Test");
        assert!(in_pane(cx, "block-0", "left") && in_pane(cx, "other-block-0", "right"));

        // A click on a block in the other pane focuses it and edits there.
        click_on(cx, "other-block-0");
        assert_eq!(split_of(&view, cx), split_is("Alpha", true));
        assert!(has(cx, "pane-focus-right") && !has(cx, "pane-focus-left"));
        view.update(cx, |app, _| {
            assert_eq!(app.editing, Some(0));
            assert_eq!(app.pages[app.selected].title, "Alpha");
        });
        cx.simulate_input("!");
        click_on(cx, "other-block-0");
        assert_eq!(split_of(&view, cx), split_is("Alpha", false));
        view.update(cx, |app, _| {
            assert_eq!(app.editing, Some(0));
            assert_eq!(app.pages[app.selected].title, "Test");
        });
        assert_eq!(
            std::fs::read_to_string(page_file(&dir, "Alpha")).unwrap(),
            "- alpha one!\n"
        );
        // Back with the key; then with the palette command.
        cx.simulate_keystrokes("escape ctrl-|");
        assert_eq!(split_of(&view, cx), split_is("Alpha", true));
        run_in_palette(cx, "focus other pane");
        assert_eq!(split_of(&view, cx), split_is("Alpha", false));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn close_pane_closes_the_focused_pane_either_side(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "split-close", &tab_pages(), "Test");
        // Not split: the pane keys do nothing.
        cx.simulate_keystrokes("ctrl-shift-w ctrl-|");
        assert_eq!(split_of(&view, cx), None);
        tabs_are(&view, cx, &["Test"], 0);

        // The right pane, with the key: the tabs are as they were.
        cx.simulate_keystrokes("ctrl-\\");
        click_sidebar_page(&view, cx, "Alpha");
        cx.simulate_keystrokes("ctrl-shift-w");
        assert_eq!(split_of(&view, cx), None);
        tabs_are(&view, cx, &["Test"], 0);
        for selector in [
            "pane-right",
            "pane-focus-left",
            "pane-close-left",
            "other-block-0",
        ] {
            assert!(!has(cx, selector), "{selector}");
        }

        // The left pane, with the key: the right page moves into the tabs.
        cx.simulate_keystrokes("ctrl-\\");
        click_sidebar_page(&view, cx, "Alpha");
        cx.simulate_keystrokes("ctrl-| ctrl-shift-w");
        assert_eq!(split_of(&view, cx), None);
        tabs_are(&view, cx, &["Test", "Alpha"], 1);

        // Ctrl+W in the right pane closes the pane, not a tab.
        cx.simulate_keystrokes("ctrl-\\ ctrl-w");
        assert_eq!(split_of(&view, cx), None);
        tabs_are(&view, cx, &["Test", "Alpha"], 1);

        // The ×s: the left one while the right pane has focus (its page
        // already has a tab, which gets focus), and the right one.
        cx.simulate_keystrokes("ctrl-\\");
        click_sidebar_page(&view, cx, "Beta");
        click_on(cx, "pane-close-left");
        assert_eq!(split_of(&view, cx), None);
        tabs_are(&view, cx, &["Test", "Alpha", "Beta"], 2);
        cx.simulate_keystrokes("ctrl-\\ ctrl-|");
        click_on(cx, "pane-close-right");
        assert_eq!(split_of(&view, cx), None);
        tabs_are(&view, cx, &["Test", "Alpha", "Beta"], 2);
        // The palette command.
        cx.simulate_keystrokes("ctrl-\\");
        run_in_palette(cx, "close pane");
        assert_eq!(split_of(&view, cx), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn an_edit_in_one_pane_shows_in_the_other(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "split-edit", "- hello\n- second\n");
        cx.simulate_keystrokes("ctrl-\\");
        click_block(cx, 0);
        cx.simulate_input(" world");
        // Live, while typing (the same page on both sides).
        assert_eq!(other_text(&view, cx, 0).as_deref(), Some("hello world"));
        cx.simulate_keystrokes("enter");
        cx.simulate_input("new");
        cx.simulate_keystrokes("escape");
        assert_eq!(other_text(&view, cx, 0).as_deref(), Some("hello world"));
        assert_eq!(other_text(&view, cx, 1).as_deref(), Some("new"));
        assert_eq!(other_text(&view, cx, 2).as_deref(), Some("second"));
        assert!(has(cx, "other-block-2"));
        // And back the other way.
        cx.simulate_keystrokes("ctrl-|");
        click_block(cx, 2);
        cx.simulate_input(" too");
        cx.simulate_keystrokes("escape");
        assert_eq!(other_text(&view, cx, 2).as_deref(), Some("second too"));
        assert_eq!(file(&dir), "- hello world\n- new\n- second too\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn renaming_sorting_and_trashing_the_right_page(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "split-pages", &tab_pages(), "Test");
        cx.simulate_keystrokes("ctrl-\\");
        click_sidebar_page(&view, cx, "Alpha");
        cx.simulate_keystrokes("ctrl-|");

        // Rename (from the sidebar's page menu): the pane follows by title.
        right_click_page(&view, cx, "Alpha");
        click_on(cx, "page-menu-rename");
        cx.simulate_input("Gamma");
        cx.simulate_keystrokes("enter");
        assert_eq!(split_of(&view, cx), split_is("Gamma", false));
        assert_eq!(other_text(&view, cx, 0).as_deref(), Some("alpha"));
        assert_eq!(selected_title(&view, cx), "Test");

        // A new order moves the pages around; the pane keeps its page.
        view.update(cx, |app, cx| {
            app.state.page_order = labels(&["Test", "Gamma", "Beta"]);
            app.sort_pages();
            cx.notify();
        });
        assert_eq!(non_journal_titles(&view, cx), ["Test", "Gamma", "Beta"]);
        cx.simulate_keystrokes("ctrl-|");
        assert_eq!(selected_title(&view, cx), "Gamma");
        run_in_palette(cx, "sort pages");
        assert_eq!(non_journal_titles(&view, cx), ["Beta", "Gamma", "Test"]);
        assert_eq!(split_of(&view, cx), split_is("Gamma", true));
        assert_eq!(selected_title(&view, cx), "Gamma");
        assert_eq!(reading_text(&view, cx, 0).as_deref(), Some("alpha"));
        assert_eq!(other_text(&view, cx, 0).as_deref(), Some("see [[Alpha]]"));

        // Trashing it from the left pane closes the right pane.
        cx.simulate_keystrokes("ctrl-|");
        right_click_page(&view, cx, "Gamma");
        click_on(cx, "page-menu-delete");
        click_on(cx, "confirm-delete-ok");
        assert_eq!(split_of(&view, cx), None);
        tabs_are(&view, cx, &["Test"], 0);
        assert!(!has(cx, "pane-right") && !has(cx, "other-block-0"));
        assert!(!page_file(&dir, "Gamma").exists());

        // Trashing it while it has focus: the left pane takes over.
        cx.simulate_keystrokes("ctrl-\\");
        click_sidebar_page(&view, cx, "Beta");
        run_in_palette(cx, "delete current page");
        assert_eq!(menu_title(&view, cx).as_deref(), Some("Beta"));
        click_on(cx, "confirm-delete-ok");
        assert_eq!(split_of(&view, cx), None);
        tabs_are(&view, cx, &["Test"], 0);
        assert!(!page_file(&dir, "Beta").exists());
        // Restoring it from the trash brings the page back, not the pane.
        run_in_palette(cx, "open trash");
        let id = view.update(cx, |app, _| app.trash[0].id.clone());
        view.update(cx, |app, cx| app.restore_from_trash(&id, cx));
        assert!(page_file(&dir, "Beta").exists());
        assert_eq!(split_of(&view, cx), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn current_page_commands_target_the_focused_pane(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "split-cmds", &command_pages(), "Beta");
        cx.simulate_keystrokes("ctrl-\\");
        click_sidebar_page(&view, cx, "Test");
        // Right: Test (with children), left: Beta.
        run_in_palette(cx, "collapse all");
        assert_eq!(shown(cx, 5), vec![0, 2]);
        run_in_palette(cx, "expand all");
        assert_eq!(shown(cx, 5), vec![0, 1, 2, 3, 4]);
        run_in_palette(cx, "toggle favorite");
        assert_eq!(UiState::load(&dir).favorites, labels(&["Test"]));
        run_in_palette(cx, "copy page title");
        let clip = |cx: &mut VisualTestContext| {
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .unwrap_or_default()
        };
        assert_eq!(clip(cx), "Test");
        run_in_palette(cx, "export page to html");
        assert!(dir.join("exports/Test.html").exists());
        assert!(!dir.join("exports/Beta.html").exists());
        run_in_palette(cx, "rename current page");
        assert_eq!(menu_title(&view, cx).as_deref(), Some("Test"));
        cx.simulate_keystrokes("escape");

        // The left pane.
        cx.simulate_keystrokes("ctrl-|");
        run_in_palette(cx, "copy page title");
        assert_eq!(clip(cx), "Beta");
        run_in_palette(cx, "toggle favorite");
        assert_eq!(UiState::load(&dir).favorites, labels(&["Test", "Beta"]));
        run_in_palette(cx, "export page to html");
        assert!(dir.join("exports/Beta.html").exists());
        run_in_palette(cx, "delete current page");
        assert_eq!(menu_title(&view, cx).as_deref(), Some("Beta"));
        cx.simulate_keystrokes("escape");
        // The local graph is the focused page's.
        cx.simulate_keystrokes("ctrl-|");
        run_in_palette(cx, "toggle local graph");
        assert_eq!(split_of(&view, cx), split_is("Test", false));
        view.update(cx, |app, _| {
            assert_eq!(app.mode, Mode::Graph);
            assert_eq!(app.pages[app.selected].title, "Test");
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn pane_commands_are_offered_only_while_split(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "split-palette", &tab_pages(), "Test");
        let offered = |view: &Entity<NoteSec>, cx: &mut VisualTestContext| {
            view.update(cx, |app, _| app.available_commands())
        };
        let list = offered(&view, cx);
        assert!(list.contains(&Command::SplitRight));
        assert!(!list.contains(&Command::ClosePane));
        assert!(!list.contains(&Command::FocusOtherPane));
        cx.simulate_keystrokes("ctrl-k");
        cx.simulate_input("pane");
        assert!(!has(cx, "command-ClosePane") && !has(cx, "command-FocusOtherPane"));
        cx.simulate_keystrokes("escape");
        // Dispatched anyway (no key, no palette row): nothing happens.
        cx.dispatch_action(ClosePane);
        cx.dispatch_action(FocusOtherPane);
        assert_eq!(split_of(&view, cx), None);
        tabs_are(&view, cx, &["Test"], 0);

        run_in_palette(cx, "split right");
        assert_eq!(split_of(&view, cx), split_is("Test", true));
        let list = offered(&view, cx);
        assert!(list.contains(&Command::ClosePane) && list.contains(&Command::FocusOtherPane));
        cx.simulate_keystrokes("ctrl-k");
        cx.simulate_input("pane");
        assert!(has(cx, "command-ClosePane") && has(cx, "command-FocusOtherPane"));
        // The pane keys don't act under the palette.
        cx.simulate_keystrokes("ctrl-|");
        assert_eq!(split_of(&view, cx), split_is("Test", true));
        cx.simulate_keystrokes("escape");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn pane_keys_are_bound_as_linux_reports_them(cx: &mut TestAppContext) {
        let (_view, cx, dir) = setup(cx, "split-keys", "- hi\n");
        let keymap = cx.update(|_, cx| cx.key_bindings());
        let keymap = keymap.borrow();
        let hint = |c: Command| binding_hint(&keymap, c.action().as_ref());
        assert_eq!(hint(Command::SplitRight).as_deref(), Some("Ctrl+\\"));
        assert_eq!(hint(Command::ClosePane).as_deref(), Some("Ctrl+Shift+W"));
        assert_eq!(hint(Command::FocusOtherPane).as_deref(), Some("Ctrl+|"));
        // Ctrl+Shift+W is its own binding, apart from Ctrl+W.
        let ctrl_shift_w = gpui::Keystroke::parse("ctrl-shift-w").unwrap();
        let ctrl_w = gpui::Keystroke::parse("ctrl-w").unwrap();
        for binding in keymap.bindings() {
            if binding.keystrokes().len() != 1 {
                continue;
            }
            let action = binding.action();
            if ctrl_shift_w.should_match(&binding.keystrokes()[0]) {
                assert!(action.partial_eq(&ClosePane), "{}", action.name());
            }
            if ctrl_w.should_match(&binding.keystrokes()[0]) {
                assert!(action.partial_eq(&CloseTab), "{}", action.name());
            }
        }
        drop(keymap);
        let _ = std::fs::remove_dir_all(dir);
    }

    // --- custom hotkeys (decision 41) -----------------------------------------

    /// Like `setup`, with `state_toml` as the graph's state.toml and
    /// `config`, both in place before the window opens.
    fn setup_with_state<'a>(
        cx: &'a mut TestAppContext,
        name: &str,
        state_toml: &str,
        config: Config,
    ) -> (Entity<NoteSec>, &'a mut VisualTestContext, PathBuf) {
        let dir = std::env::temp_dir().join(format!("notesec-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let storage = Storage::open(dir.clone()).unwrap();
        std::fs::write(dir.join("pages/Test.md"), "- hello\n").unwrap();
        std::fs::write(UiState::path(&dir), state_toml).unwrap();
        cx.update(bind_keys);
        let (view, cx) = cx.add_window_view(|window, cx| NoteSec::new(storage, config, window, cx));
        view.update(cx, |app, cx| {
            app.selected = app.find_page("Test").unwrap();
            app.tabs = Tabs::new(TabTarget::Page("Test".to_string()));
            cx.notify();
        });
        cx.run_until_parked();
        (view, cx, dir)
    }

    fn open_hotkeys(view: &Entity<NoteSec>, cx: &mut VisualTestContext) {
        cx.simulate_keystrokes("ctrl-,");
        click_on(cx, "settings-tab-shortcuts");
        view.update(cx, |app, _| {
            assert_eq!(
                app.settings.as_ref().map(|s| s.section),
                Some(SettingsSection::Shortcuts)
            )
        });
    }

    /// Scroll `command`'s row into view and click its key.
    fn click_hotkey(view: &Entity<NoteSec>, cx: &mut VisualTestContext, command: Command) {
        let ix = Command::ALL.iter().position(|c| *c == command).unwrap();
        view.update(cx, |app, cx| {
            app.settings
                .as_ref()
                .unwrap()
                .hotkey_scroll
                .scroll_to_item(ix);
            cx.notify();
        });
        cx.run_until_parked();
        click_on(cx, &format!("hotkey-key-{}", command.name()));
    }

    fn capturing(view: &Entity<NoteSec>, cx: &mut VisualTestContext) -> Option<Command> {
        view.update(cx, |app, _| app.capturing())
    }

    fn hotkey_message(view: &Entity<NoteSec>, cx: &mut VisualTestContext) -> Option<String> {
        view.update(cx, |app, _| {
            app.settings
                .as_ref()
                .and_then(|s| s.hotkey_message.as_ref().map(|m| m.text.clone()))
        })
    }

    /// The palette hint for `command`, read from the live keymap.
    fn live_hint(cx: &mut VisualTestContext, command: Command) -> Option<String> {
        let keymap = cx.update(|_, cx| cx.key_bindings());
        let keymap = keymap.borrow();
        binding_hint(&keymap, command.action().as_ref())
    }

    /// Every key bound to `command` in the live keymap.
    fn live_keys(cx: &mut VisualTestContext, command: Command) -> Vec<String> {
        let keymap = cx.update(|_, cx| cx.key_bindings());
        let keymap = keymap.borrow();
        keymap
            .bindings_for_action(command.action().as_ref())
            .map(|b| format_keystrokes(b.keystrokes()))
            .collect()
    }

    fn saved_overrides(dir: &std::path::Path) -> Vec<(String, String)> {
        UiState::load(dir).shortcuts.into_iter().collect()
    }

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn mode_of(view: &Entity<NoteSec>, cx: &mut VisualTestContext) -> Mode {
        view.update(cx, |app, _| app.mode)
    }

    #[gpui::test]
    fn the_shortcuts_section_lists_every_command_and_fits_the_window(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "hotkey-list", "- hi\n");
        cx.simulate_resize(size(px(1100.), px(700.)));
        // From the palette, straight to the section.
        run_in_palette(cx, "change keyboard shortcuts");
        assert!(settings_open(&view, cx));
        view.update(cx, |app, _| {
            assert_eq!(
                app.settings.as_ref().unwrap().section,
                SettingsSection::Shortcuts
            )
        });
        assert!(!has(cx, "git-backup-on"));
        for c in Command::ALL {
            assert!(has(cx, &format!("hotkey-row-{}", c.name())), "{c:?}");
            assert!(!has(cx, &format!("hotkey-modified-{}", c.name())), "{c:?}");
        }
        // Each row shows the keys in effect; palette-only commands are
        // unbound and can get one.
        let table = view.update(cx, |app, _| app.key_table());
        assert_eq!(
            hotkeys::command_keys(&table, Command::Redo),
            ["Ctrl+Shift+Z", "Ctrl+Y"]
        );
        assert!(hotkeys::command_keys(&table, Command::OpenAgenda).is_empty());
        // The panel and its Reset button fit the default window.
        let window = cx.debug_bounds("settings-backdrop").unwrap();
        let panel = cx.debug_bounds("settings-panel").unwrap();
        let reset = cx.debug_bounds("hotkeys-reset-all").unwrap();
        assert!(panel.bottom() <= window.bottom(), "{panel:?} in {window:?}");
        assert!(reset.bottom() <= panel.bottom());
        // The General section is still one click away.
        click_on(cx, "settings-tab-general");
        assert!(has(cx, "git-backup-on") && !has(cx, "hotkey-list"));
        // The cheatsheet points here, and its link opens the section.
        cx.simulate_keystrokes("escape ctrl-/");
        assert!(has(cx, "shortcuts-customize"));
        click_on(cx, "shortcuts-customize");
        view.update(cx, |app, _| {
            assert!(!app.shortcuts_open);
            assert_eq!(
                app.settings.as_ref().map(|s| s.section),
                Some(SettingsSection::Shortcuts)
            );
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn at_the_largest_font_the_settings_panel_scrolls_inside_the_window(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_with_state(
            cx,
            "hotkey-big-font",
            &format!("ui_size = {}\n", crate::config::MAX_FONT_SIZE),
            Config::default(),
        );
        cx.simulate_resize(size(px(1100.), px(700.)));
        cx.simulate_keystrokes("ctrl-,");
        let window = cx.debug_bounds("settings-backdrop").unwrap();
        let panel = cx.debug_bounds("settings-panel").unwrap();
        assert!(panel.bottom() <= window.bottom(), "{panel:?} in {window:?}");
        click_on(cx, "settings-tab-shortcuts");
        let panel = cx.debug_bounds("settings-panel").unwrap();
        assert!(panel.bottom() <= window.bottom(), "{panel:?} in {window:?}");
        assert!(settings_open(&view, cx));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn a_captured_key_fires_and_the_old_one_no_longer_does(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "hotkey-capture", "- hi\n");
        assert_eq!(
            live_hint(cx, Command::ToggleGraph).as_deref(),
            Some("Ctrl+G")
        );
        open_hotkeys(&view, cx);
        click_hotkey(&view, cx, Command::ToggleGraph);
        assert_eq!(capturing(&view, cx), Some(Command::ToggleGraph));

        cx.simulate_keystrokes("ctrl-alt-g");
        assert_eq!(capturing(&view, cx), None);
        // Captured, not run: the graph didn't open.
        assert_eq!(mode_of(&view, cx), Mode::Notes);
        assert!(has(cx, "hotkey-modified-ToggleGraph") && has(cx, "hotkey-reset-ToggleGraph"));
        assert_eq!(
            saved_overrides(&dir),
            pairs(&[("ToggleGraph", "ctrl-alt-g")])
        );
        // The hint, the cheatsheet and the keymap follow at once.
        assert_eq!(
            live_hint(cx, Command::ToggleGraph).as_deref(),
            Some("Ctrl+Alt+G")
        );
        assert_eq!(live_keys(cx, Command::ToggleGraph), ["Ctrl+Alt+G"]);
        let sheet: Vec<String> = view.update(cx, |app, _| {
            cheatsheet(&app.key_table())
                .into_iter()
                .flat_map(|(_, rows)| rows.into_iter().map(|(keys, _)| keys))
                .collect()
        });
        assert!(sheet.contains(&"Ctrl+Alt+G".to_string()));
        assert!(!sheet.contains(&"Ctrl+G".to_string()));
        {
            let keymap = cx.update(|_, cx| cx.key_bindings());
            let keymap = keymap.borrow();
            let table = view.update(cx, |app, _| app.key_table());
            assert_eq!(keymap.bindings().len(), table.len());
        }
        cx.simulate_keystrokes("escape ctrl-k");
        cx.simulate_input("toggle graph view");
        assert!(has(cx, "command-ToggleGraph"));
        cx.simulate_keystrokes("escape");

        // The new key works, the old one doesn't.
        cx.simulate_keystrokes("ctrl-g");
        assert_eq!(mode_of(&view, cx), Mode::Notes);
        cx.simulate_keystrokes("ctrl-alt-g");
        assert_eq!(mode_of(&view, cx), Mode::Graph);
        cx.simulate_keystrokes("ctrl-alt-g");
        assert_eq!(mode_of(&view, cx), Mode::Notes);

        // Capturing a command's own key: nothing runs, no override.
        let theme = view.update(cx, |app, _| app.theme_kind());
        open_hotkeys(&view, cx);
        click_hotkey(&view, cx, Command::ToggleTheme);
        cx.simulate_keystrokes("ctrl-shift-t");
        assert_eq!(view.update(cx, |app, _| app.theme_kind()), theme);
        assert_eq!(capturing(&view, cx), None);
        assert!(!has(cx, "hotkey-modified-ToggleTheme"));
        assert_eq!(
            saved_overrides(&dir),
            pairs(&[("ToggleGraph", "ctrl-alt-g")])
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn escape_cancels_backspace_unbinds_and_the_row_reset_restores(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "hotkey-cancel", "- hi\n");
        open_hotkeys(&view, cx);
        // A lone modifier keeps waiting; Esc cancels and keeps the panel.
        click_hotkey(&view, cx, Command::Undo);
        cx.simulate_keystrokes("shift");
        assert_eq!(capturing(&view, cx), Some(Command::Undo));
        cx.simulate_keystrokes("escape");
        assert_eq!(capturing(&view, cx), None);
        assert!(settings_open(&view, cx));
        assert!(saved_overrides(&dir).is_empty());
        assert_eq!(live_hint(cx, Command::Undo).as_deref(), Some("Ctrl+Z"));
        // Clicking the key again also stops waiting.
        click_hotkey(&view, cx, Command::Undo);
        click_hotkey(&view, cx, Command::Undo);
        assert_eq!(capturing(&view, cx), None);

        // Backspace unbinds.
        click_hotkey(&view, cx, Command::Undo);
        cx.simulate_keystrokes("backspace");
        assert_eq!(capturing(&view, cx), None);
        assert_eq!(live_hint(cx, Command::Undo), None);
        assert_eq!(saved_overrides(&dir), pairs(&[("Undo", "")]));
        assert!(has(cx, "hotkey-modified-Undo"));
        // Ctrl+Z does nothing now.
        cx.simulate_keystrokes("escape");
        edit_and_leave(cx, 0, " more");
        cx.simulate_keystrokes("ctrl-z");
        assert_eq!(file(&dir), "- hi more\n");

        // The row's ↺ brings the default back.
        open_hotkeys(&view, cx);
        click_hotkey(&view, cx, Command::Undo);
        cx.simulate_keystrokes("escape");
        click_on(cx, "hotkey-reset-Undo");
        assert!(saved_overrides(&dir).is_empty());
        assert!(!has(cx, "hotkey-modified-Undo"));
        assert_eq!(live_hint(cx, Command::Undo).as_deref(), Some("Ctrl+Z"));
        // Esc with no capture closes the panel as before.
        cx.simulate_keystrokes("escape");
        assert!(!settings_open(&view, cx));
        cx.simulate_keystrokes("ctrl-z");
        assert_eq!(file(&dir), "- hi\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn a_key_already_in_use_is_refused_with_a_message(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "hotkey-conflict", "- hi\n");
        open_hotkeys(&view, cx);
        click_hotkey(&view, cx, Command::Quit);
        // Another command's key: refused, and it didn't run either.
        cx.simulate_keystrokes("ctrl-g");
        assert_eq!(mode_of(&view, cx), Mode::Notes);
        let message = hotkey_message(&view, cx).unwrap();
        assert!(
            message.contains("Ctrl+G") && message.contains("Toggle graph view"),
            "{message}"
        );
        assert!(has(cx, "hotkey-message"));
        assert_eq!(capturing(&view, cx), Some(Command::Quit), "still waiting");
        assert!(saved_overrides(&dir).is_empty());
        assert_eq!(
            live_hint(cx, Command::ToggleGraph).as_deref(),
            Some("Ctrl+G")
        );
        // A fixed key, and an editor-only command's key.
        cx.simulate_keystrokes("ctrl-k");
        assert!(hotkey_message(&view, cx)
            .unwrap()
            .contains("can't be changed"));
        view.update(cx, |app, _| assert!(app.search.is_none()));
        cx.simulate_keystrokes("ctrl-enter");
        assert!(hotkey_message(&view, cx)
            .unwrap()
            .contains("Cycle task state"));
        // A free key is taken.
        cx.simulate_keystrokes("ctrl-alt-q");
        assert_eq!(capturing(&view, cx), None);
        assert_eq!(hotkey_message(&view, cx), None);
        assert_eq!(saved_overrides(&dir), pairs(&[("Quit", "ctrl-alt-q")]));
        // An editor command against a global key.
        click_hotkey(&view, cx, Command::CycleTask);
        cx.simulate_keystrokes("ctrl-alt-q");
        assert!(hotkey_message(&view, cx).unwrap().contains("Quit"));
        cx.simulate_keystrokes("escape");
        assert_eq!(hotkey_message(&view, cx), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn bare_keys_are_refused_but_function_keys_work(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "hotkey-bare", "- hi\n");
        let page_count = |view: &Entity<NoteSec>, cx: &mut VisualTestContext| {
            view.update(cx, |app, _| app.pages.len())
        };
        let pages = page_count(&view, cx);
        open_hotkeys(&view, cx);
        click_hotkey(&view, cx, Command::NewPage);
        for key in ["a", "shift-a", "enter", "space"] {
            cx.simulate_keystrokes(key);
            let message = hotkey_message(&view, cx).unwrap_or_default();
            assert!(message.contains("would type text"), "{key}: {message}");
            assert_eq!(capturing(&view, cx), Some(Command::NewPage), "{key}");
        }
        assert!(saved_overrides(&dir).is_empty());
        cx.simulate_keystrokes("f2");
        assert_eq!(saved_overrides(&dir), pairs(&[("NewPage", "f2")]));
        cx.simulate_keystrokes("escape f2");
        assert_eq!(page_count(&view, cx), pages + 1);
        // Alt alone is enough.
        open_hotkeys(&view, cx);
        click_hotkey(&view, cx, Command::OpenAgenda);
        cx.simulate_keystrokes("alt-a");
        assert_eq!(live_hint(cx, Command::OpenAgenda).as_deref(), Some("Alt+A"));
        cx.simulate_keystrokes("escape alt-a");
        assert_eq!(mode_of(&view, cx), Mode::Agenda);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn a_command_with_two_default_keys_gets_just_the_new_one(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "hotkey-two", "- hi\n");
        assert_eq!(live_keys(cx, Command::Redo), ["Ctrl+Shift+Z", "Ctrl+Y"]);
        open_hotkeys(&view, cx);
        click_hotkey(&view, cx, Command::Redo);
        cx.simulate_keystrokes("ctrl-alt-r");
        assert_eq!(live_keys(cx, Command::Redo), ["Ctrl+Alt+R"]);
        cx.simulate_keystrokes("escape");
        edit_and_leave(cx, 0, " more");
        cx.simulate_keystrokes("ctrl-z");
        assert_eq!(file(&dir), "- hi\n");
        cx.simulate_keystrokes("ctrl-y ctrl-shift-z");
        assert_eq!(file(&dir), "- hi\n", "the old keys are gone");
        cx.simulate_keystrokes("ctrl-alt-r");
        assert_eq!(file(&dir), "- hi more\n");
        // Its ↺ gives both back.
        open_hotkeys(&view, cx);
        click_hotkey(&view, cx, Command::Redo);
        cx.simulate_keystrokes("escape");
        click_on(cx, "hotkey-reset-Redo");
        assert_eq!(live_keys(cx, Command::Redo), ["Ctrl+Shift+Z", "Ctrl+Y"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn reset_to_defaults_restores_everything(cx: &mut TestAppContext) {
        let state = "favorites = [\"Test\"]\n[shortcuts]\nToggleGraph = \"ctrl-alt-g\"\nRedo = \"ctrl-alt-r\"\nQuit = \"\"\nSomeFutureCommand = \"ctrl-alt-f\"\n";
        let (view, cx, dir) = setup_with_state(cx, "hotkey-reset", state, Config::default());
        assert_eq!(
            live_hint(cx, Command::ToggleGraph).as_deref(),
            Some("Ctrl+Alt+G")
        );
        assert_eq!(live_hint(cx, Command::Quit), None);
        open_hotkeys(&view, cx);
        assert!(has(cx, "hotkey-modified-ToggleGraph") && has(cx, "hotkey-modified-Redo"));
        click_on(cx, "hotkeys-reset-all");
        assert!(saved_overrides(&dir).is_empty());
        assert!(!std::fs::read_to_string(UiState::path(&dir))
            .unwrap()
            .contains("[shortcuts]"));
        assert_eq!(UiState::load(&dir).favorites, vec!["Test".to_string()]);
        for c in Command::ALL {
            assert!(!has(cx, &format!("hotkey-modified-{}", c.name())), "{c:?}");
        }
        assert!(hotkey_message(&view, cx).unwrap().contains("default"));
        // The keymap is the default table again.
        {
            let keymap = cx.update(|_, cx| cx.key_bindings());
            let keymap = keymap.borrow();
            let defaults = shortcuts();
            assert_eq!(keymap.bindings().len(), defaults.len());
            for (binding, default) in keymap.bindings().zip(defaults.iter()) {
                assert_eq!(
                    format_keystrokes(binding.keystrokes()),
                    format_keystrokes(default.binding.keystrokes())
                );
                assert!(binding.action().partial_eq(default.binding.action()));
            }
        }
        cx.simulate_keystrokes("escape ctrl-g");
        assert_eq!(mode_of(&view, cx), Mode::Graph);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn overrides_load_from_state_toml_and_odd_entries_are_ignored(cx: &mut TestAppContext) {
        let state = "[shortcuts]\nSplitRight = \"ctrl-alt-s\"\nRedo = \"y\"\nUndo = \"ctrl-nope-z\"\nOpenTrash = \"ctrl-k ctrl-t\"\nNoSuchCommand = \"ctrl-alt-n\"\nExportHtml = 7\n";
        let (view, cx, dir) = setup_with_state(cx, "hotkey-load", state, Config::default());
        // The valid one applies...
        cx.simulate_keystrokes("ctrl-\\");
        assert_eq!(view.update(cx, |app, _| app.split.clone()), None);
        cx.simulate_keystrokes("ctrl-alt-s");
        assert!(view.update(cx, |app, _| app.split.is_some()));
        // ...the rest keep their defaults.
        assert_eq!(live_keys(cx, Command::Redo), ["Ctrl+Shift+Z", "Ctrl+Y"]);
        assert_eq!(live_keys(cx, Command::Undo), ["Ctrl+Z"]);
        assert!(live_keys(cx, Command::OpenTrash).is_empty());
        assert!(live_keys(cx, Command::ExportHtml).is_empty());
        open_hotkeys(&view, cx);
        assert!(has(cx, "hotkey-modified-SplitRight"));
        assert!(!has(cx, "hotkey-modified-Redo") && !has(cx, "hotkey-modified-Undo"));
        // A change keeps the entries this build doesn't use in the file.
        click_hotkey(&view, cx, Command::Quit);
        cx.simulate_keystrokes("ctrl-alt-q");
        let saved = UiState::load(&dir).shortcuts;
        assert_eq!(saved["NoSuchCommand"], "ctrl-alt-n");
        assert_eq!(saved["Redo"], "y");
        assert_eq!(saved["Quit"], "ctrl-alt-q");
        assert!(!saved.contains_key("ExportHtml"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn a_shifted_symbol_key_round_trips(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "hotkey-pipe", "- hi\n");
        open_hotkeys(&view, cx);
        // Ctrl+| is Focus other pane's: free it first, then take it.
        click_hotkey(&view, cx, Command::ToggleGraph);
        cx.simulate_keystrokes("ctrl-|");
        assert!(hotkey_message(&view, cx)
            .unwrap()
            .contains("Focus other pane"));
        cx.simulate_keystrokes("escape");
        click_hotkey(&view, cx, Command::FocusOtherPane);
        cx.simulate_keystrokes("backspace");
        click_hotkey(&view, cx, Command::ToggleGraph);
        cx.simulate_keystrokes("ctrl-|");
        assert_eq!(
            saved_overrides(&dir),
            pairs(&[("FocusOtherPane", ""), ("ToggleGraph", "ctrl-|")])
        );
        assert_eq!(
            live_hint(cx, Command::ToggleGraph).as_deref(),
            Some("Ctrl+|")
        );
        cx.simulate_keystrokes("escape ctrl-|");
        assert_eq!(mode_of(&view, cx), Mode::Graph);
        // And from the file, in a fresh window on the same graph.
        let storage = Storage::open(dir.clone()).unwrap();
        let (view2, cx2) = cx
            .cx
            .add_window_view(|window, cx| NoteSec::new(storage, Config::default(), window, cx));
        assert_eq!(
            live_hint(cx2, Command::ToggleGraph).as_deref(),
            Some("Ctrl+|")
        );
        cx2.simulate_keystrokes("ctrl-|");
        assert_eq!(mode_of(&view2, cx2), Mode::Graph);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn pane_and_new_commands_are_rebindable_with_no_extra_code(cx: &mut TestAppContext) {
        // Every `commands!` command has a row, a context and a binding.
        let (view, cx, dir) = setup(cx, "hotkey-every", "- hi\n");
        open_hotkeys(&view, cx);
        for (i, &c) in Command::ALL.iter().enumerate() {
            let key = format!("ctrl-alt-shift-f{}", i % 24 + 1);
            let binding = c.binding(&key);
            assert_eq!(format_keystrokes(binding.keystrokes()).is_empty(), false);
            assert!(binding.action().partial_eq(c.action().as_ref()), "{c:?}");
        }
        click_hotkey(&view, cx, Command::CustomizeShortcuts);
        cx.simulate_keystrokes("alt-k");
        assert_eq!(
            live_hint(cx, Command::CustomizeShortcuts).as_deref(),
            Some("Alt+K")
        );
        cx.simulate_keystrokes("escape alt-k");
        view.update(cx, |app, _| {
            assert_eq!(
                app.settings.as_ref().map(|s| s.section),
                Some(SettingsSection::Shortcuts)
            )
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    // --- global search (Ctrl+Shift+F) -----------------------------------------

    fn global_pages() -> [(&'static str, &'static str); 3] {
        [
            ("Test", "- hello\n"),
            ("Recipes", "- pasta\n- bread\n  needs Flour and water\n"),
            ("Flour", "- types of flour\n"),
        ]
    }

    /// The current global search results as (page title, matched text);
    /// the matched text is `None` for a title hit.
    fn text_hits(
        view: &Entity<NoteSec>,
        cx: &mut VisualTestContext,
    ) -> Vec<(String, Option<String>)> {
        view.update(cx, |app, _| {
            app.search_results()
                .iter()
                .map(|h| match h.target {
                    Target::Page(p) => (app.pages[p].title.clone(), None),
                    Target::Match {
                        page,
                        block,
                        start,
                        end,
                    } => (
                        app.pages[page].title.clone(),
                        Some(app.pages[page].blocks[block].content[start..end].to_string()),
                    ),
                    other => panic!("not a global search result: {other:?}"),
                })
                .collect()
        })
    }

    #[gpui::test]
    fn ctrl_shift_f_finds_text_everywhere_and_selects_the_match(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "global-search", &global_pages(), "Test");
        // Opening it saves the block being edited, like the palette.
        click_block(cx, 0);
        cx.simulate_input("!");
        cx.simulate_keystrokes("ctrl-shift-f");
        view.update(cx, |app, _| {
            assert!(app.search.as_ref().is_some_and(|s| s.global));
            assert_eq!(app.editing, None);
        });
        assert_eq!(file(&dir), "- hello!\n");
        assert!(has(cx, "global-search-heading"));
        assert!(text_hits(&view, cx).is_empty(), "nothing before typing");

        // Title match first, then each matching line, ignoring case.
        cx.simulate_input("flour");
        assert_eq!(
            text_hits(&view, cx),
            vec![
                ("Flour".to_string(), None),
                ("Flour".to_string(), Some("flour".to_string())),
                ("Recipes".to_string(), Some("Flour".to_string())),
            ]
        );
        assert!(has(cx, "snippet-1") && has(cx, "snippet-2"));
        assert!(!has(cx, "snippet-0"), "a title hit has no snippet");

        // Enter on a line opens its page and selects the match in the block.
        cx.simulate_keystrokes("down down enter");
        view.update(cx, |app, _| {
            assert!(app.search.is_none());
            assert_eq!(app.pages[app.selected].title, "Recipes");
            assert_eq!(app.editing, Some(1));
            let selected = app.editor.selection().unwrap();
            assert_eq!(&app.editor.text[selected], "Flour");
        });
        // It is a real selection: typing replaces it.
        cx.simulate_input("Rye");
        cx.simulate_keystrokes("escape");
        assert_eq!(
            std::fs::read_to_string(page_file(&dir, "Recipes")).unwrap(),
            "- pasta\n- bread\n  needs Rye and water\n"
        );

        // A title hit just opens the page.
        cx.simulate_keystrokes("ctrl-shift-f");
        cx.simulate_input("FLOUR");
        cx.simulate_keystrokes("enter");
        view.update(cx, |app, _| {
            assert_eq!(app.pages[app.selected].title, "Flour");
            assert_eq!(app.editing, None);
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn global_search_opens_closes_and_comes_from_the_palette(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "global-toggle", &global_pages(), "Test");
        let global = |view: &Entity<NoteSec>, cx: &mut VisualTestContext| {
            view.update(cx, |app, _| app.search.as_ref().map(|s| s.global))
        };
        cx.simulate_keystrokes("ctrl-shift-f");
        assert_eq!(global(&view, cx), Some(true));
        cx.simulate_keystrokes("ctrl-shift-f");
        assert_eq!(global(&view, cx), None, "the key closes it again");
        cx.simulate_keystrokes("ctrl-shift-f");
        cx.simulate_input("zzz");
        assert!(text_hits(&view, cx).is_empty());
        cx.simulate_keystrokes("escape");
        assert_eq!(global(&view, cx), None);

        // From the Ctrl-K palette: the key switches over, and so does the
        // command.
        cx.simulate_keystrokes("ctrl-k");
        assert_eq!(global(&view, cx), Some(false));
        cx.simulate_keystrokes("ctrl-shift-f");
        assert_eq!(global(&view, cx), Some(true));
        cx.simulate_keystrokes("escape");
        run_in_palette(cx, "search all pages");
        assert_eq!(global(&view, cx), Some(true));
        assert!(has(cx, "global-search-heading"));
        cx.simulate_keystrokes("escape");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn global_search_includes_journals(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_with_journal(cx, "global-journal", "- met Ada at the cafe\n");
        cx.simulate_keystrokes("ctrl-shift-f");
        cx.simulate_input("ada");
        assert_eq!(
            text_hits(&view, cx),
            vec![(today_title(), Some("Ada".to_string()))]
        );
        cx.simulate_keystrokes("enter");
        view.update(cx, |app, _| {
            assert_eq!(app.pages[app.selected].title, today_title());
            assert_eq!(app.editing, Some(0));
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    // --- "Linked from" -------------------------------------------------------

    #[gpui::test]
    fn linked_from_lists_wikilinks_and_block_references(cx: &mut TestAppContext) {
        let id = "6f9b2c1e-0000-4000-8000-0000000000aa";
        let test = format!("- the plan\n  id:: {id}\n");
        let alpha = format!("- see (({id})) now\n- unrelated\n");
        let beta = format!("- [[Test]] and (({id}))\n- also [[test]]\n");
        let (view, cx, dir) = setup_pages(
            cx,
            "linked-from",
            &[
                ("Test", test.as_str()),
                ("Alpha", alpha.as_str()),
                ("Beta", beta.as_str()),
                ("Gamma", "- nothing here\n"),
            ],
            "Test",
        );
        assert!(has(cx, "linked-from"));
        assert!(!has(cx, "linked-from-empty"));
        // Alpha's block reference, then Beta's two blocks (the one with
        // both kinds of link is listed once), shown as reading text.
        view.update(cx, |app, _| {
            assert_eq!(
                app.backlink_texts,
                vec!["see the plan now", "[[Test]] and the plan", "also [[test]]"]
            );
        });
        assert!(has(cx, "linked-page-0") && has(cx, "linked-page-1"));
        assert!(!has(cx, "linked-page-2"));
        assert!(has(cx, "backlink-2") && !has(cx, "backlink-3"));

        // A page row opens that page...
        let r = cx.debug_bounds("linked-page-1").unwrap();
        cx.simulate_click(r.center(), Modifiers::none());
        view.update(cx, |app, _| {
            assert_eq!(app.pages[app.selected].title, "Beta")
        });
        // ...which nothing links to: the section says so.
        assert!(has(cx, "linked-from") && has(cx, "linked-from-empty"));
        view.update(cx, |app, _| assert!(app.backlink_texts.is_empty()));

        // Back on Test, a block-reference row opens the referencing page.
        cx.simulate_keystrokes("ctrl-k");
        cx.simulate_input("Test");
        cx.simulate_keystrokes("enter");
        let r = cx.debug_bounds("backlink-0").unwrap();
        cx.simulate_click(r.center(), Modifiers::none());
        view.update(cx, |app, _| {
            assert_eq!(app.pages[app.selected].title, "Alpha")
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Pages for the alias tests: JavaScript declares JS and ECMAScript.
    fn alias_pages() -> [(&'static str, &'static str); 3] {
        [
            ("Test", "- \n"),
            ("JavaScript", "- alias:: JS, ECMAScript\n- the language\n"),
            ("Jazz", "- music\n"),
        ]
    }

    #[gpui::test]
    fn double_bracket_picks_a_page_by_alias_and_links_its_title(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "alias-picker", &alias_pages(), "Test");
        let pages_before = view.update(cx, |app, _| app.pages.len());
        click_block(cx, 0);
        cx.simulate_input("see [[ecma");
        assert!(has(cx, "ref-menu"));
        let js = view.update(cx, |app, _| app.find_page("JavaScript").unwrap());
        view.update(cx, |app, _| {
            assert_eq!(
                app.ref_matches(),
                vec![RefItem::Page(js, Some("ECMAScript".to_string()))]
            );
        });
        cx.simulate_keystrokes("enter");
        assert!(!has(cx, "ref-menu"));
        view.update(cx, |app, _| {
            assert_eq!(app.editor.text, "see [[JavaScript]]");
            assert_eq!(app.editor.cursor, app.editor.text.len());
            assert_eq!(app.editing, Some(0), "Enter picked, no new block");
        });
        assert_eq!(file(&dir), "- see [[JavaScript]]\n");

        // Typed inside a "[[]]" already there: the "]]" isn't doubled.
        cx.simulate_input(" [[]]");
        cx.simulate_keystrokes("left left");
        // An empty query lists pages.
        assert!(has(cx, "ref-menu"));
        cx.simulate_input("js");
        view.update(cx, |app, _| {
            assert_eq!(
                app.ref_matches(),
                vec![RefItem::Page(js, Some("JS".to_string()))]
            );
        });
        cx.simulate_keystrokes("enter");
        view.update(cx, |app, _| {
            assert_eq!(app.editor.text, "see [[JavaScript]] [[JavaScript]]");
            assert_eq!(app.editor.cursor, app.editor.text.len());
        });

        // A title match shows no alias; Esc closes the picker and keeps the
        // text, and stopping the edit doesn't create a "Ja" page.
        cx.simulate_input(" [[ja");
        view.update(cx, |app, _| {
            let items = app.ref_matches();
            assert!(items.contains(&RefItem::Page(js, None)));
            assert_eq!(items.len(), 2, "JavaScript and Jazz");
        });
        cx.simulate_keystrokes("escape");
        assert!(!has(cx, "ref-menu"));
        view.update(cx, |app, _| assert!(app.editor.text.ends_with(" [[ja")));
        cx.simulate_input("vascript]]");
        cx.simulate_keystrokes("escape");
        view.update(cx, |app, _| {
            assert_eq!(app.editing, None);
            assert_eq!(app.pages.len(), pages_before);
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn links_to_an_alias_go_to_the_page_and_count_as_backlinks(cx: &mut TestAppContext) {
        let mut pages = alias_pages();
        pages[0] = ("Test", "- \n");
        let (view, cx, dir) = setup_pages(cx, "alias-links", &pages, "Test");
        let pages_before = view.update(cx, |app, _| app.pages.len());
        // Typed by hand (picker dismissed), then committed: [[JS]] already
        // has a page, so no "JS" page is created.
        click_block(cx, 0);
        cx.simulate_input("about [[JS");
        cx.simulate_keystrokes("escape");
        cx.simulate_input("]] and #ecmascript");
        cx.simulate_keystrokes("escape");
        view.update(cx, |app, _| {
            assert_eq!(app.editing, None);
            assert_eq!(app.pages.len(), pages_before);
            assert!(app.find_page("JS").is_none());
        });

        // Following the link opens JavaScript, whose "Linked from" lists it.
        view.update(cx, |app, cx| app.open_page("js", cx));
        cx.run_until_parked();
        view.update(cx, |app, _| {
            assert_eq!(app.pages[app.selected].title, "JavaScript");
            assert_eq!(app.pages.len(), pages_before);
            assert_eq!(app.backlink_texts, vec!["about [[JS]] and #ecmascript"]);
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn global_search_finds_a_page_by_its_alias(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "alias-search", &alias_pages(), "Test");
        cx.simulate_keystrokes("ctrl-shift-f");
        cx.simulate_input("js");
        assert_eq!(
            text_hits(&view, cx),
            vec![
                ("JavaScript".to_string(), None),
                ("JavaScript".to_string(), Some("JS".to_string())),
            ]
        );
        cx.simulate_keystrokes("enter");
        view.update(cx, |app, _| {
            assert_eq!(app.pages[app.selected].title, "JavaScript")
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    // --- live embeds (decision 47) -------------------------------------------

    const EMBED_ID: &str = "6f9b2c1e-0000-4000-8000-0000000000e1";

    fn embed_lines(view: &Entity<NoteSec>, cx: &mut VisualTestContext) -> Vec<String> {
        view.update(cx, |app, _| app.embed_lines.clone())
    }

    fn lines(l: &[&str]) -> Vec<String> {
        l.iter().map(|s| s.to_string()).collect()
    }

    /// Start editing row `row` by clicking its own text (the middle of a
    /// row with an embed is the embed), with the cursor at the end.
    fn click_host_text(view: &Entity<NoteSec>, cx: &mut VisualTestContext, row: usize) {
        let at = view.update(cx, |app, _| {
            let (_, layout) = app.reading_layouts.iter().find(|(r, _)| *r == row).unwrap();
            let p = layout.position_for_index(0).unwrap();
            point(p.x + px(1.), p.y + layout.line_height() / 2.)
        });
        cx.simulate_click(at, Modifiers::none());
        cx.simulate_keystrokes("end");
    }

    #[gpui::test]
    fn embeds_show_the_source_live_and_open_it_on_click(cx: &mut TestAppContext) {
        let test = format!("- intro ![[Beta]]\n- ![[(({EMBED_ID}))]]\n");
        let beta = format!("- one\n  id:: {EMBED_ID}\n  - child\n- two\n");
        let (view, cx, dir) = setup_pages(
            cx,
            "embed-live",
            &[("Test", &test), ("Beta", &beta)],
            "Test",
        );
        let pages_before = view.update(cx, |app, _| app.pages.len());
        assert_eq!(
            embed_lines(&view, cx),
            lines(&[
                "[Beta]",
                "  one",
                "    child",
                "  two",
                "[Beta]",
                "  one",
                "    child"
            ])
        );
        assert!(has(cx, "embed-0-title-0") && has(cx, "embed-0-block-3"));
        assert!(has(cx, "embed-1-title-0") && has(cx, "embed-1-block-2"));
        assert!(!has(cx, "embed-1-block-3"));
        // The embedding block's own text stays (and stays editable).
        assert_eq!(
            reading_text(&view, cx, 0).as_deref(),
            Some("intro ![[Beta]]")
        );

        // Edit the source; the embeds show it when we come back.
        click_sidebar_page(&view, cx, "Beta");
        edit_and_leave(cx, 1, " edited");
        click_sidebar_page(&view, cx, "Test");
        assert_eq!(
            embed_lines(&view, cx),
            lines(&[
                "[Beta]",
                "  one",
                "    child edited",
                "  two",
                "[Beta]",
                "  one",
                "    child edited"
            ])
        );

        // Editing and saving the host: the block embed is no page link, so
        // no "((id))" page appears, and the file keeps the syntax.
        click_host_text(&view, cx, 1);
        cx.simulate_input(" here");
        cx.simulate_keystrokes("escape");
        view.update(cx, |app, _| assert_eq!(app.pages.len(), pages_before));
        assert_eq!(
            file(&dir),
            format!("- intro ![[Beta]]\n- ![[(({EMBED_ID}))]] here\n")
        );

        // A click on an embedded block edits it on its own page.
        click_on(cx, "embed-1-block-2");
        view.update(cx, |app, _| {
            assert_eq!(app.pages[app.selected].title, "Beta");
            assert_eq!(app.editing, Some(1));
            assert_eq!(app.editor.text, "child edited");
        });
        cx.simulate_keystrokes("escape");
        click_sidebar_page(&view, cx, "Test");
        // The title opens the source page, without editing anything.
        click_on(cx, "embed-0-title-0");
        view.update(cx, |app, _| {
            assert_eq!(app.pages[app.selected].title, "Beta");
            assert_eq!(app.editing, None);
            // Both embeds count in Beta's "Linked from".
            assert_eq!(app.backlink_texts.len(), 2);
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn embed_notes_for_cycles_missing_trashed_and_deep_targets(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(
            cx,
            "embed-notes",
            &[
                ("Test", "- ![[Test]]\n- ![[Gone]] ![[Beta]]\n- ![[P1]]\n"),
                ("Beta", "- beta ![[test]]\n"),
                ("P1", "- 1 ![[P2]]\n"),
                ("P2", "- 2 ![[P3]]\n"),
                ("P3", "- 3 ![[P4]]\n"),
                ("P4", "- 4 ![[P5]]\n"),
                ("P5", "- 5\n"),
            ],
            "Test",
        );
        assert_eq!(
            embed_lines(&view, cx),
            lines(&[
                "Circular embed of \u{201c}Test\u{201d}",
                "Page \u{201c}Gone\u{201d} not found",
                "[Beta]",
                "  beta ![[test]]",
                "    Circular embed of \u{201c}Test\u{201d}",
                "[P1]",
                "  1 ![[P2]]",
                "    [P2]",
                "      2 ![[P3]]",
                "        [P3]",
                "          3 ![[P4]]",
                "            [P4]",
                "              4 ![[P5]]",
                "                Embeds nested more than 4 deep are not shown",
            ])
        );
        assert!(has(cx, "embed-0-note-0") && has(cx, "embed-1-note-0"));

        // A trashed page is a missing one.
        view.update(cx, |app, cx| app.delete_page("Beta", cx).unwrap());
        click_sidebar_page(&view, cx, "Test");
        let shown = embed_lines(&view, cx);
        assert_eq!(
            shown[1..3],
            lines(&[
                "Page \u{201c}Gone\u{201d} not found",
                "Page \u{201c}Beta\u{201d} not found"
            ])
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn an_embed_in_one_pane_follows_typing_in_the_other(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(
            cx,
            "embed-split",
            &[("Test", "- ![[Beta]]\n"), ("Beta", "- one\n- two\n")],
            "Test",
        );
        // Test on the left, Beta on the right (focused), being edited.
        cx.simulate_keystrokes("ctrl-\\");
        click_sidebar_page(&view, cx, "Beta");
        assert_eq!(split_of(&view, cx), split_is("Beta", true));
        assert!(in_pane(cx, "other-embed-0-title-0", "left"));
        click_block(cx, 1);
        cx.simulate_input(" typing");
        let other = view.update(cx, |app, _| app.other_embed_lines.clone());
        assert_eq!(other, lines(&["[Beta]", "  one", "  two typing"]));
        // Nothing saved yet: it is the editor's text.
        assert_eq!(
            std::fs::read_to_string(page_file(&dir, "Beta")).unwrap(),
            "- one\n- two\n"
        );
        cx.simulate_keystrokes("escape");
        let other = view.update(cx, |app, _| app.other_embed_lines.clone());
        assert_eq!(other, lines(&["[Beta]", "  one", "  two typing"]));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn bang_double_bracket_picks_a_page_by_alias_and_embeds_its_title(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(
            cx,
            "embed-picker",
            &[
                ("Test", "- \n- {{embed [[js]]}}\n"),
                ("JavaScript", "alias:: JS\n\n- the language\n"),
            ],
            "Test",
        );
        let pages_before = view.update(cx, |app, _| app.pages.len());
        // Logseq's form, by alias, resolves like a link. The page's
        // properties block shows as it does on the page.
        let js = ["[JavaScript]", "  alias:: JS", "  the language"];
        assert_eq!(embed_lines(&view, cx), lines(&js));
        click_block(cx, 0);
        cx.simulate_input("![[js");
        assert!(has(cx, "ref-menu"));
        cx.simulate_keystrokes("enter");
        view.update(cx, |app, _| assert_eq!(app.editor.text, "![[JavaScript]]"));
        cx.simulate_keystrokes("escape");
        assert_eq!(file(&dir), "- ![[JavaScript]]\n- {{embed [[js]]}}\n");
        assert_eq!(embed_lines(&view, cx), lines(&[js, js].concat()));
        view.update(cx, |app, _| assert_eq!(app.pages.len(), pages_before));
        let _ = std::fs::remove_dir_all(dir);
    }

    // --- vim mode (decision 48) ----------------------------------------------

    /// Like `setup`, with vim mode on.
    fn setup_vim<'a>(
        cx: &'a mut TestAppContext,
        name: &str,
        markdown: &str,
    ) -> (Entity<NoteSec>, &'a mut VisualTestContext, PathBuf) {
        let (view, cx, dir) = setup(cx, name, markdown);
        view.update(cx, |app, cx| {
            app.config.vim_mode = true;
            cx.notify();
        });
        cx.run_until_parked();
        (view, cx, dir)
    }

    fn vim_label(view: &Entity<NoteSec>, cx: &mut VisualTestContext) -> Option<String> {
        view.update(cx, |app, _| app.vim_label())
    }

    fn block_texts(view: &Entity<NoteSec>, cx: &mut VisualTestContext) -> Vec<String> {
        view.update(cx, |app, _| {
            app.pages[app.selected]
                .blocks
                .iter()
                .map(|b| b.content.clone())
                .collect()
        })
    }

    fn editing_text(view: &Entity<NoteSec>, cx: &mut VisualTestContext) -> (Option<usize>, String) {
        view.update(cx, |app, _| (app.editing, app.editor.text.clone()))
    }

    #[gpui::test]
    fn vim_is_off_by_default_and_editing_is_unchanged(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "vim-off", "- one\n");
        view.update(cx, |app, _| assert!(!app.config.vim_mode));
        click_block(cx, 0);
        cx.simulate_keystrokes("end");
        cx.simulate_input(" ijk:wq");
        assert_eq!(editing_text(&view, cx), (Some(0), "one ijk:wq".into()));
        assert!(!has(cx, "vim-mode"));
        assert_eq!(vim_label(&view, cx), None);
        cx.simulate_keystrokes("escape");
        view.update(cx, |app, _| assert_eq!(app.editing, None));
        assert_eq!(file(&dir), "- one ijk:wq\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn the_settings_row_and_the_command_turn_vim_on_and_off(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "vim-settings", "- one\n");
        cx.simulate_keystrokes("ctrl-,");
        assert!(has(cx, "settings-editor") && has(cx, "vim-mode-off"));
        click_on(cx, "vim-mode-on");
        view.update(cx, |app, _| assert!(app.config.vim_mode));
        assert!(saved_config(&dir).vim_mode);
        click_on(cx, "vim-mode-off");
        assert!(!saved_config(&dir).vim_mode);
        cx.simulate_keystrokes("escape");

        // The palette command, which Settings > Shortcuts lists too.
        run_in_palette(cx, "Toggle vim mode");
        assert!(saved_config(&dir).vim_mode);
        assert_eq!(status_text(&view, cx).as_deref(), Some("Vim mode is on"));
        view.update(cx, |app, _| {
            assert!(app
                .key_table()
                .iter()
                .all(|s| s.description != "Toggle vim mode"));
            assert!(Command::ALL.contains(&Command::ToggleVimMode));
        });
        run_in_palette(cx, "Toggle vim mode");
        assert!(!saved_config(&dir).vim_mode);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn normal_mode_keys_never_type_and_i_and_esc_switch_modes(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_vim(cx, "vim-modes", "- hello world\n");
        // A click starts editing in Normal mode, shown at the bottom.
        click_block(cx, 0);
        assert_eq!(vim_label(&view, cx).as_deref(), Some("NORMAL"));
        assert!(has(cx, "vim-mode"));
        cx.simulate_input("qzQZ");
        assert_eq!(editing_text(&view, cx), (Some(0), "hello world".into()));
        // Motions, then insert.
        cx.simulate_input("0w");
        view.update(cx, |app, _| assert_eq!(app.editor.cursor, 6));
        cx.simulate_input("2d");
        assert_eq!(vim_label(&view, cx).as_deref(), Some("NORMAL  2d"));
        cx.simulate_keystrokes("escape");
        cx.simulate_input("i");
        assert_eq!(vim_label(&view, cx).as_deref(), Some("INSERT"));
        cx.simulate_input("big ");
        assert_eq!(editing_text(&view, cx), (Some(0), "hello big world".into()));
        // Esc: Normal, still editing. Esc again: stop, saved.
        cx.simulate_keystrokes("escape");
        assert_eq!(vim_label(&view, cx).as_deref(), Some("NORMAL"));
        view.update(cx, |app, _| assert_eq!(app.editor.cursor, 9));
        // Visual: select the word under the cursor and delete it.
        cx.simulate_input("bve");
        assert_eq!(vim_label(&view, cx).as_deref(), Some("VISUAL"));
        cx.simulate_input("d");
        assert_eq!(editing_text(&view, cx), (Some(0), "hello  world".into()));
        cx.simulate_input("x");
        assert_eq!(editing_text(&view, cx), (Some(0), "hello world".into()));
        // Each change is one undo step.
        cx.simulate_input("u");
        assert_eq!(editing_text(&view, cx), (Some(0), "hello  world".into()));
        cx.simulate_keystrokes("ctrl-r");
        assert_eq!(editing_text(&view, cx), (Some(0), "hello world".into()));
        cx.simulate_keystrokes("escape");
        view.update(cx, |app, _| assert_eq!(app.editing, None));
        assert!(!has(cx, "vim-mode"));
        assert_eq!(file(&dir), "- hello world\n");
        // An input method's text is ignored in Normal mode too.
        click_block(cx, 0);
        view.update_in(cx, |app, window, cx| {
            app.replace_text_in_range(None, "zz", window, cx);
            app.replace_and_mark_text_in_range(None, "zz", None, window, cx);
        });
        assert_eq!(editing_text(&view, cx), (Some(0), "hello world".into()));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn dd_yy_and_p_work_on_whole_blocks(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_vim(cx, "vim-blocks", "- one\n  - child\n- two\n- three\n");
        click_block(cx, 0);
        // `dd` takes the block with its child.
        cx.simulate_input("dd");
        assert_eq!(block_texts(&view, cx), ["two", "three"]);
        assert_eq!(editing_text(&view, cx), (Some(0), "two".into()));
        assert_eq!(file(&dir), "- two\n- three\n");
        // `p`: below, as it was.
        cx.simulate_input("p");
        assert_eq!(block_texts(&view, cx), ["two", "one", "child", "three"]);
        assert_eq!(editing_text(&view, cx), (Some(1), "one".into()));
        assert_eq!(file(&dir), "- two\n- one\n  - child\n- three\n");
        // `yy` + `P`: a copy above.
        cx.simulate_input("GyyggP");
        assert_eq!(
            block_texts(&view, cx),
            ["three", "two", "one", "child", "three"]
        );
        assert_eq!(editing_text(&view, cx), (Some(0), "three".into()));
        // `u` undoes the paste, then the earlier paste.
        cx.simulate_input("u");
        assert_eq!(block_texts(&view, cx), ["two", "one", "child", "three"]);
        cx.simulate_input("2dd");
        assert_eq!(block_texts(&view, cx), ["three"]);
        // The last block: it is emptied, the page keeps one block.
        cx.simulate_input("dd");
        assert_eq!(block_texts(&view, cx), [""]);
        assert_eq!(editing_text(&view, cx), (Some(0), "".into()));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn o_and_capital_o_open_blocks_and_esc_closes_the_slash_menu_first(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_vim(cx, "vim-open", "- one\n- two\n");
        click_block(cx, 0);
        cx.simulate_input("o");
        assert_eq!(block_texts(&view, cx), ["one", "", "two"]);
        assert_eq!(editing_text(&view, cx), (Some(1), "".into()));
        assert_eq!(vim_label(&view, cx).as_deref(), Some("INSERT"));
        cx.simulate_input("new");
        // Enter in Insert mode splits as always and stays in Insert.
        cx.simulate_keystrokes("enter");
        cx.simulate_input("next");
        assert_eq!(vim_label(&view, cx).as_deref(), Some("INSERT"));
        cx.simulate_keystrokes("escape");
        cx.simulate_input("O");
        cx.simulate_input("/");
        assert!(has(cx, "slash-menu"));
        cx.simulate_keystrokes("escape");
        assert!(!has(cx, "slash-menu"));
        assert_eq!(vim_label(&view, cx).as_deref(), Some("INSERT"));
        cx.simulate_keystrokes("escape escape");
        // Esc on the menu took the "/" back, as without vim.
        assert_eq!(file(&dir), "- one\n- new\n- \n- next\n- two\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn j_k_gg_g_and_indent_move_through_the_outline(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_vim(cx, "vim-moves", "- one\n- two\n  second line\n- three\n");
        click_block(cx, 0);
        cx.simulate_input("0j");
        assert_eq!(editing_text(&view, cx).0, Some(1));
        // `j` inside a multi-line block, then on to the next block.
        cx.simulate_input("j");
        view.update(cx, |app, _| {
            assert_eq!(app.editing, Some(1));
            assert_eq!(app.editor.cursor, 4);
        });
        cx.simulate_input("j");
        assert_eq!(editing_text(&view, cx).0, Some(2));
        cx.simulate_input("k");
        view.update(cx, |app, _| {
            assert_eq!(app.editing, Some(1));
            assert_eq!(app.editor.cursor, 4, "on its last line");
        });
        cx.simulate_input("gg");
        assert_eq!(editing_text(&view, cx).0, Some(0));
        cx.simulate_input("G");
        assert_eq!(editing_text(&view, cx).0, Some(2));
        cx.simulate_input(">>");
        assert_eq!(file(&dir), "- one\n- two\n  second line\n  - three\n");
        cx.simulate_input("<<");
        assert_eq!(file(&dir), "- one\n- two\n  second line\n- three\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn colon_commands_save_stop_and_complain(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_vim(cx, "vim-colon", "- one\n");
        click_block(cx, 0);
        cx.simulate_input("0x:w");
        assert_eq!(vim_label(&view, cx).as_deref(), Some(":w"));
        // The page isn't written before `:w` (only on leaving the block).
        assert_eq!(file(&dir), "- one\n");
        cx.simulate_keystrokes("enter");
        assert_eq!(file(&dir), "- ne\n");
        assert_eq!(
            status_text(&view, cx).as_deref(),
            Some("Saved \u{201c}Test\u{201d}")
        );
        assert_eq!(editing_text(&view, cx), (Some(0), "ne".into()));
        cx.simulate_input(":bogus");
        cx.simulate_keystrokes("enter");
        assert_eq!(
            status_text(&view, cx).as_deref(),
            Some("Not an editor command: bogus")
        );
        view.update(cx, |app, _| assert!(app.status.as_ref().unwrap().error));
        cx.simulate_input("x:q");
        cx.simulate_keystrokes("enter");
        view.update(cx, |app, _| assert_eq!(app.editing, None));
        assert_eq!(file(&dir), "- e\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn shortcuts_the_palette_and_the_rename_field_are_not_vim(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "vim-other-inputs", &tab_pages(), "Test");
        view.update(cx, |app, _| app.config.vim_mode = true);
        click_block(cx, 0);
        // Ctrl+K opens the palette from Normal mode, and it takes typing.
        cx.simulate_keystrokes("ctrl-k");
        view.update(cx, |app, _| assert!(app.search.is_some()));
        cx.simulate_input("dd");
        view.update(cx, |app, _| {
            assert_eq!(app.search.as_ref().unwrap().query.text, "dd");
            assert_eq!(app.vim_label(), None);
        });
        cx.simulate_keystrokes("escape");
        assert_eq!(block_texts(&view, cx), ["see [[Alpha]]"]);

        // The rename field types plain text.
        click_block(cx, 0);
        right_click_page(&view, cx, "Alpha");
        click_on(cx, "page-menu-rename");
        cx.simulate_input("Gamma");
        cx.simulate_keystrokes("enter");
        assert!(page_file(&dir, "Gamma").exists());

        // Ctrl+G still toggles the graph from Normal mode.
        click_sidebar_page(&view, cx, "Test");
        click_block(cx, 0);
        cx.simulate_keystrokes("ctrl-g");
        view.update(cx, |app, _| assert_eq!(app.mode, Mode::Graph));
        let _ = std::fs::remove_dir_all(dir);
    }

    // --- publish (decision 49) -------------------------------------------------

    /// Every file under `dir`, relative, sorted.
    fn files_under(dir: &std::path::Path) -> Vec<String> {
        fn walk(base: &std::path::Path, dir: &std::path::Path, out: &mut Vec<String>) {
            for e in std::fs::read_dir(dir).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(base, &p, out);
                } else {
                    out.push(p.strip_prefix(base).unwrap().to_string_lossy().into_owned());
                }
            }
        }
        let mut out = Vec::new();
        walk(dir, dir, &mut out);
        out.sort();
        out
    }

    const PUBLISH_SECRET: &str = "sk-test-NOTESEC-PUBLISH-0123456789";

    #[gpui::test]
    fn publish_writes_a_self_contained_bundle_without_secrets(cx: &mut TestAppContext) {
        let pages = [
            (
                "Test",
                "- see [[Alpha]] and [[Private]]\n- ![pic](../assets/p.png)\n- ![[Alpha]]\n",
            ),
            ("Alpha", "- quoted text\n"),
            ("Private", "public:: false\n\n- dear diary\n"),
        ];
        let (view, cx, dir) = setup_pages(cx, "publish", &pages, "Test");
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(image::RgbImage::new(2, 2))
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        std::fs::write(dir.join("assets/p.png"), png.into_inner()).unwrap();
        // Secrets the graph folder holds: never published.
        let key_line = format!("ai_api_key = \"{PUBLISH_SECRET}\"\n");
        std::fs::write(dir.join("state.toml"), &key_line).unwrap();
        std::fs::write(dir.join("config.toml"), format!("# {PUBLISH_SECRET}\n")).unwrap();
        std::fs::create_dir_all(dir.join(".trash/1/pages")).unwrap();
        std::fs::write(
            dir.join(".trash/1/pages/Gone.md"),
            format!("- {PUBLISH_SECRET}\n"),
        )
        .unwrap();

        let loaded = view.update(cx, |app, _| app.pages.len());
        // The block being edited is saved first.
        click_block(cx, 0);
        run_in_palette(cx, "publish page with linked pages");
        view.update(cx, |app, _| assert!(app.editing.is_none()));
        cx.run_until_parked();
        let bundle = dir.join("published/test");
        assert_eq!(
            files_under(&bundle),
            [
                ".notesec-bundle",
                "README.txt",
                "alpha.html",
                "assets/p.png",
                "index.html",
                "style.css"
            ]
        );
        for file in files_under(&bundle) {
            let bytes = std::fs::read(bundle.join(&file)).unwrap();
            let text = String::from_utf8_lossy(&bytes);
            assert!(!text.contains(PUBLISH_SECRET), "{file}");
            assert!(!text.contains("dear diary"), "{file}");
        }
        // The test is only meaningful if the key was there all along.
        assert_eq!(
            std::fs::read_to_string(dir.join("state.toml")).unwrap(),
            key_line
        );
        let index = std::fs::read_to_string(bundle.join("index.html")).unwrap();
        assert!(index.contains("<a class=\"link\" href=\"alpha.html\" data-page=\"Alpha\">"));
        assert!(index.contains("<span class=\"link\" data-page=\"Private\">"));
        assert!(index.contains("<img src=\"assets/p.png\" alt=\"pic\">"));
        assert!(index.contains("data-page=\"Alpha\">Alpha</div><ul class=\"embed-outline\">"));

        // The status says where, with the folder's buttons.
        assert_eq!(
            status_text(&view, cx),
            Some(format!(
                "Published \u{201c}Test\u{201d} and 1 linked page to {}",
                bundle.display()
            ))
        );
        click_on(cx, "publish-open-folder");
        view.update(cx, |app, _| {
            assert_eq!(app.publish.opened, [bundle.clone()])
        });
        click_on(cx, "publish-copy-path");
        let copied = cx.read_from_clipboard().and_then(|item| item.text());
        assert_eq!(copied, Some(bundle.display().to_string()));

        // Published files are never read as pages, now or on the next start.
        view.update(cx, |app, _| assert_eq!(app.pages.len(), loaded));
        assert_eq!(Storage::open(dir.clone()).unwrap().load_all().len(), loaded);
        // Another status message: the buttons go with the publish status.
        view.update(cx, |app, cx| {
            app.show_status(
                Status {
                    text: "something else".into(),
                    error: false,
                },
                cx,
            )
        });
        cx.run_until_parked();
        assert!(has(cx, "status-toast") && !has(cx, "publish-open-folder"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn publish_refuses_private_pages_and_needs_a_page(cx: &mut TestAppContext) {
        let pages = [
            ("Secret", "private:: true\n\n- plans\n"),
            ("Open", "- hi\n"),
        ];
        let (view, cx, dir) = setup_pages(cx, "publish-private", &pages, "Secret");
        let offered = view.update(cx, |app, _| app.available_commands());
        assert!(offered.contains(&Command::PublishPage));
        assert!(offered.contains(&Command::PublishPageWithLinks));
        cx.dispatch_action(PublishPage);
        let status = view.update(cx, |app, _| app.status.clone()).unwrap();
        assert!(status.error);
        assert!(
            status.text.contains("\u{201c}Secret\u{201d} is private"),
            "{}",
            status.text
        );
        assert!(!dir.join("published").exists());
        assert!(!has(cx, "publish-open-folder"));

        // On the graph or trash tab there is no page: no command, and the
        // actions do nothing.
        for tab in ["graph", "trash"] {
            match tab {
                "graph" => cx.simulate_keystrokes("ctrl-g"),
                _ => click_on(cx, "sidebar-trash"),
            }
            let offered = view.update(cx, |app, _| app.available_commands());
            assert!(!offered.contains(&Command::PublishPage), "{tab}");
            assert!(!offered.contains(&Command::PublishPageWithLinks), "{tab}");
            cx.simulate_keystrokes("ctrl-k");
            cx.simulate_input("publish page");
            assert!(!has(cx, "command-PublishPage"));
            cx.simulate_keystrokes("escape");
            cx.dispatch_action(PublishPage);
            cx.dispatch_action(PublishPageWithLinks);
            assert!(!dir.join("published").exists());
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A small Obsidian vault outside the graph, for the import tests.
    fn obsidian_vault(name: &str) -> PathBuf {
        let vault =
            std::env::temp_dir().join(format!("notesec-vault-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&vault);
        std::fs::create_dir_all(vault.join("Sub")).unwrap();
        std::fs::create_dir_all(vault.join(".obsidian")).unwrap();
        std::fs::write(vault.join(".obsidian/app.json"), "{}").unwrap();
        std::fs::write(
            vault.join("Home.md"),
            "---\ntags: [project, big idea]\n---\n# Home\nSee [[Other|the other one]] and ![[pic.png]].\n",
        )
        .unwrap();
        std::fs::write(vault.join("Sub/Other.md"), "Back to [[Home]].\n").unwrap();
        std::fs::write(vault.join("Sub/pic.png"), b"\x89PNG\r\n\x1a\nfake").unwrap();
        vault
    }

    fn pick(cx: &mut VisualTestContext, folder: &std::path::Path) {
        assert!(cx.did_prompt_for_paths());
        let folder = folder.to_path_buf();
        cx.simulate_path_prompt_response(move |options| {
            assert!(options.directories && !options.files && !options.multiple);
            Some(vec![folder])
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    fn import_from_obsidian_loads_pages_assets_and_a_log(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "import-obsidian", &[("Test", "- a\n")], "Test");
        let vault = obsidian_vault("ui");
        let offered = view.update(cx, |app, _| app.available_commands());
        for command in [
            Command::ImportObsidian,
            Command::ImportLogseq,
            Command::ImportNotion,
        ] {
            assert!(offered.contains(&command), "{command:?}");
        }
        run_in_palette(cx, "import from obsidian");
        pick(cx, &vault);

        assert!(!has(cx, "import-clash"));
        let status = status_text(&view, cx).unwrap();
        assert!(
            status.starts_with("Imported 2 pages and 1 asset from Obsidian"),
            "{status}"
        );
        let (log, home) = view.update(cx, |app, _| {
            let log = app.current_page().unwrap();
            let home = app
                .pages
                .iter()
                .find(|p| p.title == "Home")
                .unwrap()
                .to_markdown();
            (log, home)
        });
        assert!(log.starts_with("Import from Obsidian "), "{log}");
        assert!(home.contains("tags:: #project, #[[big idea]]"), "{home}");
        assert!(
            home.contains(&format!("imported-from:: [[{log}]]")),
            "{home}"
        );
        assert!(
            home.contains("See [[Other]] and ![pic.png](../assets/pic.png)."),
            "{home}"
        );
        assert!(view.update(cx, |app, _| app.find_page("Other").is_some()));
        assert!(dir.join("assets/pic.png").exists());
        assert!(dir.join("pages/Home.md").exists());
        // Nothing from the hidden app folder.
        assert!(view.update(cx, |app, _| app.find_page("app").is_none()));
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(vault);
    }

    #[gpui::test]
    fn import_asks_before_renaming_or_skipping_taken_names(cx: &mut TestAppContext) {
        let pages = [("Test", "- a\n"), ("Home", "- mine\n")];
        let (view, cx, dir) = setup_pages(cx, "import-clash", &pages, "Test");
        let vault = obsidian_vault("clash");

        // Cancel: nothing is written.
        cx.dispatch_action(ImportObsidian);
        pick(cx, &vault);
        assert!(has(cx, "import-clash"));
        click_on(cx, "import-clash-cancel");
        assert!(!has(cx, "import-clash"));
        assert!(status_text(&view, cx).unwrap().contains("cancelled"));
        assert!(!dir.join("pages/Other.md").exists());
        assert!(!dir.join("assets").join("pic.png").exists());

        // Skip: Home stays mine, Other comes in and links to my Home.
        cx.dispatch_action(ImportObsidian);
        pick(cx, &vault);
        click_on(cx, "import-clash-skip");
        assert_eq!(
            std::fs::read_to_string(dir.join("pages/Home.md")).unwrap(),
            "- mine\n"
        );
        let status = status_text(&view, cx).unwrap();
        assert!(
            status.contains("Imported 1 page") && status.contains("skipped 1"),
            "{status}"
        );
        assert!(dir.join("pages/Other.md").exists());

        // Rename: both are taken now, and get the suffix; links follow.
        cx.dispatch_action(ImportObsidian);
        pick(cx, &vault);
        assert!(has(cx, "import-clash"));
        click_on(cx, "import-clash-rename");
        let status = status_text(&view, cx).unwrap();
        assert!(
            status.contains("Imported 2 pages") && status.contains("renamed 2"),
            "{status}"
        );
        let renamed = view.update(cx, |app, _| {
            app.pages
                .iter()
                .find(|p| p.title == "Other (imported)")
                .map(|p| p.to_markdown())
        });
        assert!(renamed.unwrap().contains("Back to [[Home (imported)]]."));
        assert_eq!(
            std::fs::read_to_string(dir.join("pages/Home.md")).unwrap(),
            "- mine\n"
        );
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(vault);
    }

    #[gpui::test]
    fn import_refuses_the_graph_itself_and_a_cancelled_picker_does_nothing(
        cx: &mut TestAppContext,
    ) {
        let (view, cx, dir) = setup_pages(cx, "import-refuse", &[("Test", "- a\n")], "Test");
        let before = view.update(cx, |app, _| app.pages.len());
        cx.dispatch_action(ImportLogseq);
        assert!(cx.did_prompt_for_paths());
        cx.simulate_path_prompt_response(|_| None);
        cx.run_until_parked();
        assert_eq!(view.update(cx, |app, _| app.pages.len()), before);

        cx.dispatch_action(ImportLogseq);
        pick(cx, &dir.join("pages"));
        let status = view.update(cx, |app, _| app.status.clone()).unwrap();
        assert!(status.error, "{}", status.text);
        assert!(
            status.text.contains("outside this graph"),
            "{}",
            status.text
        );

        // A Notion zip: unzip first.
        let zip = std::env::temp_dir().join(format!("notesec-export-{}.zip", std::process::id()));
        std::fs::write(&zip, b"PK\x03\x04").unwrap();
        cx.dispatch_action(ImportNotion);
        pick(cx, &zip);
        let status = view.update(cx, |app, _| app.status.clone()).unwrap();
        assert!(
            status.error && status.text.contains("unzip"),
            "{}",
            status.text
        );
        assert_eq!(view.update(cx, |app, _| app.pages.len()), before);
        let _ = std::fs::remove_file(zip);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn vim_keys_never_reach_a_block_behind_the_import_dialog(cx: &mut TestAppContext) {
        let pages = [("Test", "- keep me\n- second\n"), ("Home", "- mine\n")];
        let (view, cx, dir) = setup_pages(cx, "import-vim", &pages, "Test");
        let vault = obsidian_vault("vim");
        view.update(cx, |app, _| app.config.vim_mode = true);
        cx.dispatch_action(ImportObsidian);
        pick(cx, &vault);
        assert!(has(cx, "import-clash"));
        // A block in edit mode behind the dialog: the user clicked one while
        // the export was still being read (too quick to time in a test, so
        // made directly). Vim keys must not reach it.
        view.update_in(cx, |app, window, cx| app.start_edit(0, window, cx));
        cx.run_until_parked();
        cx.simulate_keystrokes("d d x p");
        cx.simulate_input("typed");
        assert_eq!(block_texts(&view, cx), ["keep me", "second"]);
        view.update(cx, |app, _| {
            assert!(!app.vim_applies());
            assert_eq!(app.vim_label(), None);
        });
        // Escape cancels the dialog (nothing imported).
        cx.simulate_keystrokes("escape");
        assert!(!has(cx, "import-clash"));
        assert!(!dir.join("pages/Other.md").exists());
        view.update(cx, |app, cx| app.stop_edit(cx));

        // Keys can't open a block behind the dialog either.
        cx.dispatch_action(ImportObsidian);
        pick(cx, &vault);
        assert!(has(cx, "import-clash"));
        cx.simulate_keystrokes("enter i o down d d");
        cx.simulate_input("x");
        view.update(cx, |app, _| assert_eq!(app.editing, None));
        assert_eq!(block_texts(&view, cx), ["keep me", "second"]);
        click_on(cx, "import-clash-cancel");
        // Showing the dialog closes an open block first.
        let root = dir.clone();
        view.update_in(cx, |app, window, cx| {
            app.start_edit(0, window, cx);
            let existing = app.import_existing();
            let plan =
                crate::import::plan(crate::import::Source::Obsidian, &vault, &root, &existing)
                    .unwrap();
            app.offer_import_clash(plan, cx);
            assert_eq!(app.editing, None);
        });
        cx.run_until_parked();
        assert!(has(cx, "import-clash"));
        click_on(cx, "import-clash-cancel");
        assert_eq!(
            std::fs::read_to_string(dir.join("pages/Test.md")).unwrap(),
            "- keep me\n- second\n"
        );
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(vault);
    }

    /// Send `raw` to the clipper on `port` from another thread while the
    /// app runs (it saves clips on its UI thread); the whole answer.
    fn post_clip(cx: &mut VisualTestContext, port: u16, raw: String) -> String {
        let client = std::thread::spawn(move || {
            use std::io::{Read, Write};
            let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(20)))
                .unwrap();
            stream.write_all(raw.as_bytes()).unwrap();
            let mut answer = String::new();
            let _ = stream.read_to_string(&mut answer);
            answer
        });
        for _ in 0..4000 {
            if client.is_finished() {
                break;
            }
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(200));
            cx.run_until_parked();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        client.join().unwrap()
    }

    fn clip_request(port: u16, content_type: &str, extra: &str, body: &str) -> String {
        format!(
            "POST /clip HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: {content_type}\r\n{extra}Content-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    #[gpui::test]
    fn the_web_clipper_saves_posted_pages_as_inbox_pages(cx: &mut TestAppContext) {
        let pages = [("Test", "- a\n"), ("Article", "- mine\n")];
        let (view, cx, dir) = setup_pages(cx, "clipper", &pages, "Test");
        // Off by default: nothing listens.
        view.update(cx, |app, _| {
            assert!(!app.config.web_clipper);
            assert_eq!(app.clipper_listening(), None);
        });
        cx.simulate_keystrokes("ctrl-,");
        click_on(cx, "settings-tab-clipper");
        assert!(has(cx, "settings-clipper"));
        // Port 0: any free port (the test mustn't take the real one).
        view.update(cx, |app, _| app.config.clipper_port = 0);
        click_on(cx, "clipper-on");
        let (port, token) = view.update(cx, |app, _| {
            (
                app.clipper_listening().unwrap(),
                app.state.clipper_token.clone(),
            )
        });
        assert_ne!(port, 0);
        assert_eq!(token.len(), 64);
        assert!(saved_config(&dir).web_clipper);
        let saved_state = std::fs::read_to_string(dir.join("state.toml")).unwrap();
        assert!(saved_state.contains(&format!("clipper_token = \"{token}\"")));
        click_on(cx, "clipper-copy-bookmarklet");
        let copied = cx.read_from_clipboard().and_then(|i| i.text()).unwrap();
        assert!(copied.starts_with("javascript:"));
        assert!(copied.contains(&format!("http://127.0.0.1:{port}/clip")));
        assert!(copied.contains(&token));
        cx.simulate_keystrokes("escape");

        // A form post (the bookmarklet's): a taken title gets " (2)".
        let html = "%3Ch1%3EBig%3C%2Fh1%3E%3Cp%3Etext+%3Ca+href%3D%22javascript%3Ax()%22+onclick%3D%22y()%22%3Elink%3C%2Fa%3E%3C%2Fp%3E%3Cscript%3Eevil()%3C%2Fscript%3E";
        let body = format!("token={token}&title=Article&url=https%3A%2F%2Fe.com%2Fa&html={html}");
        let answer = post_clip(
            cx,
            port,
            clip_request(port, "application/x-www-form-urlencoded", "", &body),
        );
        assert!(answer.starts_with("HTTP/1.1 200 OK"), "{answer}");
        assert!(answer.contains("Saved to NoteSec"));
        let today = today_title();
        assert_eq!(
            std::fs::read_to_string(page_file(&dir, "Article (2)")).unwrap(),
            format!(
                "- source:: https://e.com/a\n  clipped:: [[{today}]]\n  tags:: #clipped\n- # Big\n  - text link\n"
            )
        );
        assert_eq!(
            std::fs::read_to_string(page_file(&dir, "Article")).unwrap(),
            "- mine\n"
        );
        let journal = dir
            .join("journals")
            .join(format!("{}.md", today.replace('-', "_")));
        assert_eq!(
            std::fs::read_to_string(&journal).unwrap(),
            "- Clipped [[Article (2)]]\n"
        );
        assert_eq!(
            status_text(&view, cx).as_deref(),
            Some("Clipped \u{201c}Article (2)\u{201d}")
        );
        click_on(cx, "clipper-open");
        view.update(cx, |app, _| {
            assert_eq!(app.current_page().as_deref(), Some("Article (2)"))
        });

        // JSON (an extension's), token in the header; the inbox grows.
        let json =
            r#"{"title":"From JSON","url":"https://e.com/j","html":"<ul><li>one</li></ul>"}"#;
        let answer = post_clip(
            cx,
            port,
            clip_request(
                port,
                "application/json",
                &format!("X-NoteSec-Token: {token}\r\n"),
                json,
            ),
        );
        assert!(
            answer.ends_with(r#"{"ok":true,"title":"From JSON"}"#),
            "{answer}"
        );
        assert!(page_file(&dir, "From JSON").exists());
        assert_eq!(
            std::fs::read_to_string(&journal).unwrap(),
            "- Clipped [[Article (2)]]\n- Clipped [[From JSON]]\n"
        );

        // A wrong token: 403, nothing written.
        let body = "token=0000&title=Nope&html=x".to_string();
        let answer = post_clip(
            cx,
            port,
            clip_request(port, "application/x-www-form-urlencoded", "", &body),
        );
        assert!(answer.starts_with("HTTP/1.1 403"), "{answer}");
        assert!(!page_file(&dir, "Nope").exists());
        assert!(view.update(cx, |app, _| app.find_page("Nope").is_none()));

        // Regenerate: the old token stops working at once.
        cx.simulate_keystrokes("ctrl-,");
        click_on(cx, "settings-tab-clipper");
        click_on(cx, "clipper-regenerate");
        let (port, new_token) = view.update(cx, |app, _| {
            (
                app.clipper_listening().unwrap(),
                app.state.clipper_token.clone(),
            )
        });
        assert_ne!(new_token, token);
        let body = format!("token={token}&title=Old&html=x");
        let answer = post_clip(
            cx,
            port,
            clip_request(port, "application/x-www-form-urlencoded", "", &body),
        );
        assert!(answer.starts_with("HTTP/1.1 403"), "{answer}");
        assert!(!page_file(&dir, "Old").exists());

        // Off: the port closes.
        click_on(cx, "clipper-off");
        assert_eq!(view.update(cx, |app, _| app.clipper_listening()), None);
        assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_err());
        assert!(!saved_config(&dir).web_clipper);
        // The port buttons (while off, so nothing binds the real port).
        click_on(cx, "clipper-port-reset");
        assert_eq!(
            saved_config(&dir).clipper_port,
            crate::clipper::DEFAULT_PORT
        );
        click_on(cx, "clipper-port-down");
        assert_eq!(
            saved_config(&dir).clipper_port,
            crate::clipper::DEFAULT_PORT - 1
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn a_clipper_port_in_use_is_shown_not_a_panic(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "clipper-busy", &[("Test", "- a\n")], "Test");
        let holder = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let taken = holder.local_addr().unwrap().port();
        view.update(cx, |app, _| app.config.clipper_port = taken);
        cx.simulate_keystrokes("ctrl-,");
        click_on(cx, "settings-tab-clipper");
        click_on(cx, "clipper-on");
        let status = view.update(cx, |app, _| app.status.clone()).unwrap();
        assert!(status.error);
        assert!(
            status.text.contains(&format!("port {taken} is in use")),
            "{}",
            status.text
        );
        assert!(has(cx, "clipper-status"));
        assert_eq!(view.update(cx, |app, _| app.clipper_listening()), None);
        drop(holder);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Stub programs for voice notes (decision 52) in `dir/bin`: a
    /// recorder that writes a WAV with unset sizes (as a recorder that's
    /// interrupted early leaves it) and waits for SIGINT, a whisper that
    /// writes `<-of>.txt` (or fails), and a model file. Real processes,
    /// run with argument lists like the real ones.
    fn voice_stubs(dir: &std::path::Path, whisper_body: &str) -> (PathBuf, PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let mut wav = crate::voice::wav_bytes(&[100; 1600]);
        wav[4..8].copy_from_slice(&0u32.to_le_bytes());
        wav[40..44].copy_from_slice(&u32::MAX.to_le_bytes());
        let fixture = bin.join("fixture.wav");
        std::fs::write(&fixture, wav).unwrap();
        let script = |name: &str, body: String| {
            let path = bin.join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        };
        let recorder = script(
            "stub-recorder",
            format!(
                "trap 'exit 0' INT\ncp '{}' \"$2\"\nwhile true; do sleep 0.05; done",
                fixture.display()
            ),
        );
        let whisper = script("stub-whisper", whisper_body.to_string());
        let model = bin.join("ggml-test.bin");
        std::fs::write(&model, b"lmgg-model").unwrap();
        (recorder, whisper, model)
    }

    const WHISPER_OK: &str = r#"for a in "$@"; do [ "$prev" = "-of" ] && base="$a"; [ "$prev" = "-f" ] && wav="$a"; prev="$a"; done
[ -f "$wav" ] || exit 3
printf '[00:00:00.000 --> 00:00:01.000]   Buy milk [[and]] eggs.\n[00:00:01.000 --> 00:00:02.000]  [BLANK_AUDIO]\n' > "$base.txt""#;

    fn use_voice_stubs(
        view: &Entity<NoteSec>,
        cx: &mut VisualTestContext,
        dir: &std::path::Path,
        whisper_body: &str,
    ) {
        let (recorder, whisper, model) = voice_stubs(dir, whisper_body);
        view.update(cx, |app, _| {
            app.config.voice_recorder =
                vec![recorder.display().to_string(), "-o".into(), "{file}".into()];
            app.config.whisper_binary = whisper.display().to_string();
            app.config.whisper_model = model.display().to_string();
        });
    }

    fn voice_files(dir: &std::path::Path) -> Vec<String> {
        let mut files: Vec<String> = std::fs::read_dir(dir.join("assets"))
            .map(|d| {
                d.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| n.starts_with("voice-"))
                    .collect()
            })
            .unwrap_or_default();
        files.sort();
        files
    }

    /// Let the stub recorder start and write its file (real time).
    fn let_it_record(cx: &mut VisualTestContext) {
        std::thread::sleep(std::time::Duration::from_millis(300));
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(1));
        cx.run_until_parked();
    }

    #[gpui::test]
    fn a_voice_note_is_recorded_repaired_and_transcribed(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(
            cx,
            "voice-e2e",
            &[("Test", "- first\n  - child\n- last\n")],
            "Test",
        );
        use_voice_stubs(&view, cx, &dir, WHISPER_OK);
        view.update(cx, |app, _| app.config.voice_auto_transcribe = true);
        // Editing "first": the note goes below its subtree.
        click_block(cx, 0);
        assert!(!has(cx, "voice-pill"));
        cx.dispatch_action(RecordVoiceNote);
        assert!(view.update(cx, |app, _| app.recording()));
        assert!(has(cx, "voice-pill") && has(cx, "voice-stop") && has(cx, "voice-cancel"));
        let offered = view.update(cx, |app, _| app.available_commands());
        assert!(offered.contains(&Command::StopRecording));
        assert!(offered.contains(&Command::CancelRecording));
        assert!(!offered.contains(&Command::RecordVoiceNote));
        // Typing goes on while recording.
        cx.simulate_input("!");
        let_it_record(cx);
        let elapsed = view.update(cx, |app, _| app.voice.elapsed);
        assert!(
            elapsed >= std::time::Duration::from_millis(300),
            "{elapsed:?}"
        );

        click_on(cx, "voice-stop");
        cx.run_until_parked();
        assert!(!view.update(cx, |app, _| app.recording()));
        let files = voice_files(&dir);
        assert_eq!(files.len(), 1, "{files:?}");
        let name = &files[0];
        assert!(name.starts_with("voice-") && name.ends_with(".wav"));
        // The header was repaired (sizes were 0 / 0xFFFFFFFF).
        assert_eq!(
            std::fs::read(dir.join("assets").join(name)).unwrap(),
            crate::voice::wav_bytes(&[100; 1600])
        );
        let note = format!("![voice note](../assets/{name})");
        let transcript = "**Transcript:** Buy milk [\u{200b}[and]] eggs.";
        assert_eq!(
            block_texts(&view, cx),
            ["first!", "child", note.as_str(), transcript, "last"]
        );
        view.update(cx, |app, _| {
            let blocks = &app.pages[app.selected].blocks;
            assert_eq!(blocks[2].parent_id, None, "a sibling of `first`");
            assert_eq!(blocks[3].parent_id, Some(blocks[2].id), "under the note");
            assert_eq!(app.editing, None);
        });
        assert_eq!(
            file(&dir),
            format!("- first!\n  - child\n- {note}\n  - {transcript}\n- last\n")
        );
        assert!(!has(cx, "voice-pill"));
        let status = view.update(cx, |app, _| app.status.clone()).unwrap();
        assert!(status.text.starts_with("Transcribed"), "{}", status.text);

        // Reading view: a voice chip with Play (the system player).
        assert!(has(cx, "voice-2-0") && !has(cx, "image-2-0-missing"));
        click_on(cx, "voice-2-0-play");
        view.update(cx, |app, _| {
            assert_eq!(app.voice.opened, [dir.join("pages/../assets").join(name)]);
            assert_eq!(app.editing, None, "Play doesn't start editing");
        });
        // Transcribe again: one transcript only.
        click_on(cx, "voice-2-0-transcribe");
        cx.run_until_parked();
        assert_eq!(block_texts(&view, cx).len(), 5);
        let status = view.update(cx, |app, _| app.status.clone()).unwrap();
        assert!(
            status.text.contains("already has a transcript"),
            "{}",
            status.text
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn cancel_deletes_the_recording_and_keys_still_work(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(
            cx,
            "voice-cancel",
            &[("Test", "- one\n- two\n- three\n")],
            "Test",
        );
        use_voice_stubs(&view, cx, &dir, WHISPER_OK);
        view.update(cx, |app, _| app.config.vim_mode = true);
        click_block(cx, 0);
        cx.simulate_keystrokes("ctrl-k");
        cx.simulate_input("Record voice");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert!(view.update(cx, |app, _| app.recording()));
        let_it_record(cx);
        // Recorded outside the vault until finished.
        let temp = view.update(cx, |app, _| app.recording_file()).unwrap();
        assert!(temp.is_file() && !temp.starts_with(&dir));
        // In a folder only we can enter (decision 52).
        let private = temp.parent().unwrap().to_path_buf();
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&private).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }
        assert!(voice_files(&dir).is_empty());
        // The pill is not modal: vim keys edit as usual, and Escape
        // (vim's) doesn't stop the recording.
        click_block(cx, 0);
        view.update(cx, |app, _| {
            assert!(!app.overlay_open());
            assert!(app.vim_applies(), "vim still owns the keys");
        });
        cx.simulate_keystrokes("d d");
        assert_eq!(block_texts(&view, cx), ["two", "three"]);
        cx.simulate_keystrokes("escape");
        assert!(view.update(cx, |app, _| app.recording()));
        assert!(has(cx, "voice-pill"));

        click_on(cx, "voice-cancel");
        cx.run_until_parked();
        assert!(!view.update(cx, |app, _| app.recording()));
        assert!(!temp.exists(), "the recording is deleted");
        assert!(!private.exists(), "with its folder");
        assert!(voice_files(&dir).is_empty());
        assert_eq!(block_texts(&view, cx), ["two", "three"]);
        assert!(!has(cx, "voice-pill"));
        let status = view.update(cx, |app, _| app.status.clone()).unwrap();
        assert_eq!(status.text, "Recording cancelled");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn recording_from_the_sidebar_without_an_open_block(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "voice-sidebar", &[("Test", "- a\n")], "Test");
        use_voice_stubs(&view, cx, &dir, WHISPER_OK);
        click_on(cx, "sidebar-voice");
        assert!(view.update(cx, |app, _| app.recording()));
        let_it_record(cx);
        // The toggle stops it; no auto-transcription unless asked for.
        cx.dispatch_action(RecordVoiceNote);
        cx.run_until_parked();
        let name = voice_files(&dir).pop().unwrap();
        let note = format!("![voice note](../assets/{name})");
        assert_eq!(block_texts(&view, cx), ["a", note.as_str()]);
        // Transcribing the page's notes on request.
        cx.dispatch_action(TranscribeVoiceNotes);
        cx.run_until_parked();
        assert_eq!(
            block_texts(&view, cx)[2],
            "**Transcript:** Buy milk [\u{200b}[and]] eggs."
        );
        cx.dispatch_action(TranscribeVoiceNotes);
        let status = view.update(cx, |app, _| app.status.clone()).unwrap();
        assert!(
            status.text.starts_with("No voice notes without"),
            "{}",
            status.text
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn recording_stops_at_the_length_limit(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "voice-limit", &[("Test", "- a\n")], "Test");
        use_voice_stubs(&view, cx, &dir, WHISPER_OK);
        view.update(cx, |app, _| {
            app.voice.max_length = std::time::Duration::from_millis(200)
        });
        cx.dispatch_action(RecordVoiceNote);
        let_it_record(cx);
        cx.run_until_parked();
        assert!(!view.update(cx, |app, _| app.recording()));
        let status = view.update(cx, |app, _| app.status.clone()).unwrap();
        assert!(
            status
                .text
                .starts_with("Recording stopped at the 0:00 limit. Voice note saved"),
            "{}",
            status.text
        );
        assert_eq!(block_texts(&view, cx).len(), 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn voice_errors_are_shown(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "voice-errors", &[("Test", "- a\n")], "Test");
        // No recorder anywhere: the packages to install.
        let empty = dir.join("empty-bin");
        std::fs::create_dir_all(&empty).unwrap();
        view.update(cx, |app, _| {
            app.voice.path_var = Some(empty.clone().into_os_string())
        });
        cx.dispatch_action(RecordVoiceNote);
        let status = view.update(cx, |app, _| app.status.clone()).unwrap();
        assert!(
            status.error && status.text.contains("pipewire") && status.text.contains("alsa-utils")
        );
        assert!(!view.update(cx, |app, _| app.recording()));

        // A recorder that quits at once without audio: the file goes.
        use std::os::unix::fs::PermissionsExt;
        let quitter = empty.join("arecord");
        std::fs::write(
            &quitter,
            "#!/bin/sh\necho 'arecord: no such device' >&2\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&quitter, std::fs::Permissions::from_mode(0o755)).unwrap();
        cx.dispatch_action(RecordVoiceNote);
        assert!(view.update(cx, |app, _| app.recording()));
        let_it_record(cx);
        cx.run_until_parked();
        assert!(!view.update(cx, |app, _| app.recording()));
        let status = view.update(cx, |app, _| app.status.clone()).unwrap();
        assert!(status.error, "{}", status.text);
        assert!(
            status.text.contains("stopped by itself") && status.text.contains("no such device"),
            "{}",
            status.text
        );
        assert!(voice_files(&dir).is_empty());
        assert_eq!(block_texts(&view, cx), ["a"]);

        // Whisper failing: its exit code and stderr; no transcript.
        use_voice_stubs(
            &view,
            cx,
            &dir,
            "echo 'error: failed to load model' >&2\nexit 2",
        );
        cx.dispatch_action(RecordVoiceNote);
        let_it_record(cx);
        cx.dispatch_action(StopRecording);
        cx.run_until_parked();
        cx.dispatch_action(TranscribeVoiceNotes);
        cx.run_until_parked();
        let status = view.update(cx, |app, _| app.status.clone()).unwrap();
        assert!(
            status.error
                && status.text.contains("exit 2")
                && status.text.contains("failed to load model"),
            "{}",
            status.text
        );
        assert_eq!(block_texts(&view, cx).len(), 2);
        // A missing model.
        view.update(cx, |app, _| {
            app.config.whisper_model = "/nonexistent/m.bin".into()
        });
        cx.dispatch_action(TranscribeVoiceNotes);
        cx.run_until_parked();
        let status = view.update(cx, |app, _| app.status.clone()).unwrap();
        assert!(
            status.error && status.text.contains("model not found"),
            "{}",
            status.text
        );
        // No whisper program at all.
        view.update(cx, |app, _| app.config.whisper_binary.clear());
        cx.dispatch_action(TranscribeVoiceNotes);
        let status = view.update(cx, |app, _| app.status.clone()).unwrap();
        assert!(
            status.error && status.text.contains("Settings > Voice notes"),
            "{}",
            status.text
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn voice_settings_pick_and_test_whisper(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_pages(cx, "voice-settings", &[("Test", "- a\n")], "Test");
        let help =
            "case \"$1\" in --help) echo 'usage: whisper-cli [options] file0.wav'; exit 0;; esac";
        let (recorder, whisper, model) = voice_stubs(&dir, help);
        view.update(cx, |app, _| {
            app.config.voice_recorder = vec![recorder.display().to_string()]
        });
        cx.simulate_keystrokes("ctrl-,");
        click_on(cx, "settings-tab-voice");
        assert!(has(cx, "settings-voice") && has(cx, "voice-recorder"));
        let choose = |cx: &mut VisualTestContext, button: &str, path: PathBuf| {
            click_on(cx, button);
            assert!(cx.did_prompt_for_paths());
            cx.simulate_path_prompt_response(move |options| {
                assert!(options.files && !options.directories && !options.multiple);
                Some(vec![path])
            });
            cx.run_until_parked();
        };
        choose(cx, "voice-whisper-choose", whisper.clone());
        choose(cx, "voice-model-choose", model.clone());
        assert_eq!(
            saved_config(&dir).whisper_binary,
            whisper.display().to_string()
        );
        assert_eq!(
            saved_config(&dir).whisper_model,
            model.display().to_string()
        );
        click_on(cx, "voice-test");
        cx.run_until_parked();
        let check = view.update(cx, |app, _| app.voice.check.clone()).unwrap();
        assert_eq!(
            check,
            Ok("Ready: stub-whisper with ggml-test.bin (0 MB)".to_string())
        );
        assert!(has(cx, "voice-check"));
        click_on(cx, "voice-auto-on");
        assert!(saved_config(&dir).voice_auto_transcribe);
        click_on(cx, "voice-model-clear");
        assert_eq!(saved_config(&dir).whisper_model, "");
        click_on(cx, "voice-test");
        cx.run_until_parked();
        let check = view.update(cx, |app, _| app.voice.check.clone()).unwrap();
        assert!(check.unwrap_err().contains("no model"));
        let _ = std::fs::remove_dir_all(dir);
    }

    // --- Whiteboards (decision 53) ---------------------------------------------

    const BOARD: &str = "- type:: whiteboard\n\
        - Alpha\n  x:: 0\n  y:: 0\n  w:: 200\n  h:: 100\n\
        - [[Test]]\n  x:: 400\n  y:: 0\n  w:: 200\n  h:: 100\n";

    fn setup_board<'a>(
        cx: &'a mut TestAppContext,
        name: &str,
    ) -> (Entity<NoteSec>, &'a mut VisualTestContext, PathBuf) {
        let (view, cx, dir) = setup_pages(
            cx,
            name,
            &[("Board", BOARD), ("Test", "- first line\n- second\n")],
            "Board",
        );
        // The first frame learns the canvas size; the view is fitted to it.
        cx.run_until_parked();
        (view, cx, dir)
    }

    fn board(view: &Entity<NoteSec>, cx: &mut VisualTestContext) -> crate::whiteboard::Board {
        view.update(cx, |app, _| {
            crate::whiteboard::parse(&app.pages[app.selected])
        })
    }

    fn board_zoom(view: &Entity<NoteSec>, cx: &mut VisualTestContext) -> f32 {
        view.update(cx, |app, _| app.board_view().zoom)
    }

    fn board_file(dir: &std::path::Path) -> String {
        std::fs::read_to_string(dir.join("pages/Board.md")).unwrap()
    }

    fn mouse_drag(
        cx: &mut VisualTestContext,
        button: MouseButton,
        from: Point<Pixels>,
        to: Point<Pixels>,
    ) {
        cx.simulate_mouse_down(from, button, Modifiers::none());
        let mid = point((from.x + to.x) / 2.0, (from.y + to.y) / 2.0);
        cx.simulate_mouse_move(mid, Some(button), Modifiers::none());
        cx.simulate_mouse_move(to, Some(button), Modifiers::none());
        cx.simulate_mouse_up(to, button, Modifiers::none());
    }

    fn double_click(cx: &mut VisualTestContext, at: Point<Pixels>) {
        cx.simulate_event(MouseDownEvent {
            button: MouseButton::Left,
            position: at,
            modifiers: Modifiers::none(),
            click_count: 2,
            first_mouse: false,
        });
        cx.simulate_event(MouseUpEvent {
            button: MouseButton::Left,
            position: at,
            modifiers: Modifiers::none(),
            click_count: 2,
        });
    }

    /// A spot on the canvas with no card: its bottom left corner.
    fn empty_spot(cx: &mut VisualTestContext) -> Point<Pixels> {
        let canvas = bounds_of(cx, "whiteboard");
        point(
            canvas.origin.x + px(60.),
            canvas.origin.y + canvas.size.height - px(60.),
        )
    }

    fn near(a: f32, b: f32) -> bool {
        (a - b).abs() < 1.0
    }

    #[gpui::test]
    fn whiteboard_cards_move_and_resize_in_one_undo_step_each(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_board(cx, "wb-move");
        assert!(has(cx, "whiteboard") && has(cx, "wb-card-0") && has(cx, "wb-card-1"));
        assert!(!has(cx, "block-0"), "the canvas, not the outline");
        let zoom = board_zoom(&view, cx);
        let undo = view.update(cx, |app, _| app.undo_stack.len());

        let from = bounds_of(cx, "wb-card-0").center();
        mouse_drag(cx, MouseButton::Left, from, from + point(px(50.), px(30.)));
        let r = board(&view, cx).cards[0].rect;
        assert!(
            near(r.x, (50.0 / zoom).round()) && near(r.y, (30.0 / zoom).round()),
            "{r:?}"
        );
        assert!(
            board_file(&dir).contains(&format!("x:: {}\n", r.x)),
            "saved on release"
        );
        assert_eq!(
            view.update(cx, |app, _| app.undo_stack.len()),
            undo + 1,
            "one step"
        );

        let handle = bounds_of(cx, "wb-resize").center();
        mouse_drag(
            cx,
            MouseButton::Left,
            handle,
            handle + point(px(40.), px(20.)),
        );
        let r2 = board(&view, cx).cards[0].rect;
        assert!(
            near(r2.w, 200.0 + 40.0 / zoom) && near(r2.h, 100.0 + 20.0 / zoom),
            "{r2:?}"
        );
        assert_eq!((r2.x, r2.y), (r.x, r.y));
        assert_eq!(view.update(cx, |app, _| app.undo_stack.len()), undo + 2);

        cx.simulate_keystrokes("ctrl-z");
        assert_eq!(board(&view, cx).cards[0].rect, r);
        cx.simulate_keystrokes("ctrl-z");
        assert_eq!(
            board(&view, cx).cards[0].rect,
            crate::whiteboard::geom::Rect::new(0., 0., 200., 100.)
        );
        assert!(board_file(&dir).contains("- Alpha\n  x:: 0\n  y:: 0\n"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn whiteboard_double_click_creates_and_edits_cards(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_board(cx, "wb-create");
        let blocks = view.update(cx, |app, _| app.pages[app.selected].blocks.len());
        let spot = empty_spot(cx);
        double_click(cx, spot);
        assert_eq!(board(&view, cx).cards.len(), 3);
        assert!(has(cx, "wb-card-editor"), "the new card is being edited");
        cx.simulate_input("Hello [[Test]]");
        cx.simulate_keystrokes("enter");
        let card = board(&view, cx).cards[2].clone();
        assert_eq!(card.text, "Hello [[Test]]", "Enter finishes the card");
        assert!(card.placed);
        assert_eq!(view.update(cx, |app, _| app.editing), None);
        assert_eq!(
            view.update(cx, |app, _| app.pages[app.selected].blocks.len()),
            blocks + 1
        );
        // The card is where the double-click was.
        let at = view.update(cx, |app, _| {
            let local = app.local(spot).unwrap();
            app.board_view().to_world(local)
        });
        assert!(card.rect.contains(at), "{:?} {at:?}", card.rect);
        assert!(board_file(&dir).contains("- Hello [[Test]]\n  x:: "));

        // Double-click on a card edits its text alone; its place stays.
        let before = board(&view, cx).cards[0].rect;
        let at = bounds_of(cx, "wb-card-0").center();
        double_click(cx, at);
        assert_eq!(view.update(cx, |app, _| app.editor.text.clone()), "Alpha");
        cx.simulate_input("!");
        cx.simulate_keystrokes("escape");
        let alpha = board(&view, cx).cards[0].clone();
        assert_eq!((alpha.text.as_str(), alpha.rect), ("Alpha!", before));

        // Delete removes the selected card; undo brings it back.
        click_on(cx, "wb-card-2");
        cx.simulate_keystrokes("delete");
        assert_eq!(board(&view, cx).cards.len(), 2);
        assert!(!board_file(&dir).contains("Hello"));
        cx.simulate_keystrokes("ctrl-z");
        assert_eq!(board(&view, cx).cards.len(), 3);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn whiteboard_cards_connect_and_arrows_delete(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_board(cx, "wb-connect");
        let a = bounds_of(cx, "wb-card-0");
        let b = bounds_of(cx, "wb-card-1");
        cx.simulate_click(a.center(), Modifiers::none());
        let handle = bounds_of(cx, "wb-connect").center();
        mouse_drag(cx, MouseButton::Left, handle, b.center());
        let g = board(&view, cx);
        assert_eq!(g.edges.len(), 1);
        assert_eq!(
            (g.edges[0].from, g.edges[0].to),
            (g.cards[0].id, g.cards[1].id)
        );
        let saved = board_file(&dir);
        assert!(
            saved.contains(&format!(
                "edge:: (({})) -> (({}))",
                g.cards[0].id, g.cards[1].id
            )),
            "{saved}"
        );
        assert!(
            saved.contains(&format!("id:: {}", g.cards[0].id)),
            "ids kept for the refs"
        );
        let edge = g.edges[0].id;
        assert_eq!(
            view.update(cx, |app, _| app.whiteboard.selected),
            Some(whiteboard_ui::Selection::Edge(edge))
        );

        // Backspace deletes the selected arrow.
        cx.simulate_keystrokes("backspace");
        assert!(board(&view, cx).edges.is_empty());
        cx.simulate_keystrokes("ctrl-z");
        assert_eq!(board(&view, cx).edges.len(), 1);

        // Clicking the arrow selects it.
        let spot = empty_spot(cx);
        cx.simulate_click(spot, Modifiers::none());
        assert_eq!(view.update(cx, |app, _| app.whiteboard.selected), None);
        let mid = point((a.right() + b.left()) / 2.0, a.center().y);
        cx.simulate_click(mid, Modifiers::none());
        assert_eq!(
            view.update(cx, |app, _| app.whiteboard.selected),
            Some(whiteboard_ui::Selection::Edge(edge))
        );
        cx.simulate_keystrokes("delete");
        assert!(board(&view, cx).edges.is_empty());

        // Deleting a card takes its arrows along.
        cx.simulate_keystrokes("ctrl-z");
        cx.simulate_click(a.center(), Modifiers::none());
        cx.simulate_keystrokes("delete");
        let g = board(&view, cx);
        assert_eq!((g.cards.len(), g.edges.len()), (1, 0));
        assert!(!board_file(&dir).contains("edge::"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn whiteboard_zooms_around_the_pointer_and_pans(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_board(cx, "wb-zoom");
        let at = bounds_of(cx, "whiteboard").center();
        let world = |view: &Entity<NoteSec>, cx: &mut VisualTestContext| {
            view.update(cx, |app, _| {
                app.board_view().to_world(app.local(at).unwrap())
            })
        };
        let (z0, w0) = (board_zoom(&view, cx), world(&view, cx));
        let ctrl = Modifiers {
            control: true,
            ..Modifiers::none()
        };
        cx.simulate_event(ScrollWheelEvent {
            position: at,
            delta: ScrollDelta::Pixels(point(px(0.), px(100.))),
            modifiers: ctrl,
            touch_phase: TouchPhase::Moved,
        });
        let (z1, w1) = (board_zoom(&view, cx), world(&view, cx));
        assert!(z1 > z0 * 1.5, "{z0} -> {z1}");
        assert!(
            (w1.x - w0.x).abs() < 0.5 && (w1.y - w0.y).abs() < 0.5,
            "the point stays put"
        );
        for _ in 0..20 {
            cx.simulate_event(ScrollWheelEvent {
                position: at,
                delta: ScrollDelta::Lines(point(0., 10.)),
                modifiers: ctrl,
                touch_phase: TouchPhase::Moved,
            });
        }
        assert_eq!(board_zoom(&view, cx), crate::whiteboard::geom::MAX_ZOOM);
        cx.simulate_event(gpui::PinchEvent {
            position: at,
            delta: -0.5,
            modifiers: Modifiers::none(),
            phase: TouchPhase::Moved,
        });
        assert_eq!(
            board_zoom(&view, cx),
            crate::whiteboard::geom::MAX_ZOOM / 2.0
        );

        click_on(cx, "wb-zoom-reset");
        assert_eq!(board_zoom(&view, cx), 1.0);
        click_on(cx, "wb-fit");
        assert!(
            has(cx, "wb-card-0") && has(cx, "wb-card-1"),
            "fit shows every card"
        );
        let canvas = bounds_of(cx, "whiteboard");
        for card in ["wb-card-0", "wb-card-1"] {
            let b = bounds_of(cx, card);
            assert!(
                b.left() >= canvas.left() && b.right() <= canvas.right(),
                "{card} in view"
            );
        }

        // Plain wheel and a middle-button drag pan.
        let o = view.update(cx, |app, _| app.board_view().offset);
        cx.simulate_event(ScrollWheelEvent {
            position: at,
            delta: ScrollDelta::Pixels(point(px(10.), px(-20.))),
            modifiers: Modifiers::none(),
            touch_phase: TouchPhase::Moved,
        });
        let o2 = view.update(cx, |app, _| app.board_view().offset);
        assert_eq!((o2.x - o.x, o2.y - o.y), (10.0, -20.0));
        mouse_drag(cx, MouseButton::Middle, at, at + point(px(30.), px(40.)));
        let o3 = view.update(cx, |app, _| app.board_view().offset);
        assert!(near(o3.x - o2.x, 30.0) && near(o3.y - o2.y, 40.0));
        // Panning the empty canvas with the left button, too.
        let spot = empty_spot(cx);
        mouse_drag(cx, MouseButton::Left, spot, spot + point(px(-15.), px(5.)));
        let o4 = view.update(cx, |app, _| app.board_view().offset);
        assert!(near(o4.x - o3.x, -15.0) && near(o4.y - o3.y, 5.0));
        assert_eq!(board_file(&dir), BOARD, "viewing changes nothing on disk");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn whiteboard_first_view_is_refitted_to_the_real_canvas(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_board(cx, "wb-refit");
        // Drawn before the canvas had a size; the next frame fits it.
        let ran = cx.update(|window, cx| window.simulate_next_frame(cx));
        assert!(ran >= 1);
        cx.run_until_parked();
        let canvas = bounds_of(cx, "whiteboard");
        let fitted = view.update(cx, |app, _| {
            let g = crate::whiteboard::parse(&app.pages[app.selected]);
            crate::whiteboard::geom::Viewport::fit(
                &g.rects(),
                f32::from(canvas.size.width),
                f32::from(canvas.size.height),
            )
        });
        assert_eq!(view.update(cx, |app, _| app.board_view()), fitted);
        let (a, b) = (bounds_of(cx, "wb-card-0"), bounds_of(cx, "wb-card-1"));
        let middle = (a.left() + b.right()) / 2.0;
        assert!(
            (f32::from(middle - canvas.center().x)).abs() < 1.0,
            "centred"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn whiteboard_page_cards_open_their_page(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_board(cx, "wb-page-card");
        assert!(has(cx, "wb-card-1-title"));
        click_on(cx, "wb-card-1-title");
        assert_eq!(
            view.update(cx, |app, _| app.pages[app.selected].title.clone()),
            "Test"
        );
        assert!(has(cx, "block-0") && !has(cx, "whiteboard"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn whiteboard_add_page_uses_the_palette(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_board(cx, "wb-add-page");
        click_on(cx, "wb-add-page");
        assert!(view.update(cx, |app, _| app.search.is_some()));
        cx.simulate_input("Test");
        cx.simulate_keystrokes("enter");
        let g = board(&view, cx);
        assert_eq!(g.cards.len(), 3);
        assert_eq!(
            g.cards[2].kind,
            crate::whiteboard::CardKind::Page("Test".into())
        );
        assert_eq!(
            view.update(cx, |app, _| app.pages[app.selected].title.clone()),
            "Board"
        );
        // An ordinary palette pick afterwards just opens the page.
        cx.simulate_keystrokes("ctrl-k");
        cx.simulate_input("Test");
        cx.simulate_keystrokes("enter");
        assert_eq!(
            view.update(cx, |app, _| app.pages[app.selected].title.clone()),
            "Test"
        );
        cx.simulate_keystrokes("ctrl-z");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn whiteboard_keys_never_reach_the_outline_or_vim(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_board(cx, "wb-keys");
        view.update(cx, |app, _| app.config.vim_mode = true);
        let page = |view: &Entity<NoteSec>, cx: &mut VisualTestContext| {
            view.update(cx, |app, _| app.pages[app.selected].to_markdown())
        };
        let before = page(&view, cx);
        let spot = empty_spot(cx);
        cx.simulate_click(spot, Modifiers::none());
        assert!(view.update(cx, |app, _| app.whiteboard_focused()));
        cx.simulate_keystrokes("d d o enter tab shift-tab delete backspace alt-up");
        assert_eq!(
            page(&view, cx),
            before,
            "no outline or vim edits on the canvas"
        );

        // Editing a card: plain typing, no blocks split, indented or moved.
        let at = bounds_of(cx, "wb-card-0").center();
        double_click(cx, at);
        assert!(view.update(cx, |app, _| app.card_editing() && !app.vim_applies()));
        cx.simulate_keystrokes("tab alt-down down up");
        assert!(
            view.update(cx, |app, _| app.editing == Some(1)),
            "still on the card"
        );
        cx.simulate_input("dd");
        cx.simulate_keystrokes("enter");
        let after = view.update(cx, |app, _| app.pages[app.selected].clone());
        assert_eq!(after.blocks.len(), 3);
        assert!(after.blocks.iter().all(|b| b.parent_id.is_none()));
        assert_eq!(board(&view, cx).cards[0].text, "Alphadd");

        // Delete only acts on the canvas: not while the palette has the keys.
        click_on(cx, "wb-card-0");
        cx.simulate_keystrokes("ctrl-k");
        cx.simulate_keystrokes("delete backspace");
        assert_eq!(board(&view, cx).cards.len(), 2);
        cx.simulate_keystrokes("escape");
        assert!(view.update(cx, |app, _| app.whiteboard.selected.is_some()));
        cx.simulate_keystrokes("escape");
        assert_eq!(
            view.update(cx, |app, _| app.whiteboard.selected),
            None,
            "Esc deselects"
        );
        cx.simulate_keystrokes("delete");
        assert_eq!(
            board(&view, cx).cards.len(),
            2,
            "nothing selected, nothing deleted"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn whiteboard_opens_as_outline_and_new_ones_are_made(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup_board(cx, "wb-outline");
        click_on(cx, "wb-outline");
        assert!(has(cx, "block-1") && !has(cx, "whiteboard"));
        // As an outline the card's properties are ordinary text.
        click_block(cx, 1);
        assert!(view.update(cx, |app, _| app.editor.text.contains("x:: 0")));
        cx.simulate_keystrokes("escape");
        cx.dispatch_action(ToggleWhiteboardOutline);
        assert!(has(cx, "whiteboard"));

        cx.dispatch_action(NewWhiteboard);
        cx.dispatch_action(NewWhiteboard);
        let (title, board) = view.update(cx, |app, _| {
            let page = &app.pages[app.selected];
            (page.title.clone(), crate::whiteboard::is_whiteboard(page))
        });
        assert_eq!(title, "Whiteboard 1");
        assert!(board && has(cx, "whiteboard"));
        let saved = std::fs::read_to_string(dir.join("pages/Whiteboard.md")).unwrap();
        assert!(saved.starts_with("- type:: whiteboard"), "{saved}");
        let _ = std::fs::remove_dir_all(dir);
    }

    // --- Encrypted vaults (decision 54) ----------------------------------------

    fn vault_dialog(
        view: &Entity<NoteSec>,
        cx: &mut VisualTestContext,
    ) -> Option<(String, Option<String>, bool)> {
        view.update(cx, |app, _| {
            app.vault
                .dialog
                .as_ref()
                .map(|d| (d.field().text.clone(), d.error.clone(), d.busy))
        })
    }

    #[gpui::test]
    fn vault_export_and_import_through_the_dialog(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "vault-ui", "- hello vault\n");
        std::fs::write(dir.join("state.toml"), "clipper_token = \"tok-SECRET\"\n").unwrap();
        view.update(cx, |app, _| app.config.vim_mode = true);
        let out = std::env::temp_dir().join(format!("notesec-vault-ui-out-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&out);
        std::fs::create_dir_all(out.join("dest")).unwrap();

        cx.dispatch_action(ExportVault);
        assert!(has(cx, "vault-dialog") && has(cx, "vault-pass") && has(cx, "vault-confirm"));
        assert!(view.update(cx, |app, _| app.overlay_open() && !app.vim_applies()));
        // Too short: Enter goes to the second field, then refuses.
        cx.simulate_input("dd short");
        cx.simulate_keystrokes("enter");
        cx.simulate_input("dd short");
        cx.simulate_keystrokes("enter");
        let (_, error, _) = vault_dialog(&view, cx).unwrap();
        assert!(error.unwrap().contains("At least 12"));
        assert_eq!(
            file(&dir),
            "- hello vault\n",
            "typing went to the field, not vim"
        );
        // Esc closes the dialog and wipes the fields.
        cx.simulate_keystrokes("escape");
        assert!(vault_dialog(&view, cx).is_none() && !has(cx, "vault-dialog"));

        cx.dispatch_action(ExportVault);
        assert_eq!(vault_dialog(&view, cx).unwrap().0, "", "a fresh dialog");
        cx.simulate_input("correct horse battery");
        cx.simulate_keystrokes("tab");
        cx.simulate_input("correct horse battery?");
        cx.simulate_keystrokes("enter");
        assert!(vault_dialog(&view, cx)
            .unwrap()
            .1
            .unwrap()
            .contains("don't match"));
        cx.simulate_keystrokes("backspace enter");
        assert!(
            vault_dialog(&view, cx).unwrap().2,
            "busy: the save picker is open"
        );
        cx.simulate_new_path_selection(|_| Some(out.join("mine")));
        cx.run_until_parked();
        assert!(vault_dialog(&view, cx).is_none());
        let file_path = out.join("mine.notesec-vault");
        let bytes = std::fs::read(&file_path).unwrap();
        assert!(bytes.starts_with(crate::vault::MAGIC));
        let status = view.update(cx, |app, _| app.status.clone().unwrap().text);
        assert!(status.contains("mine.notesec-vault"), "{status}");

        // Import: the file, then a new empty folder, then the passphrase.
        cx.dispatch_action(ImportVault);
        cx.simulate_path_prompt_response(|o| o.files.then(|| vec![file_path.clone()]));
        cx.run_until_parked();
        let dest = out.join("dest");
        cx.simulate_path_prompt_response(|o| o.directories.then(|| vec![dest.clone()]));
        cx.run_until_parked();
        assert!(has(cx, "vault-pass") && !has(cx, "vault-confirm"));
        cx.simulate_input("not the passphrase");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        let (text, error, busy) = vault_dialog(&view, cx).unwrap();
        assert!(error.unwrap().contains("Wrong passphrase") && !busy && text.is_empty());
        assert_eq!(
            std::fs::read_dir(&dest).unwrap().count(),
            0,
            "nothing written"
        );
        cx.simulate_input("correct horse battery");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert!(vault_dialog(&view, cx).is_none());
        assert_eq!(
            std::fs::read_to_string(dest.join("pages/Test.md")).unwrap(),
            "- hello vault\n"
        );
        assert!(!std::fs::read_to_string(dest.join("state.toml"))
            .unwrap()
            .contains("SECRET"));
        let status = view.update(cx, |app, _| app.status.clone().unwrap().text);
        assert!(status.contains("NOTESEC_DIR="), "{status}");

        // The open notes' own folder is refused as a destination.
        cx.dispatch_action(ImportVault);
        cx.simulate_path_prompt_response(|o| o.files.then(|| vec![file_path.clone()]));
        cx.run_until_parked();
        let graph = dir.clone();
        cx.simulate_path_prompt_response(move |_| Some(vec![graph]));
        cx.run_until_parked();
        assert!(vault_dialog(&view, cx).is_none());
        let status = view.update(cx, |app, _| app.status.clone().unwrap());
        assert!(status.error && status.text.contains("open notes"));
        let _ = std::fs::remove_dir_all(out);
        let _ = std::fs::remove_dir_all(dir);
    }

    // --- Plugins (decision 55) ------------------------------------------------

    const WORD_COUNT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/plugins/word-count");

    /// Put a plugin in `<dir>/plugins/<id>/` and reload the plugins.
    fn install_plugin(
        view: &Entity<NoteSec>,
        cx: &mut VisualTestContext,
        dir: &std::path::Path,
        id: &str,
        manifest: &str,
        wasm: &[u8],
    ) {
        let folder = dir.join("plugins").join(id);
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("plugin.toml"), manifest).unwrap();
        std::fs::write(folder.join("plugin.wasm"), wasm).unwrap();
        view.update(cx, |app, cx| {
            app.reload_plugins();
            cx.notify();
        });
        cx.run_until_parked();
    }

    fn install_word_count(
        view: &Entity<NoteSec>,
        cx: &mut VisualTestContext,
        dir: &std::path::Path,
    ) {
        let manifest = std::fs::read_to_string(format!("{WORD_COUNT}/plugin.toml")).unwrap();
        let wasm = std::fs::read(format!("{WORD_COUNT}/plugin.wasm")).unwrap();
        install_plugin(view, cx, dir, "word-count", &manifest, &wasm);
    }

    /// A plugin `id` with one command "Go" whose `run_command` body is
    /// `body`; its memory holds `output` at offset 100.
    fn test_plugin(output: &str, body: &str) -> Vec<u8> {
        wat::parse_str(format!(
            r#"(module
                 (memory (export "memory") 1)
                 (data (i32.const 100) "{output}")
                 (func (export "alloc") (param i32) (result i32) (i32.const 4096))
                 (func (export "run_command") (param i32 i32) (result i64) {body}))"#
        ))
        .unwrap()
    }

    fn test_manifest(id: &str) -> String {
        format!(
            "id = \"{id}\"\nname = \"{id}\"\nversion = \"1\"\napi_version = 1\n\
             [[commands]]\nid = \"go\"\nlabel = \"Go {id}\"\n"
        )
    }

    fn plugin_hits(view: &Entity<NoteSec>, cx: &mut VisualTestContext) -> usize {
        view.update(cx, |app, _| {
            app.search_results()
                .iter()
                .filter(|h| matches!(h.target, Target::Plugin(_)))
                .count()
        })
    }

    fn run_palette(cx: &mut VisualTestContext, query: &str) {
        cx.simulate_keystrokes("ctrl-k");
        cx.simulate_input(query);
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
    }

    #[gpui::test]
    fn word_count_plugin_is_off_until_enabled_then_counts_and_renders(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(
            cx,
            "plugin-word-count",
            "- one two three\n- {{word-count}} here\n",
        );
        install_word_count(&view, cx, &dir);
        // Found, but disabled: no palette entry, no render box.
        cx.simulate_keystrokes("ctrl-k");
        cx.simulate_input("count words");
        assert_eq!(plugin_hits(&view, cx), 0);
        cx.simulate_keystrokes("escape");
        assert!(!has(cx, "plugin-render-word-count"));
        assert!(saved_config(&dir).plugins.is_empty());

        cx.simulate_keystrokes("ctrl-,");
        click_on(cx, "settings-tab-plugins");
        assert!(has(cx, "plugins-reload"));
        click_on(cx, "plugin-toggle-word-count");
        let hash = view.update(cx, |app, _| app.plugins.found[0].hash.clone());
        assert_eq!(saved_config(&dir).plugins.get("word-count"), Some(&hash));
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();

        // The render hook draws under the block with the macro.
        assert!(has(cx, "plugin-render-word-count"));
        view.update(cx, |app, _| {
            assert_eq!(app.plugin_render_texts(), ["2 words"])
        });

        click_block(cx, 0);
        cx.simulate_keystrokes("ctrl-k");
        cx.simulate_input("count words");
        assert_eq!(plugin_hits(&view, cx), 1);
        view.update(cx, |app, _| {
            assert_eq!(app.search_results()[0].target, Target::Plugin(0))
        });
        assert!(has(cx, "plugin-command-0"));
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(
            status_text(&view, cx).as_deref(),
            Some("Word count: Words: 3")
        );
        view.update(cx, |app, _| assert_eq!(app.editing, Some(0)));
        assert_eq!(file(&dir), "- one two three\n- {{word-count}} here\n");

        // Turning it off again is saved too.
        cx.simulate_keystrokes("escape ctrl-,");
        click_on(cx, "settings-tab-plugins");
        click_on(cx, "plugin-toggle-word-count");
        assert!(saved_config(&dir).plugins.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn a_changed_plugin_binary_must_be_enabled_again(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "plugin-changed", "- a\n");
        install_word_count(&view, cx, &dir);
        view.update(cx, |app, cx| app.set_plugin_enabled("word-count", true, cx));
        assert_eq!(view.update(cx, |app, _| app.plugin_commands().len()), 1);
        let manifest = std::fs::read_to_string(format!("{WORD_COUNT}/plugin.toml")).unwrap();
        let other = test_plugin("", "(i64.const 0)");
        install_plugin(&view, cx, &dir, "word-count", &manifest, &other);
        view.update(cx, |app, _| {
            assert!(app.plugin_commands().is_empty());
            assert!(!app.plugin_enabled(&app.plugins.found[0]));
        });
        cx.simulate_keystrokes("ctrl-,");
        click_on(cx, "settings-tab-plugins");
        click_on(cx, "plugin-toggle-word-count");
        assert_eq!(view.update(cx, |app, _| app.plugin_commands().len()), 1);
        assert_eq!(
            saved_config(&dir).plugins.get("word-count"),
            Some(&crate::plugins::wasm_hash(&other))
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn a_plugin_failing_three_times_is_turned_off(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "plugin-failing", "- a\n");
        install_plugin(
            &view,
            cx,
            &dir,
            "crash",
            &test_manifest("crash"),
            &test_plugin("", "unreachable"),
        );
        view.update(cx, |app, cx| app.set_plugin_enabled("crash", true, cx));
        for n in 1..=2 {
            run_palette(cx, "go crash");
            let status = status_text(&view, cx).unwrap();
            assert!(
                status.contains("crash failed") && status.contains("crashed"),
                "{status}"
            );
            view.update(cx, |app, _| assert_eq!(app.plugin_failures("crash"), n));
        }
        run_palette(cx, "go crash");
        assert!(status_text(&view, cx).unwrap().contains("turned off"));
        view.update(cx, |app, _| assert!(app.plugin_commands().is_empty()));
        assert!(saved_config(&dir).plugins.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn plugin_edits_are_saved_and_each_is_one_undo_step(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "plugin-edits", "- old\n- other\n");
        let output = r#"[[actions]]\0atype = \"replace_block\"\0atext = \"REPLACED\"\0a[[actions]]\0atype = \"insert_block\"\0atext = \"NEW\"\0a"#;
        let len = output.replace("\\0a", "\n").replace("\\\"", "\"").len();
        let body = format!("(i64.or (i64.shl (i64.const 100) (i64.const 32)) (i64.const {len}))");
        install_plugin(
            &view,
            cx,
            &dir,
            "edit",
            &test_manifest("edit"),
            &test_plugin(output, &body),
        );
        view.update(cx, |app, cx| app.set_plugin_enabled("edit", true, cx));
        click_block(cx, 0);
        run_palette(cx, "go edit");
        assert_eq!(file(&dir), "- REPLACED\n- NEW\n- other\n");
        cx.simulate_keystrokes("escape ctrl-z");
        assert_eq!(file(&dir), "- REPLACED\n- other\n");
        cx.simulate_keystrokes("ctrl-z");
        assert_eq!(file(&dir), "- old\n- other\n");

        // With no block being edited, replace_block is refused; the insert
        // goes after the last top-level block.
        view.update(cx, |app, cx| {
            app.commit();
            app.editing = None;
            app.run_plugin_command(0, cx);
        });
        cx.run_until_parked();
        assert!(status_text(&view, cx).unwrap().contains("needs a block"));
        assert_eq!(file(&dir), "- old\n- other\n- NEW\n");
        let _ = std::fs::remove_dir_all(dir);
    }
}
