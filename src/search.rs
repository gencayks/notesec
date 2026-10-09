//! Fuzzy search over page titles and block contents.
//!
//! Matching is "subsequence" style, like most command palettes: every character
//! of the query must appear in the text in order, but not necessarily next to
//! each other (`fbr` matches `Foo Bar`). Matches are scored so that tighter,
//! earlier, word-aligned matches rank higher.
//!
//! Global search (Ctrl+Shift+F, `search_text`) is different: plain
//! substring matching, ignoring case, over every line of every page and
//! journal, so it finds exact words rather than scattered letters.
//!
//! This module is pure (no UI, no I/O), so it is easy to unit-test.

use crate::commands::Command;
use crate::model::{page_aliases, resolve_page, Page};
use std::ops::Range;
use uuid::Uuid;

/// Only the first this-many characters of a block are searched. Keeps the cost
/// of one keystroke bounded even if a block holds a pasted wall of text.
const MAX_SEARCHED_CHARS: usize = 200;

/// Score `query` against `text`. `None` means "no match"; higher is better.
///
/// Matching ignores case. An empty query matches everything with score 0.
pub fn fuzzy_score(query: &str, text: &str) -> Option<i32> {
    let q: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
    if q.is_empty() {
        return Some(0);
    }
    // Keep original characters too: word boundaries are detected on those.
    let orig: Vec<char> = text.chars().take(MAX_SEARCHED_CHARS).collect();
    let t: Vec<char> = orig.iter().flat_map(|c| c.to_lowercase()).collect();
    // Lowercasing can change length for exotic characters (e.g. 'İ'); bail out
    // of word-boundary logic rather than index out of range.
    if t.len() != orig.len() {
        return subsequence_only(&q, &t);
    }

    // Cheap rejection first: is the query a subsequence at all?
    subsequence_only(&q, &t)?;

    // Bonus for a match landing at the start of the text or of a word.
    let bonus = |i: usize| -> i32 {
        if i == 0 {
            8
        } else if !orig[i - 1].is_alphanumeric() {
            6
        } else {
            0
        }
    };
    const MATCH: i32 = 10; // every matched character
    const CONSECUTIVE: i32 = 8; // matched right after the previous match

    // best[i] = best score for matching q[..=j] with q[j] landing on t[i].
    // We only keep one query row at a time ("rolling" DP).
    const NONE: i32 = i32::MIN / 2;
    let n = t.len();
    let mut prev = vec![NONE; n];
    for i in 0..n {
        if t[i] == q[0] {
            // Leading gap costs 1 per skipped character.
            prev[i] = MATCH + bonus(i) - i as i32;
        }
    }
    for &qc in &q[1..] {
        let mut cur = vec![NONE; n];
        // Best of `prev[k] + k` over all k that are at least two before i;
        // a gap of g skipped characters then costs `g` = i - k - 1.
        let mut best_gapped = NONE;
        for i in 1..n {
            if i >= 2 && prev[i - 2] > NONE {
                best_gapped = best_gapped.max(prev[i - 2] + (i as i32 - 2));
            }
            if t[i] != qc {
                continue;
            }
            let mut best_before = NONE;
            if prev[i - 1] > NONE {
                best_before = prev[i - 1] + CONSECUTIVE;
            }
            if best_gapped > NONE {
                best_before = best_before.max(best_gapped - (i as i32 - 1));
            }
            if best_before > NONE {
                cur[i] = best_before + MATCH + bonus(i);
            }
        }
        prev = cur;
    }
    prev.into_iter().filter(|&s| s > NONE).max()
}

/// Plain "is `q` a subsequence of `t`" check, returning a dummy score.
fn subsequence_only(q: &[char], t: &[char]) -> Option<i32> {
    let mut qi = 0;
    for &c in t {
        if qi < q.len() && c == q[qi] {
            qi += 1;
        }
    }
    (qi == q.len()).then_some(0)
}

impl Command {
    /// How well `query` matches this command: its label, or failing that
    /// (or if better) one of its keywords at a penalty.
    fn score(self, query: &str) -> Option<i32> {
        let label = fuzzy_score(query, self.label());
        let keyword = self
            .keywords()
            .iter()
            .filter_map(|k| fuzzy_score(query, k))
            .max()
            .map(|score| score - KEYWORD_PENALTY);
        label.max(keyword)
    }
}

