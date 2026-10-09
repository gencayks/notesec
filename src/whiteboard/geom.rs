//! The canvas maths, in plain `f32`s (no GPUI): canvas ("world") units are
//! pixels at 100%; the view maps them to the screen with a zoom and an
//! offset. Hit testing, zooming around the pointer, fitting, culling and
//! arrow anchoring are here so they can be tested without a window.

use uuid::Uuid;

use super::Board;

pub const MIN_ZOOM: f32 = 0.1;
pub const MAX_ZOOM: f32 = 4.0;
/// Screen sizes of the selected card's handles.
pub const RESIZE_HANDLE: f32 = 12.0;
pub const CONNECT_HANDLE: f32 = 12.0;
/// How close (screen pixels) a click must be to an arrow.
pub const EDGE_SLOP: f32 = 6.0;
/// The title strip of a page card (canvas units).
pub const TITLE_H: f32 = 28.0;
/// Arrowheads (screen pixels).
pub const ARROW_LEN: f32 = 12.0;
pub const ARROW_HALF_WIDTH: f32 = 5.0;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Pt {
    pub x: f32,
    pub y: f32,
}

pub fn pt(x: f32, y: f32) -> Pt {
    Pt { x, y }
}

impl Pt {
    fn sub(self, o: Pt) -> Pt {
        pt(self.x - o.x, self.y - o.y)
    }
    fn add(self, o: Pt) -> Pt {
        pt(self.x + o.x, self.y + o.y)
    }
    fn scale(self, k: f32) -> Pt {
        pt(self.x * k, self.y * k)
    }
    fn len(self) -> f32 {
        (self.x * self.x + self.y * self.y).sqrt()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect { x, y, w, h }
    }

    pub fn center(&self) -> Pt {
        pt(self.x + self.w / 2.0, self.y + self.h / 2.0)
    }

    pub fn contains(&self, p: Pt) -> bool {
        p.x >= self.x && p.x <= self.x + self.w && p.y >= self.y && p.y <= self.y + self.h
    }

    pub fn intersects(&self, o: &Rect) -> bool {
        self.x < o.x + o.w && o.x < self.x + self.w && self.y < o.y + o.h && o.y < self.y + self.h
    }

    /// The smallest rectangle around all of `rects`.
    pub fn union(rects: &[Rect]) -> Option<Rect> {
        let first = rects.first()?;
        let (mut x0, mut y0) = (first.x, first.y);
        let (mut x1, mut y1) = (first.x + first.w, first.y + first.h);
        for r in rects {
            x0 = x0.min(r.x);
            y0 = y0.min(r.y);
            x1 = x1.max(r.x + r.w);
            y1 = y1.max(r.y + r.h);
        }
        Some(Rect::new(x0, y0, x1 - x0, y1 - y0))
    }

    /// Where the ray from the centre towards `toward` leaves the border.
    pub fn border_point(&self, toward: Pt) -> Pt {
        let c = self.center();
        let d = toward.sub(c);
        if d.x == 0.0 && d.y == 0.0 {
            return c;
        }
        let (hw, hh) = (self.w / 2.0, self.h / 2.0);
        let tx = if d.x != 0.0 {
            hw / d.x.abs()
        } else {
            f32::INFINITY
        };
        let ty = if d.y != 0.0 {
            hh / d.y.abs()
        } else {
            f32::INFINITY
        };
        c.add(d.scale(tx.min(ty)))
    }
}

/// How the canvas is shown: screen = world × zoom + offset (relative to
/// the canvas element's top-left corner).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Viewport {
    pub zoom: f32,
    pub offset: Pt,
}

impl Default for Viewport {
    fn default() -> Self {
        Viewport {
            zoom: 1.0,
            offset: pt(40.0, 40.0),
        }
    }
}

impl Viewport {
    pub fn to_screen(&self, p: Pt) -> Pt {
        p.scale(self.zoom).add(self.offset)
    }

    pub fn to_world(&self, s: Pt) -> Pt {
        s.sub(self.offset).scale(1.0 / self.zoom)
    }

    pub fn rect_to_screen(&self, r: Rect) -> Rect {
        let o = self.to_screen(pt(r.x, r.y));
        Rect::new(o.x, o.y, r.w * self.zoom, r.h * self.zoom)
    }

    /// Zoom by `factor` keeping the canvas point under `at` (screen) still.
    pub fn zoom_at(&mut self, at: Pt, factor: f32) {
        if !factor.is_finite() || factor <= 0.0 {
            return;
        }
        let zoom = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        let world = self.to_world(at);
        self.zoom = zoom;
        self.offset = at.sub(world.scale(zoom));
    }

