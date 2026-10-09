//! Import notes from Obsidian, Logseq and Notion (decision 50). No GPUI:
//! `app/import_ui.rs` runs it on a background thread.
//!
//! Two steps. [`plan`] reads the source folder ([`walk`]) and turns every
//! note into a [`Draft`] (a title, page properties, outline rows) with its
//! links rewritten to notesec's `[[Page]]` / `![[Page]]` / `../assets/x`;
//! nothing is written. It also lists the titles that already exist in the
//! graph, so the app can ask what to do about them. [`apply`] then settles
//! the titles ([`OnClash`]), rewrites links to renamed pages, writes each
//! page atomically (never over an existing file), copies the attachments
//! into `assets/` under new names if needed, and writes an import log page
//! that every imported page links to with `imported-from::`.

pub mod csv;
mod logseq;
pub mod markdown;
mod notion;
mod obsidian;
pub mod walk;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::model::{is_journal_title, parse_references, parse_wikilinks};
use crate::storage::write_atomic;
use markdown::Row;
use uuid::Uuid;
use walk::Entry;

/// Largest note (markdown or CSV) read, in bytes.
pub const MAX_NOTE_BYTES: u64 = 5 * 1024 * 1024;
/// Largest attachment copied, in bytes.
pub const MAX_ASSET_BYTES: u64 = 50 * 1024 * 1024;
/// Longest title, in bytes (file names must fit; see `validate_title`).
const MAX_TITLE_BYTES: usize = 150;

/// Where the notes come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Obsidian,
    Logseq,
    Notion,
}

impl Source {
    pub fn name(self) -> &'static str {
        match self {
            Source::Obsidian => "Obsidian",
            Source::Logseq => "Logseq",
            Source::Notion => "Notion",
        }
    }
}

/// A page to be written.
#[derive(Clone, Debug)]
pub struct Draft {
    /// Where it came from, relative to the source folder.
    pub source: String,
    pub title: String,
    pub is_journal: bool,
    /// Page property lines (`tags:: #a`, `alias:: B`), the first block.
    pub props: Vec<String>,
    pub rows: Vec<Row>,
}

/// An attachment to copy into `assets/<name>`.
#[derive(Clone, Debug)]
pub struct AssetCopy {
    pub from: PathBuf,
    pub name: String,
}

/// What the graph already has.
#[derive(Clone, Debug, Default)]
pub struct Existing {
    /// Every page title.
    pub titles: Vec<String>,
    /// Every page's aliases (links to them aren't unresolved).
    pub aliases: Vec<String>,
    /// Block ids saved in the graph's pages (`id::`). `apply` also reads
    /// them from the files, trash included; an imported block with one of
    /// these gets a new id.
    pub ids: HashSet<Uuid>,
}

/// What an import would do (nothing written yet).
#[derive(Debug)]
pub struct Plan {
    pub source: Source,
    pub folder: PathBuf,
    pub drafts: Vec<Draft>,
    pub assets: Vec<AssetCopy>,
    /// (relative path, why) for what isn't imported.
    pub skipped: Vec<(String, String)>,
    /// Titles of drafts whose name a page in the graph already has.
    pub clashes: Vec<String>,
}

/// What to do with a page whose name is taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnClash {
    /// Import it as "Name (imported)" (the default).
    Rename,
    /// Leave it out; links to the name go to the existing page.
    Skip,
}

/// What an import did.
#[derive(Clone, Debug, PartialEq)]
pub struct Summary {
    pub source: Source,
    /// The import log page.
    pub log_title: String,
    /// The pages written (title, journal), the log page not included.
    pub pages: Vec<(String, bool)>,
    pub assets: usize,
    /// Distinct `[[link]]` targets that are no page.
    pub unresolved: usize,
    pub skipped: usize,
    pub renamed: usize,
    /// Imported blocks given a new id: theirs was in the graph already, or
    /// twice in the import.
    pub new_ids: usize,
}

impl Summary {
    /// The status line.
    pub fn text(&self) -> String {
        let plural = |n: usize, one: &str, many: &str| {
            if n == 1 {
                format!("1 {one}")
            } else {
                format!("{n} {many}")
            }
        };
        let mut text = format!(
            "Imported {} and {} from {}",
            plural(self.pages.len(), "page", "pages"),
            plural(self.assets, "asset", "assets"),
            self.source.name()
        );
        if self.renamed > 0 {
            text.push_str(&format!(", renamed {}", self.renamed));
        }
        if self.unresolved > 0 {
            text.push_str(&format!(
                ", {}",
                plural(self.unresolved, "unresolved link", "unresolved links")
            ));
        }
        if self.skipped > 0 {
            text.push_str(&format!(", skipped {}", self.skipped));
        }
        text.push_str(&format!(" (see \u{201c}{}\u{201d})", self.log_title));
        text
    }
}

