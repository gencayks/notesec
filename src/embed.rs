//! Live embeds (decision 47): a block that embeds a page or another block
//! shows that content inline, read live from the pages in memory. Pure
//! code, NO ui code here: parsing, and resolving an embed into a tree the
//! reading view (`app/embed_ui.rs`) and the HTML export both draw.
//!
//! Syntaxes, anywhere in a block's text (not in fenced code):
//!
//! - `![[Page name]]`: the whole page. The name resolves like a link
//!   (`model::resolve_page`), so a page's `alias::` names work too.
//! - `![[((block-uuid))]]`: that block with its children.
//! - `{{embed [[Page name]]}}` and `{{embed ((block-uuid))}}`: Logseq's
//!   own macro forms (what imported Logseq graphs contain), same meaning.
//!
//! The `[[Page]]` inside a page embed is an ordinary wikilink and the
//! `((id))` inside a block embed an ordinary block reference, so backlinks
//! ("Linked from"), the graph, link-target creation and `id::` saving all
//! treat an embed as the reference it contains.

use crate::model::{find_block, resolve_page, Page};
use std::ops::Range;
use uuid::Uuid;

/// How deep embeds nest inside embeds before the rest is cut off with a
/// note (the embedding block's own embeds are level 1).
pub const MAX_EMBED_DEPTH: usize = 4;

/// What an embed points at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EmbedTarget {
    /// A page, by name as written (title or alias, any case).
    Page(String),
    /// A block (with its children), by id.
    Block(Uuid),
}

/// One embed in a block's text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Embed {
    /// Byte range of the whole embed as written.
    pub range: Range<usize>,
    pub target: EmbedTarget,
}

/// The target written between `[[ ]]` (or after `embed`): a whole
/// `((uuid))` is a block, any other valid page name a page (names may hold
/// parentheses: "Rust (language)"), anything else isn't an embed.
fn target_of(inner: &str) -> Option<EmbedTarget> {
    let inner = inner.trim();
    if let Some(id) = inner.strip_prefix("((").and_then(|s| s.strip_suffix("))")) {
        return Uuid::parse_str(id.trim()).ok().map(EmbedTarget::Block);
    }
    (!inner.is_empty() && !inner.contains(['[', ']', '\n']))
        .then(|| EmbedTarget::Page(inner.to_string()))
}

/// Every embed in `text`, in order. Malformed ones (an empty name, a bad
/// uuid) aren't embeds; embeds inside fenced code are code.
pub fn parse_embeds(text: &str) -> Vec<Embed> {
    let mut out = Vec::new();
    // `![[target]]`
    let mut pos = 0;
    while let Some(start) = text[pos..].find("![[").map(|i| pos + i) {
        let inner_start = start + 3;
        let Some(len) = text[inner_start..].find("]]") else {
            break;
        };
        let inner = &text[inner_start..inner_start + len];
        match target_of(inner) {
            Some(target) => {
                out.push(Embed {
                    range: start..inner_start + len + 2,
                    target,
                });
                pos = inner_start + len + 2;
            }
            None => pos = start + 1,
        }
    }
    // `{{embed [[Page]]}}` / `{{embed ((uuid))}}`
    let mut pos = 0;
    while let Some(start) = text[pos..].find("{{").map(|i| pos + i) {
        let Some(len) = text[start + 2..].find("}}") else {
            break;
        };
        let end = start + 2 + len + 2;
        let body = text[start + 2..start + 2 + len].trim();
        let target = body
            .get(..5)
            .filter(|w| w.eq_ignore_ascii_case("embed"))
            .map(|_| body[5..].trim())
            .and_then(|arg| {
                if let Some(name) = arg.strip_prefix("[[").and_then(|a| a.strip_suffix("]]")) {
                    target_of(name).filter(|t| matches!(t, EmbedTarget::Page(_)))
                } else if arg.starts_with("((") {
                    target_of(arg).filter(|t| matches!(t, EmbedTarget::Block(_)))
                } else {
                    None
                }
            });
        match target {
            Some(target) => {
                out.push(Embed {
                    range: start..end,
                    target,
                });
                pos = end;
            }
            None => pos = start + 2,
        }
    }
    let code = crate::code::fenced_ranges(text);
    out.retain(|e| !code.iter().any(|c| c.contains(&e.range.start)));
    out.sort_by_key(|e| e.range.start);
    out
}

