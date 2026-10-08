//! The root view: sidebar of pages on the left, selected page on the right.
//!
//! Editing model: there is ONE shared `EditorState`. Clicking a block loads that
//! block's text into it; every other block is drawn as plain text. When editing
//! stops (Escape, click elsewhere, switching pages) the text is written back to
//! the block and the page is saved to disk.

use crate::config::{Config, ThemeKind};
use crate::display::DisplayBlock;
use crate::editor::{EditorState, Emphasis, SlashMenu};
use crate::graph_view::{GraphEvent, GraphView};
use crate::model::{
    backlinks, cycle_task, find_block, parse_block_refs, parse_references, tag_counts, BlockKind,
    Page, TaskState,
};
use crate::search::{search, search_blocks, search_templates, Command, Hit, Target};
use crate::state::UiState;
use crate::storage::{today_title, Storage, Template};
use crate::tabs::{TabTarget, Tabs};
use crate::ui::{
    block_row, drag_handle, drop_line, favorite_star, fold_arrow, fold_badge, task_checkbox,
    BlockDragPreview, Theme,
};
use gpui::{
    actions, anchored, deferred, div, fill, point, prelude::*, px, relative, size, AnyElement, App,
    Bounds, ClickEvent, Context, DragMoveEvent, ElementId, ElementInputHandler, Entity,
    EntityInputHandler, FocusHandle, FontStyle, FontWeight, GlobalElementId, HighlightStyle, Hsla,
    KeyBinding, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad,
    Pixels, ShapedLine, SharedString, Style, StyledText, Subscription, TextRun, UTF16Selection,
    UnderlineStyle, Window,
};
use std::collections::HashSet;
use std::ops::Range;
use std::rc::Rc;
use uuid::Uuid;

// Actions are named, typed commands that key bindings map onto. The macro
// declares one unit struct per name inside the `notesec` namespace.
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
        CloseTab,
        NextTab,
        PrevTab,
        ToggleSearch,
        NewPage,
        OpenToday,
        Undo,
        Redo,
        ToggleTheme,
        ToggleGraph,
        IncreaseFont,
        DecreaseFont,
        ResetFont,
        OpenSettings,
        Quit,
    ]
);

/// Register keyboard shortcuts. The `"BlockEditor"` context is only active
/// while a block is being edited (see `render`), so these keys do nothing
/// otherwise.
pub fn bind_keys(cx: &mut App) {
    let ctx = Some("BlockEditor");
    cx.bind_keys([
        KeyBinding::new("enter", Enter, ctx),
        KeyBinding::new("tab", Tab, ctx),
        KeyBinding::new("shift-tab", ShiftTab, ctx),
        KeyBinding::new("backspace", Backspace, ctx),
        KeyBinding::new("delete", Delete, ctx),
        KeyBinding::new("left", Left, ctx),
        KeyBinding::new("right", Right, ctx),
        KeyBinding::new("up", Up, ctx),
        KeyBinding::new("down", Down, ctx),
        KeyBinding::new("home", Home, ctx),
        KeyBinding::new("end", End, ctx),
        KeyBinding::new("escape", Escape, ctx),
        KeyBinding::new("shift-left", SelectLeft, ctx),
        KeyBinding::new("shift-right", SelectRight, ctx),
        KeyBinding::new("shift-home", SelectHome, ctx),
        KeyBinding::new("shift-end", SelectEnd, ctx),
        KeyBinding::new("ctrl-v", Paste, ctx),
        KeyBinding::new("ctrl-b", Bold, ctx),
        KeyBinding::new("ctrl-i", Italic, ctx),
        KeyBinding::new("ctrl-enter", CycleTask, ctx),
        KeyBinding::new("alt-up", MoveBlockUp, ctx),
        KeyBinding::new("alt-down", MoveBlockDown, ctx),
        // While the settings panel is open the root's key context is
        // "Settings" instead, so Esc closes the panel.
        KeyBinding::new("escape", Escape, Some("Settings")),
        // Global (no context): works whether or not a block is being edited.
        KeyBinding::new("ctrl-k", ToggleSearch, None),
        KeyBinding::new("ctrl-n", NewPage, None),
        KeyBinding::new("ctrl-j", OpenToday, None),
        KeyBinding::new("ctrl-z", Undo, None),
        KeyBinding::new("ctrl-shift-z", Redo, None),
        KeyBinding::new("ctrl-y", Redo, None),
        KeyBinding::new("ctrl-shift-t", ToggleTheme, None),
        KeyBinding::new("ctrl-g", ToggleGraph, None),
        // `=` and `+` share a key on US layouts; bind both so Ctrl-+ works with
        // or without Shift.
        KeyBinding::new("ctrl-=", IncreaseFont, None),
        KeyBinding::new("ctrl-+", IncreaseFont, None),
        KeyBinding::new("ctrl--", DecreaseFont, None),
        KeyBinding::new("ctrl-0", ResetFont, None),
        KeyBinding::new("ctrl-w", CloseTab, None),
        KeyBinding::new("ctrl-tab", NextTab, None),
        KeyBinding::new("ctrl-shift-tab", PrevTab, None),
        KeyBinding::new("ctrl-,", OpenSettings, None),
        KeyBinding::new("ctrl-q", Quit, None),
    ]);
    cx.on_action(|_: &Quit, cx| cx.quit());
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

/// How many results the search overlay shows.
const MAX_RESULTS: usize = 12;

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
    /// `Some` once "Insert template" was chosen: the palette then lists these
    /// templates instead of pages, blocks and commands.
    templates: Option<Vec<Template>>,
}

const MAX_HISTORY: usize = 100;

/// Height of the scrollable font list in the settings panel.
const FONT_LIST_HEIGHT: f32 = 220.0;

