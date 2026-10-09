//! Unlinked mentions (decision 45): blocks on other pages whose text names
//! a page (its title or an alias that resolves to it) without linking it,
//! and turning such a mention into a `[[link]]`.
//!
//! Matching ignores case and needs word boundaries (Unicode letters and
//! digits count as word characters, so "Rust" isn't found in "Rusty" or
//! "Trust"). Nothing that is already markup counts: links (`[[..]]`, also
//! `![[..]]` embeds), tags, block references, `{{macros}}`, inline code
//! and fenced code, URLs and markdown link targets, and property lines
//! (`alias::`, `tags::`, ...). Where a title and an alias overlap, the
//! longest name wins.

use crate::autotag::is_property_line;
use crate::code::fenced_ranges;
use crate::model::{page_aliases, parse_block_refs, parse_references, resolve_page, Page};
use std::ops::Range;

/// One block's unlinked mentions: byte ranges into its content.
#[derive(Clone, Debug, PartialEq)]
pub struct BlockMentions {
    pub block: usize,
    pub ranges: Vec<Range<usize>>,
}

/// One page's blocks that mention the target.
#[derive(Clone, Debug, PartialEq)]
pub struct MentionGroup {
    pub page: usize,
    pub blocks: Vec<BlockMentions>,
}

/// The names page `target` goes by: its title, and its aliases that
/// resolve to it (as "Linked from" counts them), longest first.
pub fn names(pages: &[Page], target: usize) -> Vec<String> {
    let Some(page) = pages.get(target) else {
        return Vec::new();
    };
    let mut names: Vec<String> = std::iter::once(page.title.clone())
        .chain(
            page_aliases(page)
                .into_iter()
                .filter(|a| resolve_page(pages, a) == Some(target)),
        )
        .filter(|n| !n.trim().is_empty())
        .collect();
    names.sort_by_key(|n| std::cmp::Reverse(n.chars().count()));
    names
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Byte ranges of `text` that are markup, where a name is not a mention.
pub fn masked(text: &str) -> Vec<Range<usize>> {
    let mut out: Vec<Range<usize>> = Vec::new();
    out.extend(parse_references(text).into_iter().map(|r| r.range));
    out.extend(parse_block_refs(text).into_iter().map(|(r, _)| r));
    out.extend(fenced_ranges(text));
    // `{{embed [[x]]}}`, `{{query #x}}`, any macro.
    let mut from = 0;
    while let Some(start) = text[from..].find("{{").map(|i| from + i) {
        let end = text[start..]
            .find("}}")
            .map_or(text.len(), |i| start + i + 2);
        out.push(start..end);
        from = end;
    }
    // Inline code: a run of backticks up to the next run of the same length.
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'`' {
            let n = bytes[i..].iter().take_while(|&&b| b == b'`').count();
            let fence = &text[i..i + n];
            if let Some(close) = text[i + n..].find(fence) {
                let end = i + n + close + n;
                out.push(i..end);
                i = end;
                continue;
            }
            i += n;
            continue;
        }
        i += 1;
    }
    // URLs (to the next whitespace) and markdown link targets `](...)`.
    for scheme in ["http://", "https://", "file://", "mailto:", "www."] {
        let mut from = 0;
        while let Some(start) = text[from..].find(scheme).map(|i| from + i) {
            let end = text[start..]
                .find(char::is_whitespace)
                .map_or(text.len(), |i| start + i);
            out.push(start..end);
            from = end.max(start + scheme.len());
        }
    }
    let mut from = 0;
    while let Some(start) = text[from..].find("](").map(|i| from + i) {
        let end = text[start..]
            .find(')')
            .map_or(text.len(), |i| start + i + 1);
        out.push(start..end);
        from = end;
    }
    // Property lines.
    let mut line_start = 0;
    for line in text.split('\n') {
        if is_property_line(line) {
            out.push(line_start..line_start + line.len());
        }
        line_start += line.len() + 1;
    }
    out
}

/// Where `name` matches `text` at byte `at`, ignoring case: the end.
fn match_at(text: &str, at: usize, name: &str) -> Option<usize> {
    let mut rest = text[at..].char_indices();
    let mut end = at;
    for n in name.chars() {
        let (i, c) = rest.next()?;
        if c != n && !c.to_lowercase().eq(n.to_lowercase()) {
            return None;
        }
        end = at + i + c.len_utf8();
    }
    Some(end)
}