/// What a search result points at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// A page, by index into the `pages` slice.
    Page(usize),
    /// A block: (page index, block index within that page).
    Block(usize, usize),
    Command(Command),
    /// A plugin command, by index into the labels given to `search_with`
    /// (decision 55).
    Plugin(usize),
    /// A template, by index into the list given to `search_templates`.
    Template(usize),
    /// A global search match: the byte range `start..end` of the block's
    /// `content` (page and block are indices, as in `Block`).
    Match {
        page: usize,
        block: usize,
        start: usize,
        end: usize,
    },
}

impl Target {
    pub fn is_command(&self) -> bool {
        matches!(self, Target::Command(_) | Target::Plugin(_))
    }
}

/// One search result.
#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub target: Target,
    pub score: i32,
}

/// Extra score for title matches so a page named `Foo` beats a block that
/// merely mentions foo.
const TITLE_BONUS: i32 = 25;
/// Commands rank just below title matches but above block text.
const COMMAND_BONUS: i32 = 20;
/// Taken off a command's score when only one of its keywords matched.
const KEYWORD_PENALTY: i32 = 40;

/// Search all pages and the given `commands` (the ones that make sense
/// right now; see `commands::Needs`).
///
/// With an empty query every page is listed (at most `limit`, in the given
/// order) so the palette is useful as a page switcher before you type
/// anything, followed by every command (not limited) so it doubles as a
/// list of what the app can do.
///
/// Otherwise the best `limit` hits are kept and returned in two groups,
/// pages and blocks, and commands (the palette puts a header over each),
/// best first within each group; the group holding the best hit comes
/// first, so Enter still runs the top match.
#[cfg(test)]
pub fn search(pages: &[Page], commands: &[Command], query: &str, limit: usize) -> Vec<Hit> {
    search_with(pages, commands, &[], query, limit)
}

/// `search`, with plugin commands (`plugins`: their labels) after the
/// built-in ones.
pub fn search_with(
    pages: &[Page],
    commands: &[Command],
    plugins: &[String],
    query: &str,
    limit: usize,
) -> Vec<Hit> {
    let query = query.trim();
    if query.is_empty() {
        let pages = (0..pages.len().min(limit)).map(|page| Hit {
            target: Target::Page(page),
            score: 0,
        });
        let commands = commands.iter().map(|&command| Hit {
            target: Target::Command(command),
            score: 0,
        });
        let plugins = (0..plugins.len()).map(|i| Hit {
            target: Target::Plugin(i),
            score: 0,
        });
        return pages.chain(commands).chain(plugins).collect();
    }
    let mut hits = Vec::new();
    for (i, label) in plugins.iter().enumerate() {
        if let Some(score) = fuzzy_score(query, label) {
            hits.push(Hit {
                target: Target::Plugin(i),
                score: score + COMMAND_BONUS,
            });
        }
    }

    for &command in commands {
        if let Some(score) = command.score(query) {
            hits.push(Hit {
                target: Target::Command(command),
                score: score + COMMAND_BONUS,
            });
        }
    }
    for (page_ix, page) in pages.iter().enumerate() {
        if let Some(score) = fuzzy_score(query, &page.title) {
            hits.push(Hit {
                target: Target::Page(page_ix),
                score: score + TITLE_BONUS,
            });
        }
        for (block_ix, block) in page.blocks.iter().enumerate() {
            if block.content.is_empty() {
                continue;
            }
            if let Some(score) = fuzzy_score(query, &block.content) {
                hits.push(Hit {
                    target: Target::Block(page_ix, block_ix),
                    score,
                });
            }
        }
    }
    // Stable sort: equal scores keep page/document order.
    hits.sort_by(|a, b| b.score.cmp(&a.score));
    hits.truncate(limit);
    // Group (stable, so each group stays best first).
    let commands_first = hits.first().is_some_and(|h| h.target.is_command());
    hits.sort_by_key(|h| h.target.is_command() != commands_first);
    hits
}

