//! Core data model: pages and blocks, plus Logseq-compatible markdown
//! parsing/serialising.
//!
//! A page is stored on disk as a markdown outline:
//!
//! ```text
//! - parent block
//!   - child block
//!     - grandchild
//! - another top-level block
//! ```
//!
//! In memory a page keeps its blocks in a *flat* `Vec` in document order. The
//! tree structure is encoded by each block's `parent_id`, so rendering a block's
//! indent is just "count how many ancestors it has".

use std::collections::HashSet;
use std::ops::Range;
use uuid::Uuid;

/// Every well-formed block reference `((<uuid>))` in `text`, in order, with
/// the byte range of the whole reference (parentheses included).
pub fn parse_block_refs(text: &str) -> Vec<(Range<usize>, Uuid)> {
    let mut refs = Vec::new();
    let mut from = 0;
    while let Some(start) = text[from..].find("((").map(|i| from + i) {
        let inner = start + 2;
        let parsed = text[inner..]
            .find("))")
            .and_then(|len| Some((Uuid::parse_str(&text[inner..inner + len]).ok()?, len)));
        match parsed {
            Some((id, len)) => {
                let end = inner + len + 2;
                refs.push((start..end, id));
                from = end;
            }
            None => from = start + 1,
        }
    }
    refs
}

/// A `{{query #tag}}` (or `{{query #[[multi word]]}}`) macro in `text`:
/// its byte range and the tag it asks for. Only the first one counts.
pub fn parse_query(text: &str) -> Option<(Range<usize>, String)> {
    let start = text.find("{{query")?;
    let inner_start = start + "{{query".len();
    let len = text[inner_start..].find("}}")?;
    let inner = &text[inner_start..inner_start + len];
    let tag = inner.trim();
    // The whole inside must be exactly one tag.
    let refs = parse_references(tag);
    match refs.as_slice() {
        [r] if r.is_tag && r.range == (0..tag.len()) => {
            Some((start..inner_start + len + 2, r.target.clone()))
        }
        _ => None,
    }
}

/// Every block tagged `#tag` (or `#[[tag]]`), in page then document order,
/// as `(page, block)`. Matching ignores case, like page names. The tag
/// inside a block's own `{{query ...}}` doesn't count, so a query never
/// lists itself.
pub fn tag_query(pages: &[Page], tag: &str) -> Vec<(usize, usize)> {
    let wanted = tag.to_lowercase();
    let mut hits = Vec::new();
    for (page_ix, page) in pages.iter().enumerate() {
        for (block_ix, block) in page.blocks.iter().enumerate() {
            let skip = parse_query(&block.content).map(|(range, _)| range);
            let tagged = parse_references(&block.content).iter().any(|r| {
                r.is_tag
                    && r.target.to_lowercase() == wanted
                    && !skip
                        .as_ref()
                        .is_some_and(|s| s.start <= r.range.start && r.range.end <= s.end)
            });
            if tagged {
                hits.push((page_ix, block_ix));
            }
        }
    }
    hits
}

/// Where the block with `id` is: `(page index, block index)`.
pub fn find_block(pages: &[Page], id: Uuid) -> Option<(usize, usize)> {
    pages
        .iter()
        .enumerate()
        .find_map(|(p, page)| page.blocks.iter().position(|b| b.id == id).map(|b| (p, b)))
}

/// The `id:: <uuid>` property line that keeps a referenced block's id
/// stable on disk, if `line` (already trimmed) is one.
fn id_property(line: &str) -> Option<Uuid> {
    Uuid::parse_str(line.strip_prefix("id::")?.trim()).ok()
}

/// A reference to a page found inside a block's text: either a
/// `[[wikilink]]` or a `#tag`. Both point at the page named `target`.
#[derive(Clone, Debug, PartialEq)]
pub struct Reference {
    /// Byte range of the whole reference as written (`[[...]]`, `#tag` or
    /// `#[[...]]`), brackets and `#` included.
    pub range: Range<usize>,
    /// The page name, trimmed and without brackets or `#`.
    pub target: String,
    /// True for `#tag` / `#[[tag]]`, false for a plain `[[wikilink]]`.
    pub is_tag: bool,
}

/// Find every well-formed `[[name]]` in `text`, in order.
///
/// A link needs a non-empty name that contains no `[`, `]` or newline. In
/// `[[a [[b]]` only the inner `[[b]]` counts.
pub fn parse_wikilinks(text: &str) -> Vec<Reference> {
    let mut links = Vec::new();
    let mut pos = 0;
    while let Some(offset) = text[pos..].find("[[") {
        let start = pos + offset;
        let inner_start = start + 2;
        // No closing `]]` anywhere after this point: nothing more to find.
        let Some(len) = text[inner_start..].find("]]") else {
            break;
        };
        let end = inner_start + len;
        let inner = &text[inner_start..end];
        if inner.contains(['[', ']', '\n']) || inner.trim().is_empty() {
            // Not a valid link; resume scanning just after this `[`.
            pos = start + 1;
            continue;
        }
        links.push(Reference {
            range: start..end + 2,
            target: inner.trim().to_string(),
            is_tag: false,
        });
        pos = end + 2;
    }
    // Links inside fenced code are code, not links (the graph reads this
    // directly).
    let code = crate::code::fenced_ranges(text);
    links.retain(|l| !code.iter().any(|c| c.contains(&l.range.start)));
    links
}

/// Characters allowed in a bare `#tag`. Punctuation such as `,` `.` `!` ends it.
fn is_tag_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '/')
}

/// If a tag starts at the `#` at byte `pos`, return where it ends and its name.
fn tag_at(text: &str, pos: usize) -> Option<(usize, String)> {
    let rest = &text[pos + 1..];
    if let Some(inner) = rest.strip_prefix("[[") {
        // `#[[multi word tag]]`
        let len = inner.find("]]")?;
        let name = &inner[..len];
        if name.contains(['[', ']', '\n']) || name.trim().is_empty() {
            return None;
        }
        return Some((pos + 1 + 2 + len + 2, name.trim().to_string()));
    }
    // Bare `#tag`: run of tag characters. Trailing `-` or `/` are dropped so
    // "#todo-" is the tag `todo`.
    let len = rest
        .char_indices()
        .find(|&(_, c)| !is_tag_char(c))
        .map_or(rest.len(), |(i, _)| i);
    let name = rest[..len].trim_end_matches(['-', '/']);
    // Pure numbers (`#1`, `#42`) are issue references, not tags.
    if name.is_empty() || name.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some((pos + 1 + name.len(), name.to_string()))
}

/// Find every `#tag` and `#[[multi word tag]]` in `text`.
///
/// A `#` only starts a tag at the start of the text or after whitespace or one
/// of `( [ { , ;`. That keeps URL fragments (`page.html#top`), headings
/// (`# Title`, `##x`) and words like `C#` from being treated as tags.
/// `skip` lists byte ranges (wikilinks) that tags must not start inside.
fn parse_tags(text: &str, skip: &[Range<usize>]) -> Vec<Reference> {
    let mut tags = Vec::new();
    let mut pos = 0;
    let mut prev: Option<char> = None;
    while pos < text.len() {
        let c = text[pos..].chars().next().unwrap_or('\0');
        let boundary = prev.map_or(true, |p| p.is_whitespace() || "([{,;".contains(p));
        if c == '#' && boundary && !skip.iter().any(|r| r.contains(&pos)) {
            if let Some((end, target)) = tag_at(text, pos) {
                tags.push(Reference {
                    range: pos..end,
                    target,
                    is_tag: true,
                });
                prev = text[..end].chars().last();
                pos = end;
                continue;
            }
        }
        prev = Some(c);
        pos += c.len_utf8();
    }
    tags
}

