//! The page graph: model construction and force-directed layout.
//!
//! This module is pure maths: no GPUI types, no I/O. That keeps the physics
//! easy to unit-test and tune. `graph_view.rs` draws and drives it.
//!
//! * **Nodes** are pages. **Edges** are `[[wikilinks]]` and `#tags` between
//!   pages (a tag points at the page named like it; a reference in either
//!   direction gives one undirected edge).
//! * A **local graph** (`Graph::build_local`) keeps only one page and the
//!   pages within a few edges of it; it is an ordinary `Graph`, so the same
//!   physics and drawing apply.
//! * **Layout** is a classic force simulation: every pair of nodes repels, every
//!   edge pulls its ends together like a spring, and a weak pull toward the
//!   origin keeps disconnected pieces from drifting away. A "temperature"
//!   (`alpha`) cools each tick so the layout settles, and a hard tick cap
//!   guarantees it can never run forever.

use crate::model::{page_aliases, parse_references, Page};
use std::collections::{HashMap, HashSet};

// --- tunable physics constants ------------------------------------------------
// Units are "world" units; the view maps them to pixels with its zoom.

/// Strength of the inverse-square push between every pair of nodes.
const REPULSION: f32 = 1500.0;
/// Pairs further apart than this don't repel (also saves work).
const REPULSION_RANGE: f32 = 360.0;
/// Extra clearance kept between node edges; closer pairs are pushed apart
/// firmly, so nodes never overlap however crowded the layout is.
const COLLISION_PADDING: f32 = 5.0;
/// How hard overlapping nodes are pushed apart.
const COLLISION_STRENGTH: f32 = 0.6;
/// Distances below this are treated as this, so near-coincident nodes get a
/// bounded push instead of an explosion.
const MIN_DISTANCE: f32 = 6.0;
/// Spring stiffness and natural length of an edge.
const SPRING: f32 = 0.09;
const SPRING_REST: f32 = 62.0;
/// Pull toward the origin, per unit of distance.
const GRAVITY: f32 = 0.02;
/// Fraction of velocity kept each tick (the rest is friction).
const VELOCITY_KEEP: f32 = 0.62;
/// No node moves faster than this per tick.
const MAX_SPEED: f32 = 28.0;
/// Temperature: starts at 1, multiplied by this every tick, stops at the minimum.
const ALPHA_DECAY: f32 = 0.0228;
const ALPHA_MIN: f32 = 0.002;
/// Hard cap on ticks per simulation run, whatever the temperature.
pub const MAX_TICKS: u32 = 600;

/// One page in the graph.
#[derive(Clone, Debug)]
pub struct Node {
    pub title: String,
    pub is_journal: bool,
    /// Number of distinct pages that link to this one.
    pub backlinks: usize,
    pub x: f32,
    pub y: f32,
    vx: f32,
    vy: f32,
    /// Pinned nodes are not moved by the simulation (set when you drag one).
    pub pinned: bool,
}

impl Node {
    /// Radius in world units: grows with the square root of the backlink count,
    /// so a hub with 16 backlinks is about 3x the area of... a leaf, not 16x.
    pub fn radius(&self) -> f32 {
        (5.0 + 3.2 * (self.backlinks as f32).sqrt()).min(20.0)
    }
}

pub struct Graph {
    pub nodes: Vec<Node>,
    /// Undirected edges as `(a, b)` with `a < b`, sorted, no duplicates.
    pub edges: Vec<(usize, usize)>,
    /// `adj[i]` lists the neighbours of node `i`.
    pub adj: Vec<Vec<usize>>,
    alpha: f32,
    ticks: u32,
}

impl Graph {
    /// Build the graph from pages. With `include_journals == false`, journal
    /// pages (and any links to or from them) are left out.
    ///
    /// Links to pages that don't exist and links from a page to itself are
    /// ignored. Link targets match page titles case-insensitively, as elsewhere.
    pub fn build(pages: &[Page], include_journals: bool) -> Graph {
        Graph::build_from(pages.iter().filter(|p| include_journals || !p.is_journal))
    }

    /// The local graph around the page called `center` (ignoring case): that
    /// page plus every page within `hops` edges of it, links and tags in
    /// either direction, and the edges among them. With journals hidden,
    /// `center` is kept even if it is a journal, but no other journal is
    /// (so none bridges to a 2-hop neighbour either). Empty if there is no
    /// such page.
    pub fn build_local(pages: &[Page], include_journals: bool, center: &str, hops: usize) -> Graph {
        let is_center = |p: &Page| p.title.eq_ignore_ascii_case(center);
        let graph = Graph::build_from(
            pages
                .iter()
                .filter(|p| include_journals || !p.is_journal || is_center(p)),
        );
        match graph
            .nodes
            .iter()
            .position(|n| n.title.eq_ignore_ascii_case(center))
        {
            Some(center) => graph.local_subgraph(center, hops),
            None => Graph::build_from(std::iter::empty()),
        }
    }