/// One block shown inside an embed.
#[derive(Clone, Debug, PartialEq)]
pub struct EmbedRow {
    /// The source block (clicking it edits it on its own page).
    pub id: Uuid,
    /// Depth below the embed's first row (0: top level of the embed).
    pub depth: usize,
    /// The block's text, live (the editor's text if it's being edited).
    pub content: String,
    /// The embeds in this row's own text, resolved one level deeper.
    pub embeds: Vec<Resolved>,
}

/// An embed as it shows: content, or a note saying why not.
#[derive(Clone, Debug, PartialEq)]
pub enum Resolved {
    /// A whole page: its title and every block in outline order.
    Page { title: String, rows: Vec<EmbedRow> },
    /// A block and its children; `title` is the page it lives on.
    Block { title: String, rows: Vec<EmbedRow> },
    /// The target doesn't exist (never did, renamed away, or trashed).
    Missing(String),
    /// The target is already being shown further out (A embeds B embeds A,
    /// or a page embeds itself): drawn once, then this note.
    Circular(String),
    /// Nested deeper than `MAX_EMBED_DEPTH`.
    TooDeep,
}

impl Resolved {
    /// The note's text for the three kinds of note (`None` for content).
    pub fn note(&self) -> Option<String> {
        match self {
            Resolved::Missing(what) => Some(what.clone()),
            Resolved::Circular(title) => Some(format!("Circular embed of \u{201c}{title}\u{201d}")),
            Resolved::TooDeep => Some(format!(
                "Embeds nested more than {MAX_EMBED_DEPTH} deep are not shown"
            )),
            Resolved::Page { .. } | Resolved::Block { .. } => None,
        }
    }
}

/// What is on screen further out, for the cycle check.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Shown {
    Page(usize),
    Block(Uuid),
}

/// Resolves embeds against `pages`.
pub struct Resolver<'a> {
    pages: &'a [Page],
    /// The block being edited and its unsaved text, so embeds of it follow
    /// the typing.
    live: Option<(Uuid, &'a str)>,
}

impl<'a> Resolver<'a> {
    pub fn new(pages: &'a [Page], live: Option<(Uuid, &'a str)>) -> Self {
        Resolver { pages, live }
    }

    fn content(&self, page: usize, block: usize) -> String {
        let b = &self.pages[page].blocks[block];
        match self.live {
            Some((id, text)) if id == b.id => text.to_string(),
            _ => b.content.clone(),
        }
    }

    /// The embeds in `content`, a block of page `host` (`None`: not on a
    /// page, e.g. a preview), each resolved with everything inside it.
    pub fn resolve(&self, content: &str, host: Option<usize>) -> Vec<Resolved> {
        let mut shown: Vec<Shown> = host.map(Shown::Page).into_iter().collect();
        self.resolve_in(content, &mut shown, 1)
    }

    fn resolve_in(&self, content: &str, shown: &mut Vec<Shown>, level: usize) -> Vec<Resolved> {
        parse_embeds(content)
            .into_iter()
            .map(|embed| self.one(&embed.target, shown, level))
            .collect()
    }

    fn one(&self, target: &EmbedTarget, shown: &mut Vec<Shown>, level: usize) -> Resolved {
        let (key, title, range) = match target {
            EmbedTarget::Page(name) => match resolve_page(self.pages, name) {
                Some(p) => (
                    Shown::Page(p),
                    self.pages[p].title.clone(),
                    0..self.pages[p].blocks.len(),
                ),
                None => return Resolved::Missing(format!("Page \u{201c}{name}\u{201d} not found")),
            },
            EmbedTarget::Block(id) => match find_block(self.pages, *id) {
                Some((p, b)) => (
                    Shown::Block(*id),
                    self.pages[p].title.clone(),
                    b..self.pages[p].subtree_end(b),
                ),
                None => return Resolved::Missing("Block not found".to_string()),
            },
        };
        if shown.contains(&key) {
            return Resolved::Circular(title);
        }
        if level > MAX_EMBED_DEPTH {
            return Resolved::TooDeep;
        }
        let page = match key {
            Shown::Page(p) => p,
            Shown::Block(id) => find_block(self.pages, id).map_or(0, |(p, _)| p),
        };
        shown.push(key);
        let base = range
            .clone()
            .next()
            .map_or(0, |b| self.pages[page].depth_of(b));
        let rows = range
            .map(|b| {
                let content = self.content(page, b);
                let embeds = self.resolve_in(&content, shown, level + 1);
                EmbedRow {
                    id: self.pages[page].blocks[b].id,
                    depth: self.pages[page].depth_of(b).saturating_sub(base),
                    content,
                    embeds,
                }
            })
            .collect();
        shown.pop();
        match key {
            Shown::Page(_) => Resolved::Page { title, rows },
            Shown::Block(_) => Resolved::Block { title, rows },
        }
    }
}

