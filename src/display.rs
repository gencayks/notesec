//! Reading view of a block that isn't being edited: the text as shown, with
//! the markdown that styles it hidden, plus the mapping between offsets in
//! that shown text ("display" offsets) and offsets in the block's `content`
//! ("source" offsets, what the editor and the file use).
//!
//! Hidden are the block-type prefix (`# `, `> `, ...), the task keyword
//! (`TODO `, `DONE `, ..., drawn as a checkbox instead) and the `*` markers of
//! properly paired emphasis (`**bold**`, `*italic*`, `***both***`). Stray
//! stars stay visible. Nothing here changes `content`; storage still writes
//! the raw markers.
//!
//! Emphasis rules (a simplified CommonMark, which is what Logseq renders):
//! - A maximal run of `*` is a delimiter. Stars inside a `[[wikilink]]` or
//!   `#tag` are plain text.
//! - A run can open if the next character exists and isn't whitespace, and
//!   can close if the previous character exists and isn't whitespace. So
//!   `2 * 3`, a lone `**` and a leading `* item` stay literal.
//! - A closer pairs with the nearest earlier opener that still has stars.
//!   Two stars are used (bold) when both sides have at least two, else one
//!   (italic); the stars nearest the text are used first, so `***x***` is
//!   bold inside italic. Openers between the pair are dropped. Stars left
//!   unpaired are shown.
//!
//! A block reference `((<uuid>))` whose block exists is shown as that
//! block's text (its own reading-view text, on one line, without resolving
//! references inside it, so a cycle can't recurse). That shown text isn't in
//! `content`, so the whole reference maps to its start (or its end, from the
//! end of the shown text). A reference to a missing block stays as written.

use std::ops::Range;

use uuid::Uuid;

use crate::model::{parse_block_refs, parse_references, BlockKind, Reference, TaskState};

/// How one stretch of displayed text is styled.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Format {
    pub bold: bool,
    pub italic: bool,
    /// Inside a `[[wikilink]]`.
    pub link: bool,
    /// Inside a `#tag`.
    pub tag: bool,
    /// The shown text of a `((block reference))`.
    pub block_ref: bool,
}

/// A run of visible source text and where it starts in the display text,
/// or, for a resolved block reference, the reference's source range and the
/// length of the text shown instead (`replaced`).
#[derive(Clone, Debug)]
struct Chunk {
    display: usize,
    source: Range<usize>,
    replaced: Option<usize>,
}

impl Chunk {
    /// Length in the display text.
    fn len(&self) -> usize {
        self.replaced.unwrap_or(self.source.len())
    }
}

/// A resolved block reference in the display text.
#[derive(Clone, Debug, PartialEq)]
pub struct BlockRef {
    /// Display range of the referenced block's shown text.
    pub range: Range<usize>,
    pub id: Uuid,
}

#[derive(Clone, Debug)]
pub struct DisplayBlock {
    pub kind: BlockKind,
    pub task: Option<TaskState>,
    /// The text as shown.
    pub text: String,
    /// Wikilinks and tags, with ranges in display offsets.
    pub links: Vec<Reference>,
    /// Resolved block references, with ranges in display offsets.
    pub block_refs: Vec<BlockRef>,
    /// Emphasised display ranges; `true` for bold, `false` for italic.
    emphasis: Vec<(Range<usize>, bool)>,
    /// Visible pieces of `content`, in order; everything between them is
    /// hidden.
    chunks: Vec<Chunk>,
    source_len: usize,
}

impl DisplayBlock {
    /// The reading view of `content`, with block references left as
    /// written.
    pub fn new(content: &str) -> Self {
        Self::with_refs(content, |_| None)
    }