/// Every page reference in `text` (wikilinks and tags), in text order.
pub fn parse_references(text: &str) -> Vec<Reference> {
    let links = parse_wikilinks(text);
    let skip: Vec<Range<usize>> = links.iter().map(|l| l.range.clone()).collect();
    let tags = parse_tags(text, &skip);
    // `#[[x]]` is found by both parsers; the tag (which includes the `#`)
    // wins, so drop any link lying inside a tag.
    let mut all: Vec<Reference> = links
        .into_iter()
        .filter(|l| {
            !tags
                .iter()
                .any(|t| t.range.start <= l.range.start && l.range.end <= t.range.end)
        })
        .chain(tags.iter().cloned())
        .collect();
    // Nothing inside a fenced code block is a reference (`#include`,
    // bash's `[[ -f x ]]`).
    let code = crate::code::fenced_ranges(text);
    all.retain(|r| !code.iter().any(|c| c.contains(&r.range.start)));
    all.sort_by_key(|r| r.range.start);
    all
}

/// How many blocks use each tag, as `(name, count)`, most used first.
///
/// Names compare case-insensitively; the spelling shown is the first one seen.
/// A tag repeated within one block counts once.
pub fn tag_counts(pages: &[Page]) -> Vec<(String, usize)> {
    // lowercase name -> (display name, blocks using it)
    let mut counts: std::collections::HashMap<String, (String, usize)> = Default::default();
    for page in pages {
        for block in &page.blocks {
            let mut seen = std::collections::HashSet::new();
            for r in parse_references(&block.content)
                .into_iter()
                .filter(|r| r.is_tag)
            {
                let key = r.target.to_lowercase();
                if seen.insert(key.clone()) {
                    counts.entry(key).or_insert((r.target, 0)).1 += 1;
                }
            }
        }
    }
    let mut out: Vec<(String, usize)> = counts.into_values().collect();
    out.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
    });
    out
}

/// All blocks on one page that link to the page being viewed.
#[derive(Debug, PartialEq)]
pub struct BacklinkGroup {
    /// Index into the `pages` slice of the page the links come from.
    pub page: usize,
    /// Indices (into that page's `blocks`) of the blocks containing a link.
    pub blocks: Vec<usize>,
}

/// Find every block, on any *other* page, that contains `[[title]]`.
///
/// Title matching ignores case, like page lookup does. Groups come back in the
/// order of `pages`, and blocks in document order. A page's links to itself
/// are not backlinks and are skipped.
pub fn backlinks(pages: &[Page], title: &str) -> Vec<BacklinkGroup> {
    let wanted = title.to_lowercase();
    let mut groups = Vec::new();
    for (page_ix, page) in pages.iter().enumerate() {
        if page.title.to_lowercase() == wanted {
            continue;
        }
        let blocks: Vec<usize> = page
            .blocks
            .iter()
            .enumerate()
            .filter(|(_, b)| {
                parse_references(&b.content)
                    .iter()
                    .any(|r| r.target.to_lowercase() == wanted)
            })
            .map(|(i, _)| i)
            .collect();
        if !blocks.is_empty() {
            groups.push(BacklinkGroup {
                page: page_ix,
                blocks,
            });
        }
    }
    groups
}

/// The type of a block, expressed the way Logseq stores it: as a markdown
/// prefix at the start of the block's text (`# Title`, `> quote`). There is
/// no separate field, so the disk format stays plain Logseq markdown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockKind {
    Text,
    Heading1,
    Heading2,
    Heading3,
    Quote,
}

impl BlockKind {
    /// Every kind, in the order the "/" menu lists them.
    pub const ALL: [BlockKind; 5] = [
        BlockKind::Text,
        BlockKind::Heading1,
        BlockKind::Heading2,
        BlockKind::Heading3,
        BlockKind::Quote,
    ];

    /// Name shown in the "/" menu (and matched by its filter).
    pub fn label(self) -> &'static str {
        match self {
            BlockKind::Text => "Text",
            BlockKind::Heading1 => "Heading 1",
            BlockKind::Heading2 => "Heading 2",
            BlockKind::Heading3 => "Heading 3",
            BlockKind::Quote => "Quote",
        }
    }

    /// The markdown prefix that marks this kind (empty for plain text).
    pub fn prefix(self) -> &'static str {
        match self {
            BlockKind::Text => "",
            BlockKind::Heading1 => "# ",
            BlockKind::Heading2 => "## ",
            BlockKind::Heading3 => "### ",
            BlockKind::Quote => "> ",
        }
    }

    /// Split block `content` into its kind and the text after the prefix.
    ///
    /// Only an exact prefix counts: `#tag` and `####x` are plain text.
    pub fn parse(content: &str) -> (BlockKind, &str) {
        // Longest heading prefix first, so `## x` isn't read as `#` + `# x`.
        for kind in [
            BlockKind::Heading3,
            BlockKind::Heading2,
            BlockKind::Heading1,
            BlockKind::Quote,
        ] {
            if let Some(body) = content.strip_prefix(kind.prefix()) {
                return (kind, body);
            }
        }
        (BlockKind::Text, content)
    }

    /// `content` converted to this kind: any existing prefix is replaced and
    /// the text after it is kept.
    pub fn apply(self, content: &str) -> String {
        let (_, body) = BlockKind::parse(content);
        format!("{}{}", self.prefix(), body)
    }
}

/// Task state of a block, written as a Logseq keyword at the start of the
/// text, after any heading prefix: `TODO buy milk`, `## DOING write`,
/// `DONE x`. Logseq's markdown format uses these keywords (not `[ ]`/`[x]`);
/// see `frontend/util/marker.cljs` (`marker-pattern`, `cycle-marker-state`).
/// `LATER`/`NOW` are Logseq's other workflow and are kept as written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskState {
    Todo,
    Doing,
    Done,
    Later,
    Now,
}

impl TaskState {
    const ALL: [TaskState; 5] = [
        TaskState::Todo,
        TaskState::Doing,
        TaskState::Done,
        TaskState::Later,
        TaskState::Now,
    ];

    pub fn keyword(self) -> &'static str {
        match self {
            TaskState::Todo => "TODO",
            TaskState::Doing => "DOING",
            TaskState::Done => "DONE",
            TaskState::Later => "LATER",
            TaskState::Now => "NOW",
        }
    }

    /// Ctrl+Enter / checkbox order, as in Logseq's `cycle-marker-state`:
    /// none -> TODO -> DOING -> DONE -> none, and LATER -> NOW -> DONE.
    pub fn next(state: Option<TaskState>) -> Option<TaskState> {
        match state {
            None => Some(TaskState::Todo),
            Some(TaskState::Todo) => Some(TaskState::Doing),
            Some(TaskState::Doing) | Some(TaskState::Now) => Some(TaskState::Done),
            Some(TaskState::Later) => Some(TaskState::Now),
            Some(TaskState::Done) => None,
        }
    }

    /// In progress (drawn as a half-filled box).
    pub fn is_started(self) -> bool {
        matches!(self, TaskState::Doing | TaskState::Now)
    }

    /// Split a leading keyword off `text`. The keyword must be the whole
    /// text or be followed by a space (`TODOS` is not a task); one space
    /// after it belongs to the marker.
    pub fn parse(text: &str) -> (Option<TaskState>, &str) {
        for state in TaskState::ALL {
            if let Some(rest) = text.strip_prefix(state.keyword()) {
                if rest.is_empty() {
                    return (Some(state), rest);
                }
                if let Some(body) = rest.strip_prefix(' ') {
                    return (Some(state), body);
                }
            }
        }
        (None, text)
    }
}

