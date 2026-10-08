//! The graph view: a GPUI view that draws the page graph and handles input.
//!
//! The maths lives in `graph.rs`; this file is the glue:
//!
//! * `GraphView` holds the simulation, camera (`View`) and interaction state.
//! * Each frame, the canvas' *prepaint* step advances the physics, moves the
//!   camera and builds a `Frame`: a plain list of screen-space shapes to draw.
//! * The *paint* step registers mouse handlers and draws the `Frame`.
//!
//! Splitting "decide what to draw" (`build_frame`, no GPUI window needed) from
//! "draw it" (`paint_frame`) keeps the interesting logic unit-testable.

use crate::graph::{short_label, Graph, View};
use crate::model::Page;
use crate::ui::Theme;
use gpui::{
    canvas, div, fill, point, prelude::*, px, quad, size, App, BorderStyle, Bounds, Context,
    CursorStyle, DispatchPhase, EventEmitter, Hitbox, HitboxBehavior, Hsla, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, PathBuilder, Pixels, Point, Render, Rgba,
    ScrollWheelEvent, SharedString, TextAlign, TextRun, Window,
};

/// Physics steps per animation frame. Two makes the settling feel brisk while
/// staying far below a millisecond for a few hundred nodes.
const TICKS_PER_FRAME: u32 = 2;
/// Physics steps done up front when the view opens, so the first frame is
/// already a sensible layout instead of a heap of nodes in a spiral.
const WARM_UP_TICKS: u32 = 140;
/// Pixels of empty space kept around the graph when it is auto-fitted.
const FIT_MARGIN: f32 = 56.0;
/// How much of the remaining camera distance is covered each frame.
const CAMERA_EASE: f32 = 0.14;
/// A press that moves less than this many pixels is a click, not a drag.
const DRAG_THRESHOLD: f32 = 4.0;
/// Extra pixels around nodes that still count as hitting them.
const HIT_SLOP_PX: f32 = 4.0;
/// Zoom levels at which labels start to appear (see `build_frame`).
const LABEL_ZOOM_HUBS: f32 = 0.5;
const LABEL_ZOOM_ALL: f32 = 0.9;

/// Events the view sends to its owner.
pub enum GraphEvent {
    /// The user clicked a node: open the page with this title.
    OpenPage(String),
}

impl EventEmitter<GraphEvent> for GraphView {}

/// Which part of the graph is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// Every page.
    Global,
    /// The current page and its neighbours (see `Graph::build_local`).
    Local,
}

/// What the mouse is currently doing.
enum Drag {
    /// Pressed on a node. Becomes a real drag once the mouse moves past the
    /// threshold; released before that, it is a click.
    Node {
        index: usize,
        start: Point<Pixels>,
        moved: bool,
        was_pinned: bool,
        /// Node position minus mouse position (world units), so the node does
        /// not jump to centre itself under the cursor.
        grab: (f32, f32),
    },
    /// Pressed on the background: dragging pans the camera.
    Pan { last: Point<Pixels> },
}

pub struct GraphView {
    pages: Vec<Page>,
    graph: Graph,
    include_journals: bool,
    /// Global or local. Kept for as long as the view lives (the app keeps
    /// one for the whole session), not saved.
    scope: Scope,
    /// How many links away from the current page the local graph reaches
    /// (1 or 2).
    hops: usize,
    /// Title of the page open in the editor; its node is drawn in the accent
    /// colour, and the local graph is centred on it.
    current: String,
    theme: Theme,
    font_size: f32,
    /// Skip animation (system "reduce motion" setting).
    reduce_motion: bool,

    camera: View,
    /// While true the camera keeps fitting the whole graph in view. Any manual
    /// pan, zoom or drag turns it off; the Fit button turns it back on.
    auto_fit: bool,
    first_frame: bool,
    hovered: Option<usize>,
    drag: Option<Drag>,
    /// Size of the drawing area in pixels, from the last frame.
    pane: (f32, f32),
}

impl GraphView {
    pub fn new(
        pages: Vec<Page>,
        current: String,
        theme: Theme,
        font_size: f32,
        reduce_motion: bool,
    ) -> Self {
        let mut graph = Graph::build(&pages, true);
        graph.warm_up(WARM_UP_TICKS);
        if reduce_motion {
            graph.warm_up(crate::graph::MAX_TICKS);
        }
        GraphView {
            pages,
            graph,
            include_journals: true,
            scope: Scope::Global,
            hops: 1,
            current,
            theme,
            font_size,
            reduce_motion,
            camera: View::default(),
            auto_fit: true,
            first_frame: true,
            hovered: None,
            drag: None,
            pane: (0.0, 0.0),
        }
    }

    /// Re-read the pages (they may have been edited since the view was last
    /// open) while keeping node positions and pins.
    pub fn refresh(&mut self, pages: Vec<Page>, current: String, cx: &mut Context<Self>) {
        // A local graph around another page is a different picture: fit it.
        if self.scope == Scope::Local && !current.eq_ignore_ascii_case(&self.current) {
            self.auto_fit = true;
        }
        self.pages = pages;
        self.current = current;
        self.rebuild(cx);
    }

    /// Switch between the whole graph and the current page's neighbourhood.
    pub fn set_scope(&mut self, scope: Scope, cx: &mut Context<Self>) {
        if self.scope != scope {
            self.scope = scope;
            self.auto_fit = true;
            self.rebuild(cx);
        }
    }

    pub fn toggle_scope(&mut self, cx: &mut Context<Self>) {
        let scope = match self.scope {
            Scope::Global => Scope::Local,
            Scope::Local => Scope::Global,
        };
        self.set_scope(scope, cx);
    }

    /// The local graph's "2 hops" chip: reach 1 or 2 links out.
    fn toggle_hops(&mut self, cx: &mut Context<Self>) {
        self.hops = if self.hops == 1 { 2 } else { 1 };
        self.auto_fit = true;
        self.rebuild(cx);
    }