/// The unlinked mentions of any of `names` (longest first) in `text`.
pub fn find(text: &str, names: &[String]) -> Vec<Range<usize>> {
    let lower = text.to_lowercase();
    let present: Vec<&String> = names
        .iter()
        .filter(|n| lower.contains(&n.to_lowercase()))
        .collect();
    if present.is_empty() {
        return Vec::new();
    }
    let masked = masked(text);
    let mut out = Vec::new();
    let mut prev: Option<char> = None;
    let mut pos = 0;
    while pos < text.len() {
        let c = text[pos..].chars().next().unwrap_or('\0');
        let found = present.iter().find_map(|name| {
            let first = name.chars().next()?;
            if is_word(first) && prev.is_some_and(is_word) {
                return None;
            }
            let end = match_at(text, pos, name)?;
            let last = name.chars().last()?;
            if is_word(last) && text[end..].chars().next().is_some_and(is_word) {
                return None;
            }
            let range = pos..end;
            let hidden = masked
                .iter()
                .any(|m| m.start < range.end && range.start < m.end);
            (!hidden).then_some(range)
        });
        match found {
            Some(range) => {
                prev = text[..range.end].chars().last();
                pos = range.end;
                out.push(range);
            }
            None => {
                prev = Some(c);
                pos += c.len_utf8();
            }
        }
    }
    out
}

/// Every unlinked mention of page `target` on the other pages, grouped by
/// page in `pages` order. Blocks that already link it (they are in
/// "Linked from") are left out.
pub fn unlinked_mentions(pages: &[Page], target: usize) -> Vec<MentionGroup> {
    let names = names(pages, target);
    if names.is_empty() {
        return Vec::new();
    }
    let lower: Vec<String> = names.iter().map(|n| n.to_lowercase()).collect();
    let title = pages[target].title.to_lowercase();
    let mut groups = Vec::new();
    for (p, page) in pages.iter().enumerate() {
        if p == target || page.title.to_lowercase() == title {
            continue;
        }
        let blocks: Vec<BlockMentions> = page
            .blocks
            .iter()
            .enumerate()
            .filter(|(_, b)| {
                !parse_references(&b.content)
                    .iter()
                    .any(|r| lower.contains(&r.target.to_lowercase()))
            })
            .filter_map(|(i, b)| {
                let ranges = find(&b.content, &names);
                (!ranges.is_empty()).then_some(BlockMentions { block: i, ranges })
            })
            .collect();
        if !blocks.is_empty() {
            groups.push(MentionGroup { page: p, blocks });
        }
    }
    groups
}

/// The link that replaces the mention `matched` of page `target`: the
/// text as written when a link to it resolves to that page (titles match
/// ignoring case, aliases resolve, `model::resolve_page`), else the title.
/// (`[[Title|label]]` isn't a link form notesec reads.)
pub fn link_for(pages: &[Page], target: usize, matched: &str) -> String {
    if resolve_page(pages, matched) == Some(target) && !matched.contains(['[', ']']) {
        format!("[[{matched}]]")
    } else {
        format!("[[{}]]", pages[target].title)
    }
}

