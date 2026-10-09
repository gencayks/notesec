//! Whiteboards (decision 53): an infinite canvas of cards and arrows,
//! stored as an ordinary page so search, links, backlinks, the graph, git
//! backup, import/export and sync keep working on it:
//!
//! ```markdown
//! - type:: whiteboard
//! - Buy milk                      <- a text card
//!   x:: 120
//!   y:: 80
//!   w:: 220
//!   h:: 120
//!   color:: yellow
//!   id:: 6a1f…                    <- written once an edge points at it
//! - [[Project]]                   <- a page card
//!   x:: 400
//!   …
//! - ((6a1f…))                     <- a live block card
//! - edges::
//!   - edge:: ((6a1f…)) -> ((9c2e…))
//!     label:: next
//! ```
//!
//! Every top-level block after the `type:: whiteboard` one is a card,
//! except the `edges::` block, whose children are the arrows. Garbled or
//! missing coordinates never fail: such cards are placed below the others.
//! No GPUI here: `geom.rs` has the canvas maths, the app side is
//! `app/whiteboard_ui.rs`.

pub mod geom;

use std::collections::HashSet;

use uuid::Uuid;

use crate::model::{parse_block_refs, Block, Page};
use crate::publish::property;
pub use geom::Rect;

/// A new card's size (canvas units: pixels at 100%).
pub const CARD_W: f32 = 220.0;
pub const CARD_H: f32 = 120.0;
pub const MIN_W: f32 = 80.0;
pub const MIN_H: f32 = 48.0;
pub const MAX_SIZE: f32 = 4000.0;
/// Coordinates beyond this are treated as garbled.
pub const MAX_COORD: f32 = 1_000_000.0;
/// The space between auto-placed cards.
pub const GAP: f32 = 40.0;

/// The card properties the canvas owns (hidden from the card's text).
const CARD_KEYS: [&str; 5] = ["x", "y", "w", "h", "color"];

/// Card colours: the name written in the file, and its fill (RGB).
pub const COLORS: [(&str, u32); 6] = [
    ("yellow", 0xfff3b0),
    ("green", 0xc8f2c2),
    ("blue", 0xc6dcff),
    ("red", 0xffc9c9),
    ("purple", 0xe2d1ff),
    ("gray", 0xe4e4e4),
];

/// Whether `page` is a whiteboard: `type:: whiteboard` in its first block.
pub fn is_whiteboard(page: &Page) -> bool {
    page.blocks
        .first()
        .is_some_and(|b| is_type_block(&b.content))
}

fn is_type_block(content: &str) -> bool {
    content
        .split('\n')
        .filter_map(property)
        .any(|(k, v)| k.eq_ignore_ascii_case("type") && v.trim().eq_ignore_ascii_case("whiteboard"))
}

/// A new, empty whiteboard page.
pub fn new_page(title: &str) -> Page {
    let mut page = Page::new(title, false);
    page.push_block("type:: whiteboard".into());
    page
}