/// Attachments being collected: each source file once, under a name not
/// used in `assets/` yet.
pub struct Assets {
    taken: HashSet<String>,
    by_source: HashMap<PathBuf, String>,
    copies: Vec<AssetCopy>,
    skipped: Vec<(String, String)>,
}

/// A file name that works in `![](../assets/name)`: letters, digits,
/// `-`, `_`, `.`; anything else `_`.
fn asset_name(name: &str) -> String {
    let clean: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let clean = clean.trim_start_matches('.').to_string();
    if clean.is_empty() {
        "file".to_string()
    } else {
        clean
    }
}

impl Assets {
    fn new(graph: &Path) -> Self {
        let taken = fs::read_dir(graph.join("assets"))
            .map(|entries| {
                entries
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().to_lowercase())
                    .collect()
            })
            .unwrap_or_default();
        Assets {
            taken,
            by_source: HashMap::new(),
            copies: Vec::new(),
            skipped: Vec::new(),
        }
    }

    /// The `assets/` name `entry` will have (`None`: too large).
    pub fn add(&mut self, entry: &Entry) -> Option<String> {
        if let Some(name) = self.by_source.get(&entry.path) {
            return Some(name.clone());
        }
        if entry.size > MAX_ASSET_BYTES {
            self.skipped.push((
                entry.rel.clone(),
                format!("larger than {} MB", MAX_ASSET_BYTES / 1024 / 1024),
            ));
            return None;
        }
        let file = entry.rel.rsplit('/').next().unwrap_or(&entry.rel);
        let clean = asset_name(file);
        let (stem, ext) = match clean.rsplit_once('.') {
            Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
            _ => (clean.clone(), String::new()),
        };
        let mut name = clean.clone();
        let mut n = 2;
        while self.taken.contains(&name.to_lowercase()) {
            name = format!("{stem}-{n}{ext}");
            n += 1;
        }
        self.taken.insert(name.to_lowercase());
        self.by_source.insert(entry.path.clone(), name.clone());
        self.copies.push(AssetCopy {
            from: entry.path.clone(),
            name: name.clone(),
        });
        Some(name)
    }
}

/// The markdown for an attachment in `assets/`: an image, or a link.
pub fn asset_markdown(label: &str, name: &str) -> String {
    if crate::assets::is_image_path(Path::new(name)) {
        format!("![{label}](../assets/{name})")
    } else {
        format!("[{label}](../assets/{name})")
    }
}

/// A title notesec can store: no control characters, no `___` (it means
/// `/` in file names), trimmed, at most `MAX_TITLE_BYTES`; "Untitled" if
/// nothing is left.
pub fn clean_title(title: &str) -> String {
    let mut t: String = title.chars().filter(|c| !c.is_control()).collect();
    while t.contains("___") {
        t = t.replace("___", "_");
    }
    let mut t = t.trim().to_string();
    if t.len() > MAX_TITLE_BYTES {
        let mut cut = MAX_TITLE_BYTES;
        while !t.is_char_boundary(cut) {
            cut -= 1;
        }
        t.truncate(cut);
        t = t.trim_end().to_string();
    }
    if t.is_empty() {
        "Untitled".to_string()
    } else {
        t
    }
}

/// Unique titles (ignoring case) for wanted (title, fallback) pairs, in
/// order: the first keeps its title, a later one with the same title gets
/// its fallback (e.g. "Folder/Note") if that's free, else " (2)", " (3)".
pub fn dedupe(wanted: &[(String, Option<String>)]) -> Vec<String> {
    let mut used: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for (title, fallback) in wanted {
        let title = clean_title(title);
        let mut pick = title.clone();
        if used.contains(&pick.to_lowercase()) {
            pick = match fallback.as_ref().map(|f| clean_title(f)) {
                Some(f) if !used.contains(&f.to_lowercase()) => f,
                _ => {
                    let mut n = 2;
                    loop {
                        let t = format!("{title} ({n})");
                        if !used.contains(&t.to_lowercase()) {
                            break t;
                        }
                        n += 1;
                    }
                }
            };
        }
        used.insert(pick.to_lowercase());
        out.push(pick);
    }
    out
}

/// `rel` (a `/` path inside the source) joined to the folder `dir` and
/// normalised (`.` and `..` resolved); `None` if it climbs out.
pub fn join_rel(dir: &str, rel: &str) -> Option<String> {
    let mut parts: Vec<&str> = if rel.starts_with('/') {
        Vec::new()
    } else {
        dir.split('/').filter(|p| !p.is_empty()).collect()
    };
    for part in rel.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            p => parts.push(p),
        }
    }
    Some(parts.join("/"))
}