    /// The reading view of `content`; `resolve` gives the content of a
    /// referenced block, or `None` if there is no such block.
    pub fn with_refs(content: &str, resolve: impl Fn(Uuid) -> Option<String>) -> Self {
        let (kind, body) = BlockKind::parse(content);
        let (task, body) = TaskState::parse(body);
        let prefix = content.len() - body.len();
        let refs = parse_references(body);
        // Resolved block references, with the text each one shows.
        let block_refs: Vec<(Range<usize>, Uuid, String)> = parse_block_refs(body)
            .into_iter()
            .filter_map(|(range, id)| {
                let shown = DisplayBlock::new(&resolve(id)?).text.replace('\n', " ");
                Some((range, id, shown))
            })
            .collect();
        let literal: Vec<Range<usize>> = refs
            .iter()
            .map(|r| r.range.clone())
            .chain(block_refs.iter().map(|(r, _, _)| r.clone()))
            .collect();
        let (mut hidden, spans) = parse_emphasis(body, &literal);
        // Each hidden range, with the text shown in its place (block refs).
        let mut cuts: Vec<(Range<usize>, Option<&str>)> = hidden
            .drain(..)
            .map(|r| (r, None))
            .chain(
                block_refs
                    .iter()
                    .map(|(r, _, shown)| (r.clone(), Some(shown.as_str()))),
            )
            .collect();
        cuts.sort_by_key(|(r, _)| r.start);

        let mut text = String::new();
        let mut chunks = Vec::new();
        let mut pos = 0;
        for (h, shown) in cuts
            .iter()
            .chain(std::iter::once(&(body.len()..body.len(), None)))
        {
            if pos < h.start {
                chunks.push(Chunk {
                    display: text.len(),
                    source: prefix + pos..prefix + h.start,
                    replaced: None,
                });
                text.push_str(&body[pos..h.start]);
            }
            if let Some(shown) = shown {
                chunks.push(Chunk {
                    display: text.len(),
                    source: prefix + h.start..prefix + h.end,
                    replaced: Some(shown.len()),
                });
                text.push_str(shown);
            }
            pos = pos.max(h.end);
        }

        let mut block = DisplayBlock {
            kind,
            task,
            text,
            links: Vec::new(),
            block_refs: Vec::new(),
            emphasis: Vec::new(),
            chunks,
            source_len: content.len(),
        };
        let range = |b: &DisplayBlock, r: &Range<usize>| {
            b.to_display(prefix + r.start)..b.to_display(prefix + r.end)
        };
        block.emphasis = spans
            .iter()
            .map(|(r, bold)| (range(&block, r), *bold))
            .filter(|(r, _)| !r.is_empty())
            .collect();
        block.links = refs
            .iter()
            .map(|l| Reference {
                range: range(&block, &l.range),
                ..l.clone()
            })
            .collect();
        block.block_refs = block
            .chunks
            .iter()
            .filter_map(|c| c.replaced.map(|len| (c, len)))
            .zip(&block_refs)
            .map(|((c, len), (_, id, _))| BlockRef {
                range: c.display..c.display + len,
                id: *id,
            })
            .collect();
        block
    }

    /// Where display offset `display` is in `content`.
    ///
    /// An offset sticks to the visible character before it, so it lands
    /// before any markers hidden right after that character: just after the
    /// last letter of a bold word is inside the bold, before the closing
    /// `**`. Offset 0 is the first visible character, after the type prefix,
    /// the task keyword and any opening markers. Offsets past the end clamp to the last
    /// visible character.
    pub fn to_source(&self, display: usize) -> usize {
        let Some(first) = self.chunks.first() else {
            return self.source_len;
        };
        if display == 0 {
            return first.source.start;
        }
        self.chunks
            .iter()
            .find(|c| c.display < display && display <= c.display + c.len())
            .map_or(self.chunks.last().map_or(0, |c| c.source.end), |c| {
                match c.replaced {
                    // Inside a reference's shown text: before the reference,
                    // unless at the very end of it.
                    Some(len) if display == c.display + len => c.source.end,
                    Some(_) => c.source.start,
                    None => c.source.start + (display - c.display),
                }
            })
    }

    /// Where source offset `source` shows up: the number of visible bytes
    /// before it (a hidden offset maps to the next visible character).
    pub fn to_display(&self, source: usize) -> usize {
        self.chunks
            .iter()
            .map(|c| match c.replaced {
                Some(len) if source >= c.source.end => len,
                Some(_) => 0,
                None => source.clamp(c.source.start, c.source.end) - c.source.start,
            })
            .sum()
    }

    /// The display text cut into non-overlapping, ordered ranges with their
    /// combined formatting. Unformatted text is left out.
    pub fn segments(&self) -> Vec<(Range<usize>, Format)> {
        let mut cuts: Vec<usize> = self
            .emphasis
            .iter()
            .map(|(r, _)| r)
            .chain(self.links.iter().map(|l| &l.range))
            .chain(self.block_refs.iter().map(|r| &r.range))
            .flat_map(|r| [r.start, r.end])
            .collect();
        cuts.sort_unstable();
        cuts.dedup();
        let mut segments: Vec<(Range<usize>, Format)> = Vec::new();
        for pair in cuts.windows(2) {
            let range = pair[0]..pair[1];
            let covers = |r: &Range<usize>| r.start <= range.start && range.end <= r.end;
            let mut format = Format::default();
            for (r, bold) in &self.emphasis {
                if covers(r) {
                    if *bold {
                        format.bold = true;
                    } else {
                        format.italic = true;
                    }
                }
            }
            for l in self.links.iter().filter(|l| covers(&l.range)) {
                if l.is_tag {
                    format.tag = true;
                } else {
                    format.link = true;
                }
            }
            if self.block_refs.iter().any(|r| covers(&r.range)) {
                format.block_ref = true;
            }
            if format == Format::default() {
                continue;
            }
            match segments.last_mut() {
                Some((last, f)) if last.end == range.start && *f == format => last.end = range.end,
                _ => segments.push((range, format)),
            }
        }
        segments
    }
}

