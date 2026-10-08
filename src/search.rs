//! Fuzzy search over page titles and block contents.
//!
//! Matching is "subsequence" style, like most command palettes: every character
//! of the query must appear in the text in order, but not necessarily next to
//! each other (`fbr` matches `Foo Bar`). Matches are scored so that tighter,
//! earlier, word-aligned matches rank higher.
//!
//! This module is pure (no UI, no I/O), so it is easy to unit-test.

use crate::commands::Command;
use crate::model::Page;

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
    /// A template, by index into the list given to `search_templates`.
    Template(usize),
}

impl Target {
    pub fn is_command(&self) -> bool {
        matches!(self, Target::Command(_))
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
pub fn search(pages: &[Page], commands: &[Command], query: &str, limit: usize) -> Vec<Hit> {
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
        return pages.chain(commands).collect();
    }

    let mut hits = Vec::new();
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
}
