//! Whiteboards in the app (decision 53): the canvas that replaces the
//! outline on a `type:: whiteboard` page, its mouse and key handling, and
//! the commands. The page model and the maths are in `crate::whiteboard`.
//!
//! The canvas element: card boxes are absolutely placed `div`s (only the
//! ones in view), arrows are painted with `canvas()` + `PathBuilder`
//! behind them, and every press is hit-tested in canvas coordinates
//! (`geom::hit`), not by per-card listeners. A card's text is edited with
//! the outline's own block editor (`BlockText`), which is told to hide
//! the card's `x::`/`y::`/… lines (`editor_source`/`editor_content`).
//! Moves and resizes are drawn from the drag state and written once, on
//! release: one undo step and one save each.

use std::collections::{HashMap, HashSet};

use gpui::{
    canvas, div, point, prelude::*, px, AnyElement, Bounds, Context, CursorStyle, HighlightStyle,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PathBuilder, PinchEvent, Pixels,
    ScrollDelta, ScrollWheelEvent, StyledText, UnderlineStyle, Window,
};
use uuid::Uuid;

use super::{
    reading_highlights, BlockText, Mode, Nav, NewWhiteboard, NoteSec, Status, ToggleSearch,
    ToggleWhiteboardOutline, WhiteboardAddPage, WhiteboardDelete, WhiteboardFit,
    WhiteboardZoomReset,
};
use crate::display::DisplayBlock;
use crate::model::find_block;
use crate::search::Target;
use crate::whiteboard::geom::{self, pt, Hit, Pt, Rect, Viewport};
use crate::whiteboard::{self, Board, CardKind};

