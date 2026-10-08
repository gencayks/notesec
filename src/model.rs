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

use std::ops::Range;
use uuid::Uuid;

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
}

impl Page {
    pub fn new(title: &str, is_journal: bool) -> Self {
        Page {
            id: title.to_string(),
            title: title.to_string(),
            blocks: Vec::new(),
            is_journal,
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

    /// Delete a block that has no children. Returns false if it has children.
    pub fn delete_leaf(&mut self, index: usize) -> bool {
        if self.subtree_end(index) > index + 1 {
            return false;
        }
        self.blocks.remove(index);
        self.renumber();
        true
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
                match page.blocks.last_mut() {
                    Some(last) => {
                        last.content.push('\n');
                        last.content.push_str(line.trim());
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
    use super::*;

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
}