/// A run of `*`; its unused stars are `lo..hi`.
struct Run {
    lo: usize,
    hi: usize,
    can_open: bool,
    can_close: bool,
}

/// Pair up emphasis markers in `text` (see the module docs). Returns the
/// marker ranges to hide and the emphasised ranges (`true` = bold), both in
/// `text` offsets. Stars inside the `literal` ranges (links, tags and block
/// references) are ignored.
fn parse_emphasis(
    text: &str,
    literal: &[Range<usize>],
) -> (Vec<Range<usize>>, Vec<(Range<usize>, bool)>) {
    let literal = |i: usize| literal.iter().any(|r| r.contains(&i));
    let is_star = |i: usize| text.as_bytes().get(i) == Some(&b'*') && !literal(i);

    let mut runs = Vec::new();
    let mut i = 0;
    while i < text.len() {
        if !is_star(i) {
            i += 1;
            continue;
        }
        let start = i;
        while is_star(i) {
            i += 1;
        }
        let prev = text[..start].chars().next_back();
        let next = text[i..].chars().next();
        runs.push(Run {
            lo: start,
            hi: i,
            can_open: next.is_some_and(|c| !c.is_whitespace()),
            can_close: prev.is_some_and(|c| !c.is_whitespace()),
        });
    }

    let mut hidden = Vec::new();
    let mut spans = Vec::new();
    let mut openers: Vec<usize> = Vec::new();
    for c in 0..runs.len() {
        if runs[c].can_close {
            while runs[c].lo < runs[c].hi {
                let Some(pos) = openers.iter().rposition(|&o| runs[o].lo < runs[o].hi) else {
                    break;
                };
                let o = openers[pos];
                let n = if runs[o].hi - runs[o].lo >= 2 && runs[c].hi - runs[c].lo >= 2 {
                    2
                } else {
                    1
                };
                let open = runs[o].hi - n..runs[o].hi;
                let close = runs[c].lo..runs[c].lo + n;
                spans.push((open.end..close.start, n == 2));
                hidden.push(open);
                hidden.push(close);
                runs[o].hi -= n;
                runs[c].lo += n;
                openers.truncate(pos + 1);
                if runs[o].lo == runs[o].hi {
                    openers.pop();
                }
            }
        }
        if runs[c].can_open && runs[c].lo < runs[c].hi {
            openers.push(c);
        }
    }
    (hidden, spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The display text with `[`/`]` around bold and `/` around italic
    /// segments, for compact assertions.
    #[test]
    fn block_refs_show_the_referenced_text() {
        let id = Uuid::new_v4();
        let content = format!("**see (({id}))** now");
        let d = DisplayBlock::with_refs(&content, |i| {
            (i == id).then(|| "## the **source** [[P]]\nmore".to_string())
        });
        // The source's own reading text, on one line.
        assert_eq!(d.text, "see the source [[P]] more now");
        let start = "see ".len();
        let end = start + "the source [[P]] more".len();
        assert_eq!(
            d.block_refs,
            vec![BlockRef {
                range: start..end,
                id
            }]
        );
        // The bold around the reference still covers it.
        let segs = d.segments();
        assert!(segs
            .iter()
            .any(|(r, f)| f.block_ref && f.bold && r.start == start));
        // Mapping: into the reference goes before it, its end after it.
        let ref_start = content.find("((").unwrap();
        let ref_end = content.find("))").unwrap() + 2;
        assert_eq!(d.to_source(start + 3), ref_start);
        assert_eq!(d.to_source(end), ref_end);
        assert_eq!(d.to_display(ref_end + 2), end);
        assert_eq!(d.to_display(content.len()), d.text.len());
    }

    #[test]
    fn missing_block_refs_stay_as_written() {
        let id = Uuid::new_v4();
        let content = format!("a (({id})) b");
        let d = DisplayBlock::with_refs(&content, |_| None);
        assert_eq!(d.text, content);
        assert!(d.block_refs.is_empty());
    }

    fn show(content: &str) -> String {
        let d = DisplayBlock::new(content);
        let mut out = String::new();
        let mut pos = 0;
        for (r, f) in d.segments() {
            out.push_str(&d.text[pos..r.start]);
            let (open, close) = match (f.bold, f.italic) {
                (true, true) => ("[/", "/]"),
                (true, false) => ("[", "]"),
                (false, true) => ("/", "/"),
                (false, false) => ("", ""),
            };
            out.push_str(open);
            out.push_str(&d.text[r.clone()]);
            out.push_str(close);
            pos = r.end;
        }
        out.push_str(&d.text[pos..]);
        out
    }

    #[test]
    fn paired_markers_are_hidden_and_styled() {
        assert_eq!(show("a **bold** and *it* b"), "a [bold] and /it/ b");
        assert_eq!(show("***both***"), "[/both/]");
        assert_eq!(show("*a **b** c*"), "/a /[/b/]/ c/");
        assert_eq!(show("**bold**text"), "[bold]text");
        assert_eq!(DisplayBlock::new("x **y**").text, "x y");
    }

    #[test]
    fn stray_stars_stay_literal() {
        for s in [
            "2 * 3", "a ** b", "**", "****", "* item", "a*b", "** x**", "**x **",
        ] {
            let d = DisplayBlock::new(s);
            assert_eq!(d.text, s, "{s:?}");
            assert!(d.segments().is_empty(), "{s:?}");
        }
        // Unpaired leftovers stay visible next to a pair.
        assert_eq!(show("**a*"), "*/a/");
        assert_eq!(show("*a**"), "/a/*");
    }

    #[test]
    fn stars_inside_links_are_literal() {
        let d = DisplayBlock::new("*see [[a*b]] now");
        assert_eq!(d.text, "*see [[a*b]] now");
        assert_eq!(d.links[0].range, 5..12);
        // A link inside bold keeps both styles, with display ranges.
        let d = DisplayBlock::new("**[[Page]] and #tag**");
        assert_eq!(d.text, "[[Page]] and #tag");
        assert_eq!(d.links[0].range, 0..8);
        assert_eq!(d.links[1].range, 13..17);
        let link = Format {
            bold: true,
            link: true,
            ..Default::default()
        };
        let tag = Format {
            bold: true,
            tag: true,
            ..Default::default()
        };
        let bold = Format {
            bold: true,
            ..Default::default()
        };
        assert_eq!(
            d.segments(),
            vec![(0..8, link), (8..13, bold), (13..17, tag)]
        );
    }

    #[test]
    fn prefix_and_markers_map_both_ways() {
        // "## **Big** *x*": prefix 0..3, "**" 3..5, "Big" 5..8, "**" 8..10,
        // " " 10..11, "*" 11..12, "x" 12..13, "*" 13..14.
        let d = DisplayBlock::new("## **Big** *x*");
        assert_eq!(d.kind, BlockKind::Heading2);
        assert_eq!(d.text, "Big x");
        // Start of text: after the prefix and the opening markers.
        assert_eq!(d.to_source(0), 5);
        assert_eq!(d.to_source(1), 6);
        // Just after "Big": inside the bold, before the closing "**".
        assert_eq!(d.to_source(3), 8);
        // After the space: before the italic's opening "*".
        assert_eq!(d.to_source(4), 11);
        assert_eq!(d.to_source(5), 13);
        assert_eq!(d.to_source(99), 13, "past the end clamps");
        assert_eq!(d.to_display(0), 0);
        assert_eq!(d.to_display(6), 1);
        assert_eq!(d.to_display(9), 3, "hidden maps to the next visible");
        assert_eq!(d.to_display(14), 5);
        // No visible text at all.
        assert_eq!(DisplayBlock::new("# ").to_source(0), 2);
    }

    #[test]
    fn task_keyword_is_hidden_after_the_prefix() {
        let d = DisplayBlock::new("## DONE **Big** [[P]]");
        assert_eq!(
            (d.kind, d.task),
            (BlockKind::Heading2, Some(TaskState::Done))
        );
        assert_eq!(d.text, "Big [[P]]");
        assert_eq!(d.to_source(0), "## DONE **".len());
        assert_eq!(d.links[0].range, 4..9);
        let d = DisplayBlock::new("TODO");
        assert_eq!(
            (d.task, d.text.as_str(), d.to_source(0)),
            (Some(TaskState::Todo), "", 4)
        );
        // Not a keyword: shown as is.
        let d = DisplayBlock::new("TODOS");
        assert_eq!((d.task, d.text.as_str()), (None, "TODOS"));
    }

    #[test]
    fn mapping_is_grapheme_safe_next_to_markers() {
        let content = "é**😀ü**👍🏽*ñ*";
        let d = DisplayBlock::new(content);
        assert_eq!(d.text, "é😀ü👍🏽ñ");
        assert_eq!(show(content), "é[😀ü]👍🏽/ñ/");
        // Every display char boundary maps to a source char boundary and
        // back to the same place.
        for (i, _) in d.text.char_indices().chain([(d.text.len(), ' ')]) {
            let s = d.to_source(i);
            assert!(content.is_char_boundary(s), "{i} -> {s}");
            assert_eq!(d.to_display(s), i);
        }
        // After "ü" (inside the bold) the cursor is before the closing "**".
        let after_u = "é😀ü".len();
        assert_eq!(d.to_source(after_u), "é**😀ü".len());
    }
}