/// State of the settings panel while it is open.
struct SettingsState {
    /// Installed font families (`TextSystem::all_font_names`, which sorts and
    /// dedupes), read once when the panel opens rather than on every frame.
    fonts: Vec<String>,
}

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
    /// All pages, kept sorted for the sidebar (journals first, newest first).
    pages: Vec<Page>,
    /// Index into `pages` of the page shown in the main pane.
    selected: usize,
    /// Persisted user settings (`config.toml`).
    config: Config,
    /// Favorite and recently opened pages (`state.toml`).
    state: UiState,
    /// Colours for `config.theme`, kept in sync by `apply_theme`.
    theme: Theme,
    /// The font family actually in use: `config.font_family` if that font is
    /// installed, else `None` (the system UI font). Kept apart from the config
    /// so an unknown name in the file is never overwritten by a save.
    font_family: Option<SharedString>,

    /// Handle used to give this view keyboard focus while editing.
    focus_handle: FocusHandle,
    /// Index (into the selected page's blocks) of the block being edited.
    editing: Option<usize>,
    /// The shared text editor for the block being edited.
    editor: EditorState,
    /// `Some` while the Ctrl-K search overlay is open.
    search: Option<SearchState>,
    /// `Some` while the "/" block-type menu is open on the edited block.
    slash: Option<SlashState>,
    /// `Some` while the settings panel is open.
    settings: Option<SettingsState>,
    /// The `((` block-reference picker's highlighted entry, with the
    /// `((query` range it was chosen in (typing changes the range, which
    /// starts again from the top). The picker itself shows whenever
    /// `block_ref_query` finds a query; see `ref_query`.
    ref_selected: (Range<usize>, usize),
    /// Esc closed the picker for the `((` at this offset.
    ref_dismissed: Option<usize>,
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
    /// `selected` (see `apply_tab`).
    tabs: Tabs,
    /// The graph view, created the first time it is opened and then kept so it
    /// remembers node positions between visits.
    graph: Option<Entity<GraphView>>,
    /// Keeps the subscription to the graph's events alive (dropping a
    /// `Subscription` cancels it).
    _graph_subscription: Option<Subscription>,
    /// Shaped text + bounds from the last paint; needed to answer the OS
    /// input-method's questions about where characters are on screen.
    last_layout: Option<ShapedLine>,
    last_bounds: Option<Bounds<Pixels>>,
    /// Text layouts of the rows in reading view from the last render, so
    /// tests can find where a displayed character is on screen.
    #[cfg(test)]
    reading_layouts: Vec<(usize, gpui::TextLayout)>,
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

        sort_pages(&mut pages);
        // Open on today's journal.
        let selected = pages
            .iter()
            .position(|p| p.is_journal && p.title == today)
            .unwrap_or(0);

        // Start with one tab on that page.
        let first_tab = Tabs::new(TabTarget::Page(pages[selected].title.clone()));

        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);

        let font_family = config
            .font_family
            .as_deref()
            .and_then(|family| installed_font(family, cx));

        let state = UiState::load(storage.root());

        let mut app = NoteSec {
            storage,
            pages,
            selected,
            theme: Theme::from_kind(config.theme),
            config,
            state,
            font_family,
            focus_handle,
            editing: None,
            editor: EditorState::default(),
            search: None,
            slash: None,
            settings: None,
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
            graph: None,
            _graph_subscription: None,
            last_layout: None,
            last_bounds: None,
            #[cfg(test)]
            reading_layouts: Vec::new(),
        };
        // The startup page counts as opened.
        app.record_recent();
        app
    }

    // --- which editor is active ----------------------------------------------

    /// The text editor currently receiving input: the search box while the
    /// overlay is open, otherwise the block editor.
    fn active_editor(&self) -> &EditorState {
        match &self.search {
            Some(s) => &s.query,
            None => &self.editor,
        }
    }

    fn active_editor_mut(&mut self) -> &mut EditorState {
        match &mut self.search {
            Some(s) => &mut s.query,
            None => &mut self.editor,
        }
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

    /// Switch to theme `kind`, apply it right away and save it.
    fn set_theme(&mut self, kind: ThemeKind, cx: &mut Context<Self>) {
        if self.config.theme == kind {
            return;
        }
        self.config.theme = kind;
        self.theme = Theme::from_kind(kind);
        self.save_config();
        self.sync_graph_style(cx);
        cx.notify();
    }

    fn toggle_theme(&mut self, cx: &mut Context<Self>) {
        self.set_theme(self.config.theme.toggled(), cx);
    }

    /// Use font `family` (`None`: the system UI font), apply it right away
    /// and save it. A family that isn't installed is still saved, but the
    /// system font is used, as at startup.
    fn set_font_family(&mut self, family: Option<String>, cx: &mut Context<Self>) {
        let family = family.filter(|f| !f.trim().is_empty());
        if self.config.font_family == family {
            return;
        }
        self.font_family = family.as_deref().and_then(|f| installed_font(f, cx));
        self.config.font_family = family;
        self.save_config();
        self.sync_graph_style(cx);
        cx.notify();
    }

    // --- settings panel --------------------------------------------------------

    fn open_settings(&mut self, cx: &mut Context<Self>) {
        // Save the edited block and close the palette: the panel covers both.
        self.stop_edit(cx);
        self.close_search(cx);
        let fonts = cx.text_system().all_font_names();
        self.settings = Some(SettingsState { fonts });
        cx.notify();
    }

    fn close_settings(&mut self, cx: &mut Context<Self>) {
        self.settings = None;
        cx.notify();
    }

    /// Ctrl-, opens the panel, or closes it if it is already open.
    fn on_open_settings(&mut self, _: &OpenSettings, _: &mut Window, cx: &mut Context<Self>) {
        if self.settings.is_some() {
            self.close_settings(cx);
        } else {
            self.open_settings(cx);
        }
    }

    fn change_font_size(&mut self, delta: f32, cx: &mut Context<Self>) {
        self.config.adjust_font_size(delta);
        self.save_config();
        self.sync_graph_style(cx);
        cx.notify();
    }

    fn reset_font_size(&mut self, cx: &mut Context<Self>) {
        self.config.font_size = crate::config::DEFAULT_FONT_SIZE;
        self.save_config();
        self.sync_graph_style(cx);
        cx.notify();
    }

    fn run_command(&mut self, command: Command, cx: &mut Context<Self>) {
        match command {
            Command::ToggleTheme => self.toggle_theme(cx),
            Command::IncreaseFontSize => self.change_font_size(1.0, cx),
            Command::DecreaseFontSize => self.change_font_size(-1.0, cx),
            Command::ResetFontSize => self.reset_font_size(cx),
            Command::ToggleGraph => self.toggle_graph(cx),
            Command::InsertTemplate => self.open_template_picker(None, cx),
            Command::OpenSettings => self.open_settings(cx),
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
            templates: Some(self.storage.load_templates()),
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
            Some(s) => search(&self.pages, &s.query.text, MAX_RESULTS),
            None => Vec::new(),
        }
    }

    /// Call after the active editor's text changed: the old highlighted row no
    /// longer means anything, so go back to the top result.
    fn text_changed(&mut self) {
        if let Some(s) = &mut self.search {
            s.selected = 0;
        }
    }

    fn toggle_search(&mut self, _: &ToggleSearch, window: &mut Window, cx: &mut Context<Self>) {
        if self.search.is_some() {
            self.close_search(cx);
            return;
        }
        // Remember where the cursor was (for "Insert template"), then save
        // whatever block is being edited before covering it.
        let insert_after = self.editing;
        self.stop_edit(cx);
        self.settings = None;
        self.search = Some(SearchState {
            query: EditorState::default(),
            selected: 0,
            insert_after,
            templates: None,
        });
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn close_search(&mut self, cx: &mut Context<Self>) {
        self.search = None;
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
        self.editing = state.editing.filter(|&ix| {
            self.selected < self.pages.len() && ix < self.pages[self.selected].blocks.len()
        });
        self.editor = state.editor;
        self.slash = None;
        self.text_history_active = false;
        if let Some(ix) = self.editing {
            self.editor.clamp();
            self.editor.marked = None;
            self.pages[self.selected].blocks[ix].content = self.editor.text.clone();
            self.reveal(ix);
        }
        self.save_all_pages();
        // Tabs on pages that the restored state doesn't have are closed. The
        // restored page is brought to the front when a page is on screen or
        // a block is being edited (never edit a page that isn't shown).
        let pages = &self.pages;
        self.tabs.retain(|t| match t {
            TabTarget::Page(title) => pages.iter().any(|p| p.title == *title),
            TabTarget::Graph => true,
        });
        if self.editing.is_some() || matches!(self.tabs.active_target(), Some(TabTarget::Page(_))) {
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
        if self.search.is_some() {
            return;
        }
        self.close_slash_as_typing();
        if let Some(state) = self.undo_stack.pop() {
            self.redo_stack.push(self.history_state());
            self.restore_history(state, window, cx);
        }
    }

    fn redo(&mut self, _: &Redo, window: &mut Window, cx: &mut Context<Self>) {
        if self.search.is_some() {
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
        self.pages.push(page);
        sort_pages(&mut self.pages);
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
        let count = self.search_results().len();
        if let Some(s) = &mut self.search {
            if count > 0 {
                s.selected = (s.selected as isize + delta).clamp(0, count as isize - 1) as usize;
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
        let (insert_after, templates) = match self.search.take() {
            Some(s) => (s.insert_after, s.templates),
            None => (None, None),
        };
        self.close_search(cx);
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
            // Keeps the palette open, now listing templates.
            Target::Command(Command::InsertTemplate) => self.open_template_picker(insert_after, cx),
            Target::Command(command) => self.run_command(command, cx),
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

    // --- "((" block-reference picker ----------------------------------------

    /// The `((query` range the picker is open for, if it is: while editing a
    /// block (and no other menu or overlay is up), unless Esc closed it.
    fn ref_query(&self) -> Option<Range<usize>> {
        self.editing?;
        if self.search.is_some() || self.slash.is_some() || self.settings.is_some() {
            return None;
        }
        let range = self.editor.block_ref_query()?;
        (self.ref_dismissed != Some(range.start)).then_some(range)
    }

    /// Blocks the open picker lists, as `(page, block)`; empty when closed.
    fn ref_matches(&self) -> Vec<(usize, usize)> {
        let Some(range) = self.ref_query() else {
            return Vec::new();
        };
        let query = &self.editor.text[range.start + 2..range.end];
        let editing_id = self
            .editing
            .map(|ix| self.pages[self.selected].blocks[ix].id);
        search_blocks(&self.pages, query, editing_id, 8)
    }

    /// The picker's highlighted entry, for the current query.
    fn ref_highlight(&self) -> usize {
        match self.ref_query() {
            Some(range) if self.ref_selected.0 == range => self.ref_selected.1,
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
        if let (Some(range), true) = (self.ref_query(), count > 0) {
            let selected = (self.ref_highlight() as isize + delta).clamp(0, count as isize - 1);
            self.ref_selected = (range, selected as usize);
        }
        cx.notify();
    }

    /// Replace the typed `((query` with a reference to block `target`
    /// (`(page, block)`): `((<its id>))`, cursor after it. One undo step;
    /// saved right away, which also writes the target's id to its page (see
    /// `save_page`).
    fn insert_block_ref(&mut self, target: (usize, usize), cx: &mut Context<Self>) {
        let Some(range) = self.ref_query() else {
            return;
        };
        let Some(id) = self
            .pages
            .get(target.0)
            .and_then(|page| page.blocks.get(target.1))
            .map(|b| b.id)
        else {
            return;
        };
        self.text_history_active = false;
        let before = self.history_state();
        self.editor.replace_range(range, &format!("(({id}))"));
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

    /// Add a page, keeping the sidebar sorted and `selected` pointing at the
    /// same page as before (sorting can shift indices).
    fn add_page(&mut self, page: Page) {
        let current = self.pages[self.selected].title.clone();
        self.pages.push(page);
        sort_pages(&mut self.pages);
        self.selected = self.find_page(&current).unwrap_or(0);
    }

    /// Create (and save) a page for every `[[link]]` on the selected page that
    /// doesn't have one yet.
    fn ensure_link_targets(&mut self) {
        let targets: Vec<String> = self.pages[self.selected]
            .blocks
            .iter()
            .flat_map(|b| parse_references(&b.content))
            .map(|link| link.target)
            .collect();
        for target in targets {
            if self.find_page(&target).is_none() {
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

    /// Show the page called `title`, creating it if needed, in a tab as
    /// `nav` says.
    fn navigate(&mut self, title: &str, nav: Nav, cx: &mut Context<Self>) {
        // Leave edit mode first so the block being edited is saved (which may
        // itself create pages and reorder the sidebar).
        self.stop_edit(cx);
        let ix = match self.find_page(title) {
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
        self.selected = ix;
        self.mode = Mode::Notes;
        self.record_recent();
        cx.notify();
    }

    /// Make `mode` and `selected` match the active tab.
    fn apply_tab(&mut self, cx: &mut Context<Self>) {
        match self.tabs.active_target().cloned() {
            Some(TabTarget::Page(title)) => {
                // Focusing a page tab (click, Ctrl+Tab, or the neighbour after
                // a close) counts as a visit for RECENT.
                let ix = self.find_page(&title).unwrap_or(self.selected);
                self.enter_page(ix, cx);
            }
            Some(TabTarget::Graph) => {
                self.refresh_graph(cx);
                self.mode = Mode::Graph;
                cx.notify();
            }
            None => {
                self.mode = Mode::Empty;
                cx.notify();
            }
        }
    }

    /// Focus tab `ix` (clicking it). The block being edited is saved and
    /// editing ends, as with any navigation.
    fn activate_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.stop_edit(cx);
        self.tabs.select(ix);
        self.apply_tab(cx);
    }

    /// Close tab `ix` (its × or Ctrl+W), saving the block being edited
    /// first. Closing the last tab shows the empty state.
    fn close_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.stop_edit(cx);
        self.tabs.close(ix);
        self.apply_tab(cx);
    }

    /// The Ctrl-K palette or the settings panel covers the page. Both are
    /// modal, so the tab keys do nothing while one is open.
    fn overlay_open(&self) -> bool {
        self.search.is_some() || self.settings.is_some()
    }

    /// Ctrl+W. Ignored while an overlay is open.
    fn on_close_tab(&mut self, _: &CloseTab, _: &mut Window, cx: &mut Context<Self>) {
        if !self.overlay_open() {
            if let Some(ix) = self.tabs.active {
                self.close_tab(ix, cx);
            }
        }
    }

    fn cycle_tabs(&mut self, forward: bool, cx: &mut Context<Self>) {
        if self.overlay_open() {
            return;
        }
        self.stop_edit(cx);
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

    // --- graph view ------------------------------------------------------------

    /// Open (or focus) the graph tab, creating the graph on first use.
    fn show_graph(&mut self, cx: &mut Context<Self>) {
        // Save the block being edited so the graph sees up-to-date links.
        self.stop_edit(cx);
        self.tabs.open(TabTarget::Graph);
        self.apply_tab(cx);
    }

    /// Create the graph view, or give it the current pages.
    fn refresh_graph(&mut self, cx: &mut Context<Self>) {
        let current = self.pages[self.selected].title.clone();
        let pages = self.pages.clone();
        if let Some(graph) = self.graph.clone() {
            graph.update(cx, |g, cx| g.refresh(pages, current, cx));
        } else {
            let (theme, size, reduce_motion) =
                (self.theme, self.config.font_size, cx.reduce_motion());
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
            let (theme, size) = (self.theme, self.config.font_size);
            graph.update(cx, |g, cx| g.set_style(theme, size, cx));
        }
    }

    /// Copy the editor's text into the block being edited. Returns true if the
    /// block's content actually changed.
    fn sync_content(&mut self) -> bool {
        let Some(ix) = self.editing else { return false };
        let block = &mut self.pages[self.selected].blocks[ix];
        if block.content == self.editor.text {
            return false;
        }
        block.content = self.editor.text.clone();
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
        let content = self.pages[self.selected].blocks[ix].content.clone();
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
        // Never edit under the settings panel (e.g. Ctrl-N while it is open).
        self.settings = None;
        self.commit();
        self.text_history_active = false;
        self.load_editor(ix, false);
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn stop_edit(&mut self, cx: &mut Context<Self>) {
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
        if self.search.is_some() {
            self.confirm_search(window, cx);
            return;
        }
        if let Some(state) = &self.slash {
            if let Some(&kind) = self.slash_matches().get(state.menu.selected) {
                self.apply_slash(kind, cx);
            }
            return;
        }
        if let Some(&target) = self.ref_matches().get(self.ref_highlight()) {
            self.insert_block_ref(target, cx);
            return;
        }
        let Some(ix) = self.editing else { return };
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
        self.restructure(cx, |page, ix| page.indent(ix));
    }

    fn shift_tab(&mut self, _: &ShiftTab, _: &mut Window, cx: &mut Context<Self>) {
        self.restructure(cx, |page, ix| page.outdent(ix));
    }

    /// Shared body of Tab / Shift-Tab: apply `op` to the edited block, then
    /// save. The block keeps its index (document order never changes).
    fn restructure(&mut self, cx: &mut Context<Self>, op: impl FnOnce(&mut Page, usize) -> bool) {
        let Some(ix) = self.editing else { return };
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
        if self.search.is_some() {
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
        if self.editing.is_some() || self.search.is_some() {
            self.text_history_active = false;
            let before = self.history_state();
            if self.active_editor_mut().delete() && self.search.is_none() {
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
        let (Some(bounds), Some(line)) = (self.last_bounds, self.last_layout.as_ref()) else {
            return self.editor.cursor;
        };
        if position.y < bounds.top() {
            0
        } else if position.y > bounds.bottom() {
            len
        } else {
            line.closest_index_for_x(position.x - bounds.left())
                .min(len)
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
        if self.slash.is_some() {
            self.move_slash_selection(-1, cx);
            return;
        }
        if self.ref_menu_open() {
            self.move_ref_selection(-1, cx);
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
        if self.slash.is_some() {
            self.move_slash_selection(1, cx);
            return;
        }
        if self.ref_menu_open() {
            self.move_ref_selection(1, cx);
            return;
        }
        if let Some(next) = self.editing.and_then(|ix| self.visible_neighbor(ix, true)) {
            self.move_edit(next, cx);
        }
    }

    fn escape(&mut self, _: &Escape, _: &mut Window, cx: &mut Context<Self>) {
        if self.settings.is_some() {
            self.close_settings(cx);
        } else if self.search.is_some() {
            self.close_search(cx);
        } else if self.slash.is_some() {
            self.dismiss_slash(cx);
        } else if let (Some(range), true) = (self.ref_query(), self.ref_menu_open()) {
            self.ref_dismissed = Some(range.start);
            cx.notify();
        } else if self.editor.selection().is_some() {
            // First Esc only drops the selection; the next one stops editing.
            self.editor.clear_selection();
            cx.notify();
        } else {
            self.stop_edit(cx);
        }
    }

    fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
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

/// Journals first (newest first, since `YYYY-MM-DD` sorts lexically), then
/// regular pages alphabetically.
fn sort_pages(pages: &mut [Page]) {
    pages.sort_by(|a, b| match (a.is_journal, b.is_journal) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        (true, true) => b.title.cmp(&a.title),
        (false, false) => a.title.to_lowercase().cmp(&b.title.to_lowercase()),
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
        let range = range_utf16
            .as_ref()
            .map(|r| self.active_editor().range_from_utf16(r))
            .or(self.active_editor().marked.clone())
            .unwrap_or(self.active_editor().selected_range());
        let in_block = self.search.is_none() && self.editing.is_some();
        // Typing another "(" may start a new "((": show the picker again.
        if new_text.contains('(') {
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
        let layout = self.last_layout.as_ref()?;
        let range = self.active_editor().range_from_utf16(&range_utf16);
        Some(Bounds::from_corners(
            point(
                bounds.left() + layout.x_for_index(range.start),
                bounds.top(),
            ),
            point(
                bounds.left() + layout.x_for_index(range.end),
                bounds.bottom(),
            ),
        ))
    }

    fn character_index_for_point(
        &mut self,
        pt: gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let local = self.last_bounds?.localize(&pt)?;
        let layout = self.last_layout.as_ref()?;
        let utf8 = layout.index_for_x(pt.x - local.x)?;
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

/// Data computed in `prepaint` and consumed in `paint`.
struct PrepaintState {
    line: ShapedLine,
    cursor: PaintQuad,
    /// Highlight behind the selected text, if any.
    selection: Option<PaintQuad>,
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
        // Full width, one line tall (blocks are single-line).
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = window.line_height().into();
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
        let line = window
            .text_system()
            .shape_line(text, font_size, &runs, None);

        let x = line.x_for_index(cursor);
        let cursor = fill(
            Bounds::new(
                point(bounds.left() + x, bounds.top()),
                size(px(1.5), bounds.size.height),
            ),
            accent,
        );
        // The selection is a translucent quad painted *under* the text, so it
        // never changes the text runs (colours, IME underline) on top of it.
        let selection = selected.map(|range| {
            fill(
                Bounds::from_corners(
                    point(bounds.left() + line.x_for_index(range.start), bounds.top()),
                    point(bounds.left() + line.x_for_index(range.end), bounds.bottom()),
                ),
                selection_color,
            )
        });
        PrepaintState {
            line,
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

        if let Some(selection) = prepaint.selection.take() {
            window.paint_quad(selection);
        }
        prepaint
            .line
            .paint(
                bounds.origin,
                window.line_height(),
                gpui::TextAlign::Left,
                None,
                window,
                cx,
            )
            .expect("failed to paint block text");

        if focus_handle.is_focused(window) {
            window.paint_quad(prepaint.cursor.clone());
        }

        // Remember the layout for the OS input-method callbacks.
        let line = std::mem::take(&mut prepaint.line);
        self.app.update(cx, |app, _| {
            app.last_layout = Some(line);
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

        let theme_row = div()
            .flex()
            .flex_row()
            .gap_2()
            .child(
                button("theme-dark", "Dark".into(), config.theme == ThemeKind::Dark).on_click(
                    cx.listener(|this, _e, _window, cx| this.set_theme(ThemeKind::Dark, cx)),
                ),
            )
            .child(
                button(
                    "theme-light",
                    "Light".into(),
                    config.theme == ThemeKind::Light,
                )
                .on_click(
                    cx.listener(|this, _e, _window, cx| this.set_theme(ThemeKind::Light, cx)),
                ),
            );

        // The − / + buttons are dimmed at the limits (clicking them then
        // does nothing, as `adjust_font_size` clamps).
        let size = config.font_size;
        let at_min = size <= crate::config::MIN_FONT_SIZE;
        let at_max = size >= crate::config::MAX_FONT_SIZE;
        let size_row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(
                button("font-size-dec", "−".into(), false)
                    .when(at_min, |d| d.text_color(theme.muted))
                    .on_click(cx.listener(|this, _e, _window, cx| this.change_font_size(-1.0, cx))),
            )
            .child(
                div()
                    .debug_selector(|| "font-size-value".to_string())
                    .min_w(px(40.0))
                    .flex()
                    .justify_center()
                    .child(format!("{size}")),
            )
            .child(
                button("font-size-inc", "+".into(), false)
                    .when(at_max, |d| d.text_color(theme.muted))
                    .on_click(cx.listener(|this, _e, _window, cx| this.change_font_size(1.0, cx))),
            )
            .child(
                button(
                    "font-size-reset",
                    "Reset".into(),
                    size == crate::config::DEFAULT_FONT_SIZE,
                )
                .on_click(cx.listener(|this, _e, _window, cx| this.reset_font_size(cx))),
            );

        // Font family: "System default", then every installed family.
        let row = |id: ElementId, selector: String, text: String, active: bool| {
            div()
                .id(id)
                .debug_selector(move || selector)
                .flex_shrink_0()
                .px_3()
                .py_1()
                .rounded_md()
                .cursor_pointer()
                .text_color(if active { theme.accent } else { theme.text })
                .when(active, |d| d.bg(theme.selected_bg))
                .hover(|d| d.bg(theme.selected_bg))
                .child(text)
        };
        let current = config.font_family.as_deref();
        let font_rows: Vec<AnyElement> = state
            .fonts
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let family = name.clone();
                row(
                    ElementId::from(("font-family", i)),
                    format!("font-family-{i}"),
                    name.clone(),
                    current == Some(name.as_str()),
                )
                .on_click(cx.listener(move |this, _e, _window, cx| {
                    this.set_font_family(Some(family.clone()), cx)
                }))
                .into_any_element()
            })
            .collect();
        let no_fonts = font_rows.is_empty();
        // A configured family that isn't installed (e.g. a typo in
        // config.toml) is kept in the file but not used; say so.
        let missing = current.filter(|f| !state.fonts.iter().any(|name| name == f));
        let font_list = div()
            .id("font-family-list")
            .h(px(FONT_LIST_HEIGHT))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .p_1()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .bg(theme.bg)
            .child(
                row(
                    ElementId::from("font-family-default"),
                    "font-family-default".to_string(),
                    "System default".to_string(),
                    current.is_none(),
                )
                .on_click(cx.listener(|this, _e, _window, cx| this.set_font_family(None, cx))),
            )
            .children(font_rows)
            .when(no_fonts, |d| {
                d.child(
                    div()
                        .px_3()
                        .py_1()
                        .text_color(theme.muted)
                        .child("No installed fonts were found"),
                )
            });

        div()
            .id("settings-backdrop")
            .debug_selector(|| "settings-backdrop".to_string())
            .absolute()
            .inset_0()
            .occlude()
            .bg(gpui::black().opacity(0.45))
            .flex()
            .flex_col()
            .items_center()
            .pt(px(90.0))
            .on_click(cx.listener(|this, _e, _window, cx| this.close_settings(cx)))
            .child(
                // `occlude` keeps clicks inside the panel from reaching the
                // backdrop (which would close it).
                div()
                    .id("settings-panel")
                    .debug_selector(|| "settings-panel".to_string())
                    .occlude()
                    .w(px(460.0))
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
                            .flex()
                            .flex_row()
                            .justify_between()
                            .child(div().font_weight(FontWeight::BOLD).child("Settings"))
                            .child(label("Esc to close")),
                    )
                    .child(label("Theme"))
                    .child(theme_row)
                    .child(label("Font size"))
                    .child(size_row)
                    .child(label("Font family"))
                    .child(font_list)
                    .when_some(missing, |d, family| {
                        d.child(div().text_color(theme.muted).child(format!(
                            "\u{201c}{family}\u{201d} is not installed; using the system font"
                        )))
                    }),
            )
            .into_any_element()
    }
}

impl Render for NoteSec {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let font_size = self.config.font_size;
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

        // --- Sidebar: one clickable row per page ---------------------------
        let sidebar_items = self.pages.iter().enumerate().map(|(ix, page)| {
            let is_selected = self.mode == Mode::Notes && ix == self.selected;
            let is_favorite = self.state.is_favorite(&page.title);
            let title = page.title.clone();
            let star_title = page.title.clone();
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
            .child(div().text_color(theme.muted).child("Ctrl-J"));

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
            .child(div().text_color(theme.muted).child("Ctrl-,"));

        let sidebar_list = div()
            .id("sidebar")
            .debug_selector(|| "sidebar".to_string())
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .overflow_y_scroll()
            .child(today_item)
            .child(graph_item)
            .when(!favorite_rows.is_empty(), |d| {
                d.child(section_header("FAVORITES")).children(favorite_rows)
            })
            .when(!recent_rows.is_empty(), |d| {
                d.child(section_header("RECENT")).children(recent_rows)
            })
            .child(pages_header)
            .children(sidebar_items)
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

        // --- "((" block-reference picker (same place as the "/" menu) ------
        let ref_matches = self.ref_matches();
        if slash_menu.is_none() && !ref_matches.is_empty() {
            let selected = self.ref_highlight();
            let items: Vec<AnyElement> = ref_matches
                .into_iter()
                .enumerate()
                .map(|(i, target)| {
                    let page = &self.pages[target.0];
                    let block = &page.blocks[target.1];
                    let text = DisplayBlock::new(&block.content).text.replace('\n', " ");
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
                            this.insert_block_ref(target, cx);
                        }))
                        .child(
                            div()
                                .flex_1()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .child(text),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_color(theme.muted)
                                .child(page.title.clone()),
                        )
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

        // --- Main pane: title + blocks --------------------------------------
        let page = &self.pages[self.selected];
        #[cfg(test)]
        let mut reading_layouts = Vec::new();
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
        let rows: Vec<AnyElement> = page
            .blocks
            .iter()
            .enumerate()
            // Blocks inside a folded subtree aren't rendered at all.
            .filter(|&(ix, _)| visible[ix])
            .map(|(ix, block)| {
                let is_editing = self.editing == Some(ix);
                let depth = page.depth_of(ix);
                // Display mode (the reading view) hides the type prefix
                // (`# `, `> `) and paired `**`/`*` markers and styles the row
                // instead; the editor shows the raw markdown. `display` also
                // maps shown offsets back to `content` offsets.
                let display = (!is_editing).then(|| {
                    Rc::new(DisplayBlock::with_refs(&block.content, |id| {
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
                        .on_mouse_down(MouseButton::Left, cx.listener(Self::on_text_mouse_down))
                        .child(BlockText { app: cx.entity() })
                        .into_any_element(),
                    Some(d) => {
                        let highlights = reading_highlights(d, link_style, tag_style, ref_style);
                        // `with_highlights` resolves against the inherited text
                        // style, so heading size/weight and quote styling (and
                        // the theme's text colour) apply to the unhighlighted
                        // parts.
                        let text = StyledText::new(d.text.clone()).with_highlights(highlights);
                        text_layout = Some(text.layout().clone());
                        #[cfg(test)]
                        reading_layouts.push((ix, text.layout().clone()));
                        // DONE text is dimmed and struck through.
                        let text = div()
                            .flex_1()
                            .when(d.task == Some(TaskState::Done), |t| {
                                t.text_color(theme.muted)
                                    .line_through()
                                    .debug_selector(move || format!("done-text-{ix}"))
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
                                            format!("task-{ix}-{}", state.keyword().to_lowercase())
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
                        .debug_selector(move || format!("fold-{ix}"))
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
                        .debug_selector(move || format!("handle-{ix}"))
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
                                .debug_selector(move || format!("drop-before-{ix}")),
                        )
                    })
                    .on_drag_move(on_drag_move)
                    .when(folded, |d| {
                        d.child(
                            fold_badge(&theme, font_size, hidden_count)
                                .debug_selector(move || format!("fold-badge-{ix}-{hidden_count}")),
                        )
                    })
                    .id(("block", ix))
                    // Lets tests find this row's on-screen bounds; a no-op in
                    // normal builds.
                    .debug_selector(|| format!("block-{ix}"))
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
        #[cfg(test)]
        {
            self.reading_layouts = reading_layouts;
        }

        // --- Backlinks: blocks on other pages that link here ----------------
        // Recomputed every frame. That is a scan of every block, which is fine
        // for a personal graph; an index can replace it if it ever shows up in
        // a profile.
        let groups = backlinks(&self.pages, &page.title);
        let total: usize = groups.iter().map(|g| g.blocks.len()).sum();
        let mut backlink_items: Vec<AnyElement> = Vec::new();
        let mut n: usize = 0; // running index over all references, for ids
        for group in &groups {
            let source = &self.pages[group.page];
            let source_title = source.title.clone();
            backlink_items.push(
                div()
                    .id(("backlink-page", group.page))
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
                backlink_items.push(
                    div()
                        .id(("backlink", n))
                        .debug_selector(|| format!("backlink-{n}"))
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
                        .child(source.blocks[block_ix].content.clone())
                        .into_any_element(),
                );
                n += 1;
            }
        }
        let backlinks_panel = (total > 0).then(|| {
            div()
                .id("backlinks")
                .mt_8()
                .pt_4()
                .border_t_1()
                .border_color(theme.border)
                .flex()
                .flex_col()
                .child(div().text_color(theme.muted).child(format!(
                    "{total} LINKED REFERENCE{}",
                    if total == 1 { "" } else { "S" }
                )))
                .children(backlink_items)
        });

        let main = div()
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
            .children(rows)
            // The drop line for "after the last block".
            .when(block_drop == Some(DropGap::End), |d| {
                d.child(
                    div()
                        .relative()
                        .h(px(0.))
                        .child(drop_line(&theme, 0).debug_selector(|| "drop-end".to_string())),
                )
            })
            // Leaving the page area hides the drop line; dropping anywhere on
            // the page moves the block to where the line is.
            .on_drag_move(
                cx.listener(|this, event: &DragMoveEvent<DraggedBlock>, _, cx| {
                    if !event.bounds.contains(&event.event.position) {
                        let dragged = event.drag(cx).id;
                        this.set_block_drop(dragged, None, cx);
                    }
                }),
            )
            .on_drop(
                cx.listener(|this, dragged: &DraggedBlock, _, cx| this.drop_block(dragged.id, cx)),
            )
            .children(backlinks_panel)
            // Empty space below the blocks: clicking it leaves edit mode.
            .child(
                div()
                    .id("filler")
                    .flex_1()
                    .min_h(px(120.0))
                    .on_click(cx.listener(|this, _e, _window, cx| this.stop_edit(cx))),
            );

        // --- Search overlay (Ctrl-K) -----------------------------------------
        let overlay = self.search.as_ref().map(|state| {
            let hits = self.search_results();
            let selected = state.selected;
            let templates = state.templates.as_deref();
            let rows: Vec<AnyElement> =
                hits.into_iter()
                    .enumerate()
                    .map(|(i, hit)| {
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
                            Target::Command(c) => row(
                                div().text_color(theme.text).child(c.label()),
                                "command".into(),
                            ),
                            Target::Template(t) => row(
                                div().text_color(theme.accent).child(
                                    templates.map_or(String::new(), |ts| ts[t].name.clone()),
                                ),
                                "template".into(),
                            ),
                        };
                        div()
                            .id(("search-result", i))
                            .debug_selector(|| format!("search-result-{i}"))
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
                            .into_any_element()
                    })
                    .collect();
            let no_results = rows.is_empty();
            // In template mode: a heading, and a hint if there are no templates.
            let picking_templates = templates.is_some();
            let empty_message = match templates {
                Some([]) => format!(
                    "No templates yet: add .md files to {}",
                    self.storage.templates_dir().display()
                ),
                _ => "No results".to_string(),
            };

            // Backdrop: dims the app, swallows mouse events, closes on click.
            div()
                .id("search-backdrop")
                .debug_selector(|| "search-backdrop".to_string())
                .absolute()
                .inset_0()
                .occlude()
                .bg(gpui::black().opacity(0.45))
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
                        .child(
                            div()
                                .debug_selector(|| "search-input".to_string())
                                .px_3()
                                .py_2()
                                .rounded_md()
                                .bg(theme.bg)
                                .child(BlockText { app: cx.entity() }),
                        )
                        .children(rows)
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
        let empty_state = || {
            let hint = |keys: &'static str, what: &'static str| {
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
                        .child(hint("Ctrl-J", "today's journal"))
                        .child(hint("Ctrl-K", "search pages and blocks"))
                        .child(hint("Ctrl-N", "new page"))
                        .child(hint("Ctrl-G", "graph view"))
                        .child(hint("", "or pick a page in the sidebar")),
                )
        };

        // The main area: the tab bar, then the page, the graph or the empty
        // state.
        let view: AnyElement = match (&self.mode, &self.graph) {
            (Mode::Graph, Some(graph)) => graph.clone().into_any_element(),
            (Mode::Empty, _) => empty_state().into_any_element(),
            _ => main.into_any_element(),
        };
        let content = div()
            .flex_1()
            .h_full()
            .min_w_0()
            .flex()
            .flex_col()
            .child(tab_bar)
            .child(view);

        let settings_overlay = self
            .settings
            .as_ref()
            .map(|state| self.render_settings(state, cx));

        let is_editing = self.editing.is_some() || self.search.is_some();
        let settings_open = self.settings.is_some();
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
            .when(settings_open, |d| d.key_context("Settings"))
            .when(!settings_open && is_editing, |d| {
                d.key_context("BlockEditor")
            })
            .on_action(cx.listener(Self::enter))
            .on_action(cx.listener(Self::tab))
            .on_action(cx.listener(Self::shift_tab))
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
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
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::bold))
            .on_action(cx.listener(Self::italic))
            .on_action(cx.listener(Self::cycle_task))
            .on_action(cx.listener(Self::on_move_block_up))
            .on_action(cx.listener(Self::on_move_block_down))
            .on_action(cx.listener(Self::toggle_search))
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
            .child(sidebar)
            .child(content)
            .children(overlay)
            .children(settings_overlay)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ThemeKind;
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
        assert_eq!(view.update(cx, |app, _| app.config.theme), ThemeKind::Dark);

        cx.simulate_keystrokes("ctrl-shift-t");
        view.update(cx, |app, _| {
            assert_eq!(app.config.theme, ThemeKind::Light);
            assert_ne!(app.theme.bg, dark, "colours actually changed");
        });
        assert_eq!(
            saved_config(&dir).theme,
            ThemeKind::Light,
            "written to config.toml"
        );

        cx.simulate_keystrokes("ctrl-shift-t");
        view.update(cx, |app, _| assert_eq!(app.theme.bg, dark));
        assert_eq!(saved_config(&dir).theme, ThemeKind::Dark);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn font_size_shortcuts_change_layout_and_persist(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "font-keys", "- hi\n");
        let row_height =
            |cx: &mut VisualTestContext| cx.debug_bounds("block-0").unwrap().size.height;
        let base = row_height(cx);

        cx.simulate_keystrokes("ctrl-= ctrl-= ctrl-=");
        view.update(cx, |app, _| assert_eq!(app.config.font_size, 19.0));
        assert_eq!(saved_config(&dir).font_size, 19.0);
        assert!(row_height(cx) > base, "bigger font gives taller rows");

        cx.simulate_keystrokes("ctrl--");
        view.update(cx, |app, _| assert_eq!(app.config.font_size, 18.0));

        cx.simulate_keystrokes("ctrl-0");
        view.update(cx, |app, _| assert_eq!(app.config.font_size, 16.0));
        assert_eq!(row_height(cx), base, "reset restores the original layout");

        // Limits: it cannot shrink or grow without bound.
        for _ in 0..40 {
            cx.simulate_keystrokes("ctrl--");
        }
        view.update(cx, |app, _| {
            assert_eq!(app.config.font_size, crate::config::MIN_FONT_SIZE)
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
            assert_eq!(app.config.theme, ThemeKind::Light);
        });
        assert_eq!(saved_config(&dir).theme, ThemeKind::Light);

        cx.simulate_keystrokes("ctrl-k");
        cx.simulate_input("increase font");
        cx.simulate_keystrokes("enter");
        view.update(cx, |app, _| assert_eq!(app.config.font_size, 17.0));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn unknown_font_is_ignored_but_not_erased(cx: &mut TestAppContext) {
        let dir = std::env::temp_dir().join(format!("notesec-test-badfont-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let storage = Storage::open(dir.clone()).unwrap();
        let config = Config {
            font_family: Some("Definitely Not A Font 12345".into()),
            ..Config::default()
        };

        cx.update(bind_keys);
        let (view, cx) = cx.add_window_view(|window, cx| NoteSec::new(storage, config, window, cx));
        view.update(cx, |app, _| {
            assert_eq!(app.font_family, None, "falls back to the system font");
            assert!(
                app.config.font_family.is_some(),
                "the setting itself is kept"
            );
        });
        // Changing another setting rewrites the file; the user's font name stays.
        cx.simulate_keystrokes("ctrl-shift-t");
        assert_eq!(
            saved_config(&dir).font_family.as_deref(),
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
            assert_eq!(app.config.font_size, 22.0);
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
        cx.simulate_input("graph");
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
        cx.simulate_keystrokes("down down down down down down down down down down down down");
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
            let x = app.last_layout.as_ref().unwrap().x_for_index(ix);
            point(bounds.left() + x, bounds.center().y)
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
            assert_eq!(layout.text.as_ref(), "**bold** *it*");
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

        click_on(cx, "theme-light");
        view.update(cx, |app, _| {
            assert_eq!(app.config.theme, ThemeKind::Light);
            assert_eq!(app.theme.bg, Theme::light().bg);
        });
        assert_eq!(saved_config(&dir).theme, ThemeKind::Light);
        assert!(config_text(&dir).contains("theme = \"light\""));
        assert!(settings_open(&view, cx), "the panel stays open");

        click_on(cx, "theme-dark");
        view.update(cx, |app, _| assert_eq!(app.theme.bg, Theme::dark().bg));
        assert_eq!(saved_config(&dir).theme, ThemeKind::Dark);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn font_size_buttons_change_persist_and_clamp(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "settings-size", "- hi\n");
        click_on(cx, "settings-gear");
        let size = |view: &Entity<NoteSec>, cx: &mut VisualTestContext| {
            view.update(cx, |app, _| app.config.font_size)
        };
        assert!(has(cx, "font-size-value"));

        click_on(cx, "font-size-inc");
        click_on(cx, "font-size-inc");
        assert_eq!(size(&view, cx), 18.0);
        assert_eq!(saved_config(&dir).font_size, 18.0);
        click_on(cx, "font-size-dec");
        assert_eq!(size(&view, cx), 17.0);
        click_on(cx, "font-size-reset");
        assert_eq!(size(&view, cx), crate::config::DEFAULT_FONT_SIZE);
        assert_eq!(
            saved_config(&dir).font_size,
            crate::config::DEFAULT_FONT_SIZE
        );

        for _ in 0..10 {
            click_on(cx, "font-size-dec");
        }
        assert_eq!(size(&view, cx), crate::config::MIN_FONT_SIZE);
        assert_eq!(saved_config(&dir).font_size, crate::config::MIN_FONT_SIZE);
        for _ in 0..30 {
            click_on(cx, "font-size-inc");
        }
        assert_eq!(size(&view, cx), crate::config::MAX_FONT_SIZE);
        assert_eq!(saved_config(&dir).font_size, crate::config::MAX_FONT_SIZE);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn font_family_choice_persists_and_system_default_removes_it(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "settings-family", "- hi\n");
        click_on(cx, "settings-gear");
        // The test platform's text system reports no installed fonts, so the
        // list only has "System default"...
        assert!(has(cx, "font-family-default"));
        assert!(!has(cx, "font-family-0"));
        // ...so give the open panel a known list, as the real text system would.
        view.update(cx, |app, cx| {
            app.settings.as_mut().unwrap().fonts = vec!["Test Sans".into(), "Test Serif".into()];
            cx.notify();
        });
        cx.run_until_parked();
        assert!(has(cx, "font-family-1") && !has(cx, "font-family-2"));

        click_on(cx, "font-family-1");
        view.update(cx, |app, _| {
            assert_eq!(app.config.font_family.as_deref(), Some("Test Serif"));
            // Not really installed here, so rendering keeps the system font.
            assert_eq!(app.font_family, None);
        });
        assert_eq!(
            saved_config(&dir).font_family.as_deref(),
            Some("Test Serif")
        );
        assert!(config_text(&dir).contains("font_family = \"Test Serif\""));

        click_on(cx, "font-family-default");
        view.update(cx, |app, _| assert_eq!(app.config.font_family, None));
        assert_eq!(saved_config(&dir).font_family, None);
        assert!(
            !config_text(&dir)
                .lines()
                .any(|l| l.starts_with("font_family")),
            "only the commented-out hint is left"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn settings_from_the_panel_load_in_a_fresh_window(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "settings-reload", "- hi\n");
        click_on(cx, "settings-gear");
        click_on(cx, "theme-light");
        for _ in 0..4 {
            click_on(cx, "font-size-inc");
        }
        view.update(cx, |app, cx| {
            app.set_font_family(Some("Test Serif".into()), cx)
        });

        let storage = Storage::open(dir.clone()).unwrap();
        let config = Config::load(&dir);
        let (view2, cx2) = cx
            .cx
            .add_window_view(|window, cx| NoteSec::new(storage, config, window, cx));
        view2.update(cx2, |app, _| {
            assert_eq!(app.config.theme, ThemeKind::Light);
            assert_eq!(app.theme.bg, Theme::light().bg);
            assert_eq!(app.config.font_size, 20.0);
            assert_eq!(app.config.font_family.as_deref(), Some("Test Serif"));
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
}
