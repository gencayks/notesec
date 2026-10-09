//! A Notion export ("Markdown & CSV", unzipped). Notion adds a 32-digit
//! hex id to every file and folder name and links with URL-encoded
//! relative paths.
//!
//! | Notion | notesec |
//! |---|---|
//! | `Page 0123….md` | page "Page" (id dropped; "Parent/Page" if another is "Page" too) |
//! | `# Page` first line | dropped (it's the title) |
//! | `[Page](Parent%200123…/Page%204567….md)` | `[[Page]]` |
//! | `[Page](https://www.notion.so/Page-0123…)` | `[[Page]]` when that page is in the export |
//! | `![x](Page%200123…/image.png)` | `![x](../assets/image.png)` |
//! | `Database 0123….csv` (or `…_all.csv`) | page "Database" with a table; first column linked to the row pages |
//! | a row page's `Key: value` lines under its title | `key:: value` (`Tags` as `tags:: #a, #b`) |
//! | headings, paragraphs, lists, to-dos | blocks (see `markdown::outline`) |

use std::collections::HashMap;
use std::path::Path;

use super::markdown::{self, Link};
use super::walk::Entry;
use super::{asset_markdown, csv, dedupe, join_rel, parent_of, read_note, Assets, Draft};

/// The hex id at the end of a Notion name (`Name 0123…` or `Name-0123…`).
fn split_id(name: &str) -> (&str, Option<&str>) {
    if name.len() > 33 {
        let at = name.len() - 32;
        let (head, id) = name.split_at(at);
        if id.chars().all(|c| c.is_ascii_hexdigit()) {
            if let Some(head) = head.strip_suffix(' ').or_else(|| head.strip_suffix('-')) {
                return (head, Some(id));
            }
        }
    }
    (name, None)
}

/// A file or folder name without Notion's id (and `.md`, `.csv`, `_all`).
pub fn clean_name(file: &str) -> String {
    let stem = file
        .strip_suffix(".md")
        .or_else(|| file.strip_suffix(".csv"))
        .unwrap_or(file);
    let stem = stem.strip_suffix("_all").unwrap_or(stem);
    split_id(stem).0.trim().to_string()
}

fn file_of(rel: &str) -> &str {
    rel.rsplit('/').next().unwrap_or(rel)
}

fn is_ext(rel: &str, ext: &str) -> bool {
    Path::new(rel)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case(ext))
}