/// Below this zoom cards show no text (unreadable anyway, and cheaper).
const TEXT_MIN_ZOOM: f32 = 0.35;
/// One wheel "line" in pixels, and how strongly Ctrl+wheel zooms.
const LINE: f32 = 20.0;
const WHEEL_ZOOM: f32 = 0.0015;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Selection {
    Card(Uuid),
    Edge(Uuid),
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Drag {
    Pan {
        from: Pt,
        offset: Pt,
    },
    Move {
        id: Uuid,
        from: Pt,
        start: Rect,
        rect: Rect,
        /// A page card pressed on its title: opened if not moved.
        title: bool,
    },
    Resize {
        id: Uuid,
        from: Pt,
        start: Rect,
        rect: Rect,
    },
    Connect {
        from: Uuid,
        at: Pt,
    },
}

#[derive(Default)]
pub(super) struct WhiteboardState {
    /// Each board's view, by page title (fitted on first show).
    views: HashMap<String, Viewport>,
    pub(super) selected: Option<Selection>,
    drag: Option<Drag>,
    /// The canvas element's bounds in the window, from the last paint.
    bounds: Option<Bounds<Pixels>>,
    /// Whiteboard pages shown as their outline instead.
    outline: HashSet<String>,
    /// The palette was opened by "Add page": the pick becomes a card on
    /// this board.
    picking: Option<String>,
    /// Space is held: a left drag pans.
    space: bool,
    /// A view was fitted before the canvas had a size: refit it once the
    /// size is known (next frame).
    fit_blind: bool,
}

impl NoteSec {
    // --- state ------------------------------------------------------------

    /// The page on screen is a whiteboard shown as a canvas.
    pub(super) fn whiteboard_shown(&self) -> bool {
        self.mode == Mode::Notes
            && self.pages.get(self.selected).is_some_and(|p| {
                whiteboard::is_whiteboard(p) && !self.whiteboard.outline.contains(&p.title)
            })
    }

    /// A card's text is being edited.
    pub(super) fn card_editing(&self) -> bool {
        self.editing.is_some() && self.whiteboard_shown()
    }

    /// The canvas has the keyboard: shown, nothing edited, nothing over it.
    pub(super) fn whiteboard_focused(&self) -> bool {
        self.whiteboard_shown()
            && self.editing.is_none()
            && !self.overlay_open()
            && !self.text_input_open()
    }

    /// Block `ix` of the selected page is a card of a board shown as a
    /// canvas: the editor shows its text without the canvas properties.
    fn is_card_block(&self, ix: usize) -> bool {
        let page = &self.pages[self.selected];
        whiteboard::is_whiteboard(page)
            && !self.whiteboard.outline.contains(&page.title)
            && whiteboard::parse(page).cards.iter().any(|c| c.block == ix)
    }

    /// What the editor gets for block `ix`.
    pub(super) fn editor_source(&self, ix: usize) -> String {
        let content = &self.pages[self.selected].blocks[ix].content;
        if self.is_card_block(ix) {
            whiteboard::card_text(content)
        } else {
            content.clone()
        }
    }

    /// Block `ix`'s content with the editor's text in it.
    pub(super) fn editor_content(&self, ix: usize) -> String {
        let content = &self.pages[self.selected].blocks[ix].content;
        if self.is_card_block(ix) {
            whiteboard::with_text(content, &self.editor.text)
        } else {
            self.editor.text.clone()
        }
    }

    fn board(&self) -> Board {
        whiteboard::parse(&self.pages[self.selected])
    }

    fn canvas_size(&self) -> (f32, f32) {
        self.whiteboard.bounds.map_or((1000.0, 700.0), |b| {
            (f32::from(b.size.width), f32::from(b.size.height))
        })
    }

    /// The view of the board on screen (fitted until first changed).
    pub(super) fn board_view(&self) -> Viewport {
        let title = &self.pages[self.selected].title;
        self.whiteboard
            .views
            .get(title)
            .copied()
            .unwrap_or_else(|| {
                let (w, h) = self.canvas_size();
                Viewport::fit(&self.board().rects(), w, h)
            })
    }

    fn set_view(&mut self, view: Viewport) {
        let title = self.pages[self.selected].title.clone();
        self.whiteboard.views.insert(title, view);
    }

    /// A window position relative to the canvas.
    pub(super) fn local(&self, p: gpui::Point<Pixels>) -> Option<Pt> {
        let b = self.whiteboard.bounds?;
        Some(pt(f32::from(p.x - b.origin.x), f32::from(p.y - b.origin.y)))
    }

    fn block_ix(&self, id: Uuid) -> Option<usize> {
        self.pages[self.selected]
            .blocks
            .iter()
            .position(|b| b.id == id)
    }

    /// Record undo, change the board's page, save it (one step).
    fn change_board(
        &mut self,
        cx: &mut Context<Self>,
        op: impl FnOnce(&mut crate::model::Page) -> bool,
    ) -> bool {
        let before = self.history_state();
        let changed = op(&mut self.pages[self.selected]);
        if changed {
            self.record_state(before);
            self.save_page();
        }
        cx.notify();
        changed
    }

    // --- commands -----------------------------------------------------------

    pub(super) fn on_new_whiteboard(
        &mut self,
        _: &NewWhiteboard,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.stop_edit(cx);
        let title = (0..)
            .map(|n| match n {
                0 => "Whiteboard".to_string(),
                n => format!("Whiteboard {n}"),
            })
            .find(|t| self.find_page(t).is_none())
            .expect("a free title");
        let page = whiteboard::new_page(&title);
        if let Err(err) = self.storage.save(&page) {
            let text = format!("Could not create {title}: {err}");
            return self.show_status(Status { text, error: true }, cx);
        }
        self.add_page(page);
        self.navigate(&title, Nav::Tab, cx);
        cx.notify();
    }

    pub(super) fn on_whiteboard_fit(
        &mut self,
        _: &WhiteboardFit,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.whiteboard_shown() {
            let (w, h) = self.canvas_size();
            let view = Viewport::fit(&self.board().rects(), w, h);
            self.set_view(view);
            cx.notify();
        }
    }

    pub(super) fn on_whiteboard_zoom_reset(
        &mut self,
        _: &WhiteboardZoomReset,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.whiteboard_shown() {
            let (w, h) = self.canvas_size();
            let mut view = self.board_view();
            view.zoom_at(pt(w / 2.0, h / 2.0), 1.0 / view.zoom);
            self.set_view(view);
            cx.notify();
        }
    }

    pub(super) fn on_toggle_whiteboard_outline(
        &mut self,
        _: &ToggleWhiteboardOutline,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(page) = self.pages.get(self.selected) else {
            return;
        };
        if self.mode != Mode::Notes || !whiteboard::is_whiteboard(page) {
            return;
        }
        let title = page.title.clone();
        self.stop_edit(cx);
        if !self.whiteboard.outline.remove(&title) {
            self.whiteboard.outline.insert(title);
        }
        self.whiteboard.selected = None;
        self.whiteboard.drag = None;
        cx.notify();
    }

    /// "Add page": the palette picks a page (or a block) for a new card.
    pub(super) fn on_whiteboard_add_page(
        &mut self,
        _: &WhiteboardAddPage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.whiteboard_shown() {
            return;
        }
        let title = self.pages[self.selected].title.clone();
        if self.search.is_none() {
            self.toggle_search(&ToggleSearch, window, cx);
        }
        self.whiteboard.picking = Some(title);
    }

    /// The palette is closing.
    pub(super) fn forget_whiteboard_pick(&mut self) {
        self.whiteboard.picking = None;
    }

    pub(super) fn take_whiteboard_pick(&mut self) -> Option<String> {
        self.whiteboard.picking.take()
    }

    /// The palette's pick while "Add page" asked: a page card, or a live
    /// block card, in the middle of the view. False for other picks.
    pub(super) fn add_picked_card(
        &mut self,
        board: &str,
        target: &Target,
        cx: &mut Context<Self>,
    ) -> bool {
        let text = match *target {
            Target::Page(p) => format!("[[{}]]", self.pages[p].title),
            Target::Block(p, b)
            | Target::Match {
                page: p, block: b, ..
            } => {
                format!("(({}))", self.pages[p].blocks[b].id)
            }
            _ => return false,
        };
        if self.pages[self.selected].title != board || !self.whiteboard_shown() {
            return false;
        }
        let (w, h) = self.canvas_size();
        let view = self.board_view();
        let c = view.to_world(pt(w / 2.0, h / 2.0));
        let rect = Rect::new(
            c.x - whiteboard::CARD_W / 2.0,
            c.y - whiteboard::CARD_H / 2.0,
            whiteboard::CARD_W,
            whiteboard::CARD_H,
        );
        self.set_view(view);
        let mut id = None;
        self.change_board(cx, |page| {
            id = Some(whiteboard::add_card(page, &text, rect));
            true
        });
        self.whiteboard.selected = id.map(Selection::Card);
        true
    }

    /// Delete / Backspace on the canvas: the selected card (with its
    /// arrows) or arrow.
    pub(super) fn on_whiteboard_delete(
        &mut self,
        _: &WhiteboardDelete,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.whiteboard_focused() {
            return;
        }
        match self.whiteboard.selected.take() {
            Some(Selection::Card(id)) => {
                self.change_board(cx, |page| whiteboard::delete_card(page, id));
            }
            Some(Selection::Edge(id)) => {
                self.change_board(cx, |page| whiteboard::delete_edge(page, id));
            }
            None => {}
        }
    }

    /// Escape on the canvas: drop the selection. True if it did something.
    pub(super) fn whiteboard_escape(&mut self, cx: &mut Context<Self>) -> bool {
        if self.whiteboard_focused() && self.whiteboard.selected.is_some() {
            self.whiteboard.selected = None;
            cx.notify();
            return true;
        }
        false
    }

    fn edit_card(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.block_ix(id) {
            self.whiteboard.selected = Some(Selection::Card(id));
            self.start_edit(ix, window, cx);
        }
    }

    // --- mouse --------------------------------------------------------------

    fn on_board_mouse_down(
        &mut self,
        e: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(at) = self.local(e.position) else {
            return;
        };
        window.focus(&self.focus_handle, cx);
        if self.editing.is_some() {
            // A press outside the card being edited ends its editing.
            self.stop_edit(cx);
        }
        let view = self.board_view();
        if e.button == MouseButton::Middle
            || (e.button == MouseButton::Left && self.whiteboard.space)
        {
            self.set_view(view);
            self.whiteboard.drag = Some(Drag::Pan {
                from: at,
                offset: view.offset,
            });
            return cx.notify();
        }
        let board = self.board();
        let selected_card = match self.whiteboard.selected {
            Some(Selection::Card(id)) => Some(id),
            _ => None,
        };
        let hit = geom::hit(&board, &view, at, selected_card);
        if e.click_count >= 2 {
            match hit {
                Hit::Card(id) | Hit::PageTitle(id) | Hit::Resize(id) => {
                    self.edit_card(id, window, cx)
                }
                Hit::Empty => self.add_text_card(view.to_world(at), window, cx),
                _ => {}
            }
            return cx.notify();
        }
        self.whiteboard.drag = match hit {
            Hit::Empty => {
                self.whiteboard.selected = None;
                self.set_view(view);
                Some(Drag::Pan {
                    from: at,
                    offset: view.offset,
                })
            }
            Hit::Edge(id) => {
                self.whiteboard.selected = Some(Selection::Edge(id));
                None
            }
            Hit::Card(id) | Hit::PageTitle(id) => {
                self.whiteboard.selected = Some(Selection::Card(id));
                board.card(id).map(|c| Drag::Move {
                    id,
                    from: at,
                    start: c.rect,
                    rect: c.rect,
                    title: matches!(hit, Hit::PageTitle(_)),
                })
            }
            Hit::Resize(id) => board.card(id).map(|c| Drag::Resize {
                id,
                from: at,
                start: c.rect,
                rect: c.rect,
            }),
            Hit::Connect(id) => Some(Drag::Connect { from: id, at }),
        };
        cx.notify();
    }

    /// Mouse moves anywhere in the window (the root listens), so a drag
    /// goes on past the canvas edge.
    pub(super) fn on_board_mouse_move(
        &mut self,
        e: &MouseMoveEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(drag) = self.whiteboard.drag else {
            return;
        };
        let Some(at) = self.local(e.position) else {
            return;
        };
        let zoom = self.board_view().zoom;
        self.whiteboard.drag = Some(match drag {
            Drag::Pan { from, offset } => {
                let mut view = self.board_view();
                view.offset = pt(offset.x + at.x - from.x, offset.y + at.y - from.y);
                self.set_view(view);
                drag
            }
            Drag::Move {
                id,
                from,
                start,
                title,
                ..
            } => Drag::Move {
                id,
                from,
                start,
                title,
                rect: Rect::new(
                    start.x + (at.x - from.x) / zoom,
                    start.y + (at.y - from.y) / zoom,
                    start.w,
                    start.h,
                ),
            },
            Drag::Resize {
                id, from, start, ..
            } => Drag::Resize {
                id,
                from,
                start,
                rect: Rect::new(
                    start.x,
                    start.y,
                    (start.w + (at.x - from.x) / zoom)
                        .clamp(whiteboard::MIN_W, whiteboard::MAX_SIZE),
                    (start.h + (at.y - from.y) / zoom)
                        .clamp(whiteboard::MIN_H, whiteboard::MAX_SIZE),
                ),
            },
            Drag::Connect { from, .. } => Drag::Connect { from, at },
        });
        cx.notify();
    }

    /// The end of a drag: written to the page now, as one undo step.
    pub(super) fn on_board_mouse_up(
        &mut self,
        e: &MouseUpEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(drag) = self.whiteboard.drag.take() else {
            return;
        };
        if !self.whiteboard_shown() {
            return;
        }
        match drag {
            Drag::Pan { .. } => {}
            Drag::Move {
                id,
                start,
                rect,
                title,
                ..
            } => {
                let moved = (rect.x - start.x).abs() >= 1.0 || (rect.y - start.y).abs() >= 1.0;
                if moved {
                    self.change_board(cx, |page| whiteboard::set_rect(page, id, rect));
                } else if title {
                    self.open_page_card(id, cx);
                }
            }
            Drag::Resize {
                id, start, rect, ..
            } => {
                if rect != start {
                    self.change_board(cx, |page| whiteboard::set_rect(page, id, rect));
                }
            }
            Drag::Connect { from, .. } => {
                let at = self.local(e.position);
                let board = self.board();
                let view = self.board_view();
                let target = at.and_then(|at| match geom::hit(&board, &view, at, None) {
                    Hit::Card(to) | Hit::PageTitle(to) if to != from => Some(to),
                    _ => None,
                });
                if let Some(to) = target {
                    let mut edge = None;
                    self.change_board(cx, |page| {
                        edge = whiteboard::connect(page, from, to);
                        edge.is_some()
                    });
                    if let Some(edge) = edge {
                        self.whiteboard.selected = Some(Selection::Edge(edge));
                    }
                }
            }
        }
        cx.notify();
    }

    fn on_board_scroll(&mut self, e: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(at) = self.local(e.position) else {
            return;
        };
        let (dx, dy) = match e.delta {
            ScrollDelta::Pixels(p) => (f32::from(p.x), f32::from(p.y)),
            ScrollDelta::Lines(l) => (l.x * LINE, l.y * LINE),
        };
        let mut view = self.board_view();
        if e.modifiers.control || e.modifiers.platform {
            // Wheel up (positive) zooms in, around the pointer.
            view.zoom_at(at, (dy * WHEEL_ZOOM * 4.0).exp());
        } else {
            view.offset = pt(view.offset.x + dx, view.offset.y + dy);
        }
        self.set_view(view);
        cx.stop_propagation();
        cx.notify();
    }

    fn on_board_pinch(&mut self, e: &PinchEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(at) = self.local(e.position) else {
            return;
        };
        let mut view = self.board_view();
        view.zoom_at(at, 1.0 + e.delta);
        self.set_view(view);
        cx.notify();
    }

    /// Double-click on empty canvas: a text card there, being edited.
    fn add_text_card(&mut self, world: Pt, window: &mut Window, cx: &mut Context<Self>) {
        let rect = Rect::new(
            world.x - whiteboard::CARD_W / 2.0,
            world.y - whiteboard::CARD_H / 2.0,
            whiteboard::CARD_W,
            whiteboard::CARD_H,
        );
        let mut id = None;
        self.change_board(cx, |page| {
            id = Some(whiteboard::add_card(page, "", rect));
            true
        });
        if let Some(id) = id {
            self.edit_card(id, window, cx);
        }
    }

    fn open_page_card(&mut self, id: Uuid, cx: &mut Context<Self>) {
        let board = self.board();
        let Some(CardKind::Page(name)) = board.card(id).map(|c| c.kind.clone()) else {
            return;
        };
        match self.link_target(&name) {
            Some(ix) => {
                let title = self.pages[ix].title.clone();
                self.whiteboard.selected = None;
                self.navigate(&title, Nav::Tab, cx);
            }
            None => {
                let text = format!("No page called \u{201c}{name}\u{201d} yet");
                self.show_status(Status { text, error: false }, cx);
            }
        }
    }

    /// Space held (pans with a left drag) on the canvas.
    pub(super) fn set_whiteboard_space(&mut self, down: bool) {
        self.whiteboard.space = down && self.whiteboard_focused();
    }

    // --- drawing -------------------------------------------------------------

    pub(super) fn render_whiteboard(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let font_size = self.config.font_size;
        let mut board = self.board();
        let view = self.board_view();
        // A card being dragged is drawn where the drag has it.
        if let Some(Drag::Move { id, rect, .. } | Drag::Resize { id, rect, .. }) =
            self.whiteboard.drag
        {
            if let Some(card) = board.cards.iter_mut().find(|c| c.id == id) {
                card.rect = rect;
            }
        }
        // The view drawn is the view clicked: kept, so it doesn't jump when
        // a card moves (decision 53).
        if !self
            .whiteboard
            .views
            .contains_key(&self.pages[self.selected].title)
        {
            self.whiteboard.fit_blind |= self.whiteboard.bounds.is_none();
            self.set_view(view);
        }
        let (w, h) = self.canvas_size();
        let shown = geom::visible_cards(&board, &view, w, h);
        let editing_block = self.editing;
        let selected = self.whiteboard.selected;
        let zoom = view.zoom;

        // Arrows (screen coordinates relative to the canvas).
        let mut lines: Vec<(Pt, Pt, bool)> = board
            .edges
            .iter()
            .filter_map(|e| {
                let (a, b) = geom::edge_line(&board, e.from, e.to)?;
                Some((
                    view.to_screen(a),
                    view.to_screen(b),
                    selected == Some(Selection::Edge(e.id)),
                ))
            })
            .collect();
        let labels: Vec<(Pt, String)> = board
            .edges
            .iter()
            .filter_map(|e| {
                let label = e.label.clone()?;
                let (a, b) = geom::edge_line(&board, e.from, e.to)?;
                Some((
                    view.to_screen(pt((a.x + b.x) / 2.0, (a.y + b.y) / 2.0)),
                    label,
                ))
            })
            .collect();
        if let Some(Drag::Connect { from, at }) = self.whiteboard.drag {
            if let Some(card) = board.card(from) {
                let s = view.rect_to_screen(card.rect);
                let start = s.border_point(at);
                lines.push((start, at, true));
            }
        }
        let entity = cx.entity();
        let (edge_color, selected_color) = (theme.muted, theme.accent);
        let arrows = canvas(
            move |bounds, window, cx| {
                // The first size: a view fitted without it is fitted again
                // on the next frame (a notify while drawing is dropped).
                let refit = entity.update(cx, |this, _| {
                    this.whiteboard.bounds = Some(bounds);
                    std::mem::take(&mut this.whiteboard.fit_blind)
                });
                if refit {
                    let entity = entity.clone();
                    window.on_next_frame(move |window, cx| {
                        entity.update(cx, |this, cx| {
                            if this.whiteboard_shown() {
                                this.on_whiteboard_fit(&WhiteboardFit, window, cx);
                            }
                        });
                    });
                }
            },
            move |bounds, (), window, _cx| {
                let o = bounds.origin;
                let at = |p: Pt| o + point(px(p.x), px(p.y));
                for (a, b, sel) in lines {
                    let color = if sel { selected_color } else { edge_color };
                    let mut line = PathBuilder::stroke(px(if sel { 2.5 } else { 1.5 }));
                    line.move_to(at(a));
                    line.line_to(at(b));
                    if let Ok(path) = line.build() {
                        window.paint_path(path, color);
                    }
                    let [tip, l, r] = geom::arrowhead(a, b);
                    let mut head = PathBuilder::fill();
                    head.move_to(at(tip));
                    head.line_to(at(l));
                    head.line_to(at(r));
                    head.close();
                    if let Ok(path) = head.build() {
                        window.paint_path(path, color);
                    }
                }
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full();

        let link_style = HighlightStyle {
            color: Some(theme.accent.into()),
            underline: Some(UnderlineStyle {
                color: Some(theme.accent.into()),
                thickness: px(1.0),
                wavy: false,
            }),
            ..Default::default()
        };
        let tag_style = HighlightStyle {
            color: Some(theme.accent.into()),
            ..Default::default()
        };
        let ref_style = HighlightStyle {
            background_color: Some(gpui::Hsla::from(theme.accent).opacity(0.10)),
            ..Default::default()
        };
        let resolve =
            |id| find_block(&self.pages, id).map(|(p, b)| self.pages[p].blocks[b].content.clone());
        let styled = |text: &str| {
            let d = DisplayBlock::with_refs(text, resolve);
            StyledText::new(d.text.clone())
                .with_highlights(reading_highlights(&d, link_style, tag_style, ref_style))
        };

        let mut cards: Vec<AnyElement> = Vec::new();
        for &i in &shown {
            let card = &board.cards[i];
            let s = view.rect_to_screen(card.rect);
            let is_selected = selected == Some(Selection::Card(card.id));
            let is_editing = editing_block == Some(card.block);
            let fill = card
                .color
                .and_then(|n| whiteboard::COLORS.iter().find(|(c, _)| *c == n))
                .map(|(_, rgb)| gpui::rgb(*rgb));
            let text_color = if fill.is_some() {
                theme.ink
            } else {
                theme.text
            };
            let mut el = div()
                .debug_selector(move || format!("wb-card-{i}"))
                .absolute()
                .left(px(s.x))
                .top(px(s.y))
                .w(px(s.w))
                .h(px(s.h))
                .overflow_hidden()
                .rounded_md()
                .border_1()
                .border_color(if is_selected || is_editing {
                    theme.accent
                } else {
                    theme.border
                })
                .bg(fill.unwrap_or(theme.sidebar_bg))
                .text_color(text_color)
                .p(px(6.0 * zoom))
                .text_size(px(font_size * zoom))
                .flex()
                .flex_col();
            if is_editing {
                // The block editor itself, at its usual size; a press in it
                // places the cursor instead of reaching the canvas.
                el = el.text_size(px(font_size)).child(
                    div()
                        .debug_selector(|| "wb-card-editor".to_string())
                        .cursor(CursorStyle::IBeam)
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, e: &MouseDownEvent, w, cx| {
                                cx.stop_propagation();
                                this.on_text_mouse_down(e, w, cx);
                            }),
                        )
                        .child(BlockText { app: cx.entity() }),
                );
            } else if zoom >= TEXT_MIN_ZOOM {
                match &card.kind {
                    CardKind::Text => {
                        el = el.child(styled(&card.text));
                    }
                    CardKind::Page(name) => {
                        let target = self.link_target(name);
                        let lines: Vec<String> = target
                            .map(|p| {
                                let page = &self.pages[p];
                                page.blocks
                                    .iter()
                                    .map(|b| whiteboard::card_text(&b.content))
                                    .filter(|t| !t.trim().is_empty())
                                    .take(3)
                                    .collect()
                            })
                            .unwrap_or_default();
                        el = el
                            .child(
                                div()
                                    .debug_selector(move || format!("wb-card-{i}-title"))
                                    .h(px(geom::TITLE_H * zoom))
                                    .flex_shrink_0()
                                    .text_color(theme.accent)
                                    .cursor(CursorStyle::PointingHand)
                                    .child(format!("\u{1f4c4} {name}")),
                            )
                            .when(target.is_none(), |d| {
                                d.child(div().text_color(theme.muted).child("No such page yet"))
                            })
                            .children(lines.into_iter().map(|l| div().child(styled(&l))));
                    }
                    CardKind::Block(id) => {
                        el = el.child(match find_block(&self.pages, *id) {
                            Some((p, b)) => {
                                let text = whiteboard::card_text(&self.pages[p].blocks[b].content);
                                div().child(styled(&text)).into_any_element()
                            }
                            None => div()
                                .text_color(theme.muted)
                                .child("Block not found")
                                .into_any_element(),
                        });
                    }
                }
            }
            cards.push(el.into_any_element());
            if is_selected && !is_editing {
                let (resize, connect) = geom::handles(s);
                let handle = |r: Rect, id: &'static str, round: bool| {
                    div()
                        .debug_selector(move || id.to_string())
                        .absolute()
                        .left(px(r.x))
                        .top(px(r.y))
                        .w(px(r.w))
                        .h(px(r.h))
                        .bg(theme.accent)
                        .when(round, |d| d.rounded_full())
                        .into_any_element()
                };
                cards.push(handle(resize, "wb-resize", false));
                cards.push(handle(connect, "wb-connect", true));
            }
        }
        let labels = labels.into_iter().map(|(p, label)| {
            div()
                .absolute()
                .left(px(p.x - 60.0))
                .top(px(p.y - 18.0))
                .w(px(120.0))
                .flex()
                .justify_center()
                .text_size(px(font_size * 0.8))
                .text_color(theme.muted)
                .child(label)
        });

        let button = |id: &'static str, label: String| {
            div()
                .id(id)
                .debug_selector(move || id.to_string())
                .px_2()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .bg(theme.bg)
                .cursor_pointer()
                .hover(|d| d.bg(theme.selected_bg))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(label)
        };
        let toolbar = div()
            .absolute()
            .top(px(8.0))
            .right(px(8.0))
            .flex()
            .flex_row()
            .gap_1()
            .text_size(px(font_size * 0.85))
            .child(
                div()
                    .debug_selector(|| "wb-zoom".to_string())
                    .px_2()
                    .text_color(theme.muted)
                    .child(format!("{:.0}%", zoom * 100.0)),
            )
            .child(button("wb-fit", "Fit".into()).on_click(
                cx.listener(|this, _, w, cx| this.on_whiteboard_fit(&WhiteboardFit, w, cx)),
            ))
            .child(button("wb-zoom-reset", "100%".into()).on_click(cx.listener(
                |this, _, w, cx| this.on_whiteboard_zoom_reset(&WhiteboardZoomReset, w, cx),
            )))
            .child(button("wb-add-page", "Add page".into()).on_click(
                cx.listener(|this, _, w, cx| {
                    this.on_whiteboard_add_page(&WhiteboardAddPage, w, cx)
                }),
            ))
            .child(button("wb-outline", "Outline".into()).on_click(cx.listener(
                |this, _, w, cx| this.on_toggle_whiteboard_outline(&ToggleWhiteboardOutline, w, cx),
            )));
        // The selected card's colour: swatches under the toolbar.
        let swatches = match selected {
            Some(Selection::Card(card)) if editing_block.is_none() => Some(
                div()
                    .absolute()
                    .top(px(8.0 + font_size * 1.8))
                    .right(px(8.0))
                    .flex()
                    .flex_row()
                    .gap_1()
                    .children(
                        std::iter::once(None)
                            .chain(
                                whiteboard::COLORS
                                    .iter()
                                    .map(|&(name, rgb)| Some((name, rgb))),
                            )
                            .map(|color| {
                                let name = color.map(|(n, _)| n);
                                let id = format!("wb-color-{}", name.unwrap_or("none"));
                                div()
                                    .id(gpui::SharedString::from(id.clone()))
                                    .debug_selector(move || id.clone())
                                    .size(px(16.0))
                                    .rounded_full()
                                    .border_1()
                                    .border_color(theme.border)
                                    .bg(color.map_or(theme.sidebar_bg, |(_, rgb)| gpui::rgb(rgb)))
                                    .cursor_pointer()
                                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                        cx.stop_propagation()
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.change_board(cx, |page| {
                                            whiteboard::set_color(page, card, name)
                                        });
                                    }))
                            }),
                    ),
            ),
            _ => None,
        };
        let title = div()
            .absolute()
            .top(px(8.0))
            .left(px(12.0))
            .text_size(px(font_size * 1.3))
            .text_color(theme.text)
            .child(self.pages[self.selected].title.clone());
        let hint = board.cards.is_empty().then(|| {
            div()
                .absolute()
                .top(px(h / 2.0 - 10.0))
                .w_full()
                .flex()
                .justify_center()
                .text_color(theme.muted)
                .child(
                    "Double-click to add a card \u{00b7} drag a card's dot to another to connect",
                )
        });
        let cursor = match self.whiteboard.drag {
            Some(Drag::Pan { .. }) => CursorStyle::ClosedHand,
            Some(Drag::Connect { .. }) => CursorStyle::Crosshair,
            Some(Drag::Resize { .. }) => CursorStyle::ResizeUpLeftDownRight,
            _ => CursorStyle::Arrow,
        };
        div()
            .id("whiteboard")
            .debug_selector(|| "whiteboard".to_string())
            .relative()
            .flex_1()
            .size_full()
            .overflow_hidden()
            .bg(theme.bg)
            .cursor(cursor)
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_board_mouse_down))
            .on_mouse_down(MouseButton::Middle, cx.listener(Self::on_board_mouse_down))
            .on_scroll_wheel(cx.listener(Self::on_board_scroll))
            .on_pinch(cx.listener(Self::on_board_pinch))
            .child(arrows)
            .children(labels)
            .children(cards)
            .children(hint)
            .child(title)
            .child(toolbar)
            .children(swatches)
            .into_any_element()
    }
}