    pub fn set_style(&mut self, theme: Theme, font_size: f32, cx: &mut Context<Self>) {
        self.theme = theme;
        self.font_size = font_size;
        cx.notify();
    }

    /// Rebuild the graph for the current scope. Nodes that were already
    /// shown keep their place, so switching scope or page doesn't scramble
    /// the layout; the same physics then settles the new set.
    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let mut graph = match self.scope {
            Scope::Global => Graph::build(&self.pages, self.include_journals),
            Scope::Local => {
                Graph::build_local(&self.pages, self.include_journals, &self.current, self.hops)
            }
        };
        graph.preserve_layout(&self.graph);
        self.graph = graph;
        if self.reduce_motion {
            self.graph.warm_up(crate::graph::MAX_TICKS);
        }
        self.hovered = None;
        self.drag = None;
        cx.notify();
    }

    fn toggle_journals(&mut self, cx: &mut Context<Self>) {
        self.include_journals = !self.include_journals;
        self.auto_fit = true;
        self.rebuild(cx);
    }

    fn fit(&mut self, cx: &mut Context<Self>) {
        self.auto_fit = true;
        cx.notify();
    }

    // --- test hooks -------------------------------------------------------------

    /// Where node `title` is on screen, relative to the graph pane.
    #[cfg(test)]
    pub(crate) fn node_screen_pos(&self, title: &str) -> Option<(f32, f32)> {
        let n = &self.graph.nodes[self.graph.index_of(title)?];
        Some(self.camera.to_screen(n.x, n.y, self.pane.0, self.pane.1))
    }

    /// Titles of the nodes shown, in graph order.
    #[cfg(test)]
    pub(crate) fn node_titles(&self) -> Vec<String> {
        self.graph.nodes.iter().map(|n| n.title.clone()).collect()
    }

    #[cfg(test)]
    pub(crate) fn scope(&self) -> Scope {
        self.scope
    }

    #[cfg(test)]
    pub(crate) fn hovered_title(&self) -> Option<String> {
        self.hovered.map(|i| self.graph.nodes[i].title.clone())
    }

    #[cfg(test)]
    pub(crate) fn is_pinned(&self, title: &str) -> bool {
        self.graph
            .index_of(title)
            .is_some_and(|i| self.graph.nodes[i].pinned)
    }

    #[cfg(test)]
    pub(crate) fn zoom(&self) -> f32 {
        self.camera.zoom
    }

    #[cfg(test)]
    pub(crate) fn theme_bg(&self) -> Rgba {
        self.theme.bg
    }

    // --- per-frame update -----------------------------------------------------

    /// Advance physics and camera for one frame and build what to draw.
    /// Returns the frame and whether another frame is needed.
    fn prepare(&mut self, w: f32, h: f32) -> (Frame, bool) {
        self.pane = (w, h);
        if !self.reduce_motion {
            for _ in 0..TICKS_PER_FRAME {
                self.graph.tick();
            }
        }

        let mut camera_moving = false;
        if self.auto_fit && w > 1.0 && h > 1.0 {
            let mut target = View::default();
            target.fit(self.graph.bounds(), w, h, FIT_MARGIN);
            if self.first_frame || self.reduce_motion {
                self.camera = target;
            } else {
                camera_moving = !self.camera.approach(&target, CAMERA_EASE);
            }
            self.first_frame = false;
        }

        let animating = !self.graph.is_settled() || camera_moving || self.drag.is_some();
        (self.build_frame(w, h), animating)
    }

    /// The node that is the focus of hover highlighting: the one being dragged,
    /// else the one under the mouse.
    fn focus(&self) -> Option<usize> {
        match self.drag {
            Some(Drag::Node {
                index, moved: true, ..
            }) => Some(index),
            _ => self.hovered,
        }
    }

    /// Turn the current state into screen-space shapes.
    fn build_frame(&self, w: f32, h: f32) -> Frame {
        let t = &self.theme;
        let zoom = self.camera.zoom;
        let focus = self.focus();
        let related = |i: usize| match focus {
            None => true,
            Some(f) => i == f || self.graph.adj[f].contains(&i),
        };

        let accent: Hsla = t.accent.into();
        let mut frame = Frame {
            grid: Vec::new(),
            grid_color: Hsla::from(t.muted).opacity(0.22),
            dim_edges: Vec::new(),
            normal_edges: Vec::new(),
            lit_edges: Vec::new(),
            nodes: Vec::new(),
            labels: Vec::new(),
            label_size: (self.font_size * 0.8).max(9.0),
            colors: FrameColors {
                dim_edge: Hsla::from(t.muted).opacity(0.10),
                normal_edge: Hsla::from(t.muted).opacity(0.42),
                lit_edge: accent.opacity(0.95),
            },
            cursor: if matches!(self.drag, Some(Drag::Pan { .. }))
                || matches!(self.drag, Some(Drag::Node { moved: true, .. }))
            {
                CursorStyle::ClosedHand
            } else if self.hovered.is_some() {
                CursorStyle::PointingHand
            } else {
                CursorStyle::Arrow
            },
        };

        // Faint dot grid that pans and zooms with the graph. The spacing doubles
        // until dots are far enough apart, so zooming out never makes a haze.
        let mut spacing = 48.0f32;
        while spacing * zoom < 22.0 {
            spacing *= 2.0;
        }
        let (wx0, wy0) = self.camera.to_world(0.0, 0.0, w, h);
        let (wx1, wy1) = self.camera.to_world(w, h, w, h);
        let mut gx = (wx0 / spacing).floor() * spacing;
        'grid: while gx <= wx1 {
            let mut gy = (wy0 / spacing).floor() * spacing;
            while gy <= wy1 {
                if frame.grid.len() >= 3000 {
                    break 'grid;
                }
                frame.grid.push(self.camera.to_screen(gx, gy, w, h));
                gy += spacing;
            }
            gx += spacing;
        }

        // Edges, in three classes so each can be drawn as one path.
        let on_screen = |x: f32, y: f32| x > -60.0 && x < w + 60.0 && y > -60.0 && y < h + 60.0;
        for &(a, b) in &self.graph.edges {
            let (na, nb) = (&self.graph.nodes[a], &self.graph.nodes[b]);
            let (x1, y1) = self.camera.to_screen(na.x, na.y, w, h);
            let (x2, y2) = self.camera.to_screen(nb.x, nb.y, w, h);
            if !on_screen(x1, y1) && !on_screen(x2, y2) {
                continue;
            }
            let seg = (x1, y1, x2, y2);
            match focus {
                None => frame.normal_edges.push(seg),
                Some(f) if a == f || b == f => frame.lit_edges.push(seg),
                Some(_) => frame.dim_edges.push(seg),
            }
        }

        // Nodes. Draw order is back to front: dimmed, normal, related, focus.
        let mut order: Vec<usize> = (0..self.graph.nodes.len()).collect();
        order.sort_by_key(|&i| (Some(i) == focus, related(i)));
        let bg: Hsla = t.bg.into();
        for i in order {
            let node = &self.graph.nodes[i];
            let (sx, sy) = self.camera.to_screen(node.x, node.y, w, h);
            let r = (node.radius() * zoom).max(2.5);
            if !on_screen(sx, sy) {
                continue;
            }
            let is_current = node.title.eq_ignore_ascii_case(&self.current);
            let lit = related(i);

            // Colour: accent for the open page; otherwise a grey-blue that
            // brightens for well-linked hubs.
            let hub = (node.backlinks as f32 / 10.0).min(1.0) * 0.75;
            let base: Hsla = if is_current {
                accent
            } else {
                lerp(t.muted, t.text, hub).into()
            };
            let color = if lit { base } else { base.opacity(0.22) };

            let (fill_color, border) = if node.is_journal && !is_current {
                // Journals are hollow rings, so they read as a different kind of
                // node without needing another colour.
                (bg, Some((1.6, color)))
            } else {
                (color, None)
            };
            let border = if Some(i) == focus {
                Some((2.0, Hsla::from(t.text)))
            } else {
                border
            };
            let glow = if is_current {
                Some(accent.opacity(if lit { 0.22 } else { 0.08 }))
            } else if Some(i) == focus {
                Some(accent.opacity(0.18))
            } else {
                None
            };
            frame.nodes.push(NodeDraw {
                x: sx,
                y: sy,
                r,
                fill: fill_color,
                border,
                glow,
            });

            // Labels: always for the open page and the focus group; otherwise
            // only once zoomed in far enough (hubs first, then everything).
            // (`lit` is always true when nothing is hovered; while hovering,
            // dimmed nodes lose their labels.)
            let show = (focus.is_some() && lit)
                || is_current
                || zoom >= LABEL_ZOOM_ALL
                || (zoom >= LABEL_ZOOM_HUBS && node.backlinks >= 1);
            if show && lit {
                let label_color: Hsla = if Some(i) == focus || is_current {
                    t.text.into()
                } else {
                    Hsla::from(t.text).opacity(if focus.is_some() { 0.9 } else { 0.72 })
                };
                frame.labels.push(LabelDraw {
                    text: short_label(&node.title).into(),
                    x: sx,
                    y: sy + r + 3.0,
                    color: label_color,
                });
            }
        }
        frame
    }

    // --- input ------------------------------------------------------------------

    fn world(&self, x: f32, y: f32) -> (f32, f32) {
        self.camera.to_world(x, y, self.pane.0, self.pane.1)
    }

    fn hit(&self, x: f32, y: f32, slop_px: f32) -> Option<usize> {
        let (wx, wy) = self.world(x, y);
        self.graph.hit_test(wx, wy, slop_px / self.camera.zoom)
    }

    /// Mouse pressed at pane position `(x, y)` (screen position `pos`).
    fn mouse_down(&mut self, x: f32, y: f32, pos: Point<Pixels>, cx: &mut Context<Self>) {
        self.auto_fit = false;
        self.drag = Some(match self.hit(x, y, HIT_SLOP_PX) {
            Some(index) => {
                let (wx, wy) = self.world(x, y);
                let node = &self.graph.nodes[index];
                Drag::Node {
                    index,
                    start: pos,
                    moved: false,
                    was_pinned: node.pinned,
                    grab: (node.x - wx, node.y - wy),
                }
            }
            None => Drag::Pan { last: pos },
        });
        cx.notify();
    }

    /// Mouse moved. `pressed` is whether the left button is held; `inside` is
    /// whether the pointer is over the graph pane.
    fn mouse_move(
        &mut self,
        x: f32,
        y: f32,
        pos: Point<Pixels>,
        pressed: bool,
        inside: bool,
        cx: &mut Context<Self>,
    ) {
        if self.drag.is_some() && !pressed {
            // The button was released somewhere we did not see (outside the window).
            self.finish_drag(false, cx);
        }
        let (wx, wy) = self.world(x, y);
        match &mut self.drag {
            Some(Drag::Node {
                index,
                start,
                moved,
                grab,
                ..
            }) => {
                let dist = f32::from(pos.x - start.x).hypot(f32::from(pos.y - start.y));
                if !*moved && dist > DRAG_THRESHOLD {
                    *moved = true;
                    // Pin right away: otherwise the physics keeps pulling the
                    // node toward its neighbours and it fights the cursor.
                    self.graph.nodes[*index].pinned = true;
                    self.graph.reheat(0.35);
                }
                if *moved {
                    let node = &mut self.graph.nodes[*index];
                    node.x = wx + grab.0;
                    node.y = wy + grab.1;
                    // Keep neighbours reacting for as long as the drag lasts.
                    self.graph.reheat(0.25);
                }
                cx.notify();
            }
            Some(Drag::Pan { last }) => {
                self.camera.pan_x += f32::from(pos.x - last.x);
                self.camera.pan_y += f32::from(pos.y - last.y);
                *last = pos;
                cx.notify();
            }
            None => {
                let new = if inside { self.hit(x, y, 2.0) } else { None };
                if new != self.hovered {
                    self.hovered = new;
                    cx.notify();
                }
            }
        }
    }

    fn mouse_up(&mut self, cx: &mut Context<Self>) {
        self.finish_drag(true, cx);
    }

    /// End a drag. `released` is true for a real mouse-up (so a still press on a
    /// node counts as a click) and false when the release was missed.
    fn finish_drag(&mut self, released: bool, cx: &mut Context<Self>) {
        match self.drag.take() {
            Some(Drag::Node {
                index,
                moved,
                was_pinned,
                ..
            }) => {
                if moved {
                    // Dropped nodes stay exactly where they were let go.
                    self.graph.nodes[index].pinned = true;
                } else {
                    self.graph.nodes[index].pinned = was_pinned;
                    if released {
                        cx.emit(GraphEvent::OpenPage(self.graph.nodes[index].title.clone()));
                    }
                }
            }
            Some(Drag::Pan { .. }) | None => {}
        }
        cx.notify();
    }

    /// Scroll wheel: zoom around the cursor. Positive `dy` (scrolling up) zooms in.
    fn scroll(&mut self, x: f32, y: f32, dy: f32, cx: &mut Context<Self>) {
        let factor = (dy * 0.0018).exp().clamp(0.6, 1.7);
        let (w, h) = self.pane;
        self.camera.zoom_at(x, y, factor, w, h);
        self.auto_fit = false;
        cx.notify();
    }
}