/// The export's notes and databases as drafts.
pub fn read(
    entries: &[Entry],
    assets: &mut Assets,
    skipped: &mut Vec<(String, String)>,
) -> Vec<Draft> {
    // A database exported both ways (`DB id.csv` and `DB id_all.csv`):
    // the `_all` one has every row.
    let csv_keys: Vec<String> = entries
        .iter()
        .filter(|e| is_ext(&e.rel, "csv") && e.rel.ends_with("_all.csv"))
        .map(|e| e.rel[..e.rel.len() - "_all.csv".len()].to_lowercase())
        .collect();
    let pages: Vec<usize> = (0..entries.len())
        .filter(|&i| {
            let rel = &entries[i].rel;
            is_ext(rel, "md")
                || (is_ext(rel, "csv")
                    && !(csv_keys.contains(&rel[..rel.len() - 4].to_lowercase())
                        && !rel.ends_with("_all.csv")))
        })
        .collect();
    let wanted: Vec<(String, Option<String>)> = pages
        .iter()
        .map(|&i| {
            let rel = &entries[i].rel;
            let parent = clean_name(file_of(parent_of(rel)));
            let name = clean_name(file_of(rel));
            (
                name.clone(),
                (!parent.is_empty()).then(|| format!("{parent}/{name}")),
            )
        })
        .collect();
    let titles = dedupe(&wanted);
    // Lookups: relative path (lowercase), the folder a database's rows are
    // in, and Notion ids.
    let mut by_rel: HashMap<String, usize> = HashMap::new();
    let mut by_id: HashMap<String, usize> = HashMap::new();
    let mut db_dirs: HashMap<String, usize> = HashMap::new();
    for (n, &i) in pages.iter().enumerate() {
        let rel = &entries[i].rel;
        by_rel.insert(rel.to_lowercase(), n);
        let stem = rel.rsplit_once('.').map_or(rel.as_str(), |(s, _)| s);
        let stem = stem.strip_suffix("_all").unwrap_or(stem);
        if let (_, Some(id)) = split_id(file_of(stem)) {
            by_id.insert(id.to_lowercase(), n);
        }
        if is_ext(rel, "csv") {
            db_dirs.insert(stem.to_lowercase(), n);
            by_rel.insert(format!("{stem}.csv").to_lowercase(), n);
        }
    }
    let all_rel: HashMap<String, usize> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| (e.rel.to_lowercase(), i))
        .collect();
    let mut drafts = Vec::new();
    for (n, &i) in pages.iter().enumerate() {
        let entry = &entries[i];
        let Some(text) = read_note(entry, skipped) else {
            continue;
        };
        let title = &titles[n];
        let resolve = |target: &str| -> Option<Target> {
            let target = target.trim_start_matches('<').trim_end_matches('>');
            if target.contains("://") {
                // `https://www.notion.so/Title-<id>` to a page in the export.
                let last = target.split(['?', '#']).next()?.rsplit('/').next()?;
                let (_, id) = split_id(last);
                let id = id.or_else(|| (last.len() == 32).then_some(last))?;
                return by_id
                    .get(&id.to_lowercase())
                    .map(|&p| Target::Page(titles[p].clone()));
            }
            let path = markdown::percent_decode(target.split('#').next()?);
            let rel = join_rel(parent_of(&entry.rel), &path)?.to_lowercase();
            if let Some(&p) = by_rel.get(&rel) {
                return Some(Target::Page(titles[p].clone()));
            }
            all_rel.get(&rel).map(|&a| Target::File(a))
        };
        let mut link = |link: Link| -> Option<String> {
            let (image, label, target) = match link {
                Link::Markdown {
                    image,
                    label,
                    target,
                } => (image, label, target),
                Link::Wiki { .. } => return None,
            };
            match resolve(target)? {
                Target::Page(t) => Some(format!("[[{t}]]")),
                Target::File(a) => {
                    let label = if label.is_empty() {
                        file_of(&entries[a].rel)
                    } else {
                        label
                    };
                    let name = assets.add(&entries[a])?;
                    Some(if image {
                        format!("![{label}](../assets/{name})")
                    } else {
                        asset_markdown(label, &name)
                    })
                }
            }
        };
        if is_ext(&entry.rel, "csv") {
            let stem = &entry.rel[..entry.rel.len() - 4];
            let stem = stem.strip_suffix("_all").unwrap_or(stem).to_lowercase();
            // Row pages: the `.md` files in the database's folder.
            let rows_by_name: HashMap<String, String> = pages
                .iter()
                .enumerate()
                .filter(|(_, &p)| {
                    parent_of(&entries[p].rel).to_lowercase() == stem
                        && is_ext(&entries[p].rel, "md")
                })
                .map(|(m, &p)| {
                    (
                        clean_name(file_of(&entries[p].rel)).to_lowercase(),
                        titles[m].clone(),
                    )
                })
                .collect();
            let table = database_table(&text, &rows_by_name, &mut link);
            drafts.push(Draft {
                source: entry.rel.clone(),
                title: title.clone(),
                is_journal: false,
                props: Vec::new(),
                rows: vec![(0, table, None)],
            });
            continue;
        }
        let in_database = db_dirs.contains_key(&parent_of(&entry.rel).to_lowercase());
        let (props, body) = split_page(&text, &clean_name(file_of(&entry.rel)), in_database);
        let body = markdown::rewrite_links(&body, &mut link);
        let rows = markdown::outline(&body)
            .into_iter()
            .map(|(d, c)| (d, c, None))
            .collect();
        drafts.push(Draft {
            source: entry.rel.clone(),
            title: title.clone(),
            is_journal: false,
            props,
            rows,
        });
    }
    drafts
}

enum Target {
    Page(String),
    File(usize),
}