/// Ids of the blocks that `text` embeds (for the graph's edges).
pub fn embedded_blocks(text: &str) -> Vec<Uuid> {
    parse_embeds(text)
        .into_iter()
        .filter_map(|e| match e.target {
            EmbedTarget::Block(id) => Some(id),
            EmbedTarget::Page(_) => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{backlinks, parse_block_refs, parse_wikilinks};

    const ID: &str = "6f9b2c1e-0000-4000-8000-0000000000e1";
    const ID2: &str = "6f9b2c1e-0000-4000-8000-0000000000e2";

    fn uuid(s: &str) -> Uuid {
        Uuid::parse_str(s).unwrap()
    }

    fn targets(text: &str) -> Vec<EmbedTarget> {
        parse_embeds(text).into_iter().map(|e| e.target).collect()
    }

    fn page(name: &str) -> EmbedTarget {
        EmbedTarget::Page(name.to_string())
    }

    fn page_of(title: &str, md: &str) -> Page {
        Page::from_markdown(title, false, md)
    }

    /// Rows of a resolved embed as "depth:content".
    fn rows(r: &Resolved) -> Vec<String> {
        match r {
            Resolved::Page { rows, .. } | Resolved::Block { rows, .. } => rows
                .iter()
                .map(|r| format!("{}:{}", r.depth, r.content))
                .collect(),
            other => panic!("not content: {other:?}"),
        }
    }

    #[test]
    fn parses_page_embeds() {
        let text = "a ![[Beta]] b ![[ Rust (language) ]]";
        let embeds = parse_embeds(text);
        assert_eq!(embeds.len(), 2);
        assert_eq!(&text[embeds[0].range.clone()], "![[Beta]]");
        assert_eq!(embeds[0].target, page("Beta"));
        assert_eq!(embeds[1].target, page("Rust (language)"));
    }

    #[test]
    fn parses_block_embeds() {
        let text = format!("![[(({ID}))]] and ![[ (( {ID2} )) ]]");
        assert_eq!(
            targets(&text),
            vec![EmbedTarget::Block(uuid(ID)), EmbedTarget::Block(uuid(ID2))]
        );
        assert_eq!(parse_embeds(&text)[0].range, 0..ID.len() + 9);
    }

    #[test]
    fn parses_logseq_embed_macros() {
        let text = format!("{{{{embed [[Beta]]}}}} x {{{{ EMBED  (({ID})) }}}}");
        assert_eq!(
            targets(&text),
            vec![page("Beta"), EmbedTarget::Block(uuid(ID))]
        );
        assert_eq!(
            &text[parse_embeds(&text)[0].range.clone()],
            "{{embed [[Beta]]}}"
        );
    }

    #[test]
    fn rejects_malformed_embeds_links_and_code() {
        for text in [
            "[[Beta]]",
            "!Beta",
            "![[]]",
            "![[  ]]",
            "![[((not-a-uuid))]]",
            "![[Beta",
            "{{embed Beta}}",
            "{{query #x}}",
            "{{embed [[]]}}",
            "{{embed ((nope))}}",
            "```\n![[Beta]]\n```",
        ] {
            assert_eq!(targets(text), vec![], "{text}");
        }
        // Mixed in order with a bad one before it.
        assert_eq!(
            targets(&format!("![[((x))]] {{{{embed (({ID}))}}}} ![[A]]")),
            vec![EmbedTarget::Block(uuid(ID)), page("A")]
        );
    }

    #[test]
    fn block_embeds_are_block_refs_not_page_links() {
        let text = format!("![[(({ID}))]] and ![[Beta]]");
        let links: Vec<String> = parse_wikilinks(&text)
            .into_iter()
            .map(|l| l.target)
            .collect();
        assert_eq!(links, vec!["Beta"]);
        let refs: Vec<Uuid> = parse_block_refs(&text)
            .into_iter()
            .map(|(_, id)| id)
            .collect();
        assert_eq!(refs, vec![uuid(ID)]);
    }

    #[test]
    fn page_embed_shows_the_whole_outline_live() {
        let mut pages = vec![
            page_of("Alpha", "- ![[Beta]]\n"),
            page_of("Beta", "- one\n  - child\n- two\n"),
        ];
        let r = Resolver::new(&pages, None).resolve("![[Beta]]", Some(0));
        assert_eq!(r.len(), 1);
        assert!(matches!(&r[0], Resolved::Page { title, .. } if title == "Beta"));
        assert_eq!(rows(&r[0]), vec!["0:one", "1:child", "0:two"]);
        // An edit to the source shows on the next resolve.
        pages[1].blocks[1].content = "child edited".into();
        let r = Resolver::new(&pages, None).resolve("![[Beta]]", Some(0));
        assert_eq!(rows(&r[0]), vec!["0:one", "1:child edited", "0:two"]);
        // And the block being edited shows its unsaved text.
        let live = Some((pages[1].blocks[2].id, "two typing"));
        let r = Resolver::new(&pages, live).resolve("![[Beta]]", Some(0));
        assert_eq!(rows(&r[0]), vec!["0:one", "1:child edited", "0:two typing"]);
    }

    #[test]
    fn block_embed_shows_the_block_and_its_children() {
        let pages = vec![
            page_of("Alpha", "- x\n"),
            page_of(
                "Beta",
                &format!("- before\n- plan\n  id:: {ID}\n  - step 1\n    - detail\n  - step 2\n- after\n"),
            ),
        ];
        let r = Resolver::new(&pages, None).resolve(&format!("![[(({ID}))]]"), Some(0));
        assert!(matches!(&r[0], Resolved::Block { title, .. } if title == "Beta"));
        assert_eq!(
            rows(&r[0]),
            vec!["0:plan", "1:step 1", "2:detail", "1:step 2"]
        );
        if let Resolved::Block { rows, .. } = &r[0] {
            assert_eq!(rows[0].id, uuid(ID));
        }
    }

    #[test]
    fn nested_embeds_resolve_inside_rows() {
        let pages = vec![
            page_of("A", "- ![[B]]\n"),
            page_of("B", "- b says ![[C]]\n"),
            page_of("C", "- c content\n"),
        ];
        let r = Resolver::new(&pages, None).resolve("![[B]]", Some(0));
        let Resolved::Page { rows, .. } = &r[0] else {
            panic!()
        };
        assert_eq!(rows[0].embeds.len(), 1);
        assert_eq!(super::tests::rows(&rows[0].embeds[0]), vec!["0:c content"]);
    }

    #[test]
    fn cycles_become_a_note() {
        let pages = vec![
            page_of("A", "- ![[B]]\n- ![[A]]\n"),
            page_of("B", "- back to ![[a]]\n"),
        ];
        let resolver = Resolver::new(&pages, None);
        // A page embedding itself.
        let r = resolver.resolve("![[A]]", Some(0));
        assert_eq!(r, vec![Resolved::Circular("A".into())]);
        assert_eq!(r[0].note().unwrap(), "Circular embed of \u{201c}A\u{201d}");
        // A embeds B embeds A: B is drawn once, then the note.
        let r = resolver.resolve("![[B]]", Some(0));
        let Resolved::Page { rows, .. } = &r[0] else {
            panic!()
        };
        assert_eq!(rows[0].embeds, vec![Resolved::Circular("A".into())]);
        // A block embedding itself (or its ancestor) too.
        let pages = vec![page_of(
            "P",
            &format!("- top\n  id:: {ID}\n  - child ![[(({ID}))]]\n"),
        )];
        let r = Resolver::new(&pages, None).resolve(&format!("![[(({ID}))]]"), None);
        let Resolved::Block { rows, .. } = &r[0] else {
            panic!()
        };
        assert_eq!(rows[1].embeds, vec![Resolved::Circular("P".into())]);
    }

    #[test]
    fn nesting_stops_at_the_depth_cap() {
        // P0 embeds P1 embeds P2 ... P6: no cycle, just deep.
        let pages: Vec<Page> = (0..7)
            .map(|i| page_of(&format!("P{i}"), &format!("- level {i} ![[P{}]]\n", i + 1)))
            .collect();
        let r = Resolver::new(&pages, None).resolve("![[P1]]", Some(0));
        let mut level = &r[0];
        let mut shown = 0;
        while let Resolved::Page { rows, .. } = level {
            shown += 1;
            level = &rows[0].embeds[0];
        }
        assert_eq!(shown, MAX_EMBED_DEPTH);
        assert_eq!(level, &Resolved::TooDeep);
        assert!(level.note().unwrap().contains("nested more than 4 deep"));
    }

    #[test]
    fn missing_and_trashed_targets_are_notes() {
        // A trashed page is no longer in `pages` (it lives in the trash).
        let pages = vec![page_of("A", "- x\n")];
        let r = Resolver::new(&pages, None).resolve(
            &format!("![[Gone]] ![[(({ID}))]] {{{{embed [[Nope]]}}}}"),
            Some(0),
        );
        assert_eq!(
            r,
            vec![
                Resolved::Missing("Page \u{201c}Gone\u{201d} not found".into()),
                Resolved::Missing("Block not found".into()),
                Resolved::Missing("Page \u{201c}Nope\u{201d} not found".into()),
            ]
        );
    }

    #[test]
    fn aliases_and_case_resolve_like_links() {
        let pages = vec![
            page_of("Host", "- x\n"),
            page_of("JavaScript", "alias:: JS, ecmascript\n\n- the language\n"),
        ];
        let resolver = Resolver::new(&pages, None);
        for text in ["![[JS]]", "![[javascript]]", "{{embed [[ECMAScript]]}}"] {
            let r = resolver.resolve(text, Some(0));
            assert!(
                matches!(&r[0], Resolved::Page { title, .. } if title == "JavaScript"),
                "{text}: {r:?}"
            );
        }
    }

    #[test]
    fn embeds_count_as_backlinks_and_graph_edges() {
        let pages = vec![
            page_of("Beta", &format!("- plan\n  id:: {ID}\n")),
            page_of("Gamma", "alias:: G\n\n- g\n"),
            page_of("Host", &format!("- ![[(({ID}))]]\n- {{{{embed [[G]]}}}}\n")),
        ];
        let from = |target: usize| -> Vec<(usize, Vec<usize>)> {
            backlinks(&pages, target)
                .into_iter()
                .map(|g| (g.page, g.blocks))
                .collect()
        };
        assert_eq!(from(0), vec![(2, vec![0])]);
        assert_eq!(from(1), vec![(2, vec![1])]);
        let graph = crate::graph::Graph::build(&pages, true);
        let node = |t: &str| graph.nodes.iter().position(|n| n.title == t).unwrap();
        let (beta, gamma, host) = (node("Beta"), node("Gamma"), node("Host"));
        assert!(graph.edges.contains(&(beta.min(host), beta.max(host))));
        assert!(graph.edges.contains(&(gamma.min(host), gamma.max(host))));
        assert_eq!(graph.nodes[beta].backlinks, 1);
        assert_eq!(graph.nodes[gamma].backlinks, 1);
        assert_eq!(graph.edges.len(), 2);
    }

    #[test]
    fn search_and_agenda_read_only_the_source() {
        let pages = vec![
            page_of("Source", "- TODO unique needle task\n"),
            page_of("Host", "- ![[Source]]\n"),
        ];
        let hits = crate::search::search_text(&pages, "needle", 10);
        assert_eq!(hits.len(), 1);
        assert!(matches!(
            hits[0].target,
            crate::search::Target::Match { page: 0, .. }
        ));
        let today = chrono::NaiveDate::from_ymd_opt(2026, 10, 9).unwrap();
        let agenda = crate::agenda::Agenda::build(&pages, today);
        assert_eq!(agenda.unscheduled.len(), 1);
        assert_eq!(agenda.unscheduled[0].page, "Source");
    }
}