fn lerp(a: Rgba, b: Rgba, t: f32) -> Rgba {
    Rgba {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: 1.0,
    }
}

// --- frame data ---------------------------------------------------------------

struct NodeDraw {
    x: f32,
    y: f32,
    r: f32,
    fill: Hsla,
    /// `(width, colour)` of an outline, if any.
    border: Option<(f32, Hsla)>,
    /// A larger, translucent circle drawn behind the node.
    glow: Option<Hsla>,
}

struct LabelDraw {
    text: SharedString,
    /// Horizontal centre and top of the label, in pane pixels.
    x: f32,
    y: f32,
    color: Hsla,
}

struct FrameColors {
    dim_edge: Hsla,
    normal_edge: Hsla,
    lit_edge: Hsla,
}

/// Everything needed to draw one frame, in pane-local pixels.
struct Frame {
    grid: Vec<(f32, f32)>,
    grid_color: Hsla,
    /// Edge segments as `(x1, y1, x2, y2)`.
    dim_edges: Vec<(f32, f32, f32, f32)>,
    normal_edges: Vec<(f32, f32, f32, f32)>,
    lit_edges: Vec<(f32, f32, f32, f32)>,
    nodes: Vec<NodeDraw>,
    labels: Vec<LabelDraw>,
    label_size: f32,
    colors: FrameColors,
    cursor: CursorStyle,
}