    /// Node `center` and every node within `hops` edges of it (edges are
    /// undirected, so incoming and outgoing links both count), keeping
    /// their positions, pins and backlink counts, with only the edges whose
    /// ends are both kept. Nodes stay in their original order.
    pub fn local_subgraph(&self, center: usize, hops: usize) -> Graph {
        // Breadth-first search, one ring of neighbours per hop.
        let mut keep = vec![false; self.nodes.len()];
        keep[center] = true;
        let mut ring = vec![center];
        for _ in 0..hops {
            let mut next = Vec::new();
            for &i in &ring {
                for &j in &self.adj[i] {
                    if !keep[j] {
                        keep[j] = true;
                        next.push(j);
                    }
                }
            }
            ring = next;
        }

        // Old index -> new index for the kept nodes.
        let mut new_index = vec![usize::MAX; self.nodes.len()];
        let mut nodes = Vec::new();
        for (i, node) in self.nodes.iter().enumerate() {
            if keep[i] {
                new_index[i] = nodes.len();
                nodes.push(node.clone());
            }
        }
        // Old edges are sorted with a < b, and renumbering keeps the order.
        let edges: Vec<(usize, usize)> = self
            .edges
            .iter()
            .filter(|&&(a, b)| keep[a] && keep[b])
            .map(|&(a, b)| (new_index[a], new_index[b]))
            .collect();
        let mut adj = vec![Vec::new(); nodes.len()];
        for &(a, b) in &edges {
            adj[a].push(b);
            adj[b].push(a);
        }
        Graph {
            nodes,
            edges,
            adj,
            alpha: self.alpha,
            ticks: 0,
        }
    }

