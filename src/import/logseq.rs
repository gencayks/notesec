//! A Logseq graph folder. Logseq writes the same markdown as notesec, so
//! blocks, nesting, `id::` lines and `((block refs))` carry over as they
//! are.
//!
//! | Logseq | notesec |
//! |---|---|
//! | `pages/A___B.md`, `pages/A%2FB.md` | page "A/B" |
//! | `title:: X` page property | the title X |
//! | `journals/2026_10_09.md` (also `2026-10-09`, `20261009`, `2026.10.09`) | journal 2026-10-09 |
//! | links to journals by their title (`[[Oct 9th, 2026]]`, or the graph's `:journal/page-title-format`) | `[[2026-10-09]]` |
//! | `tags:: a, [[b c]]` | `tags:: #a, #[[b c]]` |
//! | `alias::`, other page properties | kept |
//! | `assets/x.png` | `assets/x.png` (renamed if the name is taken; links follow) |
//! | `.org` files, whiteboards | skipped |

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use chrono::NaiveDate;

use super::markdown::{self, Row};
use super::walk::Entry;
use super::{clean_title, dedupe, read_note, Assets, Draft};
use crate::model::{parse_references, Page};
use crate::publish::property;

/// A journal's file name as a date.
fn journal_file_date(stem: &str) -> Option<NaiveDate> {
    ["%Y_%m_%d", "%Y-%m-%d", "%Y%m%d", "%Y.%m.%d"]
        .iter()
        .find_map(|f| NaiveDate::parse_from_str(stem, f).ok())
}

/// `9th` → `9` (English ordinals in journal titles).
fn strip_ordinals(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        out.push(c);
        if c.is_ascii_digit() {
            let rest = &text[i + 1..];
            let next_digit = rest.starts_with(|c: char| c.is_ascii_digit());
            if !next_digit {
                for suffix in ["st", "nd", "rd", "th"] {
                    if rest.starts_with(suffix)
                        && !rest[2..].starts_with(|c: char| c.is_alphanumeric())
                    {
                        chars.next();
                        chars.next();
                        break;
                    }
                }
            }
        }
    }
    out
}