// --- painting ------------------------------------------------------------------

fn paint_frame(frame: &Frame, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
    let origin = bounds.origin;
    let at = |x: f32, y: f32| point(origin.x + px(x), origin.y + px(y));

    for &(x, y) in &frame.grid {
        window.paint_quad(fill(
            Bounds::new(at(x - 0.9, y - 0.9), size(px(1.8), px(1.8))),
            frame.grid_color,
        ));
    }

    // One stroked path per edge class: far cheaper than a path per edge.
    for (edges, width, color) in [
        (&frame.dim_edges, 1.0, frame.colors.dim_edge),
        (&frame.normal_edges, 1.1, frame.colors.normal_edge),
        (&frame.lit_edges, 1.8, frame.colors.lit_edge),
    ] {
        if edges.is_empty() {
            continue;
        }
        let mut builder = PathBuilder::stroke(px(width));
        for &(x1, y1, x2, y2) in edges {
            builder.move_to(at(x1, y1));
            builder.line_to(at(x2, y2));
        }
        if let Ok(path) = builder.build() {
            window.paint_path(path, color);
        }
    }

    // A circle is a square quad whose corner radius is half its width.
    let circle = |x: f32, y: f32, r: f32, color: Hsla, border: Option<(f32, Hsla)>| {
        let (bw, bc) = border.unwrap_or((0.0, color));
        quad(
            Bounds::new(at(x - r, y - r), size(px(2.0 * r), px(2.0 * r))),
            px(r),
            color,
            px(bw),
            bc,
            BorderStyle::Solid,
        )
    };
    for n in &frame.nodes {
        if let Some(glow) = n.glow {
            window.paint_quad(circle(n.x, n.y, n.r + 7.0, glow, None));
        }
        window.paint_quad(circle(n.x, n.y, n.r, n.fill, n.border));
    }

    let style = window.text_style();
    let font_size = px(frame.label_size);
    for label in &frame.labels {
        let run = TextRun {
            len: label.text.len(),
            font: style.font(),
            color: label.color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let line = window
            .text_system()
            .shape_line(label.text.clone(), font_size, &[run], None);
        let x = label.x - f32::from(line.width()) / 2.0;
        let _ = line.paint(
            at(x, label.y),
            px(frame.label_size * 1.3),
            TextAlign::Left,
            None,
            window,
            cx,
        );
    }
}

/// Hook up this frame's mouse handlers. GPUI handlers last one frame, so they
/// are registered again on every paint.
fn register_mouse(
    view: &gpui::Entity<GraphView>,
    bounds: Bounds<Pixels>,
    hitbox: &Hitbox,
    window: &mut Window,
) {
    let local = move |p: Point<Pixels>| {
        (
            f32::from(p.x - bounds.origin.x),
            f32::from(p.y - bounds.origin.y),
        )
    };

    window.on_mouse_event({
        let (view, hitbox) = (view.clone(), hitbox.clone());
        move |e: &MouseDownEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble
                && e.button == MouseButton::Left
                && hitbox.is_hovered(window)
            {
                let (x, y) = local(e.position);
                view.update(cx, |v, cx| v.mouse_down(x, y, e.position, cx));
            }
        }
    });
    window.on_mouse_event({
        let (view, hitbox) = (view.clone(), hitbox.clone());
        move |e: &MouseMoveEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble {
                let (x, y) = local(e.position);
                let pressed = e.pressed_button == Some(MouseButton::Left);
                let inside = hitbox.is_hovered(window);
                view.update(cx, |v, cx| {
                    v.mouse_move(x, y, e.position, pressed, inside, cx)
                });
            }
        }
    });
    window.on_mouse_event({
        let view = view.clone();
        move |e: &MouseUpEvent, phase, _window, cx| {
            if phase == DispatchPhase::Bubble && e.button == MouseButton::Left {
                view.update(cx, |v, cx| v.mouse_up(cx));
            }
        }
    });
    window.on_mouse_event({
        let (view, hitbox) = (view.clone(), hitbox.clone());
        move |e: &ScrollWheelEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble && hitbox.is_hovered(window) {
                let (x, y) = local(e.position);
                let dy = f32::from(e.delta.pixel_delta(px(20.0)).y);
                view.update(cx, |v, cx| v.scroll(x, y, dy, cx));
            }
        }
    });
}