    /// Build the graph from exactly these pages.
    fn build_from<'a>(pages: impl Iterator<Item = &'a Page>) -> Graph {
        let kept: Vec<&Page> = pages.collect();
        let mut index: HashMap<String, usize> = kept
            .iter()
            .enumerate()
            .map(|(i, p)| (p.title.to_lowercase(), i))
            .collect();
        // A link to an alias is a link to its page (`model::resolve_page`):
        // titles win, then the first page claiming the alias.
        for (i, p) in kept.iter().enumerate() {
            for alias in page_aliases(p) {
                index.entry(alias.to_lowercase()).or_insert(i);
            }
        }

        let mut undirected: HashSet<(usize, usize)> = HashSet::new();
        // Distinct (source, target) pairs, so one page linking to another ten
        // times counts as a single backlink.
        let mut directed: HashSet<(usize, usize)> = HashSet::new();
        for (src, page) in kept.iter().enumerate() {
            for block in &page.blocks {
                // `[[links]]` and `#tags` alike.
                for link in parse_references(&block.content) {
                    let Some(&dst) = index.get(&link.target.to_lowercase()) else {
                        continue;
                    };
                    if dst != src {
                        directed.insert((src, dst));
                        undirected.insert((src.min(dst), src.max(dst)));
                    }
                }
            }
        }
        let mut backlinks = vec![0usize; kept.len()];
        for &(_, dst) in &directed {
            backlinks[dst] += 1;
        }

        let mut edges: Vec<(usize, usize)> = undirected.into_iter().collect();
        edges.sort_unstable();
        let mut adj = vec![Vec::new(); kept.len()];
        for &(a, b) in &edges {
            adj[a].push(b);
            adj[b].push(a);
        }

        // Start on a golden-angle spiral: deterministic, evenly spread, and no
        // two nodes begin at the same point.
        let nodes = kept
            .iter()
            .enumerate()
            .map(|(i, page)| {
                let angle = i as f32 * 2.399_963; // golden angle in radians
                let r = 34.0 * (i as f32 + 0.5).sqrt();
                Node {
                    title: page.title.clone(),
                    is_journal: page.is_journal,
                    backlinks: backlinks[i],
                    x: r * angle.cos(),
                    y: r * angle.sin(),
                    vx: 0.0,
                    vy: 0.0,
                    pinned: false,
                }
            })
            .collect();

        Graph {
            nodes,
            edges,
            adj,
            alpha: 1.0,
            ticks: 0,
        }
    }

    /// Give nodes that also exist in `old` their old position and pin
    /// state, and set the temperature for re-settling (gentle if anything
    /// was kept). Used when the graph is rebuilt (journal toggle, edits,
    /// global/local switch) so it doesn't jump around.
    pub fn preserve_layout(&mut self, old: &Graph) {
        let previous: HashMap<&str, &Node> =
            old.nodes.iter().map(|n| (n.title.as_str(), n)).collect();
        let mut kept_any = false;
        for node in &mut self.nodes {
            if let Some(old_node) = previous.get(node.title.as_str()) {
                node.x = old_node.x;
                node.y = old_node.y;
                node.pinned = old_node.pinned;
                kept_any = true;
            }
        }
        // Gentle reheat so new nodes find a place, without scrambling the rest.
        self.alpha = if kept_any { 0.3 } else { 1.0 };
        self.ticks = 0;
    }

    pub fn is_settled(&self) -> bool {
        self.alpha < ALPHA_MIN || self.ticks >= MAX_TICKS
    }

    /// Warm up the simulation up to `ticks` steps (stops early when settled).
    pub fn warm_up(&mut self, ticks: u32) {
        for _ in 0..ticks {
            if self.is_settled() {
                break;
            }
            self.tick();
        }
    }

    /// Raise the temperature (e.g. while dragging) so the layout reacts. The
    /// tick budget restarts, but is still capped at `MAX_TICKS`.
    pub fn reheat(&mut self, alpha: f32) {
        self.alpha = self.alpha.max(alpha);
        self.ticks = 0;
    }

    /// Advance the simulation by one step.
    pub fn tick(&mut self) {
        if self.is_settled() {
            return;
        }
        let n = self.nodes.len();
        let mut fx = vec![0.0f32; n];
        let mut fy = vec![0.0f32; n];

        // Repulsion between every pair. O(n^2): about 45k pairs for 300 nodes,
        // which is well under a millisecond per tick.
        for i in 0..n {
            for j in (i + 1)..n {
                let mut dx = self.nodes[i].x - self.nodes[j].x;
                let mut dy = self.nodes[i].y - self.nodes[j].y;
                let mut d2 = dx * dx + dy * dy;
                if d2 > REPULSION_RANGE * REPULSION_RANGE {
                    continue;
                }
                if d2 < 1e-6 {
                    // Exactly on top of each other: nudge apart deterministically.
                    dx = 0.5 * (((i + j) % 3) as f32 - 1.0 + 0.1);
                    dy = 0.5 * (((i * 7 + j) % 3) as f32 - 1.0 + 0.1);
                    d2 = dx * dx + dy * dy;
                }
                let dist = d2.sqrt();
                let d = dist.max(MIN_DISTANCE);
                let mut force = REPULSION / (d * d);
                // Collision: if the circles (plus padding) overlap, add a push
                // proportional to the overlap depth.
                let clearance = self.nodes[i].radius() + self.nodes[j].radius() + COLLISION_PADDING;
                if dist < clearance {
                    force += COLLISION_STRENGTH * (clearance - dist);
                }
                let (ux, uy) = (dx / dist, dy / dist);
                fx[i] += force * ux;
                fy[i] += force * uy;
                fx[j] -= force * ux;
                fy[j] -= force * uy;
            }
        }

        // Springs along edges: pull together when stretched, push apart when
        // squeezed, relative to the rest length.
        for &(a, b) in &self.edges {
            let dx = self.nodes[b].x - self.nodes[a].x;
            let dy = self.nodes[b].y - self.nodes[a].y;
            let d = (dx * dx + dy * dy).sqrt().max(1e-3);
            let force = SPRING * (d - SPRING_REST);
            let (ux, uy) = (dx / d, dy / d);
            fx[a] += force * ux;
            fy[a] += force * uy;
            fx[b] -= force * ux;
            fy[b] -= force * uy;
        }

        // Integrate. Forces are scaled by the temperature so motion calms down.
        for (i, node) in self.nodes.iter_mut().enumerate() {
            if node.pinned {
                node.vx = 0.0;
                node.vy = 0.0;
                continue;
            }
            fx[i] -= GRAVITY * node.x;
            fy[i] -= GRAVITY * node.y;
            node.vx = (node.vx + fx[i] * self.alpha) * VELOCITY_KEEP;
            node.vy = (node.vy + fy[i] * self.alpha) * VELOCITY_KEEP;
            let speed = (node.vx * node.vx + node.vy * node.vy).sqrt();
            if speed > MAX_SPEED {
                node.vx *= MAX_SPEED / speed;
                node.vy *= MAX_SPEED / speed;
            }
            node.x += node.vx;
            node.y += node.vy;
        }

        self.alpha *= 1.0 - ALPHA_DECAY;
        self.ticks += 1;
    }

    /// Index of the node under the world-space point `(wx, wy)`, if any. `slop`
    /// widens every node's hit area (in world units) so small nodes stay
    /// clickable. The closest node wins when several overlap.
    pub fn hit_test(&self, wx: f32, wy: f32, slop: f32) -> Option<usize> {
        self.nodes
            .iter()
            .enumerate()
            .filter_map(|(i, n)| {
                let d = ((n.x - wx).powi(2) + (n.y - wy).powi(2)).sqrt();
                let edge_gap = d - n.radius();
                (edge_gap <= slop).then_some((i, edge_gap))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(i, _)| i)
    }

    #[cfg(test)]
    pub fn index_of(&self, title: &str) -> Option<usize> {
        let wanted = title.to_lowercase();
        self.nodes
            .iter()
            .position(|n| n.title.to_lowercase() == wanted)
    }

    /// World-space bounding box `(min_x, min_y, max_x, max_y)` of all nodes,
    /// including their radii. An empty graph gets a small box around the origin.
    pub fn bounds(&self) -> (f32, f32, f32, f32) {
        if self.nodes.is_empty() {
            return (-1.0, -1.0, 1.0, 1.0);
        }
        let mut b = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for n in &self.nodes {
            let r = n.radius();
            b.0 = b.0.min(n.x - r);
            b.1 = b.1.min(n.y - r);
            b.2 = b.2.max(n.x + r);
            b.3 = b.3.max(n.y + r);
        }
        b
    }
}

