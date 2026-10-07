//! The root view: sidebar of pages on the left, selected page on the right.
//!
//! Editing model: there is ONE shared `EditorState`. Clicking a block loads that
//! block's text into it; every other block is drawn as plain text. When editing
//! stops (Escape, click elsewhere, switching pages) the text is written back to
//! the block and the page is saved to disk.

use crate::editor::EditorState;
use crate::model::{backlinks, parse_wikilinks, Page};
use crate::storage::{today_title, Storage};
use crate::ui::{block_row, Theme};
use gpui::{
    actions, div, fill, point, prelude::*, px, relative, size, AnyElement, App, Bounds, ClickEvent,
    Context, ElementId, ElementInputHandler, Entity, EntityInputHandler, FocusHandle,
    GlobalElementId, HighlightStyle, Hsla, KeyBinding, LayoutId, PaintQuad, Pixels, ShapedLine,
    SharedString, Style, StyledText, TextRun, UTF16Selection, UnderlineStyle, Window,
};
use std::ops::Range;

// Actions are named, typed commands that key bindings map onto. The macro
// declares one unit struct per name inside the `notesec` namespace.
actions!(
    notesec,
    [
        Enter, Tab, ShiftTab, Backspace, Delete, Left, Right, Up, Down, Home, End, Escape, Paste,
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
        KeyBinding::new("ctrl-v", Paste, ctx),
        KeyBinding::new("ctrl-q", Quit, None),
    ]);
    cx.on_action(|_: &Quit, cx| cx.quit());
}

pub struct NoteSec {
    storage: Storage,
    /// All pages, kept sorted for the sidebar (journals first, newest first).
    pages: Vec<Page>,
    /// Index into `pages` of the page shown in the main pane.
    selected: usize,
    theme: Theme,

    /// Handle used to give this view keyboard focus while editing.
    focus_handle: FocusHandle,
    /// Index (into the selected page's blocks) of the block being edited.
    editing: Option<usize>,
    /// The shared text editor for the block being edited.
    editor: EditorState,
    /// Shaped text + bounds from the last paint; needed to answer the OS
    /// input-method's questions about where characters are on screen.
    last_layout: Option<ShapedLine>,
    last_bounds: Option<Bounds<Pixels>>,
}

impl NoteSec {
    pub fn new(storage: Storage, cx: &mut Context<Self>) -> Self {
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

        NoteSec {
            storage,
            pages,
            selected,
            theme: Theme::dark(),
            focus_handle: cx.focus_handle(),
            editing: None,
            editor: EditorState::default(),
            last_layout: None,
            last_bounds: None,
        }
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
            .flat_map(|b| parse_wikilinks(&b.content))
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
        cx.notify();
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
        if cursor_at_start {
            self.editor.cursor = 0;
        }
        self.editing = Some(ix);
    }

    fn start_edit(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.commit();
        self.load_editor(ix, false);
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn stop_edit(&mut self, cx: &mut Context<Self>) {
        self.commit();
        self.editing = None;
        self.last_layout = None;
        self.last_bounds = None;
        cx.notify();
    }

    /// Commit the current block and start editing block `ix` (arrow-key
    /// navigation between blocks).
    fn move_edit(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.commit();
        self.load_editor(ix, false);
        cx.notify();
    }

    // --- action handlers -----------------------------------------------------

    /// Enter: split the block at the cursor; the right half becomes a new block.
    fn enter(&mut self, _: &Enter, _: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.editing else { return };
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
        self.sync_content();
        if op(&mut self.pages[self.selected], ix) {
            self.save_page();
        }
        cx.notify();
    }

    fn backspace(&mut self, _: &Backspace, _: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.editing else { return };
        if !self.editor.text.is_empty() {
            self.editor.backspace();
            cx.notify();
            return;
        }
        // Empty block: delete it (never the last remaining block, and never a
        // block that still has children).
        let page = &mut self.pages[self.selected];
        if page.blocks.len() > 1 && page.delete_leaf(ix) {
            self.save_page();
            // Move to the block above (or the new first block if we deleted #0).
            self.load_editor(ix.saturating_sub(1), false);
        }
        cx.notify();
    }

    fn delete(&mut self, _: &Delete, _: &mut Window, cx: &mut Context<Self>) {
        if self.editing.is_some() {
            self.editor.delete();
            cx.notify();
        }
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        self.editor.move_left();
        cx.notify();
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        self.editor.move_right();
        cx.notify();
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.editor.move_home();
        cx.notify();
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.editor.move_end();
        cx.notify();
    }

    fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.editing.filter(|&ix| ix > 0) {
            self.move_edit(ix - 1, cx);
        }
    }