// --- the view ------------------------------------------------------------------

impl Render for GraphView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let view = cx.entity();

        let chip = |id: &'static str, label: &'static str, active: bool| {
            div()
                .id(id)
                .debug_selector(move || id.to_string())
                .px_3()
                .py_1()
                .rounded_md()
                .border_1()
                .border_color(if active { theme.accent } else { theme.border })
                .text_color(if active { theme.accent } else { theme.muted })
                .when(active, |d| d.bg(theme.selected_bg))
                .cursor_pointer()
                .hover(|d| d.bg(theme.selected_bg))
                .child(label)
        };

        // Global | Local: two halves of one bordered control.
        let local = self.scope == Scope::Local;
        let segment = |id: &'static str, label: &'static str, active: bool| {
            div()
                .id(id)
                .debug_selector(move || id.to_string())
                .px_3()
                .py_1()
                .border_1()
                .border_color(if active { theme.accent } else { theme.border })
                .text_color(if active { theme.accent } else { theme.muted })
                .when(active, |d| d.bg(theme.selected_bg))
                .cursor_pointer()
                .hover(|d| d.bg(theme.selected_bg))
                .child(label)
        };
        let scope_control = div()
            .flex()
            .flex_row()
            .child(
                segment("graph-mode-global", "Global", !local)
                    .rounded_l_md()
                    .on_click(cx.listener(|this, _e, _w, cx| this.set_scope(Scope::Global, cx))),
            )
            .child(
                segment("graph-mode-local", "Local", local)
                    .rounded_r_md()
                    .on_click(cx.listener(|this, _e, _w, cx| this.set_scope(Scope::Local, cx))),
            );

        let link_count = self.graph.edges.len();
        let toolbar = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .px_4()
            .py_2()
            .border_b_1()
            .border_color(theme.border)
            .child(div().text_color(theme.text).child("Graph"))
            .child(div().text_color(theme.muted).child(format!(
                "{} pages \u{b7} {} links",
                self.graph.nodes.len(),
                link_count
            )))
            .child(div().flex_1())
            .child(
                div()
                    .text_color(theme.muted)
                    .child("scroll to zoom \u{b7} drag to pan \u{b7} click a node to open"),
            )
            .child(scope_control)
            .when(local, |d| {
                d.child(
                    chip("graph-local-depth", "2 hops", self.hops == 2)
                        .on_click(cx.listener(|this, _e, _w, cx| this.toggle_hops(cx))),
                )
            })
            .child(
                chip("graph-journals", "Journals", self.include_journals)
                    .on_click(cx.listener(|this, _e, _w, cx| this.toggle_journals(cx))),
            )
            .child(
                chip("graph-fit", "Fit", self.auto_fit)
                    .on_click(cx.listener(|this, _e, _w, cx| this.fit(cx))),
            );

        let canvas = canvas(
            {
                let view = view.clone();
                move |bounds, window, cx| {
                    let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
                    let (frame, animating) = view.update(cx, |v, _| {
                        v.prepare(f32::from(bounds.size.width), f32::from(bounds.size.height))
                    });
                    if animating {
                        window.request_animation_frame();
                    }
                    (hitbox, frame)
                }
            },
            move |bounds, (hitbox, frame), window, cx| {
                register_mouse(&view, bounds, &hitbox, window);
                window.set_cursor_style(frame.cursor, &hitbox);
                paint_frame(&frame, bounds, window, cx);
            },
        )
        // Fill the container whatever its sizing mode (a percentage height
        // would collapse inside a flex item).
        .absolute()
        .inset_0();

        div()
            .id("graph-view")
            .flex_1()
            .h_full()
            .min_w_0()
            .flex()
            .flex_col()
            .bg(theme.bg)
            .child(toolbar)
            .child(
                div()
                    .debug_selector(|| "graph-canvas".to_string())
                    .flex_1()
                    .relative()
                    .overflow_hidden()
                    .child(canvas),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn pages() -> Vec<Page> {
        vec![
            Page::from_markdown("Hub", false, "- [[A]] [[B]] [[C]]\n"),
            Page::from_markdown("A", false, "- back [[Hub]]\n"),
            Page::from_markdown("B", false, "- back [[Hub]]\n"),
            Page::from_markdown("C", false, "- nothing\n"),
            Page::from_markdown("Island", false, "- alone\n"),
            Page::from_markdown("2026-01-01", true, "- [[Hub]]\n"),
        ]
    }

    fn new_view(current: &str) -> GraphView {
        let mut v = GraphView::new(pages(), current.into(), Theme::dark(), 16.0, true);
        let _ = v.prepare(800.0, 600.0); // fits the camera
        v
    }

    fn screen_pos(v: &GraphView, title: &str) -> (f32, f32) {
        let n = &v.graph.nodes[v.graph.index_of(title).unwrap()];
        v.camera.to_screen(n.x, n.y, 800.0, 600.0)
    }

    #[test]
    fn open_page_is_accent_coloured_and_labelled() {
        let v = new_view("Hub");
        let frame = v.build_frame(800.0, 600.0);
        let accent: Hsla = v.theme.accent.into();
        assert_eq!(frame.nodes.len(), 6);
        assert_eq!(frame.nodes.iter().filter(|n| n.fill == accent).count(), 1);
        assert!(
            frame.nodes.iter().any(|n| n.glow.is_some()),
            "open page glows"
        );
        assert!(frame.labels.iter().any(|l| l.text.as_ref() == "Hub"));
    }

    #[test]
    fn journals_are_hollow_rings() {
        let v = new_view("Hub");
        let frame = v.build_frame(800.0, 600.0);
        let bg: Hsla = v.theme.bg.into();
        let rings = frame
            .nodes
            .iter()
            .filter(|n| n.fill == bg && n.border.is_some())
            .count();
        assert_eq!(rings, 1);
    }

    #[test]
    fn labels_hide_when_zoomed_out_but_the_open_page_keeps_its_own() {
        let mut v = new_view("Island");
        v.camera.zoom = 0.3;
        let frame = v.build_frame(800.0, 600.0);
        assert_eq!(frame.labels.len(), 1, "only the open page is labelled");
        assert_eq!(frame.labels[0].text.as_ref(), "Island");

        // Mid zoom: hubs (pages with backlinks) get labels too.
        v.camera.zoom = 0.6;
        let mid: Vec<String> = v
            .build_frame(800.0, 600.0)
            .labels
            .iter()
            .map(|l| l.text.to_string())
            .collect();
        assert!(mid.contains(&"Hub".to_string()) && mid.contains(&"Island".to_string()));
        assert!(
            !mid.contains(&"C".to_string())
                || v.graph.nodes[v.graph.index_of("C").unwrap()].backlinks >= 1
        );

        // Zoomed in: everything.
        v.camera.zoom = 1.2;
        assert_eq!(v.build_frame(800.0, 600.0).labels.len(), 6);
    }

    #[test]
    fn hovering_dims_everything_but_the_node_and_its_neighbours() {
        let mut v = new_view("Island");
        let hub = v.graph.index_of("Hub").unwrap();
        v.hovered = Some(hub);
        let frame = v.build_frame(800.0, 600.0);
        // Hub has 4 neighbours (A, B, C, the journal): its 4 edges light up and
        // nothing else is left to dim here.
        assert_eq!(frame.lit_edges.len(), 4);
        assert!(frame.normal_edges.is_empty());
        // Island is not a neighbour: it is drawn faded.
        let faded = frame.nodes.iter().filter(|n| n.fill.a < 0.5).count();
        assert!(faded >= 1, "unrelated nodes are dimmed");
        assert_eq!(frame.cursor as u8, CursorStyle::PointingHand as u8);
    }

    #[test]
    fn hit_testing_uses_the_camera() {
        let v = new_view("Hub");
        let (x, y) = screen_pos(&v, "A");
        assert_eq!(v.hit(x, y, 0.0), v.graph.index_of("A"));
        assert_eq!(v.hit(x + 400.0, y + 400.0, 0.0), None);
    }

    #[gpui::test]
    fn click_without_moving_opens_the_page_and_unpins(cx: &mut TestAppContext) {
        let view = cx.new(|_| new_view("Hub"));
        let opened = Rc::new(RefCell::new(Vec::new()));
        let sink = opened.clone();
        cx.update(|cx| {
            cx.subscribe(&view, move |_, e: &GraphEvent, _| {
                let GraphEvent::OpenPage(title) = e;
                sink.borrow_mut().push(title.clone());
            })
            .detach();
        });

        let (x, y) = view.read_with(cx, |v, _| screen_pos(v, "A"));
        let pos = point(px(x), px(y));
        view.update(cx, |v, cx| {
            v.mouse_down(x, y, pos, cx);
            v.mouse_up(cx);
        });
        assert_eq!(*opened.borrow(), vec!["A".to_string()]);
        view.read_with(cx, |v, _| {
            let a = &v.graph.nodes[v.graph.index_of("A").unwrap()];
            assert!(!a.pinned, "a click must not pin the node");
        });
    }

    #[gpui::test]
    fn dragging_moves_and_pins_a_node_without_navigating(cx: &mut TestAppContext) {
        let view = cx.new(|_| new_view("Hub"));
        let opened = Rc::new(RefCell::new(0));
        let sink = opened.clone();
        cx.update(|cx| {
            cx.subscribe(&view, move |_, _: &GraphEvent, _| *sink.borrow_mut() += 1)
                .detach();
        });

        let (x, y) = view.read_with(cx, |v, _| screen_pos(v, "C"));
        let (tx, ty) = (x + 120.0, y - 60.0);
        view.update(cx, |v, cx| {
            v.mouse_down(x, y, point(px(x), px(y)), cx);
            v.mouse_move(
                x + 40.0,
                y - 20.0,
                point(px(x + 40.0), px(y - 20.0)),
                true,
                true,
                cx,
            );
            v.mouse_move(tx, ty, point(px(tx), px(ty)), true, true, cx);
            v.mouse_up(cx);
        });
        assert_eq!(*opened.borrow(), 0, "a drag is not a click");
        view.read_with(cx, |v, _| {
            let c = &v.graph.nodes[v.graph.index_of("C").unwrap()];
            assert!(c.pinned, "dropped nodes stay put");
            let (sx, sy) = v.camera.to_screen(c.x, c.y, 800.0, 600.0);
            assert!(
                (sx - tx).abs() < 0.5 && (sy - ty).abs() < 0.5,
                "node follows the cursor: ({sx},{sy})"
            );
        });

        // Further simulation must not move the pinned node.
        view.update(cx, |v, _| {
            let before = {
                let c = &v.graph.nodes[v.graph.index_of("C").unwrap()];
                (c.x, c.y)
            };
            v.graph.reheat(1.0);
            v.graph.warm_up(50);
            let c = &v.graph.nodes[v.graph.index_of("C").unwrap()];
            assert_eq!((c.x, c.y), before);
        });
    }

    #[gpui::test]
    fn background_drag_pans_and_scroll_zooms_at_the_cursor(cx: &mut TestAppContext) {
        let view = cx.new(|_| new_view("Hub"));
        view.update(cx, |v, cx| {
            let before = v.camera;
            // Far corner: certainly empty background.
            v.mouse_down(5.0, 5.0, point(px(5.0), px(5.0)), cx);
            v.mouse_move(35.0, 25.0, point(px(35.0), px(25.0)), true, true, cx);
            v.mouse_up(cx);
            assert_eq!(v.camera.pan_x, before.pan_x + 30.0);
            assert_eq!(v.camera.pan_y, before.pan_y + 20.0);
            assert!(!v.auto_fit, "manual panning stops auto-fit");

            // Zooming keeps the world point under the cursor fixed.
            let anchor = (300.0, 200.0);
            let w0 = v.world(anchor.0, anchor.1);
            v.scroll(anchor.0, anchor.1, 120.0, cx);
            assert!(v.camera.zoom > before.zoom, "scrolling up zooms in");
            let w1 = v.world(anchor.0, anchor.1);
            assert!((w0.0 - w1.0).abs() < 0.01 && (w0.1 - w1.1).abs() < 0.01);
            v.scroll(anchor.0, anchor.1, -240.0, cx);
            assert!(v.camera.zoom < before.zoom * 1.5);
        });
    }

    #[gpui::test]
    fn journal_toggle_rebuilds_but_keeps_positions(cx: &mut TestAppContext) {
        // Normal (animated) mode: a rebuild starts from the old positions and
        // animates from there. (With reduce-motion on, the layout re-settles
        // instantly instead, so positions legitimately shift a little.)
        let view = cx.new(|_| GraphView::new(pages(), "Hub".into(), Theme::dark(), 16.0, false));
        view.update(cx, |v, cx| {
            let hub_before = {
                let n = &v.graph.nodes[v.graph.index_of("Hub").unwrap()];
                (n.x, n.y)
            };
            v.toggle_journals(cx);
            assert_eq!(v.graph.nodes.len(), 5, "journal hidden");
            let n = &v.graph.nodes[v.graph.index_of("Hub").unwrap()];
            assert_eq!((n.x, n.y), hub_before);
            v.toggle_journals(cx);
            assert_eq!(v.graph.nodes.len(), 6);
            assert!(v.auto_fit, "toggling refits the camera");
        });
    }

    fn sorted(mut titles: Vec<String>) -> Vec<String> {
        titles.sort();
        titles
    }

    #[gpui::test]
    fn local_scope_shows_the_neighbourhood_and_follows_the_current_page(cx: &mut TestAppContext) {
        let view = cx.new(|_| new_view("A"));
        view.update(cx, |v, cx| {
            v.auto_fit = false;
            v.set_scope(Scope::Local, cx);
            assert!(v.auto_fit, "switching scope refits");
            assert_eq!(sorted(v.node_titles()), vec!["A", "Hub"]);
            assert_eq!(v.graph.edges.len(), 1);

            v.toggle_hops(cx);
            assert_eq!(
                sorted(v.node_titles()),
                vec!["2026-01-01", "A", "B", "C", "Hub"]
            );
            v.toggle_hops(cx);

            // Another current page: re-centred and refitted.
            v.auto_fit = false;
            let pages = v.pages.clone();
            v.refresh(pages, "Island".into(), cx);
            assert_eq!(v.node_titles(), vec!["Island"]);
            assert!(v.auto_fit);

            v.toggle_scope(cx);
            assert_eq!(v.scope, Scope::Global);
            assert_eq!(v.node_titles().len(), 6);
        });
    }

    #[gpui::test]
    fn missed_mouse_up_ends_the_drag(cx: &mut TestAppContext) {
        let view = cx.new(|_| new_view("Hub"));
        let (x, y) = view.read_with(cx, |v, _| screen_pos(v, "A"));
        view.update(cx, |v, cx| {
            v.mouse_down(x, y, point(px(x), px(y)), cx);
            // Next event says the button is no longer held.
            v.mouse_move(x + 5.0, y, point(px(x + 5.0), px(y)), false, true, cx);
            assert!(v.drag.is_none());
        });
    }

    #[test]
    fn camera_settles_and_animation_stops() {
        let mut v = GraphView::new(pages(), "Hub".into(), Theme::dark(), 16.0, false);
        let mut frames = 0;
        loop {
            let (_, animating) = v.prepare(800.0, 600.0);
            frames += 1;
            if !animating {
                break;
            }
            assert!(frames < 2000, "animation never stopped");
        }
        assert!(v.graph.is_settled());
    }

    /// A believable sample graph: a few topic hubs, leaf pages, journals that
    /// mention things, cross-links and some orphans.
    fn sample_pages() -> Vec<Page> {
        let hubs: [(&str, &[&str]); 6] = [
            (
                "Projects",
                &[
                    "notesec",
                    "Website",
                    "Garden",
                    "Home lab",
                    "Book club",
                    "Budget",
                    "Trip planning",
                ],
            ),
            (
                "Ideas",
                &[
                    "Spatial notes",
                    "Daily review",
                    "Tag taxonomy",
                    "Offline first",
                    "Graph layouts",
                    "Voice capture",
                ],
            ),
            (
                "Reading",
                &[
                    "Designing Data-Intensive Applications",
                    "The Pragmatic Programmer",
                    "Gardens of the Moon",
                    "Staff Engineer",
                    "A Philosophy of Software Design",
                    "Piranesi",
                ],
            ),
            ("People", &["Ada", "Linus", "Grace", "Margaret", "Dennis"]),
            (
                "Rust",
                &[
                    "Ownership",
                    "Lifetimes",
                    "Traits",
                    "GPUI",
                    "Async",
                    "Cargo",
                    "Macros",
                ],
            ),
            ("Meetings", &["Standup", "Retro", "Planning", "One on one"]),
        ];
        let mut pages = Vec::new();
        for (hub, leaves) in hubs {
            let body: String = leaves.iter().map(|l| format!("- [[{l}]]\n")).collect();
            pages.push(Page::from_markdown(hub, false, &body));
            for leaf in leaves {
                pages.push(Page::from_markdown(
                    leaf,
                    false,
                    &format!("- part of [[{hub}]]\n"),
                ));
            }
        }
        // Cross-links between topics.
        pages.push(Page::from_markdown(
            "Dashboard",
            false,
            "- [[Projects]] [[Ideas]] [[Reading]] [[Rust]] [[People]] [[Meetings]]\n",
        ));
        for (a, b) in [
            ("notesec", "GPUI"),
            ("notesec", "Graph layouts"),
            ("notesec", "Ownership"),
            ("Garden", "Budget"),
            ("Ada", "Standup"),
            ("Linus", "Rust"),
            ("Staff Engineer", "One on one"),
            ("Spatial notes", "Graph layouts"),
        ] {
            let page = pages.iter_mut().find(|p| p.title == a).unwrap();
            page.blocks.push(crate::model::Block {
                id: uuid::Uuid::new_v4(),
                content: format!("see also [[{b}]]"),
                parent_id: None,
                page_id: a.into(),
                order: 99,
            });
        }
        for orphan in ["Scratch", "Someday", "Recipes", "Quotes"] {
            pages.push(Page::from_markdown(orphan, false, "- nothing links here\n"));
        }
        for (i, links) in [
            "[[notesec]] [[GPUI]]",
            "[[Garden]] [[Budget]]",
            "[[Standup]] [[Ada]]",
            "[[Piranesi]]",
            "[[notesec]] [[Graph layouts]]",
            "[[Retro]] [[Planning]]",
        ]
        .iter()
        .enumerate()
        {
            pages.push(Page::from_markdown(
                &format!("2026-10-0{}", i + 1),
                true,
                &format!("- {links}\n"),
            ));
        }
        pages
    }

    /// Render the real view with the real GPU renderer to PNG files in
    /// `target/shots/` so the look can be checked by eye. Not a regression test:
    /// `cargo test --offline render_screenshots -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn render_screenshots() {
        use gpui::{AppContext as _, HeadlessAppContext};
        use std::sync::Arc;

        std::fs::create_dir_all("target/shots").unwrap();
        let text_system = Arc::new(gpui_wgpu::CosmicTextSystem::new("DejaVu Sans"));
        let mut cx = HeadlessAppContext::with_platform(text_system, Arc::new(()), || {
            gpui_platform::current_headless_renderer()
        });

        for (name, theme, hover, zoom) in [
            ("dark", Theme::dark(), None, None),
            ("light", Theme::light(), None, None),
            ("hover", Theme::dark(), Some("Projects"), None),
            ("zoomed", Theme::dark(), None, Some(1.3f32)),
        ] {
            let current = "notesec".to_string();
            let window = cx
                .open_window(size(px(1100.), px(720.)), |_, cx| {
                    cx.new(|_| GraphView::new(sample_pages(), current, theme, 16.0, true))
                })
                .unwrap();
            let view = window.root(&mut cx).unwrap();
            let _ = cx
                .update_window(window.into(), |_, window, cx| window.draw(cx))
                .unwrap();
            cx.update(|cx| {
                view.update(cx, |v, cx| {
                    if let Some(title) = hover {
                        v.hovered = v.graph.index_of(title);
                    }
                    if let Some(z) = zoom {
                        v.auto_fit = false;
                        v.camera.zoom = z;
                        v.camera.pan_x = 0.0;
                        v.camera.pan_y = 0.0;
                    }
                    cx.notify();
                })
            });
            let _ = cx
                .update_window(window.into(), |_, window, cx| window.draw(cx))
                .unwrap();
            match cx.capture_screenshot(window.into()) {
                Ok(image) => {
                    image
                        .save(format!("target/shots/graph-{name}.png"))
                        .unwrap();
                    println!(
                        "wrote target/shots/graph-{name}.png ({}x{})",
                        image.width(),
                        image.height()
                    );
                }
                Err(err) => panic!("screenshot failed: {err:#}"),
            }
        }
    }

    #[test]
    fn stroke_paths_accept_many_separate_segments() {
        // The renderer draws all edges of a class as ONE path of disjoint
        // segments; make sure the path builder really supports that.
        let mut b = PathBuilder::stroke(px(1.0));
        for i in 0..50 {
            let y = i as f32 * 10.0;
            b.move_to(point(px(0.0), px(y)));
            b.line_to(point(px(100.0), px(y + 5.0)));
        }
        let path = b.build().expect("multi-segment stroke builds");
        assert!(
            path.bounds.size.height > px(400.0),
            "all segments are present"
        );
    }
}