/// Where the parts of block `content` start: the task marker (right after the
/// block-type prefix) and the text after the marker. Without a marker both
/// are the same offset.
pub fn task_split(content: &str) -> (usize, Option<TaskState>, usize) {
    let (_, after_kind) = BlockKind::parse(content);
    let marker = content.len() - after_kind.len();
    let (task, body) = TaskState::parse(after_kind);
    (marker, task, content.len() - body.len())
}

/// `content` with its task state moved to the next one (see
/// [`TaskState::next`]), keeping the type prefix and the text.
pub fn cycle_task(content: &str) -> String {
    let (marker, task, body) = task_split(content);
    let (prefix, text) = (&content[..marker], &content[body..]);
    match TaskState::next(task) {
        Some(next) => format!("{prefix}{} {text}", next.keyword()),
        None => format!("{prefix}{text}"),
    }
}

/// True for titles shaped like `YYYY-MM-DD` (daily journal pages).
pub fn is_journal_title(title: &str) -> bool {
    chrono::NaiveDate::parse_from_str(title, "%Y-%m-%d").is_ok()
}

/// One bullet in the outline.
#[derive(Clone, Debug, PartialEq)]
pub struct Block {
    pub id: Uuid,
    /// Markdown text of the block (may contain `\n` for continuation lines).
    pub content: String,
    /// `None` for top-level blocks.
    pub parent_id: Option<Uuid>,
    /// Title of the page this block lives on (pages are identified by title).
    pub page_id: String,
    /// Position among siblings that share the same parent (0-based).
    pub order: usize,
}

/// A page: a title plus its blocks in document order.
#[derive(Clone, Debug)]
pub struct Page {
    /// Unique identifier; we use the title (case-sensitive for now).
    pub id: String,
    pub title: String,
    pub blocks: Vec<Block>,
    /// True for daily journal pages (`YYYY-MM-DD`).
    pub is_journal: bool,
    /// Blocks whose id is written to the file (as an `id::` line under the
    /// bullet) because something references them. Every other block gets a
    /// fresh id on each load, which keeps the markdown clean.
    pub saved_ids: HashSet<Uuid>,
}

impl Page {
    pub fn new(title: &str, is_journal: bool) -> Self {
        Page {
            id: title.to_string(),
            title: title.to_string(),
            blocks: Vec::new(),
            is_journal,
            saved_ids: HashSet::new(),
        }
    }

    /// Give the page a new title. Pages are identified by title, so its id
    /// and every block's `page_id` change with it.
    pub fn rename(&mut self, title: &str) {
        self.id = title.to_string();
        self.title = title.to_string();
        for block in &mut self.blocks {
            block.page_id = title.to_string();
        }
    }

    /// A new page containing a single empty block, so there is something to
    /// click into. Used when a `[[link]]` points at a page that doesn't exist.
    pub fn with_empty_block(title: &str) -> Self {
        let mut page = Page::new(title, is_journal_title(title));
        page.blocks.push(Block {
            id: Uuid::new_v4(),
            content: String::new(),
            parent_id: None,
            page_id: page.id.clone(),
            order: 0,
        });
        page
    }

    /// Nesting depth of the block at `index` (0 = top level).
    ///
    /// Walks up the `parent_id` chain. Pages are small, so a linear lookup per
    /// ancestor is fine.
    pub fn depth_of(&self, index: usize) -> usize {
        let mut depth = 0;
        let mut parent = self.blocks[index].parent_id;
        while let Some(pid) = parent {
            depth += 1;
            parent = self
                .blocks
                .iter()
                .find(|b| b.id == pid)
                .and_then(|b| b.parent_id);
        }
        depth
    }

    /// Index one past the last descendant of the block at `index`, i.e. the
    /// end of its subtree in the flat list.
    pub fn subtree_end(&self, index: usize) -> usize {
        let depth = self.depth_of(index);
        let mut end = index + 1;
        while end < self.blocks.len() && self.depth_of(end) > depth {
            end += 1;
        }
        end
    }

    /// Number of descendants (children, grandchildren, ...) of block `index`.
    pub fn descendant_count(&self, index: usize) -> usize {
        self.subtree_end(index) - index - 1
    }

    /// Ids of the ancestors of block `index`, nearest first.
    pub fn ancestor_ids(&self, index: usize) -> Vec<Uuid> {
        let mut ids = Vec::new();
        let mut parent = self.blocks[index].parent_id;
        while let Some(pid) = parent {
            ids.push(pid);
            parent = self
                .blocks
                .iter()
                .find(|b| b.id == pid)
                .and_then(|b| b.parent_id);
        }
        ids
    }

    /// Which blocks are shown when the blocks whose ids are in `collapsed`
    /// are folded: a block is hidden when any of its ancestors is collapsed.
    pub fn visible_blocks(&self, collapsed: &std::collections::HashSet<Uuid>) -> Vec<bool> {
        let mut visible = Vec::with_capacity(self.blocks.len());
        // Depth of the collapsed block whose subtree we are inside, if any.
        let mut folded_at: Option<usize> = None;
        for (i, block) in self.blocks.iter().enumerate() {
            let depth = self.depth_of(i);
            if folded_at.is_some_and(|d| depth > d) {
                visible.push(false);
                continue;
            }
            visible.push(true);
            folded_at = collapsed.contains(&block.id).then_some(depth);
        }
        visible
    }

    /// Recompute every block's `order` from document order. Called after any
    /// structural change so `order` never goes stale.
    pub fn renumber(&mut self) {
        let mut counts: std::collections::HashMap<Option<Uuid>, usize> = Default::default();
        for block in &mut self.blocks {
            let n = counts.entry(block.parent_id).or_insert(0);
            block.order = *n;
            *n += 1;
        }
    }

    /// Enter: insert a new block with `content` right after block `index` and
    /// return its index. If the block has children the new block becomes its
    /// first child (as in Logseq); otherwise it is a sibling just below.
    pub fn insert_after(&mut self, index: usize, content: String) -> usize {
        let has_children = self.subtree_end(index) > index + 1;
        let (parent_id, at) = if has_children {
            (Some(self.blocks[index].id), index + 1)
        } else {
            (self.blocks[index].parent_id, index + 1)
        };
        self.blocks.insert(
            at,
            Block {
                id: Uuid::new_v4(),
                content,
                parent_id,
                page_id: self.id.clone(),
                order: 0,
            },
        );
        self.renumber();
        at
    }