/// Shorten a page title for use as a node label.
pub fn short_label(title: &str) -> String {
    const MAX: usize = 26;
    if title.chars().count() <= MAX {
        title.to_string()
    } else {
        let head: String = title.chars().take(MAX - 1).collect();
        format!("{}…", head.trim_end())
    }
}

// --- view transform ----------------------------------------------------------

pub const MIN_ZOOM: f32 = 0.1;
pub const MAX_ZOOM: f32 = 4.0;

/// Maps between world space (where the layout lives) and screen space (pixels
/// inside the graph pane). World `(0, 0)` appears at the pane centre when
/// `pan` is zero.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct View {
    pub pan_x: f32,
    pub pan_y: f32,
    pub zoom: f32,
}

impl Default for View {
    fn default() -> Self {
        View {
            pan_x: 0.0,
            pan_y: 0.0,
            zoom: 1.0,
        }
    }
}

impl View {
    /// World -> pane-local pixels, for a pane of size `vw` x `vh`.
    pub fn to_screen(&self, wx: f32, wy: f32, vw: f32, vh: f32) -> (f32, f32) {
        (
            vw / 2.0 + self.pan_x + wx * self.zoom,
            vh / 2.0 + self.pan_y + wy * self.zoom,
        )
    }

    /// Pane-local pixels -> world.
    pub fn to_world(&self, sx: f32, sy: f32, vw: f32, vh: f32) -> (f32, f32) {
        (
            (sx - vw / 2.0 - self.pan_x) / self.zoom,
            (sy - vh / 2.0 - self.pan_y) / self.zoom,
        )
    }

    /// Multiply the zoom by `factor`, keeping the world point under the screen
    /// point `(sx, sy)` fixed, so zooming feels anchored to the cursor.
    pub fn zoom_at(&mut self, sx: f32, sy: f32, factor: f32, vw: f32, vh: f32) {
        let (wx, wy) = self.to_world(sx, sy, vw, vh);
        self.zoom = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        self.pan_x = sx - vw / 2.0 - wx * self.zoom;
        self.pan_y = sy - vh / 2.0 - wy * self.zoom;
    }

    /// Move a fraction `t` (0..1) of the way toward `target`. Returns true
    /// once close enough that the camera can be considered arrived. Zoom is
    /// interpolated in log space so zooming in and out feel equally fast.
    pub fn approach(&mut self, target: &View, t: f32) -> bool {
        self.pan_x += (target.pan_x - self.pan_x) * t;
        self.pan_y += (target.pan_y - self.pan_y) * t;
        self.zoom = (self.zoom.ln() + (target.zoom.ln() - self.zoom.ln()) * t).exp();
        (target.pan_x - self.pan_x).abs() < 0.5
            && (target.pan_y - self.pan_y).abs() < 0.5
            && (target.zoom / self.zoom - 1.0).abs() < 0.002
    }

