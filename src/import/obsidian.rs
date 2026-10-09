//! An Obsidian vault: every `.md` file a page, everything else an
//! attachment (copied only if a note uses it).
//!
//! | Obsidian | notesec |
//! |---|---|
//! | `Folder/Note.md` | page "Note" (flat; "Folder/Note" if another note is "Note" too) |
//! | `2026-10-09.md` (daily note) | journal 2026-10-09 |
//! | front matter `tags`, `aliases` | `tags:: #a, #[[b c]]`, `alias:: A, B` |
//! | other front matter | `key:: value` |
//! | `[[Note]]`, `[[Note\|text]]`, `[[Note#Heading]]` | `[[Note]]` (no labels or heading anchors) |
//! | `[[#Heading]]` (same note) | the heading's text |
//! | `![[Note]]` | `![[Note]]` (page embed) |
//! | `![[image.png]]`, `![](image.png)` | `![image.png](../assets/image.png)` |
//! | `[[file.pdf]]`, `[x](file.pdf)` | `[file.pdf](../assets/file.pdf)` |
//! | `[x](Other%20Note.md)` | `[[Other Note]]` |
//! | headings, paragraphs, lists | blocks (see `markdown::outline`) |
//!
//! Links resolve as Obsidian does: a path (`[[Folder/Note]]`, also
//! relative to the note), else the note with that name, the one nearest
//! the vault root when several share it.

use std::collections::HashMap;
use std::path::Path;

use super::markdown::{self, Link};
use super::walk::Entry;
use super::{asset_markdown, dedupe, join_rel, journal_title, parent_of, read_note, Assets, Draft};

fn is_md(rel: &str) -> bool {
    Path::new(rel)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("md"))
}

fn stem(rel: &str) -> &str {
    let file = rel.rsplit('/').next().unwrap_or(rel);
    file.rsplit_once('.').map_or(file, |(s, _)| s)
}

fn depth(rel: &str) -> usize {
    rel.matches('/').count()
}

/// Lookups from link text to files.
pub(super) struct Index<'a> {
    /// Lowercase relative path → entry index; for notes also without `.md`.
    by_path: HashMap<String, usize>,
    /// Lowercase file name (notes: also without `.md`) → entries, nearest the root first.
    by_name: HashMap<String, Vec<usize>>,
    pub entries: &'a [Entry],
}

impl<'a> Index<'a> {
    pub fn new(entries: &'a [Entry]) -> Self {
        let mut order: Vec<usize> = (0..entries.len()).collect();
        order.sort_by_key(|&i| (depth(&entries[i].rel), entries[i].rel.to_lowercase()));
        let mut by_path = HashMap::new();
        let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
        for i in order {
            let rel = entries[i].rel.to_lowercase();
            let file = rel.rsplit('/').next().unwrap_or(&rel).to_string();
            by_path.insert(rel.clone(), i);
            by_name.entry(file.clone()).or_default().push(i);
            if is_md(&rel) {
                by_path.insert(rel[..rel.len() - 3].to_string(), i);
                by_name
                    .entry(file[..file.len() - 3].to_string())
                    .or_default()
                    .push(i);
            }
        }
        Index {
            by_path,
            by_name,
            entries,
        }
    }

    /// The file `target` (as written in a link in the note at `from`) means.
    pub fn find(&self, target: &str, from: &str) -> Option<usize> {
        let target = target.trim().trim_start_matches("./");
        if target.is_empty() {
            return None;
        }
        let lc = target.to_lowercase();
        if let Some(&i) = self.by_path.get(lc.trim_start_matches('/')) {
            return Some(i);
        }
        if let Some(rel) = join_rel(parent_of(from), &lc) {
            if let Some(&i) = self.by_path.get(&rel) {
                return Some(i);
            }
        }
        let name = lc.rsplit('/').next().unwrap_or(&lc);
        self.by_name.get(name).and_then(|v| v.first().copied())
    }
}