/// Blocks for the `((` reference picker, as `(page, block)` indices: every
/// non-empty block whose text fuzzy-matches `query`, best first (ties keep
/// page and document order), except the block `exclude` (the one being
/// edited, which can't usefully reference itself).
pub fn search_blocks(
    pages: &[Page],
    query: &str,
    exclude: Option<Uuid>,
    limit: usize,
) -> Vec<(usize, usize)> {
    let mut hits: Vec<(i32, usize, usize)> = Vec::new();
    for (page_ix, page) in pages.iter().enumerate() {
        for (block_ix, block) in page.blocks.iter().enumerate() {
            if block.content.trim().is_empty() || Some(block.id) == exclude {
                continue;
            }
            if let Some(score) = fuzzy_score(query, &block.content) {
                hits.push((score, page_ix, block_ix));
            }
        }
    }
    hits.sort_by(|a, b| b.0.cmp(&a.0));
    hits.into_iter()
        .take(limit)
        .map(|(_, page, block)| (page, block))
        .collect()
}

/// Filter template names for the palette's "Insert template" step. An empty
/// query lists every template in the given order; otherwise names are fuzzy
/// matched like page titles, best first.
pub fn search_templates(names: &[&str], query: &str, limit: usize) -> Vec<Hit> {
    let query = query.trim();
    let mut hits: Vec<Hit> = names
        .iter()
        .enumerate()
        .filter_map(|(ix, name)| {
            fuzzy_score(query, name).map(|score| Hit {
                target: Target::Template(ix),
                score,
            })
        })
        .collect();
    // Stable sort, so an empty query (all scores 0) keeps the given order.
    hits.sort_by(|a, b| b.score.cmp(&a.score));
    hits.truncate(limit);
    hits
}

/// The byte range of the first occurrence of `needle` in `haystack`,
/// ignoring case (character by character, so offsets always fall on
/// `haystack`'s own char boundaries even where lowercasing would change a
/// character's length). An empty needle matches nothing.
pub fn find_ignore_case(haystack: &str, needle: &str) -> Option<Range<usize>> {
    if needle.is_empty() {
        return None;
    }
    let same = |a: char, b: char| a == b || a.to_lowercase().eq(b.to_lowercase());
    'start: for (start, _) in haystack.char_indices() {
        let mut rest = haystack[start..].char_indices();
        let mut end = start;
        for n in needle.chars() {
            match rest.next() {
                Some((i, h)) if same(h, n) => end = start + i + h.len_utf8(),
                _ => continue 'start,
            }
        }
        return Some(start..end);
    }
    None
}

/// How a page title matches a global search query: the whole title, its
/// start, or somewhere inside. Higher ranks first.
fn title_rank(title: &str, query: &str) -> Option<i32> {
    let range = find_ignore_case(title, query)?;
    Some(if range == (0..title.len()) {
        3
    } else if range.start == 0 {
        2
    } else {
        1
    })
}

/// Global search: every page (journals included) whose title, or one of
/// whose aliases (`alias::`, where it resolves to that page), contains
/// `query`, then every line of every block that contains it, ignoring case.
/// A page is listed once, ranked by its best-matching name.
///
/// Title matches come first, whole-title matches before prefix matches
/// before the rest; body matches follow in page and document order, one
/// per matching line (its first occurrence), as `Target::Match`. At most
/// `limit` results; an empty (or all-blank) query finds nothing.
pub fn search_text(pages: &[Page], query: &str, limit: usize) -> Vec<Hit> {
    let query = query.trim();
    if query.is_empty() {
        return Vec::new();
    }
    let mut hits: Vec<Hit> = pages
        .iter()
        .enumerate()
        .filter_map(|(page, p)| {
            let alias_rank = page_aliases(p)
                .iter()
                .filter(|a| resolve_page(pages, a) == Some(page))
                .filter_map(|a| title_rank(a, query))
                .max();
            title_rank(&p.title, query)
                .max(alias_rank)
                .map(|score| Hit {
                    target: Target::Page(page),
                    score,
                })
        })
        .collect();
    // Stable: equal ranks keep page order.
    hits.sort_by(|a, b| b.score.cmp(&a.score));
    'pages: for (page, p) in pages.iter().enumerate() {
        for (block, b) in p.blocks.iter().enumerate() {
            let mut offset = 0;
            for line in b.content.split('\n') {
                if hits.len() >= limit {
                    break 'pages;
                }
                if let Some(r) = find_ignore_case(line, query) {
                    hits.push(Hit {
                        target: Target::Match {
                            page,
                            block,
                            start: offset + r.start,
                            end: offset + r.end,
                        },
                        score: 0,
                    });
                }
                offset += line.len() + 1;
            }
        }
    }
    hits.truncate(limit);
    hits
}