    /// Choose pan and zoom so the world box `bounds` fits the pane with
    /// `margin` pixels to spare. Never zooms in past 1.4x, so a tiny graph
    /// isn't blown up absurdly.
    pub fn fit(&mut self, bounds: (f32, f32, f32, f32), vw: f32, vh: f32, margin: f32) {
        let (min_x, min_y, max_x, max_y) = bounds;
        let bw = (max_x - min_x).max(1.0);
        let bh = (max_y - min_y).max(1.0);
        let zoom = ((vw - 2.0 * margin) / bw).min((vh - 2.0 * margin) / bh);
        self.zoom = zoom.clamp(MIN_ZOOM, 1.4);
        let (cx, cy) = ((min_x + max_x) / 2.0, (min_y + max_y) / 2.0);
        self.pan_x = -cx * self.zoom;
        self.pan_y = -cy * self.zoom;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pages() -> Vec<Page> {
        vec![
            Page::from_markdown(
                "Hub",
                false,
                "- a [[Leaf1]] and [[leaf2]]\n- again [[Leaf1]]\n",
            ),
            Page::from_markdown("Leaf1", false, "- back to [[Hub]] and itself [[Leaf1]]\n"),
            Page::from_markdown("Leaf2", false, "- links to [[Missing]]\n"),
            Page::from_markdown("Island", false, "- no links\n"),
            Page::from_markdown("2026-01-01", true, "- today [[Hub]]\n"),
        ]
    }

    fn titles(g: &Graph) -> Vec<&str> {
        g.nodes.iter().map(|n| n.title.as_str()).collect()
    }

    #[test]
    fn builds_nodes_and_deduplicated_edges() {
        let g = Graph::build(&pages(), true);
        assert_eq!(
            titles(&g),
            vec!["Hub", "Leaf1", "Leaf2", "Island", "2026-01-01"]
        );
        // Hub-Leaf1 (linked both ways and twice), Hub-Leaf2 (case-insensitive),
        // journal-Hub. No self-loop, no edge to the missing page.
        assert_eq!(g.edges, vec![(0, 1), (0, 2), (0, 4)]);
        assert_eq!(g.adj[0], vec![1, 2, 4]);
        assert!(g.adj[3].is_empty());
    }

    #[test]
    fn links_to_an_alias_are_edges_to_its_page() {
        let pages = vec![
            Page::from_markdown("JavaScript", false, "- alias:: JS, Web\n"),
            Page::from_markdown("A", false, "- uses [[js]] and #JS\n"),
            Page::from_markdown("Web", false, "- links [[javascript]]\n"),
            Page::from_markdown("B", false, "- [[Web]] is the real page\n"),
        ];
        let g = Graph::build(&pages, true);
        // A-JavaScript (via the alias, counted once), Web-JavaScript, and
        // B-Web: the real page wins over JavaScript's "Web" alias.
        assert_eq!(g.edges, vec![(0, 1), (0, 2), (2, 3)]);
        assert_eq!(g.nodes[0].backlinks, 2);
    }

    /// The local-graph fixture: Center links out to Out and tags #topic;
    /// In and a journal link to it; Far is two hops out (via Out), Island
    /// three (via Far); Unrelated is not connected at all.
    fn local_pages() -> Vec<Page> {
        vec![
            Page::from_markdown("Center", false, "- see [[Out]] about #topic\n"),
            Page::from_markdown("Out", false, "- onwards to [[Far]]\n"),
            Page::from_markdown("In", false, "- mentions [[center]] and [[Out]]\n"),
            Page::from_markdown("topic", false, "- a tag page\n"),
            Page::from_markdown("Far", false, "- the end\n"),
            Page::from_markdown("Island", false, "- only [[Far]]\n"),
            Page::from_markdown("Unrelated", false, "- nothing\n"),
            Page::from_markdown("2026-01-01", true, "- worked on [[Center]]\n"),
        ]
    }

    /// Edges as sorted title pairs, so tests don't depend on node order.
    fn edge_titles(g: &Graph) -> Vec<(String, String)> {
        let mut edges: Vec<(String, String)> = g
            .edges
            .iter()
            .map(|&(a, b)| {
                let (a, b) = (g.nodes[a].title.clone(), g.nodes[b].title.clone());
                if a < b {
                    (a, b)
                } else {
                    (b, a)
                }
            })
            .collect();
        edges.sort();
        edges
    }

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    #[test]
    fn tags_are_edges_to_their_page() {
        let g = Graph::build(&local_pages(), true);
        assert!(edge_titles(&g).contains(&("Center".into(), "topic".into())));
        assert_eq!(g.nodes[g.index_of("topic").unwrap()].backlinks, 1);
        // `#[[x]]` is one reference, not a tag plus a link.
        let pages = vec![
            Page::from_markdown("A", false, "- #[[B]] and [[B]]\n"),
            Page::from_markdown("B", false, "- b\n"),
        ];
        let g = Graph::build(&pages, true);
        assert_eq!(g.edges, vec![(0, 1)]);
        assert_eq!(g.nodes[1].backlinks, 1);
    }

    #[test]
    fn local_graph_is_the_page_and_its_neighbours_both_ways() {
        let g = Graph::build_local(&local_pages(), true, "center", 1);
        // Outgoing link, tag, incoming link and the journal's link; in the
        // original order. Far (2 hops), Island and Unrelated are out.
        assert_eq!(
            titles(&g),
            vec!["Center", "Out", "In", "topic", "2026-01-01"]
        );
        // Only edges between kept pages: In-Out stays, Out-Far is gone.
        assert_eq!(
            edge_titles(&g),
            pairs(&[
                ("2026-01-01", "Center"),
                ("Center", "In"),
                ("Center", "Out"),
                ("Center", "topic"),
                ("In", "Out"),
            ])
        );
        // Adjacency matches the edges.
        let center = g.index_of("Center").unwrap();
        assert_eq!(g.adj[center].len(), 4);
        assert_eq!(g.adj.iter().map(Vec::len).sum::<usize>(), 2 * g.edges.len());
        // Node sizes still reflect the whole graph.
        assert_eq!(g.nodes[center].backlinks, 2);
    }

    #[test]
    fn two_hops_reach_one_ring_further() {
        let g = Graph::build_local(&local_pages(), true, "Center", 2);
        assert_eq!(
            titles(&g),
            vec!["Center", "Out", "In", "topic", "Far", "2026-01-01"]
        );
        assert!(edge_titles(&g).contains(&("Far".into(), "Out".into())));
        assert!(g.index_of("Island").is_none(), "three hops away");
        // Zero hops: just the page.
        let g = Graph::build_local(&local_pages(), true, "Center", 0);
        assert_eq!(titles(&g), vec!["Center"]);
        assert!(g.edges.is_empty());
    }

    #[test]
    fn local_graph_with_journals_hidden_keeps_a_journal_centre() {
        let g = Graph::build_local(&local_pages(), false, "Center", 1);
        assert_eq!(titles(&g), vec!["Center", "Out", "In", "topic"]);
        // Centred on a journal: it stays, other journals don't.
        let g = Graph::build_local(&local_pages(), false, "2026-01-01", 2);
        assert_eq!(
            titles(&g),
            vec!["Center", "Out", "In", "topic", "2026-01-01"]
        );
        // An unknown page gives an empty graph.
        let g = Graph::build_local(&local_pages(), true, "Nope", 1);
        assert!(g.nodes.is_empty() && g.edges.is_empty());
    }

    #[test]
    fn local_subgraph_keeps_positions_and_pins() {
        let mut full = Graph::build(&local_pages(), true);
        settle(&mut full);
        let out = full.index_of("Out").unwrap();
        full.nodes[out].pinned = true;
        let local = full.local_subgraph(full.index_of("Center").unwrap(), 1);
        for n in &local.nodes {
            let o = &full.nodes[full.index_of(&n.title).unwrap()];
            assert_eq!((n.x, n.y, n.pinned), (o.x, o.y, o.pinned));
        }
        // The subgraph runs the same physics.
        let mut local = local;
        local.reheat(1.0);
        local.tick();
        assert!(local.nodes.iter().all(|n| n.x.is_finite()));
    }

    #[test]
    fn backlinks_count_distinct_source_pages() {
        let g = Graph::build(&pages(), true);
        // Hub is linked from Leaf1 and the journal; Leaf1 only from Hub (the
        // repeated link counts once, the self-link not at all).
        assert_eq!(g.nodes[0].backlinks, 2);
        assert_eq!(g.nodes[1].backlinks, 1);
        assert_eq!(g.nodes[2].backlinks, 1);
        assert_eq!(g.nodes[3].backlinks, 0);
        assert!(g.nodes[0].radius() > g.nodes[3].radius(), "hubs are bigger");
    }

    #[test]
    fn hiding_journals_removes_them_and_their_edges() {
        let g = Graph::build(&pages(), false);
        assert_eq!(titles(&g), vec!["Hub", "Leaf1", "Leaf2", "Island"]);
        assert_eq!(g.edges, vec![(0, 1), (0, 2)]);
        assert_eq!(g.nodes[0].backlinks, 1);
    }

    #[test]
    fn radius_is_capped() {
        let mut g = Graph::build(&pages(), true);
        g.nodes[0].backlinks = 10_000;
        assert_eq!(g.nodes[0].radius(), 20.0);
    }

    #[test]
    fn initial_layout_is_deterministic_and_distinct() {
        let a = Graph::build(&pages(), true);
        let b = Graph::build(&pages(), true);
        for (n, m) in a.nodes.iter().zip(&b.nodes) {
            assert_eq!((n.x, n.y), (m.x, m.y));
        }
        for i in 0..a.nodes.len() {
            for j in (i + 1)..a.nodes.len() {
                assert!((a.nodes[i].x - a.nodes[j].x).hypot(a.nodes[i].y - a.nodes[j].y) > 1.0);
            }
        }
    }

    /// A ring of `n` pages, each linking to the next, plus a few chords.
    fn ring(n: usize) -> Vec<Page> {
        (0..n)
            .map(|i| {
                let mut md = format!("- next [[P{}]]\n", (i + 1) % n);
                if i % 5 == 0 {
                    md.push_str(&format!("- chord [[P{}]]\n", (i + n / 2) % n));
                }
                Page::from_markdown(&format!("P{i}"), false, &md)
            })
            .collect()
    }

    fn settle(g: &mut Graph) -> u32 {
        let mut ticks = 0;
        while !g.is_settled() {
            g.tick();
            ticks += 1;
        }
        ticks
    }

    #[test]
    fn simulation_settles_within_the_cap_and_stays_finite() {
        let mut g = Graph::build(&ring(300), true);
        let ticks = settle(&mut g);
        assert!(ticks <= MAX_TICKS, "ran {ticks} ticks");
        assert!(g.nodes.iter().all(|n| n.x.is_finite() && n.y.is_finite()));
        // Further ticks do nothing once settled.
        let before: Vec<(f32, f32)> = g.nodes.iter().map(|n| (n.x, n.y)).collect();
        g.tick();
        let after: Vec<(f32, f32)> = g.nodes.iter().map(|n| (n.x, n.y)).collect();
        assert_eq!(before, after);
    }

    #[test]
    fn layout_quality_edges_near_rest_length_and_no_overlaps() {
        let mut g = Graph::build(&ring(40), true);
        settle(&mut g);

        // Connected nodes sit near the spring rest length (repulsion stretches
        // them a bit, so allow a generous band).
        let mean_edge: f32 = g
            .edges
            .iter()
            .map(|&(a, b)| (g.nodes[a].x - g.nodes[b].x).hypot(g.nodes[a].y - g.nodes[b].y))
            .sum::<f32>()
            / g.edges.len() as f32;
        assert!(
            (SPRING_REST * 0.5..SPRING_REST * 2.5).contains(&mean_edge),
            "mean edge length {mean_edge}"
        );

        // No two nodes overlap (centres further apart than both radii).
        for i in 0..g.nodes.len() {
            for j in (i + 1)..g.nodes.len() {
                let d = (g.nodes[i].x - g.nodes[j].x).hypot(g.nodes[i].y - g.nodes[j].y);
                assert!(
                    d > g.nodes[i].radius() + g.nodes[j].radius(),
                    "nodes {i},{j} overlap"
                );
            }
        }
    }

    #[test]
    fn linked_nodes_end_up_closer_than_unlinked_ones() {
        let mut g = Graph::build(&ring(30), true);
        settle(&mut g);
        let dist =
            |a: usize, b: usize| (g.nodes[a].x - g.nodes[b].x).hypot(g.nodes[a].y - g.nodes[b].y);
        let linked: f32 =
            g.edges.iter().map(|&(a, b)| dist(a, b)).sum::<f32>() / g.edges.len() as f32;
        let mut all = 0.0;
        let mut count = 0;
        for i in 0..30 {
            for j in (i + 1)..30 {
                all += dist(i, j);
                count += 1;
            }
        }
        assert!(
            linked < all / count as f32,
            "linked {linked} vs average {}",
            all / count as f32
        );
    }

    #[test]
    fn disconnected_pieces_stay_together() {
        // Several isolated pages: gravity must keep them from flying apart.
        let pages: Vec<Page> = (0..12)
            .map(|i| Page::from_markdown(&format!("I{i}"), false, "- x\n"))
            .collect();
        let mut g = Graph::build(&pages, true);
        settle(&mut g);
        let (min_x, min_y, max_x, max_y) = g.bounds();
        assert!(max_x - min_x < 1200.0 && max_y - min_y < 1200.0);
    }

    #[test]
    fn pinned_nodes_do_not_move_and_reheat_restarts_the_clock() {
        let mut g = Graph::build(&ring(20), true);
        g.nodes[3].pinned = true;
        let (x, y) = (g.nodes[3].x, g.nodes[3].y);
        settle(&mut g);
        assert_eq!((g.nodes[3].x, g.nodes[3].y), (x, y));
        assert!(g.is_settled());
        g.reheat(0.3);
        assert!(!g.is_settled());
        settle(&mut g);
        assert!(g.is_settled());
    }

    #[test]
    fn tick_cap_holds_even_if_the_temperature_never_cools() {
        let mut g = Graph::build(&ring(10), true);
        g.reheat(1e9); // absurdly hot
        let ticks = settle(&mut g);
        assert_eq!(ticks, MAX_TICKS);
    }

    #[test]
    fn coincident_nodes_are_separated() {
        let mut g = Graph::build(&ring(3), true);
        for n in &mut g.nodes {
            n.x = 5.0;
            n.y = 5.0;
        }
        settle(&mut g);
        assert!((g.nodes[0].x - g.nodes[1].x).hypot(g.nodes[0].y - g.nodes[1].y) > 1.0);
        assert!(g.nodes.iter().all(|n| n.x.is_finite()));
    }

    #[test]
    fn rebuild_preserves_positions_and_pins() {
        let mut old = Graph::build(&pages(), true);
        settle(&mut old);
        old.nodes[1].pinned = true;
        let mut new = Graph::build(&pages(), false);
        new.preserve_layout(&old);
        // Journal gone; the others kept their place.
        assert_eq!(new.nodes.len(), 4);
        for n in &new.nodes {
            let o = old.nodes.iter().find(|o| o.title == n.title).unwrap();
            assert_eq!((n.x, n.y, n.pinned), (o.x, o.y, o.pinned));
        }
        assert!(!new.is_settled(), "reheated so new layout can adapt");
    }

    #[test]
    fn hit_test_picks_the_closest_and_honours_slop() {
        let mut g = Graph::build(&pages(), true);
        for (i, n) in g.nodes.iter_mut().enumerate() {
            n.x = i as f32 * 100.0;
            n.y = 0.0;
        }
        assert_eq!(g.hit_test(100.0, 2.0, 0.0), Some(1));
        assert_eq!(g.hit_test(150.0, 0.0, 0.0), None);
        // Just outside node 1's radius: missed without slop, hit with it.
        let r = g.nodes[1].radius();
        assert_eq!(g.hit_test(100.0 + r + 3.0, 0.0, 0.0), None);
        assert_eq!(g.hit_test(100.0 + r + 3.0, 0.0, 4.0), Some(1));
        // Overlapping nodes: the nearer one wins.
        g.nodes[2].x = 104.0;
        assert_eq!(g.hit_test(103.0, 0.0, 0.0), Some(2));
    }

    #[test]
    fn view_round_trips_and_zoom_is_anchored_at_the_cursor() {
        let mut v = View {
            pan_x: 13.0,
            pan_y: -7.0,
            zoom: 1.7,
        };
        let (sx, sy) = v.to_screen(40.0, -25.0, 800.0, 600.0);
        let (wx, wy) = v.to_world(sx, sy, 800.0, 600.0);
        assert!((wx - 40.0).abs() < 1e-3 && (wy + 25.0).abs() < 1e-3);

        // The world point under the cursor stays under the cursor when zooming.
        let (cx, cy) = (250.0, 410.0);
        let before = v.to_world(cx, cy, 800.0, 600.0);
        v.zoom_at(cx, cy, 1.5, 800.0, 600.0);
        let after = v.to_world(cx, cy, 800.0, 600.0);
        assert!((before.0 - after.0).abs() < 1e-2 && (before.1 - after.1).abs() < 1e-2);

        // Zoom is clamped at both ends.
        v.zoom_at(cx, cy, 1e6, 800.0, 600.0);
        assert_eq!(v.zoom, MAX_ZOOM);
        v.zoom_at(cx, cy, 1e-9, 800.0, 600.0);
        assert_eq!(v.zoom, MIN_ZOOM);
    }

    #[test]
    fn fit_puts_every_node_inside_the_pane() {
        let mut g = Graph::build(&ring(60), true);
        settle(&mut g);
        let (vw, vh) = (900.0, 600.0);
        let mut v = View::default();
        v.fit(g.bounds(), vw, vh, 40.0);
        for n in &g.nodes {
            let (sx, sy) = v.to_screen(n.x, n.y, vw, vh);
            assert!(
                (0.0..=vw).contains(&sx) && (0.0..=vh).contains(&sy),
                "({sx},{sy}) outside"
            );
        }
        // A single node isn't blown up past the cap.
        let one = Graph::build(&ring(1), true);
        v.fit(one.bounds(), vw, vh, 40.0);
        assert!(v.zoom <= 1.4);
    }

    /// Tuning aid, not a regression test: prints layout metrics for a few
    /// graph shapes. Run with
    /// `cargo test --offline print_layout_stats -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn print_layout_stats() {
        // A hub with spokes, some of which have their own leaves.
        let mut star: Vec<Page> = vec![Page::from_markdown(
            "Hub",
            false,
            &(0..24).map(|i| format!("- [[S{i}]]\n")).collect::<String>(),
        )];
        for i in 0..24 {
            let leaves: String = (0..(i % 4)).map(|j| format!("- [[L{i}x{j}]]\n")).collect();
            star.push(Page::from_markdown(
                &format!("S{i}"),
                false,
                &format!("- back [[Hub]]\n{leaves}"),
            ));
            for j in 0..(i % 4) {
                star.push(Page::from_markdown(&format!("L{i}x{j}"), false, "- leaf\n"));
            }
        }
        for (name, pages) in [("ring30", ring(30)), ("ring300", ring(300)), ("star", star)] {
            let mut g = Graph::build(&pages, true);
            let ticks = settle(&mut g);
            let d = |a: usize, b: usize| {
                (g.nodes[a].x - g.nodes[b].x).hypot(g.nodes[a].y - g.nodes[b].y)
            };
            let edge = g.edges.iter().map(|&(a, b)| d(a, b)).sum::<f32>() / g.edges.len() as f32;
            let n = g.nodes.len();
            let mut all = 0.0;
            let mut min_gap = f32::MAX;
            for i in 0..n {
                for j in (i + 1)..n {
                    all += d(i, j);
                    min_gap = min_gap.min(d(i, j) - g.nodes[i].radius() - g.nodes[j].radius());
                }
            }
            let avg = all / (n * (n - 1) / 2) as f32;
            let (x0, y0, x1, y1) = g.bounds();
            println!(
                "{name:8} nodes={n:3} ticks={ticks:3} edge={edge:6.1} avg_pair={avg:6.1} min_gap={min_gap:6.1} size={:.0}x{:.0}",
                x1 - x0,
                y1 - y0
            );
        }
    }