/// The vault's notes as drafts; attachments the notes use go to `assets`.
pub fn read(
    entries: &[Entry],
    assets: &mut Assets,
    skipped: &mut Vec<(String, String)>,
) -> Vec<Draft> {
    let index = Index::new(entries);
    let mut notes: Vec<usize> = (0..entries.len())
        .filter(|&i| is_md(&entries[i].rel))
        .collect();
    notes.sort_by_key(|&i| (depth(&entries[i].rel), entries[i].rel.to_lowercase()));
    let wanted: Vec<(String, Option<String>)> = notes
        .iter()
        .map(|&i| {
            let rel = &entries[i].rel;
            let folder = parent_of(rel).rsplit('/').next().unwrap_or("");
            let fallback = (!folder.is_empty()).then(|| format!("{folder}/{}", stem(rel)));
            (stem(rel).to_string(), fallback)
        })
        .collect();
    let titles = dedupe(&wanted);
    let title_of: HashMap<usize, String> =
        notes.iter().copied().zip(titles.iter().cloned()).collect();
    let mut drafts = Vec::new();
    for (&i, title) in notes.iter().zip(&titles) {
        let entry = &entries[i];
        let Some(text) = read_note(entry, skipped) else {
            continue;
        };
        let (yaml, body) = markdown::split_frontmatter(&text);
        let props = yaml
            .map(|y| markdown::properties(&markdown::parse_yaml(y)))
            .unwrap_or_default();
        let body = convert_links(body, &entry.rel, &index, &title_of, assets);
        let rows = markdown::outline(&body)
            .into_iter()
            .map(|(d, c)| (d, c, None))
            .collect();
        drafts.push(Draft {
            source: entry.rel.clone(),
            title: title.clone(),
            is_journal: journal_title(title),
            props,
            rows,
        });
    }
    drafts
}

/// The attachment or note link as notesec markdown (see the module table).
fn convert_links(
    body: &str,
    from: &str,
    index: &Index,
    title_of: &HashMap<usize, String>,
    assets: &mut Assets,
) -> String {
    markdown::rewrite_links(body, |link| match link {
        Link::Wiki { embed, inner } => {
            let (target, label) = inner.split_once('|').unwrap_or((inner, ""));
            let (name, heading) = target.split_once('#').unwrap_or((target, ""));
            if name.trim().is_empty() {
                // `[[#Heading]]` inside the same note.
                let text = if label.is_empty() {
                    heading.trim_start_matches('^')
                } else {
                    label
                };
                return Some(text.trim().to_string());
            }
            match index.find(name, from) {
                Some(i) => match title_of.get(&i) {
                    Some(t) if embed => Some(format!("![[{t}]]")),
                    Some(t) => Some(format!("[[{t}]]")),
                    None => {
                        let file = index.entries[i].rel.rsplit('/').next().unwrap_or("");
                        // A size like `|300` isn't a caption.
                        let label = if label.is_empty()
                            || label.chars().all(|c| c.is_ascii_digit() || c == 'x')
                        {
                            file
                        } else {
                            label
                        };
                        assets
                            .add(&index.entries[i])
                            .map(|n| asset_markdown(label, &n))
                    }
                },
                // An attachment that isn't there stays as written.
                None if Path::new(name)
                    .extension()
                    .is_some_and(|e| !e.eq_ignore_ascii_case("md")) =>
                {
                    None
                }
                None => {
                    let name = name.trim().trim_end_matches(".md");
                    Some(if embed {
                        format!("![[{name}]]")
                    } else {
                        format!("[[{name}]]")
                    })
                }
            }
        }
        Link::Markdown {
            image,
            label,
            target,
        } => {
            let target = target.trim_start_matches('<').trim_end_matches('>');
            if target.contains("://") || target.starts_with("mailto:") || target.starts_with('#') {
                return None;
            }
            let path = markdown::percent_decode(target.split('#').next().unwrap_or(target));
            let i = index.find(&path, from)?;
            match title_of.get(&i) {
                Some(t) => Some(format!("[[{t}]]")),
                None => {
                    let file = index.entries[i].rel.rsplit('/').next().unwrap_or("");
                    let label = if label.is_empty() { file } else { label };
                    let name = assets.add(&index.entries[i])?;
                    Some(if image {
                        format!("![{label}](../assets/{name})")
                    } else {
                        asset_markdown(label, &name)
                    })
                }
            }
        }
    })
}
