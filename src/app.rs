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
    backlinks, cycle_task, parse_references, tag_counts, BlockKind, Page, TaskState,
};
use crate::search::{search, search_templates, Command, Hit, Target};
use crate::state::UiState;
use crate::storage::{today_title, validate_title, Storage, Template};
use crate::tabs::{TabTarget, Tabs};
use crate::ui::{block_row, favorite_star, fold_arrow, fold_badge, task_checkbox, Theme};
use gpui::{
    actions, anchored, deferred, div, fill, point, prelude::*, px, relative, size, AnyElement, App,
    Bounds, ClickEvent, ClipboardItem, Context, DragMoveEvent, ElementId, ElementInputHandler,
    Entity, EntityInputHandler, FocusHandle, FontStyle, FontWeight, GlobalElementId,
    HighlightStyle, Hsla, KeyBinding, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, PaintQuad, Pixels, ShapedLine, SharedString, Style, StyledText, Subscription,
    TextRun, UTF16Selection, UnderlineStyle, Window,
};
use std::collections::{HashMap, HashSet};
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
        // While the settings panel is open the root's key context is
        // "Settings" instead, so Esc closes the panel.
        KeyBinding::new("escape", Escape, Some("Settings")),
        // Likewise "PageMenu" while a page's context menu or its delete
        // confirmation is open (renaming uses "BlockEditor": it types).
        KeyBinding::new("escape", Escape, Some("PageMenu")),
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
    /// `Some` while a page's context menu (or its rename / delete step) is
    /// open.
    page_menu: Option<PageMenu>,
    /// Where the page being dragged in the sidebar would land. Only
    /// meaningful while a drag is active; `render` clears it otherwise.
    page_drop: Option<PageDrop>,
    /// True between a mouse-down in the edited block and the mouse-up: mouse
    /// moves in between extend the selection.
    selecting: bool,
    /// Ids of folded blocks (their descendants are hidden). UI-only: not
    /// saved, and block ids are regenerated on load anyway.
    collapsed: HashSet<Uuid>,
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

        let state = UiState::load(storage.root());
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

        let font_family = config
            .font_family
            .as_deref()
            .and_then(|family| installed_font(family, cx));

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
            page_menu: None,
            page_drop: None,
            selecting: false,
            collapsed: HashSet::new(),
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
    /// overlay is open, the rename field while renaming a page, otherwise
    /// the block editor.
    fn active_editor(&self) -> &EditorState {
        match (&self.search, &self.page_menu) {
            (Some(s), _) => &s.query,
            (
                None,
                Some(PageMenu {
                    step: MenuStep::Rename { editor, .. },
                    ..
                }),
            ) => editor,
            _ => &self.editor,
        }
    }

    fn active_editor_mut(&mut self) -> &mut EditorState {
        match (&mut self.search, &mut self.page_menu) {
            (Some(s), _) => &mut s.query,
            (
                None,
                Some(PageMenu {
                    step: MenuStep::Rename { editor, .. },
                    ..
                }),
            ) => editor,
            _ => &mut self.editor,
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
        self.search.is_some() || self.renaming()
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
        self.page_menu = None;
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
            Command::SortPagesAz => self.sort_pages_az(cx),
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
        if self.search.is_some() {
            self.close_search(cx);
            return;
        }
        // Remember where the cursor was (for "Insert template"), then save
        // whatever block is being edited before covering it.
        let insert_after = self.editing;
        self.stop_edit(cx);
        self.settings = None;
        self.page_menu = None;
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
        if self.search.is_some() || self.page_menu.is_some() {
            return;
        }
        self.close_slash_as_typing();
        if let Some(state) = self.undo_stack.pop() {
            self.redo_stack.push(self.history_state());
            self.restore_history(state, window, cx);
        }
    }

    fn redo(&mut self, _: &Redo, window: &mut Window, cx: &mut Context<Self>) {
        if self.search.is_some() || self.page_menu.is_some() {
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

    // --- persistence helpers -------------------------------------------------

    /// Save the selected page, then create any pages its `[[links]]` point to.
    ///
    /// Doing link-target creation here (rather than per keystroke) means typing
    /// `[[Ne` never creates a half-named page: links are only acted on once the
    /// block is committed.
    fn save_page(&mut self) {
        let page = &self.pages[self.selected];
        if let Err(err) = self.storage.save(page) {
            eprintln!("notesec: failed to save {}: {err}", page.title);
        }
        self.ensure_link_targets();
    }

    /// Index of the page called `title`. Matching ignores case, like Logseq.
    fn find_page(&self, title: &str) -> Option<usize> {
        let wanted = title.to_lowercase();
        self.pages
            .iter()
            .position(|p| p.title.to_lowercase() == wanted)
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

    /// The Ctrl-K palette, the settings panel or a page menu covers the
    /// page. All are modal, so the tab keys do nothing while one is open.
    fn overlay_open(&self) -> bool {
        self.search.is_some() || self.settings.is_some() || self.page_menu.is_some()
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

    /// The last page can't be deleted: there is always a page to show.
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

    /// Delete the page called `title`: its file (no trash in v1), the page,
    /// its tabs (the neighbour tab takes focus, as when closing a tab) and
    /// its favorites/recent entries.
    fn delete_page(&mut self, title: &str, cx: &mut Context<Self>) -> Result<(), String> {
        let ix = self
            .find_page(title)
            .ok_or_else(|| "This page no longer exists".to_string())?;
        if !self.can_delete() {
            return Err("The only page can't be deleted".into());
        }
        self.stop_edit(cx);
        self.storage
            .delete(&self.pages[ix])
            .map_err(|err| format!("Could not delete the file: {err}"))?;

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
    /// rename or delete would write the old file back. Those two actions
    /// can't be undone (v1), and edits from before them can't either.
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
        // Never edit under the settings panel or a page menu (e.g. Ctrl-N
        // while one is open).
        self.settings = None;
        self.page_menu = None;
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
        if self.renaming() {
            self.confirm_rename(cx);
            return;
        }
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
        if let Some(next) = self.editing.and_then(|ix| self.visible_neighbor(ix, true)) {
            self.move_edit(next, cx);
        }
    }

    fn escape(&mut self, _: &Escape, _: &mut Window, cx: &mut Context<Self>) {
        if self.page_menu.is_some() {
            // Closes the menu, cancels a rename or a delete.
            self.close_page_menu(cx);
        } else if self.settings.is_some() {
            self.close_settings(cx);
        } else if self.search.is_some() {
            self.close_search(cx);
        } else if self.slash.is_some() {
            self.dismiss_slash(cx);
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
) -> Vec<(Range<usize>, HighlightStyle)> {
    display
        .segments()
        .into_iter()
        .map(|(range, format)| {
            let mut style = if format.tag {
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
        let range = range_utf16
            .as_ref()
            .map(|r| self.active_editor().range_from_utf16(r))
            .or(self.active_editor().marked.clone())
            .unwrap_or(self.active_editor().selected_range());
        let in_block = self.search.is_none() && self.editing.is_some();
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
            // A modal like the settings panel: dimmed backdrop, centred box.
            let title = menu.title.clone();
            return div()
                .id("confirm-delete-backdrop")
                .absolute()
                .inset_0()
                .occlude()
                .bg(gpui::black().opacity(0.45))
                .flex()
                .flex_col()
                .items_center()
                .pt(px(160.0))
                .on_click(cx.listener(|this, _e, _window, cx| this.close_page_menu(cx)))
                .child(
                    panel()
                        .id("confirm-delete")
                        .debug_selector(|| "confirm-delete".to_string())
                        .w(px(420.0))
                        .gap_2()
                        .p_4()
                        .child(
                            div()
                                .font_weight(FontWeight::BOLD)
                                .child(format!("Delete \u{201c}{title}\u{201d}?")),
                        )
                        .child(
                            div()
                                .text_color(theme.muted)
                                .child("Its file is deleted from the graph. This can't be undone."),
                        )
                        .when_some(error.clone(), |d, error| {
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
                                        .id("confirm-delete-cancel")
                                        .debug_selector(|| "confirm-delete-cancel".to_string())
                                        .px_3()
                                        .py_1()
                                        .rounded_md()
                                        .border_1()
                                        .border_color(theme.border)
                                        .cursor_pointer()
                                        .hover(|d| d.bg(theme.selected_bg))
                                        .on_click(cx.listener(|this, _e, _window, cx| {
                                            this.close_page_menu(cx)
                                        }))
                                        .child("Cancel"),
                                )
                                .child(
                                    div()
                                        .id("confirm-delete-ok")
                                        .debug_selector(|| "confirm-delete-ok".to_string())
                                        .px_3()
                                        .py_1()
                                        .rounded_md()
                                        .bg(theme.danger)
                                        .text_color(theme.bg)
                                        .font_weight(FontWeight::BOLD)
                                        .cursor_pointer()
                                        .hover(|d| d.opacity(0.85))
                                        .on_click(cx.listener(|this, _e, _window, cx| {
                                            this.confirm_delete(cx)
                                        }))
                                        .child("Delete"),
                                ),
                        ),
                )
                .into_any_element();
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

impl Render for NoteSec {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // A drag released outside the sidebar just ends (GPUI drops it), so
        // forget where it would have landed.
        if !cx.has_active_drag() {
            self.page_drop = None;
        }
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

        // --- Sidebar: one clickable row per page ---------------------------
        // The accent line showing where a dragged page would land: above the
        // row it is drawn in, in the gap between rows.
        let drop_line = move || {
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
                .when(drop_here, |d| d.child(drop_line()))
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
            .when(drop_before == Some(None), |d| d.child(drop_line()));

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
            .when(!favorite_rows.is_empty(), |d| {
                d.child(section_header("FAVORITES")).children(favorite_rows)
            })
            .when(!recent_rows.is_empty(), |d| {
                d.child(section_header("RECENT")).children(recent_rows)
            })
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

        // --- Main pane: title + blocks --------------------------------------
        let page = &self.pages[self.selected];
        #[cfg(test)]
        let mut reading_layouts = Vec::new();
        let visible = page.visible_blocks(&self.collapsed);
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
                let display = (!is_editing).then(|| Rc::new(DisplayBlock::new(&block.content)));
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
                        let highlights = reading_highlights(d, link_style, tag_style);
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
                let on_press = {
                    let (layout, display, link_at) =
                        (text_layout.clone(), display.clone(), link_at.clone());
                    cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                        if link_at(event.position).is_some() {
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
                let on_click = cx.listener(move |this, event: &ClickEvent, _window, cx| {
                    // The press already started editing unless it was on a
                    // link; only links act on click.
                    if this.editing == Some(ix) {
                        return;
                    }
                    if let Some(link) = link_at(event.position()) {
                        this.open_page(&link.target, cx);
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
                let row = block_row(&theme, depth, font_size, kind, content, fold)
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

        let page_menu_overlay = self
            .page_menu
            .as_ref()
            .map(|menu| self.render_page_menu(menu, cx));

        let is_editing = self.editing.is_some() || self.text_input_open();
        let settings_open = self.settings.is_some();
        let page_menu_open = self.page_menu.is_some() && !is_editing;
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
            .when(!settings_open && page_menu_open, |d| {
                d.key_context("PageMenu")
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
            .children(page_menu_overlay)
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
        let h = reading_highlights(&DisplayBlock::new("a **bold** and *it*"), link, tag);
        assert_eq!(h.len(), 2);
        assert_eq!((h[0].0.clone(), h[0].1.font_weight), (2..6, bold));
        assert_eq!((h[1].0.clone(), h[1].1.font_style), (11..13, italic));
        let h = reading_highlights(&DisplayBlock::new("## ***Big*** [[Page]]"), link, tag);
        assert_eq!(h[0].0, 0..3);
        assert_eq!((h[0].1.font_weight, h[0].1.font_style), (bold, italic));
        assert_eq!((h[1].0.clone(), h[1].1), (4..12, link));
        let h = reading_highlights(&DisplayBlock::new("**see [[Page]] #tag**"), link, tag);
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
    fn delete_after_confirm_removes_file_page_and_tab(cx: &mut TestAppContext) {
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

    fn bounds_of(cx: &mut VisualTestContext, selector: &str) -> Bounds<Pixels> {
        let selector: &'static str = Box::leak(selector.to_string().into_boxed_str());
        cx.debug_bounds(selector).expect(selector)
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
}