#[derive(Clone, Debug, PartialEq)]
pub enum CardKind {
    /// Text (markdown, as a block shows it).
    Text,
    /// `[[Page]]` alone: the page's title and first lines.
    Page(String),
    /// `((uuid))` alone: that block's text, live.
    Block(Uuid),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Card {
    /// The card's block id.
    pub id: Uuid,
    /// Its block's index in the page.
    pub block: usize,
    pub rect: Rect,
    /// A name from `COLORS`.
    pub color: Option<&'static str>,
    /// The block's text without the canvas properties.
    pub text: String,
    pub kind: CardKind,
    /// False when its coordinates were missing or garbled (it was
    /// auto-placed; the next move writes real ones).
    pub placed: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Edge {
    /// The edge's block id.
    pub id: Uuid,
    pub from: Uuid,
    pub to: Uuid,
    pub label: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Board {
    /// In drawing order (later ones on top).
    pub cards: Vec<Card>,
    /// Only those whose two ends are cards here.
    pub edges: Vec<Edge>,
}

impl Board {
    pub fn card(&self, id: Uuid) -> Option<&Card> {
        self.cards.iter().find(|c| c.id == id)
    }

    /// Every card's rectangle, for fitting the view.
    pub fn rects(&self) -> Vec<Rect> {
        self.cards.iter().map(|c| c.rect).collect()
    }
}

/// Which top-level blocks are what.
fn roles(page: &Page) -> (Option<usize>, Vec<usize>, Vec<usize>) {
    let mut edges_parent = None;
    let mut cards = Vec::new();
    let mut edges = Vec::new();
    let skip_first = is_whiteboard(page);
    for (ix, block) in page.blocks.iter().enumerate() {
        if ix == 0 && skip_first {
            continue;
        }
        let parent_is_edges = block.parent_id.is_some()
            && edges_parent.is_some_and(|p: usize| Some(page.blocks[p].id) == block.parent_id);
        if is_edge(&block.content) && (block.parent_id.is_none() || parent_is_edges) {
            edges.push(ix);
        } else if block.parent_id.is_none() {
            if edges_parent.is_none() && is_edges_parent(&block.content) {
                edges_parent = Some(ix);
            } else {
                cards.push(ix);
            }
        }
    }
    (edges_parent, cards, edges)
}

fn first_property(content: &str) -> Option<(&str, &str)> {
    property(content.split('\n').next().unwrap_or(""))
}

fn is_edges_parent(content: &str) -> bool {
    first_property(content).is_some_and(|(k, _)| k.eq_ignore_ascii_case("edges"))
}

fn is_edge(content: &str) -> bool {
    first_property(content).is_some_and(|(k, _)| k.eq_ignore_ascii_case("edge"))
}

/// A card property's key, if `line` is one (`x`, `y`, `w`, `h`, `color`).
fn card_key(line: &str) -> Option<&'static str> {
    let (key, _) = property(line)?;
    CARD_KEYS
        .iter()
        .copied()
        .find(|k| k.eq_ignore_ascii_case(key))
}

/// A card's text: its block content without the canvas properties.
pub fn card_text(content: &str) -> String {
    content
        .split('\n')
        .filter(|l| card_key(l).is_none())
        .collect::<Vec<_>>()
        .join("\n")
}

/// `content` with its text replaced by `text`, canvas properties kept.
pub fn with_text(content: &str, text: &str) -> String {
    let props: Vec<&str> = content
        .split('\n')
        .filter(|l| card_key(l).is_some())
        .collect();
    let mut out = text.to_string();
    for p in props {
        out.push('\n');
        out.push_str(p);
    }
    out
}

/// `content` with `x/y/w/h` set to `rect` (whole numbers), other lines
/// kept in place.
pub fn with_rect(content: &str, rect: Rect) -> String {
    let values = [
        ("x", rect.x.round()),
        ("y", rect.y.round()),
        ("w", rect.w.round()),
        ("h", rect.h.round()),
    ];
    let mut lines: Vec<String> = content
        .split('\n')
        .filter(|l| !matches!(card_key(l), Some("x" | "y" | "w" | "h")))
        .map(str::to_string)
        .collect();
    for (k, v) in values {
        lines.push(format!("{k}:: {}", v as i64));
    }
    lines.join("\n")
}

/// `content` with `color::` set (or removed for `None`).
pub fn with_color(content: &str, color: Option<&str>) -> String {
    let mut lines: Vec<String> = content
        .split('\n')
        .filter(|l| card_key(l) != Some("color"))
        .map(str::to_string)
        .collect();
    if let Some(color) = color {
        lines.push(format!("color:: {color}"));
    }
    lines.join("\n")
}

fn number(v: &str) -> Option<f32> {
    v.trim()
        .parse::<f32>()
        .ok()
        .filter(|n| n.is_finite() && n.abs() <= MAX_COORD)
}

fn kind(text: &str) -> CardKind {
    let t = text.trim();
    if let Some(inner) = t.strip_prefix("[[").and_then(|r| r.strip_suffix("]]")) {
        if !inner.trim().is_empty() && !inner.contains("[[") && !inner.contains("]]") {
            return CardKind::Page(inner.trim().to_string());
        }
    }
    if let [(range, id)] = parse_block_refs(t).as_slice() {
        if *range == (0..t.len()) {
            return CardKind::Block(*id);
        }
    }
    CardKind::Text
}

fn parse_card(block: &Block, ix: usize) -> Card {
    let (mut x, mut y, mut w, mut h, mut color) = (None, None, None, None, None);
    for line in block.content.split('\n') {
        let Some(key) = card_key(line) else { continue };
        let value = property(line).map_or("", |(_, v)| v);
        match key {
            "x" => x = number(value),
            "y" => y = number(value),
            "w" => w = number(value),
            "h" => h = number(value),
            _ => {
                color = COLORS
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case(value.trim()))
                    .map(|(name, _)| *name)
            }
        }
    }
    let size = |v: Option<f32>, default: f32, min: f32| {
        v.filter(|v| *v > 0.0)
            .map_or(default, |v| v.clamp(min, MAX_SIZE))
    };
    let text = card_text(&block.content);
    Card {
        id: block.id,
        block: ix,
        rect: Rect::new(
            x.unwrap_or(0.0),
            y.unwrap_or(0.0),
            size(w, CARD_W, MIN_W),
            size(h, CARD_H, MIN_H),
        ),
        color,
        kind: kind(&text),
        text,
        placed: x.is_some() && y.is_some(),
    }
}

fn parse_edge(block: &Block) -> Option<Edge> {
    let first = block.content.split('\n').next().unwrap_or("");
    let (_, value) = property(first)?;
    let refs = parse_block_refs(value);
    let [(_, from), (_, to)] = refs.as_slice() else {
        return None;
    };
    let label = block
        .content
        .split('\n')
        .skip(1)
        .filter_map(property)
        .find(|(k, _)| k.eq_ignore_ascii_case("label"))
        .map(|(_, v)| v.to_string())
        .filter(|v| !v.is_empty());
    Some(Edge {
        id: block.id,
        from: *from,
        to: *to,
        label,
    })
}

/// The board on `page`. Never fails: cards without usable coordinates are
/// placed in rows below the others; edges to missing cards are left out
/// (and stay in the file).
pub fn parse(page: &Page) -> Board {
    let (_, card_ix, edge_ix) = roles(page);
    let mut cards: Vec<Card> = card_ix
        .iter()
        .map(|&ix| parse_card(&page.blocks[ix], ix))
        .collect();
    auto_place(&mut cards);
    let ids: HashSet<Uuid> = cards.iter().map(|c| c.id).collect();
    let edges = edge_ix
        .iter()
        .filter_map(|&ix| parse_edge(&page.blocks[ix]))
        .filter(|e| e.from != e.to && ids.contains(&e.from) && ids.contains(&e.to))
        .collect();
    Board { cards, edges }
}

/// Put the unplaced cards in rows of four under the placed ones.
fn auto_place(cards: &mut [Card]) {
    let placed: Vec<Rect> = cards.iter().filter(|c| c.placed).map(|c| c.rect).collect();
    let left = placed.iter().map(|r| r.x).fold(f32::INFINITY, f32::min);
    let left = if left.is_finite() { left } else { 0.0 };
    let mut y = placed
        .iter()
        .map(|r| r.y + r.h + GAP)
        .fold(f32::NEG_INFINITY, f32::max);
    if !y.is_finite() {
        y = 0.0;
    }
    let (mut x, mut row_h, mut col) = (left, 0.0f32, 0);
    for card in cards.iter_mut().filter(|c| !c.placed) {
        if col == 4 {
            col = 0;
            x = left;
            y += row_h + GAP;
            row_h = 0.0;
        }
        card.rect.x = x;
        card.rect.y = y;
        x += card.rect.w + GAP;
        row_h = row_h.max(card.rect.h);
        col += 1;
    }
}

// --- edits (the app records undo and saves around these) ----------------

fn block_index(page: &Page, id: Uuid) -> Option<usize> {
    page.blocks.iter().position(|b| b.id == id)
}

/// Add a card with `text` at `rect`; returns its block id. Cards go before
/// the `edges::` block, so the file reads cards first.
pub fn add_card(page: &mut Page, text: &str, rect: Rect) -> Uuid {
    let (edges_parent, _, _) = roles(page);
    let id = Uuid::new_v4();
    let block = Block {
        id,
        content: with_rect(text, rect),
        parent_id: None,
        page_id: page.id.clone(),
        order: 0,
    };
    match edges_parent {
        Some(at) => page.blocks.insert(at, block),
        None => page.blocks.push(block),
    }
    page.renumber();
    id
}

/// Move or resize card `id`.
pub fn set_rect(page: &mut Page, id: Uuid, rect: Rect) -> bool {
    let Some(ix) = block_index(page, id) else {
        return false;
    };
    let rect = Rect::new(
        rect.x.clamp(-MAX_COORD, MAX_COORD),
        rect.y.clamp(-MAX_COORD, MAX_COORD),
        rect.w.clamp(MIN_W, MAX_SIZE),
        rect.h.clamp(MIN_H, MAX_SIZE),
    );
    let new = with_rect(&page.blocks[ix].content, rect);
    let changed = new != page.blocks[ix].content;
    page.blocks[ix].content = new;
    changed
}

pub fn set_color(page: &mut Page, id: Uuid, color: Option<&str>) -> bool {
    let Some(ix) = block_index(page, id) else {
        return false;
    };
    let new = with_color(&page.blocks[ix].content, color);
    let changed = new != page.blocks[ix].content;
    page.blocks[ix].content = new;
    changed
}

/// Remove card `id` (with any blocks nested under it) and its edges.
pub fn delete_card(page: &mut Page, id: Uuid) -> bool {
    let Some(ix) = block_index(page, id) else {
        return false;
    };
    let end = page.subtree_end(ix);
    page.blocks.drain(ix..end);
    let gone: Vec<Uuid> = parse_edges_raw(page)
        .into_iter()
        .filter(|e| e.from == id || e.to == id)
        .map(|e| e.id)
        .collect();
    for edge in gone {
        delete_edge(page, edge);
    }
    page.saved_ids.remove(&id);
    page.renumber();
    true
}

/// Every edge block, whether or not its ends exist.
fn parse_edges_raw(page: &Page) -> Vec<Edge> {
    let (_, _, edges) = roles(page);
    edges
        .iter()
        .filter_map(|&ix| parse_edge(&page.blocks[ix]))
        .collect()
}

/// Connect card `from` to card `to` (an arrow); returns the edge's block
/// id. `None` for a card to itself, an unknown card or an existing edge.
pub fn connect(page: &mut Page, from: Uuid, to: Uuid) -> Option<Uuid> {
    let cards: HashSet<Uuid> = parse(page).cards.iter().map(|c| c.id).collect();
    if from == to || !cards.contains(&from) || !cards.contains(&to) {
        return None;
    }
    if parse_edges_raw(page)
        .iter()
        .any(|e| e.from == from && e.to == to)
    {
        return None;
    }
    let parent = match roles(page).0 {
        Some(ix) => ix,
        None => {
            page.push_block("edges::".into());
            page.blocks.len() - 1
        }
    };
    let id = Uuid::new_v4();
    let at = page.subtree_end(parent);
    page.blocks.insert(
        at,
        Block {
            id,
            content: format!("edge:: (({from})) -> (({to}))"),
            parent_id: Some(page.blocks[parent].id),
            page_id: page.id.clone(),
            order: 0,
        },
    );
    // The cards' ids go into the file so the edge finds them again.
    page.saved_ids.insert(from);
    page.saved_ids.insert(to);
    page.renumber();
    Some(id)
}

/// Remove edge `id`.
pub fn delete_edge(page: &mut Page, id: Uuid) -> bool {
    let Some(ix) = block_index(page, id) else {
        return false;
    };
    if !is_edge(&page.blocks[ix].content) {
        return false;
    }
    let end = page.subtree_end(ix);
    page.blocks.drain(ix..end);
    page.renumber();
    true
}

#[cfg(test)]
mod tests;