    fn down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.editing {
            if ix + 1 < self.pages[self.selected].blocks.len() {
                self.move_edit(ix + 1, cx);
            }
        }
    }

    fn escape(&mut self, _: &Escape, _: &mut Window, cx: &mut Context<Self>) {
        self.stop_edit(cx);
    }

    fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.editor.insert(&text);
            cx.notify();
        }
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
        let range = self.editor.range_from_utf16(&range_utf16);
        actual_range.replace(self.editor.range_to_utf16(&range));
        Some(self.editor.text[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        // We have a cursor but no selection, so the range is empty.
        let c = self.editor.cursor;
        Some(UTF16Selection {
            range: self.editor.range_to_utf16(&(c..c)),
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.editor
            .marked
            .as_ref()
            .map(|r| self.editor.range_to_utf16(r))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.editor.marked = None;
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
            .map(|r| self.editor.range_from_utf16(r))
            .or(self.editor.marked.clone())
            .unwrap_or(self.editor.cursor..self.editor.cursor);
        self.editor.replace_range(range, new_text);
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
            .map(|r| self.editor.range_from_utf16(r))
            .or(self.editor.marked.clone())
            .unwrap_or(self.editor.cursor..self.editor.cursor);
        self.editor.replace_range(range.clone(), new_text);
        let len = self.editor.cursor - range.start;
        self.editor.marked = (len > 0).then_some(range.start..range.start + len);
        if let Some(sel) = new_selected_range_utf16 {
            // The OS gives the cursor relative to the composition text.
            let end = self.editor.offset_from_utf16(sel.end).min(len);
            self.editor.cursor = range.start + end;
        }
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
        let range = self.editor.range_from_utf16(&range_utf16);
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
        Some(self.editor.offset_to_utf16(utf8))
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
        let text: SharedString = app.editor.text.clone().into();
        let cursor = app.editor.cursor;
        let marked = app.editor.marked.clone();
        let accent: Hsla = app.theme.accent.into();
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
        PrepaintState { line, cursor }
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        // Style for `[[wikilinks]]` in display mode: accent colour + underline.
        let text_style = window.text_style();
        let link_style = HighlightStyle {
            color: Some(theme.accent.into()),
            underline: Some(UnderlineStyle {
                color: Some(theme.accent.into()),
                thickness: px(1.0),
                wavy: false,
            }),
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
                    cx.notify(); // ask GPUI to re-render this view
                }))
                .child(page.title.clone())
        });

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
            .child(div().px_3().py_2().text_color(theme.muted).child("PAGES"))
            .children(sidebar_items);

        // --- Main pane: title + blocks --------------------------------------
        let page = &self.pages[self.selected];
        let rows: Vec<AnyElement> = page
            .blocks
            .iter()
            .enumerate()
            .map(|(ix, block)| {
                let is_editing = self.editing == Some(ix);
                let depth = page.depth_of(ix);
                let links = parse_wikilinks(&block.content);
                // Layout of this row's text, kept so a click can be mapped to
                // the character (and so the link) under the mouse.
                let mut text_layout = None;
                let content: AnyElement = if is_editing {
                    BlockText { app: cx.entity() }.into_any_element()
                } else if links.is_empty() {
                    div().child(block.content.clone()).into_any_element()
                } else {
                    let highlights = links.iter().map(|l| (l.range.clone(), link_style));
                    let text = StyledText::new(block.content.clone())
                        .with_default_highlights(&text_style, highlights);
                    text_layout = Some(text.layout().clone());
                    div().child(text).into_any_element()
                };
                // One click handler per row: follow the link under the mouse,
                // or else start editing the block. Deciding in one place avoids
                // the link click also triggering edit mode.
                let on_click = cx.listener(move |this, event: &ClickEvent, window, cx| {
                    let link = text_layout.as_ref().and_then(|layout| {
                        // `Ok` only when the mouse is over an actual glyph.
                        let char_ix = layout.index_for_position(event.position()).ok()?;
                        links.iter().find(|l| l.range.contains(&char_ix))
                    });
                    match link {
                        Some(link) => this.open_page(&link.target, cx),
                        None => this.start_edit(ix, window, cx),
                    }
                });
                block_row(&theme, depth, content)
                    .id(("block", ix))
                    // Lets tests find this row's on-screen bounds; a no-op in
                    // normal builds.
                    .debug_selector(|| format!("block-{ix}"))
                    .cursor_text()
                    .when(!is_editing, |d| d.on_click(on_click))
                    .into_any_element()
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
                    .text_3xl()
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

        let is_editing = self.editing.is_some();
        div()
            .size_full()
            .flex()
            .flex_row()
            .bg(theme.bg)
            .text_color(theme.text)
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
            .on_action(cx.listener(Self::paste))
            .child(sidebar)
            .child(main)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Modifiers, TestAppContext, VisualTestContext};
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
        let (view, cx) = cx.add_window_view(|_, cx| NoteSec::new(storage, cx));
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
}