/// The folder part of a relative path (`a/b/c.md` → `a/b`).
pub fn parent_of(rel: &str) -> &str {
    rel.rsplit_once('/').map_or("", |(d, _)| d)
}

/// Read a note's text, or say why not.
fn read_note(entry: &Entry, skipped: &mut Vec<(String, String)>) -> Option<String> {
    if entry.size > MAX_NOTE_BYTES {
        skipped.push((
            entry.rel.clone(),
            format!("larger than {} MB", MAX_NOTE_BYTES / 1024 / 1024),
        ));
        return None;
    }
    match fs::read(&entry.path) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(text) => Some(text),
            Err(_) => {
                skipped.push((entry.rel.clone(), "not UTF-8 text".into()));
                None
            }
        },
        Err(err) => {
            skipped.push((entry.rel.clone(), format!("unreadable: {err}")));
            None
        }
    }
}

/// Read `folder` as a `source` export (nothing is written). `graph` is
/// the graph folder (for `assets/` names, and refusing to import a graph
/// into itself).
pub fn plan(
    source: Source,
    folder: &Path,
    graph: &Path,
    existing: &Existing,
) -> Result<Plan, String> {
    if folder
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("zip"))
        || folder.is_file()
    {
        return Err(format!(
            "{} is a file: unzip the export first, then pick the folder",
            folder.display()
        ));
    }
    let real = folder
        .canonicalize()
        .map_err(|err| format!("Can't read {}: {err}", folder.display()))?;
    if let Ok(graph) = graph.canonicalize() {
        if real.starts_with(&graph) || graph.starts_with(&real) {
            return Err("Pick a folder outside this graph (and not containing it)".into());
        }
    }
    let walked =
        walk::walk(&real).map_err(|err| format!("Can't read {}: {err}", folder.display()))?;
    let mut skipped = walked.skipped;
    let mut assets = Assets::new(graph);
    let drafts = match source {
        Source::Obsidian => obsidian::read(&walked.files, &mut assets, &mut skipped),
        Source::Logseq => logseq::read(&real, &walked.files, &mut assets, &mut skipped)?,
        Source::Notion => notion::read(&walked.files, &mut assets, &mut skipped),
    };
    if drafts.is_empty() {
        return Err(format!(
            "No {} notes found in {}",
            source.name(),
            folder.display()
        ));
    }
    skipped.append(&mut assets.skipped);
    let taken: HashSet<String> = existing.titles.iter().map(|t| t.to_lowercase()).collect();
    let clashes = drafts
        .iter()
        .filter(|d| taken.contains(&d.title.to_lowercase()))
        .map(|d| d.title.clone())
        .collect();
    Ok(Plan {
        source,
        folder: folder.to_path_buf(),
        drafts,
        assets: assets.copies,
        skipped,
        clashes,
    })
}

/// `text` with references to renamed pages (`[[old]]`, `#old`,
/// `#[[old]]`, also inside `![[old]]`) pointing at the new names.
fn rename_refs(text: &str, renamed: &HashMap<String, String>) -> String {
    if renamed.is_empty() {
        return text.to_string();
    }
    let mut out = text.to_string();
    for r in parse_references(text).into_iter().rev() {
        if let Some(new) = renamed.get(&r.target.to_lowercase()) {
            let with = if r.is_tag {
                markdown::tag_ref(new)
            } else {
                format!("[[{new}]]")
            };
            out.replace_range(r.range, &with);
        }
    }
    out
}