/// `content` with the mentions at `ranges` (unlinked mentions of
/// `target`) turned into links.
pub fn link_ranges(
    pages: &[Page],
    target: usize,
    content: &str,
    ranges: &[Range<usize>],
) -> String {
    let mut out = content.to_string();
    let mut sorted: Vec<&Range<usize>> = ranges.iter().collect();
    sorted.sort_by_key(|r| std::cmp::Reverse(r.start));
    for r in sorted {
        if let Some(matched) = content.get(r.clone()) {
            out.replace_range(r.clone(), &link_for(pages, target, matched));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn found<'a>(text: &'a str, names: &[&str]) -> Vec<&'a str> {
        find(text, &n(names))
            .into_iter()
            .map(|r| &text[r])
            .collect()
    }

    #[test]
    fn matches_whole_words_ignoring_case() {
        assert_eq!(
            found("I like rust. RUST! Rust's", &["Rust"]),
            vec!["rust", "RUST", "Rust"]
        );
        assert!(found("Rusty trust rusting", &["Rust"]).is_empty());
        assert_eq!(found("rust_lang? no; (rust)", &["Rust"]), vec!["rust"]);
        // Unicode: letters around it are word characters.
        assert!(found("Größeres", &["Größe"]).is_empty());
        assert_eq!(found("die GRÖSSE, die Größe.", &["Größe"]), vec!["Größe"]);
        assert_eq!(found("Über çay içtim", &["Çay"]), vec!["çay"]);
        // Names that start or end with punctuation, and dates.
        assert_eq!(found("I write C++ daily", &["C++"]), vec!["C++"]);
        assert_eq!(
            found("see 2026-10-09 and 2026-10-091", &["2026-10-09"]),
            vec!["2026-10-09"]
        );
        assert!(found("", &["Rust"]).is_empty());
    }

    #[test]
    fn markup_is_not_a_mention() {
        let skipped = [
            "[[Rust]] and [[rust book]]",
            "#rust #[[Rust]]",
            "![[Rust]]",
            "{{embed [[Rust]]}} {{query #rust}} {{video rust}}",
            "`rust` and ``a `rust` b``",
            "```\nrust\n```",
            "https://rust.org/rust www.rust.dev",
            "[site](http://x/rust) [doc](rust)",
            "alias:: Rust\ntags:: rust",
        ];
        for text in skipped {
            assert!(found(text, &["Rust"]).is_empty(), "{text:?}");
        }
        let id = "6f9b2c1e-0000-4000-8000-0000000000e1";
        assert!(found(&format!("(({id}))"), &[id]).is_empty());
        // Plain text around markup still counts.
        assert_eq!(
            found("[[Go]] vs Rust; `x` rust", &["Rust"]),
            vec!["Rust", "rust"]
        );
        assert_eq!(found("[Rust docs](http://x)", &["Rust"]), vec!["Rust"]);
        assert_eq!(found("note: rust\nalias:: x", &["Rust"]), vec!["rust"]);
    }

    #[test]
    fn the_longest_name_wins_where_they_overlap() {
        let names = n(&["Rust lang", "Rust"]);
        let text = "Rust lang and rust";
        let got: Vec<&str> = find(text, &names).into_iter().map(|r| &text[r]).collect();
        assert_eq!(got, vec!["Rust lang", "rust"]);
    }

    #[test]
    fn mentions_skip_the_page_itself_and_blocks_that_link_it() {
        let pages = vec![
            Page::from_markdown("JavaScript", false, "- alias:: JS\n- JavaScript is mine\n"),
            Page::from_markdown(
                "Notes",
                false,
                "- learning js and javascript\n- [[JS]] plus javascript\n- nothing\n",
            ),
            Page::from_markdown("2026-10-09", true, "- wrote JavaScript\n"),
            Page::from_markdown("Other", false, "- alias:: Web\n"),
        ];
        let groups = unlinked_mentions(&pages, 0);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].page, 1);
        assert_eq!(
            groups[0].blocks.len(),
            1,
            "the block linking [[JS]] is in Linked from"
        );
        assert_eq!(groups[0].blocks[0].block, 0);
        assert_eq!(groups[0].blocks[0].ranges.len(), 2);
        assert_eq!(groups[1].page, 2);
        // A journal title is a name like any other.
        let pages2 = vec![
            Page::from_markdown("2026-10-09", true, "- day\n"),
            Page::from_markdown("P", false, "- on 2026-10-09 we met\n"),
        ];
        assert_eq!(
            unlinked_mentions(&pages2, 0)[0].blocks[0].ranges,
            vec![3..13]
        );
        // An alias another page owns by title isn't this page's name.
        let pages3 = vec![
            Page::from_markdown("JavaScript", false, "- alias:: Web\n"),
            Page::from_markdown("Web", false, "- x\n"),
            Page::from_markdown("P", false, "- the web\n"),
        ];
        assert_eq!(names(&pages3, 0), n(&["JavaScript"]));
        assert!(unlinked_mentions(&pages3, 0).is_empty());
    }

    #[test]
    fn links_keep_the_written_text_when_it_resolves() {
        let pages = vec![
            Page::from_markdown("JavaScript", false, "- alias:: JS\n"),
            Page::from_markdown("P", false, "- x\n"),
        ];
        assert_eq!(link_for(&pages, 0, "javascript"), "[[javascript]]");
        assert_eq!(link_for(&pages, 0, "js"), "[[js]]");
        assert_eq!(link_for(&pages, 0, "ECMAScript"), "[[JavaScript]]");
        let text = "js and JavaScript";
        let ranges = find(text, &names(&pages, 0));
        assert_eq!(
            link_ranges(&pages, 0, text, &ranges),
            "[[js]] and [[JavaScript]]"
        );
        assert_eq!(
            link_ranges(&pages, 0, text, &ranges[1..]),
            "js and [[JavaScript]]"
        );
    }
}