    /// Add a top-level block with `content` at the end of the page and
    /// return its index.
    pub fn push_block(&mut self, content: String) -> usize {
        self.blocks.push(Block {
            id: Uuid::new_v4(),
            content,
            parent_id: None,
            page_id: self.id.clone(),
            order: 0,
        });
        self.renumber();
        self.blocks.len() - 1
    }

    /// Enter on a collapsed block: insert a new sibling with `content` right
    /// after the block's whole subtree (as Logseq does) and return its index.
    pub fn insert_after_subtree(&mut self, index: usize, content: String) -> usize {
        let at = self.subtree_end(index);
        self.blocks.insert(
            at,
            Block {
                id: Uuid::new_v4(),
                content,
                parent_id: self.blocks[index].parent_id,
                page_id: self.id.clone(),
                order: 0,
            },
        );
        self.renumber();
        at
    }

    /// Tab: make the block a child of its previous sibling. Fails (returns
    /// false) for the first child, which has nothing to nest under.
    pub fn indent(&mut self, index: usize) -> bool {
        let parent = self.blocks[index].parent_id;
        let order = self.blocks[index].order;
        if order == 0 {
            return false;
        }
        // The previous sibling is the block with the same parent and order-1.
        let Some(prev) = self
            .blocks
            .iter()
            .find(|b| b.parent_id == parent && b.order + 1 == order)
            .map(|b| b.id)
        else {
            return false;
        };
        // Document order is unchanged: the block already sits directly after
        // the previous sibling's subtree, so it just becomes that subtree's
        // last child. Its own children come along automatically.
        self.blocks[index].parent_id = Some(prev);
        self.renumber();
        true
    }

    /// Shift-Tab: make the block a sibling placed right after its parent.
    /// Later siblings become the block's children so document order is kept
    /// (this matches Logseq). Fails for top-level blocks.
    pub fn outdent(&mut self, index: usize) -> bool {
        let Some(parent_id) = self.blocks[index].parent_id else {
            return false;
        };
        let id = self.blocks[index].id;
        let order = self.blocks[index].order;
        let grandparent = self
            .blocks
            .iter()
            .find(|b| b.id == parent_id)
            .and_then(|b| b.parent_id);
        for b in &mut self.blocks {
            if b.parent_id == Some(parent_id) && b.order > order {
                b.parent_id = Some(id);
            }
        }
        self.blocks[index].parent_id = grandparent;
        self.renumber();
        true
    }

    /// Move block `from` together with its whole subtree so it lands just
    /// before block `before`, as `before`'s sibling (it takes `before`'s
    /// parent). `None` appends it at the end of the page, at the top level.
    /// Returns the block's new index, or `None` (nothing changes) when
    /// `before` is inside the moved subtree: a block can't go inside itself.
    pub fn move_subtree(&mut self, from: usize, before: Option<usize>) -> Option<usize> {
        let end = self.subtree_end(from);
        if before.is_some_and(|b| (from..end).contains(&b)) {
            return None;
        }
        let parent_id = before.and_then(|b| self.blocks[b].parent_id);
        let moved: Vec<Block> = self.blocks.drain(from..end).collect();
        let len = moved.len();
        // Indices after the removed range shift down by its length.
        let at = match before {
            Some(b) if b >= end => b - len,
            Some(b) => b,
            None => self.blocks.len(),
        };
        self.blocks.splice(at..at, moved);
        // Only the moved root changes parent; its descendants still point
        // at it (or at each other), so their nesting comes along.
        self.blocks[at].parent_id = parent_id;
        self.renumber();
        Some(at)
    }

    /// Index of the sibling of block `index` that comes `delta` places
    /// later (`1`) or earlier (`-1`), if there is one.
    fn sibling(&self, index: usize, delta: isize) -> Option<usize> {
        let block = &self.blocks[index];
        let order = block.order.checked_add_signed(delta)?;
        self.blocks
            .iter()
            .position(|b| b.parent_id == block.parent_id && b.order == order)
    }

    /// Alt+Up: swap block `index` (with its children) with the previous
    /// sibling (with its children). Returns its new index, or `None` for a
    /// first child.
    pub fn move_up(&mut self, index: usize) -> Option<usize> {
        let prev = self.sibling(index, -1)?;
        self.move_subtree(index, Some(prev))
    }

    /// Alt+Down: swap block `index` (with its children) with the next
    /// sibling (with its children). Returns its new index, or `None` for a
    /// last child.
    pub fn move_down(&mut self, index: usize) -> Option<usize> {
        let next = self.sibling(index, 1)?;
        let next_len = self.subtree_end(next) - next;
        // Moving the next sibling up in front of us is the same swap.
        self.move_subtree(next, Some(index))?;
        Some(index + next_len)
    }

    /// Delete a block that has no children. Returns false if it has children.
    pub fn delete_leaf(&mut self, index: usize) -> bool {
        if self.subtree_end(index) > index + 1 {
            return false;
        }
        self.blocks.remove(index);
        self.renumber();
        true
    }

    /// Copy all of `source`'s blocks into this page (used to insert a
    /// template) and return the index range they now occupy.
    ///
    /// They go right after block `anchor` and its children, as siblings of
    /// `anchor`; the source's own nesting is kept below that. If `anchor` is an
    /// empty block with no children, the copy replaces it instead, so inserting
    /// into a fresh page or journal doesn't leave a stray blank bullet. With no
    /// anchor (or one past the end) the blocks are appended at the top level.
    /// The copies get fresh IDs, so the same template can be inserted twice.
    pub fn insert_blocks_from(&mut self, anchor: Option<usize>, source: &Page) -> Range<usize> {
        let (parent_id, at) = match anchor.filter(|&a| a < self.blocks.len()) {
            Some(a) => {
                let parent = self.blocks[a].parent_id;
                let end = self.subtree_end(a);
                if !source.blocks.is_empty()
                    && end == a + 1
                    && self.blocks[a].content.trim().is_empty()
                {
                    self.blocks.remove(a);
                    (parent, a)
                } else {
                    (parent, end)
                }
            }
            None => (None, self.blocks.len()),
        };
        let new_ids: std::collections::HashMap<Uuid, Uuid> = source
            .blocks
            .iter()
            .map(|b| (b.id, Uuid::new_v4()))
            .collect();
        let copies: Vec<Block> = source
            .blocks
            .iter()
            .map(|b| Block {
                id: new_ids[&b.id],
                content: b.content.clone(),
                // Top-level source blocks hang off the anchor's parent; nested
                // ones keep pointing at (the copy of) their own parent.
                parent_id: match b.parent_id {
                    Some(p) => new_ids.get(&p).copied(),
                    None => parent_id,
                },
                page_id: self.id.clone(),
                order: 0,
            })
            .collect();
        let count = copies.len();
        self.blocks.splice(at..at, copies);
        self.renumber();
        at..at + count
    }