/// Pages for the `[[` link picker, best first: each page whose title or
/// alias fuzzy-matches `query`, once, with the alias when that is what
/// matched better (the picker shows it, and inserts the page's real
/// title). An alias that resolves to another page (a real title, or an
/// earlier claim) is skipped. With an empty query, the first `limit`
/// pages in the given order.
pub fn search_link_pages(
    pages: &[Page],
    query: &str,
    limit: usize,
) -> Vec<(usize, Option<String>)> {
    let query = query.trim();
    let mut hits: Vec<(i32, usize, Option<String>)> = Vec::new();
    for (ix, page) in pages.iter().enumerate() {
        let mut best = fuzzy_score(query, &page.title).map(|s| (s, None));
        for alias in page_aliases(page) {
            if resolve_page(pages, &alias) != Some(ix) {
                continue;
            }
            if let Some(score) = fuzzy_score(query, &alias) {
                if best.as_ref().is_none_or(|(b, _)| score > *b) {
                    best = Some((score, Some(alias)));
                }
            }
        }
        if let Some((score, alias)) = best {
            hits.push((score, ix, alias));
        }
    }
    // Stable: ties (and an empty query) keep page order.
    hits.sort_by(|a, b| b.0.cmp(&a.0));
    hits.into_iter()
        .take(limit)
        .map(|(_, ix, alias)| (ix, alias))
        .collect()
}