/// `((id))` references in `text` (also inside `![[((id))]]`) to an id in
/// `remap` pointing at the new id.
fn remap_block_refs(text: &str, remap: &HashMap<Uuid, Uuid>) -> String {
    if remap.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("((") {
        let (before, from) = rest.split_at(start);
        out.push_str(before);
        let inner = &from[2..];
        let new = inner
            .find("))")
            .and_then(|end| Some((end, Uuid::parse_str(inner[..end].trim()).ok()?)))
            .and_then(|(end, old)| Some((end, *remap.get(&old)?)));
        match new {
            Some((end, new)) => {
                out.push_str(&format!("(({new}))"));
                rest = &inner[end + 2..];
            }
            None => {
                out.push_str("((");
                rest = inner;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Every `id:: <uuid>` in the graph's markdown files (`pages/`,
/// `journals/`, and `.trash/`, whose pages can come back), without
/// following symbolic links.
fn ids_in_graph(graph: &Path) -> HashSet<Uuid> {
    fn scan(dir: &Path, ids: &mut HashSet<Uuid>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                scan(&path, ids);
            } else if kind.is_file() && path.extension().is_some_and(|e| e == "md") {
                let Ok(text) = fs::read_to_string(&path) else {
                    continue;
                };
                for line in text.lines() {
                    if let Some(id) = line.trim().strip_prefix("id::") {
                        if let Ok(id) = Uuid::parse_str(id.trim()) {
                            ids.insert(id);
                        }
                    }
                }
            }
        }
    }
    let mut ids = HashSet::new();
    for sub in ["pages", "journals", ".trash"] {
        scan(&graph.join(sub), &mut ids);
    }
    ids
}

/// `title`, or `title (2)`, ... : the first not in `used` (lowercase).
fn unused(title: &str, used: &HashSet<String>) -> String {
    let mut pick = title.to_string();
    let mut n = 2;
    while used.contains(&pick.to_lowercase()) {
        pick = format!("{title} ({n})");
        n += 1;
    }
    pick
}

/// Carry out `plan` in the graph at `graph`: settle the names, write the
/// pages and the log page (titled with `stamp`, e.g. "2026-10-09 14.03"),
/// copy the attachments. Never overwrites a file.
pub fn apply(
    plan: Plan,
    on_clash: OnClash,
    graph: &Path,
    existing: &Existing,
    stamp: &str,
) -> Result<Summary, String> {
    let taken: HashSet<String> = existing.titles.iter().map(|t| t.to_lowercase()).collect();
    let mut skipped = plan.skipped;
    let mut renamed_list: Vec<(String, String)> = Vec::new();
    let mut renamed: HashMap<String, String> = HashMap::new();
    let mut drafts = Vec::new();
    let mut used: HashSet<String> = taken.clone();
    used.extend(plan.drafts.iter().map(|d| d.title.to_lowercase()));
    for mut draft in plan.drafts {
        if !taken.contains(&draft.title.to_lowercase()) {
            drafts.push(draft);
            continue;
        }
        match on_clash {
            OnClash::Skip => skipped.push((
                draft.source.clone(),
                format!("\u{201c}{}\u{201d} already exists", draft.title),
            )),
            OnClash::Rename => {
                let new = unused(&format!("{} (imported)", draft.title), &used);
                used.insert(new.to_lowercase());
                renamed.insert(draft.title.to_lowercase(), new.clone());
                renamed_list.push((draft.title.clone(), new.clone()));
                draft.title = new;
                // "2026-10-09 (imported)" isn't a date: a normal page.
                draft.is_journal = false;
                drafts.push(draft);
            }
        }
    }
    let log_title = unused(
        &format!("Import from {} {stamp}", plan.source.name()),
        &used,
    );
    // Everything a link may name without being unresolved.
    let mut known: HashSet<String> = taken.clone();
    known.extend(existing.aliases.iter().map(|a| a.to_lowercase()));
    for d in &drafts {
        known.insert(d.title.to_lowercase());
        for p in &d.props {
            if let Some(list) = p.strip_prefix("alias:: ") {
                known.extend(list.split(',').map(|a| a.trim().to_lowercase()));
            }
        }
    }
    // Block ids: one already in the graph (the same graph imported again)
    // would make its `((id))` references reach either block, so the
    // imported block gets a new id and the imported pages' references
    // follow (`remap`, this import only; the graph's pages keep pointing
    // at their own blocks). An id twice in the import: the first keeps it.
    let mut taken_ids = existing.ids.clone();
    taken_ids.extend(ids_in_graph(graph));
    let mut seen_ids: HashSet<Uuid> = HashSet::new();
    let mut remap: HashMap<Uuid, Uuid> = HashMap::new();
    let mut new_ids = 0;
    for draft in &mut drafts {
        for (_, _, id) in &mut draft.rows {
            let Some(old) = *id else { continue };
            let in_graph = taken_ids.contains(&old);
            if !in_graph && seen_ids.insert(old) {
                continue;
            }
            let new = loop {
                let new = Uuid::new_v4();
                if !taken_ids.contains(&new) && !seen_ids.contains(&new) {
                    break new;
                }
            };
            seen_ids.insert(new);
            if in_graph {
                remap.entry(old).or_insert(new);
            }
            *id = Some(new);
            new_ids += 1;
        }
    }
    let mut unresolved: Vec<String> = Vec::new();
    let mut pages = Vec::new();
    for draft in &mut drafts {
        for (_, content, _) in &mut draft.rows {
            *content = remap_block_refs(&rename_refs(content, &renamed), &remap);
            for link in parse_wikilinks(content) {
                let lc = link.target.to_lowercase();
                if !known.contains(&lc) && !unresolved.iter().any(|u| u.to_lowercase() == lc) {
                    unresolved.push(link.target.clone());
                }
            }
        }
        for p in &mut draft.props {
            *p = remap_block_refs(&rename_refs(p, &renamed), &remap);
        }
        let mut props = draft.props.clone();
        props.push(format!("imported-from:: [[{log_title}]]"));
        let mut rows: Vec<Row> = vec![(0, props.join("\n"), None)];
        rows.extend(draft.rows.iter().cloned());
        pages.push(markdown::page_from_rows(
            &draft.title,
            draft.is_journal,
            rows,
        ));
    }

    for sub in ["pages", "journals", "assets"] {
        fs::create_dir_all(graph.join(sub)).map_err(|err| format!("Can't create {sub}/: {err}"))?;
    }
    let mut written = Vec::new();
    for (page, draft) in pages.iter().zip(&drafts) {
        let path = crate::storage::page_file(graph, &page.title, page.is_journal);
        if path.symlink_metadata().is_ok() {
            skipped.push((
                draft.source.clone(),
                format!("{} already exists", path.display()),
            ));
            continue;
        }
        match write_atomic(&path, &page.to_markdown()) {
            Ok(()) => written.push((page.title.clone(), page.is_journal)),
            Err(err) => skipped.push((draft.source.clone(), format!("could not write: {err}"))),
        }
    }
    let mut copied = 0;
    for asset in &plan.assets {
        let to = graph.join("assets").join(&asset.name);
        if to.symlink_metadata().is_ok() {
            skipped.push((asset.name.clone(), "already in assets/".into()));
            continue;
        }
        let tmp = graph.join("assets").join(format!(".{}.tmp", asset.name));
        let result = fs::copy(&asset.from, &tmp).and_then(|_| fs::rename(&tmp, &to));
        match result {
            Ok(()) => copied += 1,
            Err(err) => {
                let _ = fs::remove_file(&tmp);
                skipped.push((asset.name.clone(), format!("could not copy: {err}")));
            }
        }
    }

    // The log page: what happened, and the way back.
    let mut rows: Vec<Row> = vec![(
        0,
        format!(
            "Imported from {} on {stamp}: `{}`",
            plan.source.name(),
            plan.folder.display()
        ),
        None,
    )];
    rows.push((0, format!("Pages ({})", written.len()), None));
    rows.extend(written.iter().map(|(t, _)| (1, format!("[[{t}]]"), None)));
    rows.push((0, format!("Assets copied to assets/: {copied}"), None));
    if !renamed_list.is_empty() {
        rows.push((
            0,
            format!("Renamed, the name was taken ({})", renamed_list.len()),
            None,
        ));
        rows.extend(
            renamed_list
                .iter()
                .map(|(old, new)| (1, format!("{old} \u{2192} [[{new}]]"), None)),
        );
    }
    if new_ids > 0 {
        rows.push((
            0,
            format!(
                "Block ids renewed, already in this graph or repeated in the import: {new_ids} ({} remapped in the imported pages' references)",
                remap.len()
            ),
            None,
        ));
    }
    if !skipped.is_empty() {
        rows.push((0, format!("Skipped ({})", skipped.len()), None));
        rows.extend(
            skipped
                .iter()
                .map(|(what, why)| (1, format!("`{what}`: {why}"), None)),
        );
    }
    if !unresolved.is_empty() {
        rows.push((
            0,
            format!("Links to pages that don't exist ({})", unresolved.len()),
            None,
        ));
        rows.extend(unresolved.iter().map(|u| (1, format!("`{u}`"), None)));
    }
    rows.push((
        0,
        "To undo this import, delete the pages listed above (each one's imported-from:: links here, so they are also under Linked from), then this page.".to_string(),
        None,
    ));
    let log = markdown::page_from_rows(&log_title, false, rows);
    let log_path = crate::storage::page_file(graph, &log_title, false);
    if log_path.symlink_metadata().is_err() {
        write_atomic(&log_path, &log.to_markdown())
            .map_err(|err| format!("Could not write the import log: {err}"))?;
    }
    Ok(Summary {
        source: plan.source,
        log_title,
        pages: written,
        assets: copied,
        unresolved: unresolved.len(),
        skipped: skipped.len(),
        renamed: renamed_list.len(),
        new_ids,
    })
}

/// Shared by the importers: a note is a journal if its title is a date.
pub fn journal_title(title: &str) -> bool {
    is_journal_title(title)
}

#[cfg(test)]
mod tests;