    /// Parse Logseq-style markdown into a page.
    pub fn from_markdown(title: &str, is_journal: bool, text: &str) -> Self {
        let mut page = Page::new(title, is_journal);

        // Stack of (depth, block id) for the chain of currently "open" ancestors.
        let mut stack: Vec<(usize, Uuid)> = Vec::new();
        // How many children each parent has received so far (for `order`).
        let mut child_counts: std::collections::HashMap<Option<Uuid>, usize> = Default::default();

        for line in text.lines() {
            let Some((depth, content)) = parse_bullet(line) else {
                // Not a bullet: a continuation line of the previous block (or
                // stray text before the first bullet, which we also keep).
                if line.trim().is_empty() {
                    continue;
                }
                // `id:: <uuid>`: the stable id of a referenced block (the
                // block on top of the stack, which has no children yet).
                if let (Some(id), Some(last)) = (id_property(line.trim()), page.blocks.last()) {
                    let duplicate = page.blocks.iter().any(|b| b.id == id);
                    if !duplicate && page.saved_ids.insert(id) {
                        let old = last.id;
                        if let Some(top) = stack.last_mut().filter(|(_, top)| *top == old) {
                            top.1 = id;
                        }
                        if let Some(count) = child_counts.remove(&Some(old)) {
                            child_counts.insert(Some(id), count);
                        }
                        if let Some(last) = page.blocks.last_mut() {
                            last.id = id;
                        }
                    }
                    continue;
                }
                // Strip exactly the indent `to_markdown` writes (two spaces
                // per level, plus two to line up under the bullet text) and
                // keep the rest as is, so spacing inside the line, like a
                // table's padding, survives a save byte for byte. Lines
                // indented some other way just lose their leading space.
                let indent = "  ".repeat(stack.len());
                let text = line
                    .strip_prefix(indent.as_str())
                    .unwrap_or(line.trim_start());
                match page.blocks.last_mut() {
                    Some(last) => {
                        last.content.push('\n');
                        last.content.push_str(text);
                    }
                    None => {
                        // Stray text before any bullet: promote it to a block.
                        let id = Uuid::new_v4();
                        page.blocks.push(Block {
                            id,
                            content: line.trim().to_string(),
                            parent_id: None,
                            page_id: page.id.clone(),
                            order: 0,
                        });
                        child_counts.insert(None, 1);
                        stack.push((0, id));
                    }
                }
                continue;
            };

            // Pop ancestors until the top of the stack is shallower than us.
            while stack.last().is_some_and(|&(d, _)| d >= depth) {
                stack.pop();
            }
            // A bad jump in indentation (e.g. 0 -> 3) just nests one level
            // under the nearest ancestor, which `stack.last()` gives us.
            let parent_id = stack.last().map(|&(_, id)| id);
            let order = child_counts.entry(parent_id).or_insert(0);
            let id = Uuid::new_v4();
            page.blocks.push(Block {
                id,
                content: content.to_string(),
                parent_id,
                page_id: page.id.clone(),
                order: *order,
            });
            *order += 1;
            stack.push((depth, id));
        }
        page
    }

    /// Serialise back to Logseq-style markdown (`- ` bullets, 2-space indent).
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        for (i, block) in self.blocks.iter().enumerate() {
            let indent = "  ".repeat(self.depth_of(i));
            let mut lines = block.content.split('\n');
            out.push_str(&indent);
            out.push_str("- ");
            out.push_str(lines.next().unwrap_or(""));
            out.push('\n');
            // A referenced block's id goes right under its first line, as
            // a Logseq property.
            if self.saved_ids.contains(&block.id) {
                out.push_str(&indent);
                out.push_str("  id:: ");
                out.push_str(&block.id.to_string());
                out.push('\n');
            }
            // Continuation lines are indented to line up under the bullet text.
            for extra in lines {
                out.push_str(&indent);
                out.push_str("  ");
                out.push_str(extra);
                out.push('\n');
            }
        }
        out
    }
}

/// If `line` is a bullet (`<indent>- text`), return `(depth, text)`.
/// Depth is the indent width divided by 2; a tab counts as one level.
fn parse_bullet(line: &str) -> Option<(usize, &str)> {
    let mut width = 0;
    let mut rest = line;
    loop {
        if let Some(r) = rest.strip_prefix("  ") {
            width += 1;
            rest = r;
        } else if let Some(r) = rest.strip_prefix('\t') {
            width += 1;
            rest = r;
        } else {
            break;
        }
    }
    if rest == "-" {
        return Some((width, ""));
    }
    rest.strip_prefix("- ").map(|text| (width, text))
}

#[cfg(test)]
mod tests {
    #[test]
    fn rename_updates_title_id_and_blocks() {
        let mut page = super::Page::from_markdown("Old", false, "- a\n  - b\n");
        page.rename("New");
        assert_eq!((page.id.as_str(), page.title.as_str()), ("New", "New"));
        assert!(page.blocks.iter().all(|b| b.page_id == "New"));
        assert_eq!(page.to_markdown(), "- a\n  - b\n");
    }

    use super::*;

    #[test]
    fn query_macros_take_one_tag() {
        let (range, tag) = parse_query("see {{query #proj}} here").unwrap();
        assert_eq!((range, tag.as_str()), (4..19, "proj"));
        let (_, tag) = parse_query("{{query #[[big plan]]}}").unwrap();
        assert_eq!(tag, "big plan");
        assert_eq!(parse_query("{{query  #proj }}").unwrap().1, "proj");
        assert_eq!(parse_query("{{query proj}}"), None);
        assert_eq!(parse_query("{{query #a #b}}"), None);
        assert_eq!(parse_query("{{query #a"), None);
    }

    #[test]
    fn tag_query_finds_tagged_blocks_across_pages() {
        let pages = vec![
            Page::from_markdown("A", false, "- milk #Proj\n- no tag\n- [[proj]] is a link\n"),
            Page::from_markdown(
                "B",
                false,
                "- {{query #proj}}\n- plan #[[proj]] and #other\n",
            ),
            Page::from_markdown("C", false, "- {{query #[[big plan]]}}\n- x #[[Big Plan]]\n"),
        ];
        assert_eq!(tag_query(&pages, "proj"), vec![(0, 0), (1, 1)]);
        assert_eq!(tag_query(&pages, "big plan"), vec![(2, 1)]);
        assert_eq!(tag_query(&pages, "nothing"), vec![]);
    }

    #[test]
    fn block_refs_are_found_and_bad_ones_skipped() {
        let id = Uuid::new_v4();
        let text = format!("see (({id})) and ((nope)) or (( (({id}))");
        let refs = parse_block_refs(&text);
        assert_eq!(refs.len(), 2);
        assert_eq!(&text[refs[0].0.clone()], format!("(({id}))"));
        assert_eq!(refs[0].1, id);
        assert_eq!(&text[refs[1].0.clone()], format!("(({id}))"));
    }

    #[test]
    fn referenced_block_ids_survive_save_and_load() {
        let mut page = Page::from_markdown("P", false, "- a\n  - child\n- b\n");
        let a = page.blocks[0].id;
        let child = page.blocks[1].id;
        page.saved_ids.insert(a);
        let md = page.to_markdown();
        assert_eq!(md, format!("- a\n  id:: {a}\n  - child\n- b\n"));

        let loaded = Page::from_markdown("P", false, &md);
        assert_eq!(loaded.blocks[0].id, a);
        assert_eq!(loaded.blocks[0].content, "a");
        // The child still hangs off the block, under its saved id.
        assert_eq!(loaded.blocks[1].parent_id, Some(a));
        // Unreferenced blocks are not written with ids and get new ones.
        assert_ne!(loaded.blocks[1].id, child);
        assert!(loaded.saved_ids.contains(&a));
        // Byte for byte the same file again.
        assert_eq!(loaded.to_markdown(), md);
    }