/// What a global search result shows for a match: the line of `content`
/// holding `range`, without its indentation, and cut to about `max`
/// characters around the match ("…" marks a cut end). Returns that text and
/// where the match is in it.
pub fn snippet(content: &str, range: Range<usize>, max: usize) -> (String, Range<usize>) {
    let line_start = content[..range.start].rfind('\n').map_or(0, |i| i + 1);
    let line_end = content[range.end..]
        .find('\n')
        .map_or(content.len(), |i| range.end + i);
    let line = &content[line_start..line_end];
    let indent = line.len() - line.trim_start().len();
    // The match itself is never cut.
    let mut from = (line_start + indent).min(range.start);
    let mut to = line_end;
    let chars = |a: usize, b: usize| content[a..b].chars().count();
    let matched = chars(range.start, range.end);
    if chars(from, to) > max {
        // A third of the room before the match, the rest after it.
        let room = max.saturating_sub(matched);
        let before = (room / 3).min(chars(from, range.start));
        let after = room - before;
        from = content[..range.start]
            .char_indices()
            .rev()
            .take(before)
            .last()
            .map_or(range.start, |(i, _)| i)
            .max(from);
        to = content[range.end..]
            .char_indices()
            .nth(after)
            .map_or(line_end, |(i, _)| range.end + i)
            .min(line_end);
    }
    let lead = if from > line_start + indent {
        "…"
    } else {
        ""
    };
    let tail = if to < line_end { "…" } else { "" };
    let text = format!("{lead}{}{tail}", &content[from..to]);
    let start = lead.len() + range.start - from;
    (text, start..start + range.end - range.start)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(q: &str, t: &str) -> Option<i32> {
        fuzzy_score(q, t)
    }

    #[test]
    fn subsequence_required() {
        assert!(s("fbr", "Foo Bar").is_some());
        assert!(s("rbf", "Foo Bar").is_none());
        assert!(s("xyz", "Foo Bar").is_none());
        assert!(s("", "anything").is_some());
        assert!(s("a", "").is_none());
    }

    #[test]
    fn ignores_case_and_handles_unicode() {
        assert!(s("FOO", "foo").is_some());
        assert!(s("ü", "Über").is_some());
        assert!(s("é", "café").is_some());
        // Characters whose lowercase form is longer must not panic.
        assert!(s("i", "İstanbul").is_some());
    }

    #[test]
    fn better_matches_rank_higher() {
        let exact = s("foo", "foo").unwrap();
        let prefix = s("foo", "foobar").unwrap();
        let later = s("foo", "a foo").unwrap();
        let scattered = s("foo", "f-o-o").unwrap();
        assert!(exact >= prefix);
        assert!(prefix > later, "start of text beats middle");
        assert!(later > scattered, "contiguous beats scattered");
    }

    #[test]
    fn prefers_word_starts() {
        // "fb" lands on word starts in "foo bar" but mid-word in "xfxb".
        assert!(s("fb", "foo bar").unwrap() > s("fb", "xfxb").unwrap());
    }

    #[test]
    fn finds_best_placement_not_first() {
        // A greedy left-to-right scan would match the scattered 'a' ... 'b'
        // at the start; the best match is the contiguous "ab" at the end.
        let best = s("ab", "axxxxxxxb ab").unwrap();
        let only_scattered = s("ab", "axxxxxxxb").unwrap();
        assert!(best > only_scattered);
    }

    fn pages() -> Vec<Page> {
        vec![
            Page::from_markdown("Alpha", false, "- first\n- needle in haystack\n"),
            Page::from_markdown("Needle", false, "- unrelated\n"),
            Page::from_markdown("Zed", false, "- other\n-\n"),
        ]
    }

    #[test]
    fn title_hit_outranks_block_hit() {
        let hits = search(&pages(), Command::ALL, "needle", 10);
        assert_eq!(hits[0].target, Target::Page(1));
        assert_eq!(hits[1].target, Target::Block(0, 1));
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn commands_match_by_label() {
        let hits = search(&pages(), Command::ALL, "theme", 10);
        assert_eq!(hits[0].target, Target::Command(Command::ToggleTheme));
        // Several commands share the word "font".
        let font: Vec<Target> = search(&pages(), Command::ALL, "font", 10)
            .into_iter()
            .map(|h| h.target)
            .collect();
        assert!(font.contains(&Target::Command(Command::IncreaseFont)));
        assert!(font.contains(&Target::Command(Command::ResetFont)));
        // An empty query lists only pages, never commands.
        // An empty query lists the pages first, then every command.
        let empty = search(&pages(), Command::ALL, "", 10);
        assert_eq!(empty[0].target, Target::Page(0));
        assert_eq!(
            empty.iter().filter(|h| h.target.is_command()).count(),
            Command::ALL.len()
        );
    }

    #[test]
    fn empty_query_lists_pages_and_limit_applies_to_pages() {
        let hits = search(&pages(), &[], "  ", 2);
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|h| matches!(h.target, Target::Page(_))));
        let hits = search(&pages(), Command::ALL, "  ", 2);
        assert_eq!(hits.len(), 2 + Command::ALL.len());
    }

    #[test]
    fn insert_template_is_a_command() {
        let hits = search(&pages(), Command::ALL, "insert template", 10);
        assert_eq!(hits[0].target, Target::Command(Command::InsertTemplate));
    }

    #[test]
    fn sort_pages_command_is_found_by_label_and_keywords() {
        let top = |q: &str| search(&pages(), Command::ALL, q, 10)[0].target;
        assert_eq!(top("sort pages"), Target::Command(Command::SortPagesAz));
        assert_eq!(top("alphabetical"), Target::Command(Command::SortPagesAz));
    }

    #[test]
    fn settings_command_is_found_by_its_keywords() {
        let top = |q: &str| search(&pages(), Command::ALL, q, 10)[0].target;
        assert_eq!(top("settings"), Target::Command(Command::OpenSettings));
        assert_eq!(top("open settings"), Target::Command(Command::OpenSettings));
        assert_eq!(top("preferences"), Target::Command(Command::OpenSettings));
        for q in ["theme", "font"] {
            let targets: Vec<Target> = search(&pages(), Command::ALL, q, 10)
                .into_iter()
                .map(|h| h.target)
                .collect();
            assert!(
                targets.contains(&Target::Command(Command::OpenSettings)),
                "{q}"
            );
        }
        // A keyword match ranks below a label match.
        assert_eq!(top("theme"), Target::Command(Command::ToggleTheme));
    }

    #[test]
    fn template_search_lists_all_then_filters() {
        let names = ["Book notes", "Daily review", "Meeting"];
        let all: Vec<Target> = search_templates(&names, "", 10)
            .into_iter()
            .map(|h| h.target)
            .collect();
        assert_eq!(
            all,
            [
                Target::Template(0),
                Target::Template(1),
                Target::Template(2)
            ]
        );
        let daily = search_templates(&names, "daily", 10);
        assert_eq!(daily.len(), 1);
        assert_eq!(daily[0].target, Target::Template(1));
        assert!(search_templates(&names, "zzz", 10).is_empty());
        assert_eq!(search_templates(&names, "", 2).len(), 2);
    }

    #[test]
    fn no_match_is_empty() {
        assert!(search(&pages(), Command::ALL, "qqqq", 10).is_empty());
    }

    #[test]
    fn empty_query_lists_every_command_after_the_pages() {
        let hits = search(&pages(), Command::ALL, "", 12);
        let (pages, commands): (Vec<_>, Vec<_>) =
            hits.into_iter().partition(|h| !h.target.is_command());
        assert_eq!(pages.len(), 3);
        assert_eq!(commands.len(), Command::ALL.len());
    }

    #[test]
    fn fuzzy_query_groups_commands_when_they_rank_best() {
        let hits = search(&pages(), Command::ALL, "agnd", 12);
        assert_eq!(hits[0].target, Target::Command(Command::OpenAgenda));
        // Commands and pages/blocks each come as one group, whichever
        // holds the best hit first.
        for (query, commands_first) in [
            ("zed", false),
            ("toggle", true),
            ("needle", false),
            ("e", true),
        ] {
            let hits = search(&pages(), Command::ALL, query, 12);
            assert_eq!(hits[0].target.is_command(), commands_first, "{query}");
            let switches = hits
                .windows(2)
                .filter(|w| w[0].target.is_command() != w[1].target.is_command())
                .count();
            assert!(switches <= 1, "{query}");
        }
        // "col all" finds Collapse all (subsequence across the label).
        assert_eq!(
            search(&pages(), Command::ALL, "col all", 12)[0].target,
            Target::Command(Command::CollapseAll)
        );
        assert_eq!(
            search(&pages(), Command::ALL, "shrt", 12)[0].target,
            Target::Command(Command::ShowShortcuts)
        );
    }

    #[test]
    fn only_the_commands_offered_are_found() {
        let offered = [Command::OpenAgenda];
        let targets: Vec<Target> = search(&pages(), &offered, "", 20)
            .into_iter()
            .map(|h| h.target)
            .collect();
        assert!(targets.contains(&Target::Command(Command::OpenAgenda)));
        assert!(!targets.contains(&Target::Command(Command::NewPage)));
    }

    fn page(title: &str, markdown: &str) -> Page {
        Page::from_markdown(title, false, markdown)
    }

    #[test]
    fn find_ignore_case_returns_original_offsets() {
        assert_eq!(find_ignore_case("Hello World", "world"), Some(6..11));
        assert_eq!(find_ignore_case("ÜBER über", "über"), Some(0..5));
        assert_eq!(find_ignore_case("aÄb", "äB"), Some(1..4));
        assert_eq!(find_ignore_case("abc", "abcd"), None);
        assert_eq!(find_ignore_case("abc", ""), None);
        assert_eq!(find_ignore_case("aab", "ab"), Some(1..3));
    }

    #[test]
    fn global_search_ranks_titles_above_body_matches() {
        let pages = [
            page("Notes on rust", "- nothing here\n"),
            page("Garden", "- water the plants\n- Rust on the shed\n"),
            page("Rust", "- the language\n"),
            page("Rusty tools", "- old saw\n  rust everywhere\n"),
        ];
        let hits = search_text(&pages, "  rust ", 20);
        let targets: Vec<Target> = hits.iter().map(|h| h.target).collect();
        assert_eq!(
            targets,
            vec![
                Target::Page(2), // whole title
                Target::Page(3), // title prefix
                Target::Page(0), // inside the title
                Target::Match {
                    page: 1,
                    block: 1,
                    start: 0,
                    end: 4
                },
                // Second line of a multi-line block: offset into content.
                Target::Match {
                    page: 3,
                    block: 0,
                    start: 8,
                    end: 12
                },
            ]
        );
        assert_eq!(&pages[3].blocks[0].content[8..12], "rust");
        assert!(search_text(&pages, "   ", 20).is_empty());
        assert!(search_text(&pages, "nowhere at all", 20).is_empty());
        assert_eq!(search_text(&pages, "rust", 2).len(), 2);
    }

    #[test]
    fn global_search_is_a_substring_search_one_hit_per_line() {
        let pages = [page("A", "- ab ab\n  xab\n- a b\n")];
        let hits = search_text(&pages, "ab", 20);
        // Not fuzzy: "a b" doesn't match; one hit per line, first occurrence.
        assert_eq!(
            hits.iter().map(|h| h.target).collect::<Vec<_>>(),
            vec![
                Target::Match {
                    page: 0,
                    block: 0,
                    start: 0,
                    end: 2
                },
                Target::Match {
                    page: 0,
                    block: 0,
                    start: 7,
                    end: 9
                },
            ]
        );
    }

    #[test]
    fn snippet_shows_the_matching_line_around_the_match() {
        let content = "first line\n    second line with the needle in it\nthird";
        let start = content.find("needle").unwrap();
        let (text, r) = snippet(content, start..start + 6, 80);
        assert_eq!(text, "second line with the needle in it");
        assert_eq!(&text[r], "needle");

        // Long lines are cut around the match, marked with "…".
        let long = format!("{} needle {}", "é".repeat(100), "x".repeat(100));
        let start = long.find("needle").unwrap();
        let (text, r) = snippet(&long, start..start + 6, 40);
        assert_eq!(&text[r.clone()], "needle");
        assert!(text.starts_with('…') && text.ends_with('…'), "{text}");
        assert!(text.chars().count() <= 42, "{text}");
        assert_eq!(
            text[..r.start].chars().count(),
            1 + 11,
            "a third of the room before"
        );

        // A match at the very start of a long line: nothing cut before it.
        let start_hit = format!("needle {}", "y".repeat(200));
        let (text, r) = snippet(&start_hit, 0..6, 30);
        assert_eq!(r, 0..6);
        assert!(text.starts_with("needle") && text.ends_with('…'));
    }

    #[test]
    fn global_search_finds_pages_by_alias() {
        let pages = [
            page("JavaScript", "- alias:: JS, Web\n- body\n"),
            page("Web", "- the real web page\n"),
            page("Notes", "- js tips\n"),
        ];
        let hits = search_text(&pages, "js", 20);
        let targets: Vec<Target> = hits.iter().map(|h| h.target).collect();
        // The alias is a whole-name match, ranked like a title; the alias
        // line itself and Notes' text are ordinary line matches.
        assert_eq!(
            targets,
            vec![
                Target::Page(0),
                Target::Match {
                    page: 0,
                    block: 0,
                    start: 8,
                    end: 10
                },
                Target::Match {
                    page: 2,
                    block: 0,
                    start: 0,
                    end: 2
                },
            ]
        );
        // "Web" is a real page: JavaScript's alias doesn't claim it.
        let hits = search_text(&pages, "web", 20);
        assert_eq!(hits[0].target, Target::Page(1));
        assert!(!hits.iter().any(|h| h.target == Target::Page(0)));
    }

    #[test]
    fn link_picker_matches_titles_and_aliases_once() {
        let pages = [
            page("JavaScript", "- alias:: JS, Web\n"),
            page("Web", "- real\n"),
            page("Jazz", "- music\n"),
        ];
        assert_eq!(
            search_link_pages(&pages, "js", 8),
            vec![(0, Some("JS".to_string()))]
        );
        // The title matches better than any alias: no alias shown.
        assert_eq!(search_link_pages(&pages, "javas", 8), vec![(0, None)]);
        // "Web" belongs to the real page.
        assert_eq!(search_link_pages(&pages, "web", 8), vec![(1, None)]);
        // Empty query: pages in order, limited.
        assert_eq!(search_link_pages(&pages, "", 2), vec![(0, None), (1, None)]);
        assert_eq!(search_link_pages(&pages, "ja", 8).len(), 2);
    }
}