/// A Logseq (date-fns) date format as a chrono one, for the tokens journal
/// titles use; `do` (ordinal day) becomes `%d` after ordinals are dropped.
fn chrono_format(format: &str) -> String {
    let tokens = [
        ("yyyy", "%Y"),
        ("MMMM", "%B"),
        ("MMM", "%b"),
        ("MM", "%m"),
        ("EEEE", "%A"),
        ("EEE", "%a"),
        ("do", "%d"),
        ("dd", "%d"),
        ("d", "%d"),
        ("M", "%m"),
    ];
    let mut out = String::new();
    let mut rest = format;
    'outer: while let Some(c) = rest.chars().next() {
        for (token, with) in tokens {
            if let Some(r) = rest.strip_prefix(token) {
                out.push_str(with);
                rest = r;
                continue 'outer;
            }
        }
        if c == '%' {
            out.push_str("%%");
        } else {
            out.push(c);
        }
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// The journal title formats to read links with: the graph's own (from
/// `logseq/config.edn`), then Logseq's default and the ISO-like ones.
fn title_formats(folder: &Path) -> Vec<String> {
    let mut formats = Vec::new();
    if let Ok(config) = fs::read_to_string(folder.join("logseq").join("config.edn")) {
        if let Some(at) = config.find(":journal/page-title-format") {
            let rest = &config[at..];
            if let Some(start) = rest.find('"') {
                if let Some(len) = rest[start + 1..].find('"') {
                    formats.push(chrono_format(&rest[start + 1..start + 1 + len]));
                }
            }
        }
    }
    for f in [
        "%b %d, %Y",
        "%B %d, %Y",
        "%Y-%m-%d",
        "%Y/%m/%d",
        "%Y_%m_%d",
        "%A, %d.%m.%Y",
        "%d-%m-%Y",
    ] {
        if !formats.iter().any(|x| x == f) {
            formats.push(f.to_string());
        }
    }
    formats
}

/// A link target that is a journal title, as notesec's journal title.
fn journal_link(target: &str, formats: &[String]) -> Option<String> {
    let plain = strip_ordinals(target.trim());
    formats
        .iter()
        .find_map(|f| NaiveDate::parse_from_str(&plain, f).ok())
        .map(|d| d.format("%Y-%m-%d").to_string())
}

/// A page's file name as its title (`___` and `%2F` are `/`).
fn page_title(stem: &str) -> String {
    markdown::percent_decode(&stem.replace("___", "/"))
}

/// The graph's pages and journals as drafts; `assets/` files that pages use
/// go to `assets`.
pub fn read(
    folder: &Path,
    entries: &[Entry],
    assets: &mut Assets,
    skipped: &mut Vec<(String, String)>,
) -> Result<Vec<Draft>, String> {
    if !entries
        .iter()
        .any(|e| e.rel.starts_with("pages/") || e.rel.starts_with("journals/"))
    {
        return Err(format!(
            "{} has no pages/ or journals/ folder: is it a Logseq graph?",
            folder.display()
        ));
    }
    let formats = title_formats(folder);
    // Attachments: renamed ones must be relinked.
    let mut asset_names: HashMap<String, String> = HashMap::new();
    for e in entries.iter().filter(|e| e.rel.starts_with("assets/")) {
        let old = e.rel["assets/".len()..].to_string();
        if let Some(new) = assets.add(e) {
            asset_names.insert(old, new);
        }
    }
    struct Note<'a> {
        entry: &'a Entry,
        page: Page,
        props: Vec<String>,
        title: String,
        /// The first block is the page properties (now in `props`).
        skip_first: bool,
    }
    let mut notes: Vec<Note> = Vec::new();
    for e in entries {
        let (dir, file) = e.rel.split_once('/').unwrap_or(("", &e.rel));
        if dir != "pages" && dir != "journals" {
            if dir == "whiteboards" {
                skipped.push((e.rel.clone(), "whiteboards aren't imported".into()));
            }
            continue;
        }
        let Some(stem) = file.strip_suffix(".md") else {
            if file.ends_with(".org") {
                skipped.push((e.rel.clone(), "Org-mode files aren't imported".into()));
            }
            continue;
        };
        if stem.contains('/') {
            continue;
        }
        let Some(text) = read_note(e, skipped) else {
            continue;
        };
        let date = (dir == "journals")
            .then(|| journal_file_date(stem))
            .flatten();
        let mut title = match date {
            Some(d) => d.format("%Y-%m-%d").to_string(),
            None => page_title(stem),
        };
        let page = Page::from_markdown(&title, date.is_some(), &text);
        // Page properties: a first block of only `key:: value` lines.
        let mut props = Vec::new();
        let first_is_props = page.blocks.first().is_some_and(|b| {
            page.subtree_end(0) == 1
                && !b.content.trim().is_empty()
                && b.content
                    .lines()
                    .all(|l| l.trim().is_empty() || property(l).is_some())
        });
        if first_is_props {
            for line in page.blocks[0].content.lines() {
                let Some((key, value)) = property(line) else {
                    continue;
                };
                match key.to_lowercase().as_str() {
                    "title" if date.is_none() => title = clean_title(value),
                    "tags" => {
                        // Comma-separated; `b c` is one tag.
                        let tags: Vec<String> = value
                            .split(',')
                            .map(|t| {
                                t.trim()
                                    .trim_start_matches('#')
                                    .trim_start_matches("[[")
                                    .trim_end_matches("]]")
                            })
                            .filter(|t| !t.is_empty())
                            .map(markdown::tag_ref)
                            .collect();
                        if !tags.is_empty() {
                            props.push(format!("tags:: {}", tags.join(", ")));
                        }
                    }
                    _ => props.push(line.trim().to_string()),
                }
            }
        }
        notes.push(Note {
            entry: e,
            page,
            props,
            title,
            skip_first: first_is_props,
        });
    }
    // Journals keep their date; pages are deduplicated after them.
    let wanted: Vec<(String, Option<String>)> =
        notes.iter().map(|n| (n.title.clone(), None)).collect();
    let titles = dedupe(&wanted);
    let mut drafts = Vec::new();
    for (note, title) in notes.into_iter().zip(titles) {
        let base = usize::from(note.skip_first);
        let rows: Vec<Row> = note
            .page
            .blocks
            .iter()
            .enumerate()
            .skip(base)
            .map(|(i, b)| {
                let depth = note.page.depth_of(i);
                let content = relink(&b.content, &asset_names, &formats);
                let id = note.page.saved_ids.contains(&b.id).then_some(b.id);
                (depth, content, id)
            })
            .collect();
        drafts.push(Draft {
            source: note.entry.rel.clone(),
            is_journal: note.page.is_journal && title == note.page.title,
            title,
            props: note.props,
            rows,
        });
    }
    Ok(drafts)
}

/// Links to journal titles as notesec's dates, attachments under their new names.
fn relink(content: &str, asset_names: &HashMap<String, String>, formats: &[String]) -> String {
    let mut out = content.to_string();
    for r in parse_references(content).into_iter().rev() {
        if let Some(date) = journal_link(&r.target, formats) {
            let with = if r.is_tag {
                format!("#[[{date}]]")
            } else {
                format!("[[{date}]]")
            };
            out.replace_range(r.range, &with);
        }
    }
    for (old, new) in asset_names {
        if old != new {
            out = out.replace(&format!("../assets/{old}"), &format!("../assets/{new}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_names_and_titles() {
        assert_eq!(
            journal_file_date("2026_10_09"),
            NaiveDate::from_ymd_opt(2026, 10, 9)
        );
        assert_eq!(
            journal_file_date("20261009"),
            NaiveDate::from_ymd_opt(2026, 10, 9)
        );
        assert_eq!(journal_file_date("Oct 9th"), None);
        let formats = title_formats(Path::new("/nonexistent"));
        assert_eq!(
            journal_link("Oct 9th, 2026", &formats).as_deref(),
            Some("2026-10-09")
        );
        assert_eq!(
            journal_link("October 1st, 2026", &formats).as_deref(),
            Some("2026-10-01")
        );
        assert_eq!(
            journal_link("2026/10/09", &formats).as_deref(),
            Some("2026-10-09")
        );
        assert_eq!(journal_link("Project 1st", &formats), None);
        assert_eq!(chrono_format("EEE, dd.MM.yyyy"), "%a, %d.%m.%Y");
        assert_eq!(chrono_format("MMM do, yyyy"), "%b %d, %Y");
        assert_eq!(strip_ordinals("Oct 22nd, 2026 1st"), "Oct 22, 2026 1");
        assert_eq!(strip_ordinals("1sthing"), "1sthing");
        assert_eq!(page_title("A___B"), "A/B");
        assert_eq!(page_title("A%2FB%3F"), "A/B?");
    }
}