    #[test]
    fn a_multiline_block_keeps_its_id_line_and_text() {
        let id = Uuid::new_v4();
        let md = format!("- first\n  id:: {id}\n  second\n");
        let page = Page::from_markdown("P", false, &md);
        assert_eq!(page.blocks[0].id, id);
        assert_eq!(page.blocks[0].content, "first\nsecond");
        assert_eq!(page.to_markdown(), md);
        // A duplicated id line is just dropped, never two blocks with one id.
        let dup = format!("- a\n  id:: {id}\n- b\n  id:: {id}\n");
        let page = Page::from_markdown("P", false, &dup);
        assert_ne!(page.blocks[0].id, page.blocks[1].id);
    }

    fn template() -> Page {
        Page::from_markdown("T", false, "- Wins\n  - one\n- Plan\n")
    }

    #[test]
    fn insert_blocks_after_anchor_keeps_template_nesting() {
        let mut page = Page::from_markdown("p", false, "- a\n  - a1\n- b\n");
        let range = page.insert_blocks_from(Some(0), &template());
        // After `a` and its child, before `b`, as siblings of `a`.
        assert_eq!(range, 2..5);
        assert_eq!(
            page.to_markdown(),
            "- a\n  - a1\n- Wins\n  - one\n- Plan\n- b\n"
        );
        assert!(page.blocks.iter().all(|b| b.page_id == "p"));
    }

    #[test]
    fn insert_blocks_under_a_nested_anchor_nests_them_too() {
        let mut page = Page::from_markdown("p", false, "- a\n  - a1\n- b\n");
        page.insert_blocks_from(Some(1), &template());
        assert_eq!(
            page.to_markdown(),
            "- a\n  - a1\n  - Wins\n    - one\n  - Plan\n- b\n"
        );
        // `order` is renumbered: Wins and Plan are a's 2nd and 3rd children.
        assert_eq!(page.blocks[2].order, 1);
        assert_eq!(page.blocks[4].order, 2);
    }

    #[test]
    fn insert_blocks_replaces_an_empty_anchor() {
        let mut page = Page::from_markdown("p", false, "- \n");
        let range = page.insert_blocks_from(Some(0), &template());
        assert_eq!(range, 0..3);
        assert_eq!(page.to_markdown(), "- Wins\n  - one\n- Plan\n");
    }

