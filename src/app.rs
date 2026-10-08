//! The root view: sidebar of pages on the left, selected page on the right.
//!
//! Editing model: there is ONE shared `EditorState`. Clicking a block loads that
//! block's text into it; every other block is drawn as plain text. When editing
//! stops (Escape, click elsewhere, switching pages) the text is written back to
//! the block and the page is saved to disk.

use crate::config::Config;
use crate::editor::{EditorState, Emphasis, SlashMenu};
use crate::graph_view::{GraphEvent, GraphView};
use crate::model::{backlinks, parse_references, tag_counts, BlockKind, Page};
use crate::search::{search, Command, Hit, Target};
use crate::storage::{today_title, Storage};
use crate::ui::{block_row, Theme};
use gpui::{
    actions, anchored, deferred, div, fill, point, prelude::*, px, relative, size, AnyElement, App,
    Bounds, ClickEvent, Context, ElementId, ElementInputHandler, Entity, EntityInputHandler,
    FocusHandle, GlobalElementId, HighlightStyle, Hsla, KeyBinding, LayoutId, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, ShapedLine, SharedString,
    Style, StyledText, Subscription, TextRun, UTF16Selection, UnderlineStyle, Window,
};
use std::ops::Range;
use std::rc::Rc;

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
        ToggleSearch,
        NewPage,
        Undo,
        Redo,
        ToggleTheme,
        ToggleGraph,
        IncreaseFont,
        DecreaseFont,
        ResetFont,
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
        // Global (no context): works whether or not a block is being edited.
        KeyBinding::new("ctrl-k", ToggleSearch, None),
        KeyBinding::new("ctrl-n", NewPage, None),
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
}

