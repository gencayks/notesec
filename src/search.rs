//! Fuzzy search over page titles and block contents.
//!
//! Matching is "subsequence" style, like most command palettes: every character
//! of the query must appear in the text in order, but not necessarily next to
//! each other (`fbr` matches `Foo Bar`). Matches are scored so that tighter,
//! earlier, word-aligned matches rank higher.
//!
//! This module is pure (no UI, no I/O), so it is easy to unit-test.

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

/// One search result.
#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    /// Index into the `pages` slice.
    pub page: usize,
    /// `None` for a page-title hit, `Some(i)` for the page's i-th block.
    pub block: Option<usize>,
    pub score: i32,
}

/// Extra score for title matches so a page named `Foo` beats a block that
/// merely mentions foo.
const TITLE_BONUS: i32 = 25;

/// Search all pages. Returns at most `limit` hits, best first.
///
/// With an empty query every page is listed (in the given order) so the
/// palette is useful as a page switcher before you type anything.
pub fn search(pages: &[Page], query: &str, limit: usize) -> Vec<Hit> {
    let query = query.trim();
    if query.is_empty() {
        return (0..pages.len().min(limit))
            .map(|page| Hit {
                page,
                block: None,
                score: 0,
            })
            .collect();
    }

    let mut hits = Vec::new();
    for (page_ix, page) in pages.iter().enumerate() {
        if let Some(score) = fuzzy_score(query, &page.title) {
            hits.push(Hit {
                page: page_ix,
                block: None,
                score: score + TITLE_BONUS,
            });
        }
        for (block_ix, block) in page.blocks.iter().enumerate() {
            if block.content.is_empty() {
                continue;
            }
            if let Some(score) = fuzzy_score(query, &block.content) {
                hits.push(Hit {
                    page: page_ix,
                    block: Some(block_ix),
                    score,
                });
            }
        }
    }
    // Stable sort: equal scores keep page/document order.
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
        let hits = search(&pages(), "needle", 10);
        assert_eq!(hits[0].page, 1);
        assert_eq!(hits[0].block, None);
        assert_eq!((hits[1].page, hits[1].block), (0, Some(1)));
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn empty_query_lists_pages_and_limit_applies() {
        let hits = search(&pages(), "  ", 2);
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|h| h.block.is_none()));
    }

    #[test]
    fn no_match_is_empty() {
        assert!(search(&pages(), "qqqq", 10).is_empty());
    }
}