    #[test]
    fn insert_blocks_without_anchor_appends_and_ids_are_fresh() {
        let source = template();
        let mut page = Page::from_markdown("p", false, "- a\n");
        page.insert_blocks_from(None, &source);
        page.insert_blocks_from(None, &source);
        assert_eq!(
            page.to_markdown(),
            "- a\n- Wins\n  - one\n- Plan\n- Wins\n  - one\n- Plan\n"
        );
        let mut ids: Vec<Uuid> = page.blocks.iter().map(|b| b.id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), page.blocks.len(), "every copy has its own id");
        assert!(source.blocks.iter().all(|b| !ids.contains(&b.id)));
    }

    #[test]
    fn insert_empty_source_changes_nothing() {
        let mut page = Page::from_markdown("p", false, "- \n");
        let range = page.insert_blocks_from(Some(0), &Page::new("empty", false));
        assert!(range.is_empty());
        assert_eq!(page.to_markdown(), "- \n");
    }

    #[test]
    fn task_states_cycle_in_logseq_order() {
        let mut content = "buy milk".to_string();
        let mut seen = Vec::new();
        for _ in 0..4 {
            content = cycle_task(&content);
            seen.push(content.clone());
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
        // Logseq's other workflow, and an empty block.
        assert_eq!(cycle_task("LATER x"), "NOW x");
        assert_eq!(cycle_task("NOW x"), "DONE x");
        assert_eq!(cycle_task(""), "TODO ");
        assert_eq!(cycle_task("TODO"), "DOING ");
        // The marker goes after a heading prefix, as Logseq writes it.
        assert_eq!(cycle_task("## Plan"), "## TODO Plan");
        assert_eq!(cycle_task("## DONE Plan"), "## Plan");
        assert_eq!(cycle_task("> DOING q"), "> DONE q");
    }

    #[test]
    fn task_markers_need_a_whole_keyword() {
        assert_eq!(TaskState::parse("TODO x"), (Some(TaskState::Todo), "x"));
        assert_eq!(TaskState::parse("DONE"), (Some(TaskState::Done), ""));
        for text in ["TODOS x", "todo x", "xTODO x", " TODO x", "DOING:x"] {
            assert_eq!(TaskState::parse(text), (None, text));
        }
        assert_eq!(task_split("# NOW a"), (2, Some(TaskState::Now), 6));
        assert_eq!(task_split("# a"), (2, None, 2));
    }

    #[test]
    fn collapsing_hides_all_descendants() {
        let md = "- a\n  - b\n    - c\n  - d\n- e\n  - f\n";
        let page = Page::from_markdown("t", false, md);
        let id = |i: usize| page.blocks[i].id;
        let none = std::collections::HashSet::new();
        assert!(page.visible_blocks(&none).iter().all(|&v| v));
        let folded: std::collections::HashSet<_> = [id(0)].into();
        assert_eq!(
            page.visible_blocks(&folded),
            [true, false, false, false, true, true]
        );
        assert_eq!(page.descendant_count(0), 3);
        // A collapsed block inside a collapsed block; and a nested one only.
        let folded: std::collections::HashSet<_> = [id(1)].into();
        assert_eq!(
            page.visible_blocks(&folded),
            [true, true, false, true, true, true]
        );
        assert_eq!(page.descendant_count(1), 1);
        let folded: std::collections::HashSet<_> = [id(0), id(1), id(4)].into();
        assert_eq!(
            page.visible_blocks(&folded),
            [true, false, false, false, true, false]
        );
        // A collapsed leaf hides nothing.
        let folded: std::collections::HashSet<_> = [id(2), id(5)].into();
        assert!(page.visible_blocks(&folded).iter().all(|&v| v));
        assert_eq!(page.ancestor_ids(2), [id(1), id(0)]);
        assert!(page.ancestor_ids(4).is_empty());
    }

    #[test]
    fn insert_after_subtree_adds_a_sibling() {
        let mut page = Page::from_markdown("t", false, "- a\n  - b\n    - c\n- d\n");
        let at = page.insert_after_subtree(0, "new".into());
        assert_eq!(at, 3);
        assert_eq!(page.to_markdown(), "- a\n  - b\n    - c\n- new\n- d\n");
        let at = page.insert_after_subtree(1, "x".into());
        assert_eq!(at, 3);
        assert_eq!(
            page.to_markdown(),
            "- a\n  - b\n    - c\n  - x\n- new\n- d\n"
        );
    }

    #[test]
    fn round_trip_nested() {
        let md = "- a\n  - b\n    - c\n  - d\n- e\n";
        let page = Page::from_markdown("t", false, md);
        assert_eq!(page.blocks.len(), 5);
        assert_eq!(page.depth_of(2), 2);
        assert_eq!(page.blocks[3].parent_id, Some(page.blocks[0].id));
        assert_eq!(page.blocks[3].order, 1);
        assert_eq!(page.to_markdown(), md);
    }

    fn targets(text: &str) -> Vec<String> {
        parse_wikilinks(text)
            .into_iter()
            .map(|l| l.target)
            .collect()
    }

    #[test]
    fn wikilinks_basic_and_ranges() {
        let text = "see [[Foo]] and [[ Bar Baz ]]!";
        let links = parse_wikilinks(text);
        assert_eq!(targets(text), vec!["Foo", "Bar Baz"]);
        assert_eq!(&text[links[0].range.clone()], "[[Foo]]");
        assert_eq!(&text[links[1].range.clone()], "[[ Bar Baz ]]");
    }

    #[test]
    fn wikilinks_reject_malformed() {
        assert!(targets("[[]] [[ ]] [single] [[unclosed").is_empty());
        assert!(targets("[[a\nb]]").is_empty());
        // Only the inner, well-formed link counts.
        assert_eq!(targets("[[a [[b]]"), vec!["b"]);
        // Multi-byte text around and inside links is fine.
        assert_eq!(targets("é [[ü]] 😀"), vec!["ü"]);
    }

    #[test]
    fn backlinks_skip_self_and_ignore_case() {
        let pages = vec![
            Page::from_markdown("A", false, "- one [[Target]]\n- none\n- two [[target]]\n"),
            Page::from_markdown("Target", false, "- self [[Target]]\n"),
            Page::from_markdown("B", false, "- nothing\n"),
            Page::from_markdown("C", false, "- [[Other]]\n  - deep [[ TARGET ]]\n"),
        ];
        assert_eq!(
            backlinks(&pages, "Target"),
            vec![
                BacklinkGroup {
                    page: 0,
                    blocks: vec![0, 2]
                },
                BacklinkGroup {
                    page: 3,
                    blocks: vec![1]
                },
            ]
        );
        assert!(backlinks(&pages, "Nobody").is_empty());
    }

    fn refs(text: &str) -> Vec<(String, bool)> {
        parse_references(text)
            .into_iter()
            .map(|r| (r.target, r.is_tag))
            .collect()
    }

    #[test]
    fn tags_basic_and_ranges() {
        let text = "#start mid #two, and #[[multi word]]!";
        let found = parse_references(text);
        let shown: Vec<&str> = found.iter().map(|r| &text[r.range.clone()]).collect();
        assert_eq!(shown, vec!["#start", "#two", "#[[multi word]]"]);
        assert_eq!(
            refs(text),
            vec![
                ("start".into(), true),
                ("two".into(), true),
                ("multi word".into(), true)
            ]
        );
    }

    #[test]
    fn tags_boundaries() {
        // Not tags: URL fragments, C#, headings, issue numbers, lone hashes.
        assert!(refs("see page.html#top and C# and ## x").is_empty());
        assert!(refs("# Heading").is_empty());
        assert!(refs("fixed #42 and # and #").is_empty());
        assert!(refs("##double").is_empty());
        // Allowed after an opening bracket; punctuation ends the tag.
        assert_eq!(
            refs("(#a) [#b] #c."),
            vec![("a".into(), true), ("b".into(), true), ("c".into(), true)]
        );
        // Nested names and trailing dashes.
        assert_eq!(
            refs("#proj/sub #todo-"),
            vec![("proj/sub".into(), true), ("todo".into(), true)]
        );
        // Multi-byte tag names work.
        assert_eq!(
            refs("#café 😀 #über"),
            vec![("café".into(), true), ("über".into(), true)]
        );
        // Malformed multi-word tags are ignored.
        assert!(refs("#[[unclosed and #[[]]").is_empty());
    }

    #[test]
    fn links_and_tags_mix_without_double_counting() {
        // `#[[x]]` is one tag, not a tag plus a link; a `#` inside a link is
        // part of the link name, not a tag.
        assert_eq!(
            refs("[[a #b]] #[[c d]] [[e]] #f"),
            vec![
                ("a #b".into(), false),
                ("c d".into(), true),
                ("e".into(), false),
                ("f".into(), true)
            ]
        );
    }

    #[test]
    fn tag_counts_group_case_insensitively_per_block() {
        let pages = vec![
            Page::from_markdown(
                "A",
                false,
                "- #Idea one #idea\n- #idea and #b\n- [[idea]]\n",
            ),
            Page::from_markdown("B", false, "- #b #IDEA\n- #c\n"),
        ];
        // idea: blocks 0,1 of A and 0 of B = 3 (repeat in one block counts once,
        // the [[idea]] link is not a tag). b: 2. c: 1.
        assert_eq!(
            tag_counts(&pages),
            vec![("Idea".into(), 3), ("b".into(), 2), ("c".into(), 1)]
        );
    }

    #[test]
    fn backlinks_include_tags() {
        let pages = vec![
            Page::from_markdown("A", false, "- tagged #target\n"),
            Page::from_markdown("Target", false, "- x\n"),
        ];
        assert_eq!(
            backlinks(&pages, "Target"),
            vec![BacklinkGroup {
                page: 0,
                blocks: vec![0]
            }]
        );
    }

    #[test]
    fn journal_titles() {
        assert!(is_journal_title("2026-10-07"));
        assert!(!is_journal_title("Welcome"));
        assert!(!is_journal_title("2026-13-40"));
    }

    /// Helper: `(depth, content)` for each block, for readable assertions.
    fn shape(page: &Page) -> Vec<(usize, String)> {
        (0..page.blocks.len())
            .map(|i| (page.depth_of(i), page.blocks[i].content.clone()))
            .collect()
    }

    #[test]
    fn enter_makes_sibling_or_first_child() {
        let mut page = Page::from_markdown("t", false, "- a\n  - b\n- c\n");
        // `b` has no children -> sibling below it.
        assert_eq!(page.insert_after(1, "x".into()), 2);
        // `a` has children -> new block is its first child.
        assert_eq!(page.insert_after(0, "y".into()), 1);
        assert_eq!(
            shape(&page),
            vec![
                (0, "a".into()),
                (1, "y".into()),
                (1, "b".into()),
                (1, "x".into()),
                (0, "c".into())
            ]
        );
        assert_eq!(page.blocks[1].order, 0);
        assert_eq!(page.blocks[3].order, 2);
    }

    #[test]
    fn move_down_swaps_with_the_next_sibling() {
        let mut page = Page::from_markdown("p", false, "- a\n- b\n- c\n");
        assert_eq!(page.move_down(0), Some(1));
        assert_eq!(page.to_markdown(), "- b\n- a\n- c\n");
        assert_eq!(page.move_down(1), Some(2));
        assert_eq!(page.to_markdown(), "- b\n- c\n- a\n");
        // The last sibling can't go further down.
        assert_eq!(page.move_down(2), None);
        assert_eq!(page.to_markdown(), "- b\n- c\n- a\n");
        assert_eq!(
            page.blocks.iter().map(|b| b.order).collect::<Vec<_>>(),
            [0, 1, 2]
        );
    }

    #[test]
    fn move_up_swaps_with_the_previous_sibling() {
        let mut page = Page::from_markdown("p", false, "- a\n- b\n- c\n");
        assert_eq!(page.move_up(2), Some(1));
        assert_eq!(page.to_markdown(), "- a\n- c\n- b\n");
        assert_eq!(page.move_up(1), Some(0));
        assert_eq!(page.to_markdown(), "- c\n- a\n- b\n");
        // The first sibling can't go further up.
        assert_eq!(page.move_up(0), None);
    }

    #[test]
    fn moving_a_block_takes_its_children_along() {
        let md = "- a\n  - a1\n    - a11\n  - a2\n- b\n  - b1\n- c\n";
        let mut page = Page::from_markdown("p", false, md);
        // a (with a1, a11, a2) swaps with b (with b1).
        assert_eq!(page.move_down(0), Some(2));
        assert_eq!(
            page.to_markdown(),
            "- b\n  - b1\n- a\n  - a1\n    - a11\n  - a2\n- c\n"
        );
        // And back up.
        assert_eq!(page.move_up(2), Some(0));
        assert_eq!(page.to_markdown(), md);
        // Children move among their own siblings only.
        assert_eq!(page.move_down(1), Some(2));
        assert_eq!(
            page.to_markdown(),
            "- a\n  - a2\n  - a1\n    - a11\n- b\n  - b1\n- c\n"
        );
    }

    #[test]
    fn move_subtree_reparents_onto_the_target_and_refuses_itself() {
        let md = "- a\n  - a1\n- b\n  - b1\n";
        let mut page = Page::from_markdown("p", false, md);
        // Drop a (with a1) just before b1: it becomes b's first child.
        assert_eq!(page.move_subtree(0, Some(3)), Some(1));
        assert_eq!(page.to_markdown(), "- b\n  - a\n    - a1\n  - b1\n");
        assert_eq!(page.blocks[1].parent_id, Some(page.blocks[0].id));
        assert_eq!((page.blocks[1].order, page.blocks[3].order), (0, 1));
        // A block can't be dropped inside its own subtree.
        assert_eq!(page.move_subtree(1, Some(2)), None);
        assert_eq!(page.move_subtree(1, Some(1)), None);
        // `None` appends it at the top level.
        assert_eq!(page.move_subtree(1, None), Some(2));
        assert_eq!(page.to_markdown(), "- b\n  - b1\n- a\n  - a1\n");
    }

    #[test]
    fn indent_and_outdent() {
        let mut page = Page::from_markdown("t", false, "- a\n- b\n- c\n");
        assert!(!page.indent(0), "first block cannot indent");
        assert!(page.indent(1));
        assert_eq!(page.to_markdown(), "- a\n  - b\n- c\n");
        assert!(page.indent(2)); // c nests under a, after b
        assert_eq!(page.to_markdown(), "- a\n  - b\n  - c\n");
        // Outdenting b makes the later sibling c its child.
        assert!(page.outdent(1));
        assert_eq!(page.to_markdown(), "- a\n- b\n  - c\n");
        assert!(!page.outdent(0));
    }

    #[test]
    fn delete_only_leaves() {
        let mut page = Page::from_markdown("t", false, "- a\n  - b\n- c\n");
        assert!(!page.delete_leaf(0));
        assert!(page.delete_leaf(1));
        assert_eq!(page.to_markdown(), "- a\n- c\n");
    }

    #[test]
    fn block_kind_parses_exact_prefixes() {
        assert_eq!(BlockKind::parse("plain"), (BlockKind::Text, "plain"));
        assert_eq!(BlockKind::parse("# Title"), (BlockKind::Heading1, "Title"));
        assert_eq!(BlockKind::parse("## Sub"), (BlockKind::Heading2, "Sub"));
        assert_eq!(
            BlockKind::parse("### Small"),
            (BlockKind::Heading3, "Small")
        );
        assert_eq!(BlockKind::parse("> said"), (BlockKind::Quote, "said"));
        assert_eq!(BlockKind::parse("# "), (BlockKind::Heading1, ""));
        // Not prefixes: tags, too many hashes, no space, markers mid-text.
        assert_eq!(BlockKind::parse("#tag x").0, BlockKind::Text);
        assert_eq!(BlockKind::parse("#### deep").0, BlockKind::Text);
        assert_eq!(BlockKind::parse(">no space").0, BlockKind::Text);
        assert_eq!(BlockKind::parse("a # b").0, BlockKind::Text);
        assert_eq!(BlockKind::parse("").0, BlockKind::Text);
    }

    #[test]
    fn block_kind_apply_keeps_text() {
        assert_eq!(BlockKind::Heading2.apply("hello"), "## hello");
        assert_eq!(BlockKind::Quote.apply("# hello"), "> hello");
        assert_eq!(BlockKind::Heading1.apply("### hello"), "# hello");
        assert_eq!(BlockKind::Text.apply("> hello"), "hello");
        assert_eq!(BlockKind::Text.apply("hello"), "hello");
        assert_eq!(BlockKind::Heading3.apply(""), "### ");
        // Every kind round-trips through parse.
        for kind in BlockKind::ALL {
            assert_eq!(BlockKind::parse(&kind.apply("x")), (kind, "x"));
        }
    }

    #[test]
    fn block_kinds_round_trip_as_logseq_markdown() {
        let md = "- # Title\n  - ## Sub\n  - ### Small\n- > quoted\n- plain\n";
        let page = Page::from_markdown("t", false, md);
        let kinds: Vec<BlockKind> = page
            .blocks
            .iter()
            .map(|b| BlockKind::parse(&b.content).0)
            .collect();
        assert_eq!(
            kinds,
            vec![
                BlockKind::Heading1,
                BlockKind::Heading2,
                BlockKind::Heading3,
                BlockKind::Quote,
                BlockKind::Text
            ]
        );
        assert_eq!(page.to_markdown(), md);
    }

    #[test]
    fn continuation_lines_are_kept() {
        let md = "- one\n  more text\n- two\n";
        let page = Page::from_markdown("t", false, md);
        assert_eq!(page.blocks[0].content, "one\nmore text");
        assert_eq!(page.to_markdown(), md);
    }

    #[test]
    fn continuation_lines_keep_their_inner_spacing() {
        // Deeper indent and trailing spaces past the block's own indent are
        // part of the text; a line with too little indent is re-indented.
        let md = "- a\n  - b\n    |  x  |  \n      deeper\n- c\n";
        let page = Page::from_markdown("t", false, md);
        assert_eq!(page.blocks[1].content, "b\n|  x  |  \n  deeper");
        assert_eq!(page.to_markdown(), md);
        let page = Page::from_markdown("t", false, "- a\n  - b\n  under\n");
        assert_eq!(page.blocks[1].content, "b\nunder");
    }

    #[test]
    fn references_inside_code_blocks_are_ignored() {
        let text = "see [[Real]] #tag\n```c\n#include <x>\nif [[ -f a ]]\n```\nand #after";
        let names: Vec<String> = parse_references(text)
            .into_iter()
            .map(|r| r.target)
            .collect();
        assert_eq!(names, ["Real", "tag", "after"]);
        assert_eq!(parse_wikilinks(text).len(), 1);
    }

    #[test]
    fn push_block_appends_at_the_top_level() {
        let mut page = Page::from_markdown("t", false, "- a\n  - b\n");
        let ix = page.push_block("c".into());
        assert_eq!(ix, 2);
        assert_eq!(page.blocks[2].parent_id, None);
        assert_eq!(page.to_markdown(), "- a\n  - b\n- c\n");
        let mut empty = Page::new("e", false);
        assert_eq!(empty.push_block("x".into()), 0);
        assert_eq!(empty.to_markdown(), "- x\n");
    }
}