    /// The view showing all of `rects` in a `w`×`h` canvas with a margin,
    /// at most 100%; the default view without any.
    pub fn fit(rects: &[Rect], w: f32, h: f32) -> Viewport {
        const MARGIN: f32 = 40.0;
        let Some(all) = Rect::union(rects) else {
            return Viewport::default();
        };
        let avail_w = (w - 2.0 * MARGIN).max(1.0);
        let avail_h = (h - 2.0 * MARGIN).max(1.0);
        let zoom = (avail_w / all.w.max(1.0))
            .min(avail_h / all.h.max(1.0))
            .clamp(MIN_ZOOM, 1.0);
        let c = all.center();
        Viewport {
            zoom,
            offset: pt(w / 2.0 - c.x * zoom, h / 2.0 - c.y * zoom),
        }
    }

    /// The canvas area a `w`×`h` canvas shows.
    pub fn visible_world(&self, w: f32, h: f32) -> Rect {
        let a = self.to_world(pt(0.0, 0.0));
        Rect::new(a.x, a.y, w / self.zoom, h / self.zoom)
    }
}

/// Indices of the cards at least partly inside the view: the only ones
/// drawn (culling keeps a board of hundreds of cards light).
pub fn visible_cards(board: &Board, view: &Viewport, w: f32, h: f32) -> Vec<usize> {
    let area = view.visible_world(w, h);
    (0..board.cards.len())
        .filter(|&i| board.cards[i].rect.intersects(&area))
        .collect()
}

/// What is under a screen point.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Hit {
    Empty,
    Card(Uuid),
    /// A page card's title strip (opens the page).
    PageTitle(Uuid),
    /// The selected card's bottom-right corner.
    Resize(Uuid),
    /// The selected card's dot on its right edge (drag to connect).
    Connect(Uuid),
    Edge(Uuid),
}

/// The selected card's handles, in screen coordinates: (resize, connect).
pub fn handles(screen: Rect) -> (Rect, Rect) {
    let resize = Rect::new(
        screen.x + screen.w - RESIZE_HANDLE,
        screen.y + screen.h - RESIZE_HANDLE,
        RESIZE_HANDLE,
        RESIZE_HANDLE,
    );
    let connect = Rect::new(
        screen.x + screen.w - CONNECT_HANDLE / 2.0,
        screen.y + screen.h / 2.0 - CONNECT_HANDLE / 2.0,
        CONNECT_HANDLE,
        CONNECT_HANDLE,
    );
    (resize, connect)
}

/// What a press at screen point `s` lands on: the selected card's handles
/// first, then cards from the top down, then arrows.
pub fn hit(board: &Board, view: &Viewport, s: Pt, selected: Option<Uuid>) -> Hit {
    if let Some(card) = selected.and_then(|id| board.card(id)) {
        let (resize, connect) = handles(view.rect_to_screen(card.rect));
        if connect.contains(s) {
            return Hit::Connect(card.id);
        }
        if resize.contains(s) {
            return Hit::Resize(card.id);
        }
    }
    let w = view.to_world(s);
    if let Some(card) = board.cards.iter().rev().find(|c| c.rect.contains(w)) {
        let title = matches!(card.kind, super::CardKind::Page(_))
            && w.y <= card.rect.y + TITLE_H.min(card.rect.h);
        return if title {
            Hit::PageTitle(card.id)
        } else {
            Hit::Card(card.id)
        };
    }
    for edge in &board.edges {
        if let Some((a, b)) = edge_line(board, edge.from, edge.to) {
            let (a, b) = (view.to_screen(a), view.to_screen(b));
            if distance_to_segment(s, a, b) <= EDGE_SLOP {
                return Hit::Edge(edge.id);
            }
        }
    }
    Hit::Empty
}

/// The arrow from card `from` to card `to`, between their borders along
/// the line joining their centres (canvas units).
pub fn edge_line(board: &Board, from: Uuid, to: Uuid) -> Option<(Pt, Pt)> {
    let (a, b) = (board.card(from)?.rect, board.card(to)?.rect);
    Some(anchors(a, b))
}

pub fn anchors(a: Rect, b: Rect) -> (Pt, Pt) {
    (a.border_point(b.center()), b.border_point(a.center()))
}

/// The arrowhead's triangle (tip first) for a line ending at `tip` coming
/// from `from`, in screen units.
pub fn arrowhead(from: Pt, tip: Pt) -> [Pt; 3] {
    let d = tip.sub(from);
    let len = d.len();
    if len < f32::EPSILON {
        return [tip, tip, tip];
    }
    let u = d.scale(1.0 / len);
    let n = pt(-u.y, u.x);
    let base = tip.sub(u.scale(ARROW_LEN.min(len)));
    [
        tip,
        base.add(n.scale(ARROW_HALF_WIDTH)),
        base.sub(n.scale(ARROW_HALF_WIDTH)),
    ]
}

pub fn distance_to_segment(p: Pt, a: Pt, b: Pt) -> f32 {
    let ab = b.sub(a);
    let len2 = ab.x * ab.x + ab.y * ab.y;
    if len2 == 0.0 {
        return p.sub(a).len();
    }
    let t = ((p.x - a.x) * ab.x + (p.y - a.y) * ab.y) / len2;
    let t = t.clamp(0.0, 1.0);
    p.sub(a.add(ab.scale(t))).len()
}