/// A Notion page's text without its `# Title` line, and (for a database
/// row) the `Key: value` lines under the title as properties.
fn split_page(text: &str, name: &str, in_database: bool) -> (Vec<String>, String) {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines: Vec<&str> = text.lines().collect();
    while lines.first().is_some_and(|l| l.trim().is_empty()) {
        lines.remove(0);
    }
    if lines.first().is_some_and(|l| {
        l.strip_prefix("# ")
            .is_some_and(|t| t.trim().eq_ignore_ascii_case(name.trim()))
    }) {
        lines.remove(0);
    }
    let mut props = Vec::new();
    if in_database {
        while lines.first().is_some_and(|l| l.trim().is_empty()) {
            lines.remove(0);
        }
        let mut pairs: Vec<(String, Vec<String>)> = Vec::new();
        while let Some(line) = lines.first() {
            let Some((key, value)) = line.split_once(": ") else {
                break;
            };
            if key.is_empty() || key.len() > 60 || key.contains(['#', '[', '*']) {
                break;
            }
            let values = if key.eq_ignore_ascii_case("tags") {
                value.split(',').map(|v| v.trim().to_string()).collect()
            } else {
                vec![value.trim().to_string()]
            };
            pairs.push((key.trim().to_string(), values));
            lines.remove(0);
        }
        // Tags are comma-separated in Notion: one value each.
        for (key, values) in &mut pairs {
            if key.eq_ignore_ascii_case("tags") {
                *values = values.iter().map(|v| format!("{v},")).collect();
            }
        }
        props = markdown::properties(&pairs);
    }
    (props, lines.join("\n"))
}

/// A CSV database as one markdown table; a cell of the first column that
/// names a row page links to it, other cells' links are rewritten too.
fn database_table(
    text: &str,
    rows_by_name: &HashMap<String, String>,
    link: &mut dyn FnMut(Link) -> Option<String>,
) -> String {
    let rows = csv::parse(text);
    let Some(header) = rows.first() else {
        return String::new();
    };
    let width = rows.iter().map(Vec::len).max().unwrap_or(1).max(1);
    let cell = |text: &str| text.replace(['\r', '\n'], " ").replace('|', "\\|");
    let mut out = String::new();
    let line = |cells: Vec<String>| format!("| {} |", cells.join(" | "));
    let mut head: Vec<String> = header.iter().map(|h| cell(h)).collect();
    head.resize(width, String::new());
    out.push_str(&line(head));
    out.push('\n');
    out.push_str(&line(vec!["---".to_string(); width]));
    for row in &rows[1..] {
        let mut cells: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(c, v)| match rows_by_name.get(&v.trim().to_lowercase()) {
                Some(t) if c == 0 => format!("[[{t}]]"),
                _ => markdown::rewrite_links(&cell(v), &mut *link),
            })
            .collect();
        cells.resize(width, String::new());
        out.push('\n');
        out.push_str(&line(cells));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_dropped_from_names() {
        let id = "0123456789abcdef0123456789abcdef";
        assert_eq!(clean_name(&format!("My Page {id}.md")), "My Page");
        assert_eq!(clean_name(&format!("Tasks {id}_all.csv")), "Tasks");
        assert_eq!(clean_name(&format!("Folder {id}")), "Folder");
        assert_eq!(clean_name("No id.md"), "No id");
        assert_eq!(
            clean_name(&format!("{id}.md")),
            id,
            "a name that is only an id stays"
        );
        assert_eq!(split_id(&format!("Title-{id}")), ("Title", Some(id)));
    }

    #[test]
    fn row_pages_get_their_properties() {
        let (props, body) = split_page(
            "# Write report\n\nStatus: Done\nTags: work, big deal\nDue: October 9, 2026\n\nThe text: here.\n",
            "Write report",
            true,
        );
        assert_eq!(
            props,
            [
                "tags:: #work, #[[big deal]]",
                "status:: Done",
                "due:: October 9, 2026"
            ]
        );
        assert_eq!(body, "\nThe text: here.");
        let (props, body) = split_page("# Note\nKey: value\n", "Note", false);
        assert!(props.is_empty());
        assert_eq!(body, "Key: value");
    }
}