const MAX_HISTORY: usize = 100;

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
    /// True between a mouse-down in the edited block and the mouse-up: mouse
    /// moves in between extend the selection.
    selecting: bool,
    undo_stack: Vec<HistoryState>,
    redo_stack: Vec<HistoryState>,
    text_history_active: bool,
    mode: Mode,
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
            let mut page = Page::from_markdown(&today, true, "- \n");
            page.blocks[0].content.clear();
            if let Err(err) = storage.save(&page) {
                eprintln!("notesec: could not create journal {today}: {err}");
            }
            pages.push(page);
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

        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);

        // Only use the configured font if it is actually installed; GPUI would
        // otherwise fall back silently and the user would not know why.
        let font_family = config.font_family.clone().and_then(|family| {
            if cx.text_system().all_font_names().contains(&family) {
                Some(SharedString::from(family))
            } else {
                eprintln!("notesec: font {family:?} is not installed; using the system font");
                None
            }
        });

        NoteSec {
            storage,
            pages,
            selected,
            theme: Theme::from_kind(config.theme),
            config,
            font_family,
            focus_handle,
            editing: None,
            editor: EditorState::default(),
            search: None,
            slash: None,
            selecting: false,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            text_history_active: false,
            mode: Mode::Notes,
            graph: None,
            _graph_subscription: None,
            last_layout: None,
            last_bounds: None,
        }
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

    fn toggle_theme(&mut self, cx: &mut Context<Self>) {
        self.config.theme = self.config.theme.toggled();
        self.theme = Theme::from_kind(self.config.theme);
        self.save_config();
        self.sync_graph_style(cx);
        cx.notify();
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
        }
    }

    // --- search overlay --------------------------------------------------------

    fn search_results(&self) -> Vec<Hit> {
        match &self.search {
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
        // Save whatever block is being edited before covering it.
        self.stop_edit(cx);
        self.search = Some(SearchState {
            query: EditorState::default(),
            selected: 0,
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
        }
        self.save_all_pages();
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
        self.selected = self.find_page(&title).unwrap_or(0);
        self.mode = Mode::Notes;
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
        self.close_search(cx);
        match hit.target {
            Target::Page(page) => {
                let title = self.pages[page].title.clone();
                self.open_page(&title, cx);
            }
            Target::Block(page, block) => {
                let title = self.pages[page].title.clone();
                self.open_page(&title, cx);
                if block < self.pages[self.selected].blocks.len() {
                    self.start_edit(block, window, cx);
                }
            }
            Target::Command(command) => self.run_command(command, cx),
        }
    }

    fn on_toggle_theme(&mut self, _: &ToggleTheme, _: &mut Window, cx: &mut Context<Self>) {
        self.toggle_theme(cx);
    }

    fn on_new_page(&mut self, _: &NewPage, window: &mut Window, cx: &mut Context<Self>) {
        self.new_page(window, cx);
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

    /// Navigate to the page called `title`, creating it if needed.
    fn open_page(&mut self, title: &str, cx: &mut Context<Self>) {
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
        self.selected = ix;
        self.mode = Mode::Notes;
        cx.notify();
    }

    // --- graph view ------------------------------------------------------------

    /// Switch to the graph view, creating it on first use.
    fn show_graph(&mut self, cx: &mut Context<Self>) {
        // Save the block being edited so the graph sees up-to-date links.
        self.stop_edit(cx);
        let current = self.pages[self.selected].title.clone();
        let pages = self.pages.clone();
        if let Some(graph) = self.graph.clone() {
            graph.update(cx, |g, cx| g.refresh(pages, current, cx));
        } else {
            let (theme, size, reduce_motion) =
                (self.theme, self.config.font_size, cx.reduce_motion());
            let graph = cx.new(|_| GraphView::new(pages, current, theme, size, reduce_motion));
            // Clicking a node asks us to open that page.
            self._graph_subscription = Some(cx.subscribe(
                &graph,
                |this, _graph, event: &GraphEvent, cx| {
                    let GraphEvent::OpenPage(title) = event;
                    this.open_page(title, cx);
                },
            ));
            self.graph = Some(graph);
        }
        self.mode = Mode::Graph;
        cx.notify();
    }

    fn toggle_graph(&mut self, cx: &mut Context<Self>) {
        if self.mode == Mode::Graph {
            self.mode = Mode::Notes;
            cx.notify();
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
    }

    fn start_edit(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
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
        let Some(ix) = self.editing else { return };
        self.text_history_active = false;
        self.record_edit();
        let rest = self.editor.split_off_at_cursor();
        self.sync_content();
        let new_ix = self.pages[self.selected].insert_after(ix, rest);
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
        let page = &mut self.pages[self.selected];
        if page.blocks.len() > 1 && page.delete_leaf(ix) {
            self.record_state(before);
            self.save_page();
            // Move to the block above (or the new first block if we deleted #0).
            self.load_editor(ix.saturating_sub(1), false);
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
        if let Some(ix) = self.editing.filter(|&ix| ix > 0) {
            self.move_edit(ix - 1, cx);
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
        if let Some(ix) = self.editing {
            if ix + 1 < self.pages[self.selected].blocks.len() {
                self.move_edit(ix + 1, cx);
            }
        }
    }

    fn escape(&mut self, _: &Escape, _: &mut Window, cx: &mut Context<Self>) {
        if self.search.is_some() {
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

        // --- Sidebar: one clickable row per page ---------------------------
        let sidebar_items = self.pages.iter().enumerate().map(|(ix, page)| {
            let is_selected = ix == self.selected;
            div()
                // Interactive elements need a stable id; (name, index) is the idiom.
                .id(("page", ix))
                .px_3()
                .py_1()
                .rounded_md()
                .cursor_pointer()
                .text_color(if is_selected {
                    theme.accent
                } else {
                    theme.text
                })
                .when(is_selected, |d| d.bg(theme.selected_bg))
                .hover(|d| d.bg(theme.selected_bg))
                // `cx.listener` turns a closure over `&mut Self` into a GPUI handler.
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    this.stop_edit(cx); // save the block being edited first
                    this.selected = ix;
                    this.mode = Mode::Notes;
                    cx.notify(); // ask GPUI to re-render this view
                }))
                .child(page.title.clone())
        });

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
                        this.open_page(&target, cx);
                    }))
                    .child(format!("#{name}"))
                    .child(div().text_color(theme.muted).child(count.to_string()))
                    .into_any_element()
            })
            .collect();
        let has_tags = !tag_rows.is_empty();

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

        let sidebar = div()
            .id("sidebar")
            .w(px(240.0))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .bg(theme.sidebar_bg)
            .border_r_1()
            .border_color(theme.border)
            .overflow_y_scroll()
            .child(graph_item)
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
        let rows: Vec<AnyElement> = page
            .blocks
            .iter()
            .enumerate()
            .map(|(ix, block)| {
                let is_editing = self.editing == Some(ix);
                let depth = page.depth_of(ix);
                // Display mode hides the type prefix (`# `, `> `) and styles
                // the row instead; the editor shows the raw markdown.
                let (kind, body) = if is_editing {
                    (
                        BlockKind::parse(&self.editor.text).0,
                        block.content.as_str(),
                    )
                } else {
                    BlockKind::parse(&block.content)
                };
                let links = Rc::new(parse_references(body));
                // Bytes of `content` hidden in display mode (the type prefix),
                // to map a display position back to an editor offset.
                let hidden = block.content.len() - body.len();
                // Layout of this row's text, kept so a press can be mapped to
                // the character (and so the link) under the mouse.
                let mut text_layout = None;
                let content: AnyElement = if is_editing {
                    div()
                        .on_mouse_down(MouseButton::Left, cx.listener(Self::on_text_mouse_down))
                        .child(BlockText { app: cx.entity() })
                        .into_any_element()
                } else {
                    let highlights = links.iter().map(|l| {
                        let style = if l.is_tag { tag_style } else { link_style };
                        (l.range.clone(), style)
                    });
                    // `with_highlights` resolves against the inherited text
                    // style, so heading size/weight and quote styling (and the
                    // theme's text colour) apply to the unhighlighted parts.
                    let text = StyledText::new(body.to_string()).with_highlights(highlights);
                    text_layout = Some(text.layout().clone());
                    div().child(text).into_any_element()
                };
                // A press on a row that isn't being edited starts editing it
                // with the cursor under the mouse, and a drag from there
                // selects (the root's mouse-move handler extends it). A press
                // on a link does nothing here: the click handler navigates.
                let link_at = {
                    let (layout, links) = (text_layout.clone(), links.clone());
                    move |position: gpui::Point<Pixels>| {
                        let layout = layout.as_ref()?;
                        // `Ok` only when the mouse is over an actual glyph.
                        let char_ix = layout.index_for_position(position).ok()?;
                        links.iter().find(|l| l.range.contains(&char_ix)).cloned()
                    }
                };
                let on_press = {
                    let (layout, link_at) = (text_layout.clone(), link_at.clone());
                    cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                        if link_at(event.position).is_some() {
                            return;
                        }
                        let offset = layout.as_ref().map_or(0, |layout| {
                            match layout.index_for_position(event.position) {
                                Ok(i) | Err(i) => i,
                            }
                        });
                        this.start_edit(ix, window, cx);
                        this.editor.set_cursor(hidden + offset);
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
                let row = block_row(&theme, depth, font_size, kind, content)
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
                        .on_click(
                            cx.listener(move |this, _e, _window, cx| this.open_page(&title, cx)),
                        )
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
                                    .child("No results"),
                            )
                        }),
                )
        });

        // The main area shows either the page or the graph.
        let content: AnyElement = match (&self.mode, &self.graph) {
            (Mode::Graph, Some(graph)) => graph.clone().into_any_element(),
            _ => main.into_any_element(),
        };

        let is_editing = self.editing.is_some() || self.search.is_some();
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
            // the "BlockEditor" key bindings conditional.
            .when(is_editing, |d| d.key_context("BlockEditor"))
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
            .on_action(cx.listener(Self::toggle_search))
            .on_action(cx.listener(Self::on_new_page))
            .on_action(cx.listener(Self::on_undo))
            .on_action(cx.listener(Self::on_redo))
            .on_action(cx.listener(Self::on_toggle_theme))
            .on_action(cx.listener(Self::on_toggle_graph))
            .on_action(cx.listener(Self::on_increase_font))
            .on_action(cx.listener(Self::on_decrease_font))
            .on_action(cx.listener(Self::on_reset_font))
            .child(sidebar)
            .child(content)
            .children(overlay)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ThemeKind;
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
}
