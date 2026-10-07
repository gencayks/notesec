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

use uuid::Uuid;

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
    fn round_trip_nested() {
        let md = "- a\n  - b\n    - c\n  - d\n- e\n";
        let page = Page::from_markdown("t", false, md);
        assert_eq!(page.blocks.len(), 5);
        assert_eq!(page.depth_of(2), 2);
        assert_eq!(page.blocks[3].parent_id, Some(page.blocks[0].id));
        assert_eq!(page.blocks[3].order, 1);
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