    #[test]
    fn labels_are_shortened_on_char_boundaries() {
        assert_eq!(short_label("Short"), "Short");
        let long = "a very long page title that keeps going and going";
        let s = short_label(long);
        assert!(s.chars().count() <= 26 && s.ends_with('…'));
        // Multi-byte characters must not be split.
        let accented = "é".repeat(40);
        assert_eq!(short_label(&accented).chars().count(), 26);
    }

    #[test]
    fn camera_approaches_its_target_and_arrives() {
        let target = View {
            pan_x: 120.0,
            pan_y: -80.0,
            zoom: 0.5,
        };
        let mut v = View::default();
        let mut steps = 0;
        while !v.approach(&target, 0.18) {
            steps += 1;
            assert!(steps < 200, "never converged");
        }
        assert!((v.zoom - 0.5).abs() < 0.01);
        assert!((v.pan_x - 120.0).abs() < 1.0);
    }

    #[test]
    fn empty_graph_is_safe() {
        let mut g = Graph::build(&[], true);
        g.tick();
        assert!(g.nodes.is_empty());
        let mut v = View::default();
        v.fit(g.bounds(), 500.0, 400.0, 20.0);
        assert!(v.zoom.is_finite());
        assert_eq!(g.hit_test(0.0, 0.0, 5.0), None);
    }
}
