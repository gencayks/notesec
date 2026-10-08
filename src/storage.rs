//! Reading and writing the on-disk graph.
//!
//! Layout (same as Logseq, so a graph can be opened by either app):
//!
//! ```text
//! <graph>/pages/<title>.md
//! <graph>/journals/YYYY_MM_DD.md      (page title is shown as YYYY-MM-DD)
//! <graph>/templates/<name>.md         (notesec only; Logseq ignores it)
//! <graph>/.trash/<millis>/pages/<title>.md        (deleted pages, see below)
//! <graph>/.trash/<millis>/journals/YYYY_MM_DD.md
//! <graph>/exports/<page file name>.html          (Export page to HTML)
//! ```
//!
//! The markdown files are the source of truth; nothing is cached elsewhere.
//!
//! Deleting a page moves its file into the trash: a folder per deleted page,
//! named by the deletion time (milliseconds since the Unix epoch, which
//! sorts and needs no time zone), holding the file under its original path
//! relative to the graph. Restoring moves it back to that
//! path. Only `pages/` and `journals/` are read as pages, so nothing in the
//! hidden `.trash/` folder is ever loaded as one.

use crate::model::Page;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Write `contents` to `path` atomically: write a temporary file next to it,
/// flush it to disk, then rename it over the target. A crash or kill at any
/// point leaves either the complete old file or the complete new one, never a
/// truncated mix. (Rename is atomic when both paths are on the same
/// filesystem, which is why the temp file lives in the same directory.)
pub fn write_atomic(path: &Path, contents: &str) -> io::Result<()> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    // Hidden, and not `.md`, so page loading never mistakes it for a page.
    let tmp = path.with_file_name(format!(".{}.tmp", name.to_string_lossy()));
    let mut file = fs::File::create(&tmp)?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, path).inspect_err(|_| {
        // Don't leave the temp file behind if the rename failed.
        let _ = fs::remove_file(&tmp);
    })
}

/// The example template written into a new graph's `templates/` folder.
const DEFAULT_TEMPLATE: (&str, &str) = (
    "Daily review",
    include_str!("../assets/templates/Daily review.md"),
);

/// A page template: a markdown file in `<graph>/templates/`. Its name is the
/// file name without `.md`; its body uses the same bullet format as pages.
#[derive(Clone, Debug, PartialEq)]
pub struct Template {
    pub name: String,
    pub markdown: String,
}

pub struct Storage {
    root: PathBuf,
}

impl Storage {
    /// Open (creating if needed) a graph directory.
    pub fn open(root: PathBuf) -> io::Result<Self> {
        fs::create_dir_all(root.join("pages"))?;
        fs::create_dir_all(root.join("journals"))?;
        let storage = Storage { root };
        storage.seed_templates()?;
        Ok(storage)
    }

    /// Create `templates/` with one example template, but only if the folder
    /// doesn't exist yet: once it exists the user owns it, so deleting the
    /// example (or every template) sticks.
    fn seed_templates(&self) -> io::Result<()> {
        let dir = self.templates_dir();
        if dir.exists() {
            return Ok(());
        }
        fs::create_dir_all(&dir)?;
        let (name, markdown) = DEFAULT_TEMPLATE;
        write_atomic(&dir.join(format!("{name}.md")), markdown)
    }

    /// Where templates live: `<graph>/templates/`.
    pub fn templates_dir(&self) -> PathBuf {
        self.root.join("templates")
    }

    /// Every template, sorted by name (ignoring case). Read fresh from disk
    /// each time so files added while the app runs show up straight away.
    /// Unreadable files are skipped with a warning.
    pub fn load_templates(&self) -> Vec<Template> {
        let Ok(entries) = fs::read_dir(self.templates_dir()) else {
            return Vec::new();
        };
        let mut templates: Vec<Template> = entries
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("md") {
                    return None;
                }
                let name = path.file_stem()?.to_str()?.to_string();
                // Skip hidden files, e.g. the temp files of `write_atomic`.
                if name.starts_with('.') {
                    return None;
                }
                match fs::read_to_string(&path) {
                    Ok(markdown) => Some(Template { name, markdown }),
                    Err(err) => {
                        eprintln!("notesec: skipping template {}: {err}", path.display());
                        None
                    }
                }
            })
            .collect();
        templates.sort_by_key(|t| t.name.to_lowercase());
        templates
    }

    /// Default graph location: `~/notesec`. Override with `NOTESEC_DIR`.
    pub fn default_root() -> PathBuf {
        if let Some(dir) = std::env::var_os("NOTESEC_DIR") {
            return PathBuf::from(dir);
        }
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("notesec")
    }

    /// Load every page in the graph. Unreadable files are skipped with a warning
    /// rather than aborting startup.
    pub fn load_all(&self) -> Vec<Page> {
        let mut pages = Vec::new();
        for (sub, is_journal) in [("pages", false), ("journals", true)] {
            let Ok(entries) = fs::read_dir(self.root.join(sub)) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("md") {
                    continue;
                }
                let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                match fs::read_to_string(&path) {
                    Ok(text) => {
                        let title = title_from_filename(stem, is_journal);
                        pages.push(Page::from_markdown(&title, is_journal, &text));
                    }
                    Err(err) => eprintln!("notesec: skipping {}: {err}", path.display()),
                }
            }
        }
        pages
    }

    /// Write a page to its file (creating it if needed).
    pub fn save(&self, page: &Page) -> io::Result<()> {
        write_atomic(&self.path_for(page), &page.to_markdown())
    }

    /// Rename `page`'s file to the one for `new_title` (same folder). The
    /// page's current content must already be saved. `fs::rename` is atomic
    /// on one filesystem, so a crash leaves either the old file or the new
    /// one. Fails with `AlreadyExists` rather than overwrite another page's
    /// file (a case-only rename of the same file is allowed). If the old
    /// file is missing, the page is simply written under the new name.
    pub fn rename(&self, page: &Page, new_title: &str) -> io::Result<()> {
        let old = self.path_for(page);
        let renamed = Page {
            title: new_title.to_string(),
            ..page.clone()
        };
        let new = self.path_for(&renamed);
        let same_file =
            old.to_string_lossy().to_lowercase() == new.to_string_lossy().to_lowercase();
        if new.exists() && !same_file {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} already exists", new.display()),
            ));
        }
        if old.exists() {
            fs::rename(&old, &new)
        } else {
            self.save(&renamed)
        }
    }

    /// Where deleted pages go: `<graph>/.trash/`.
    pub fn trash_dir(&self) -> PathBuf {
        self.root.join(TRASH_DIR)
    }

    /// Move `page`'s file into the trash, deleted now. See [`Storage::trash_at`].
    pub fn trash(&self, page: &Page) -> io::Result<TrashEntry> {
        let now = chrono::Local::now().timestamp_millis();
        self.trash_at(page, now)
    }

    /// Move `page`'s file into a new trash folder named by `deleted_at`
    /// (milliseconds since the epoch; bumped by one until the name is free,
    /// so two deletions in the same millisecond don't collide), keeping its
    /// path relative to the graph (`pages/<title>.md` or
    /// `journals/YYYY_MM_DD.md`). The page's current content must already
    /// be saved; if its file is missing, that content is written into the
    /// trash instead, so the page can still be restored.
    pub fn trash_at(&self, page: &Page, deleted_at: i64) -> io::Result<TrashEntry> {
        let source = self.path_for(page);
        let relative = source
            .strip_prefix(&self.root)
            .map_err(|_| io::Error::other("page file outside the graph"))?
            .to_path_buf();
        let trash = self.trash_dir();
        fs::create_dir_all(&trash)?;
        let mut deleted_at = deleted_at;
        let folder = loop {
            let folder = trash.join(deleted_at.to_string());
            match fs::create_dir(&folder) {
                Ok(()) => break folder,
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => deleted_at += 1,
                Err(err) => return Err(err),
            }
        };
        let file = folder.join(&relative);
        let moved = (|| {
            fs::create_dir_all(file.parent().unwrap_or(&folder))?;
            if source.exists() {
                fs::rename(&source, &file)
            } else {
                write_atomic(&file, &page.to_markdown())
            }
        })();
        if let Err(err) = moved {
            // Leave no half-made entry behind; the page file is untouched.
            let _ = fs::remove_dir_all(&folder);
            return Err(err);
        }
        Ok(TrashEntry {
            id: deleted_at.to_string(),
            title: page.title.clone(),
            is_journal: page.is_journal,
            deleted_at,
            relative,
        })
    }

    /// Every page in the trash, newest first. Folders that don't look like
    /// a trash entry (not a number, or not exactly one page file under
    /// `pages/` or `journals/`) are skipped and left alone.
    pub fn list_trash(&self) -> Vec<TrashEntry> {
        let Ok(folders) = fs::read_dir(self.trash_dir()) else {
            return Vec::new();
        };
        let mut entries: Vec<TrashEntry> = folders
            .flatten()
            .filter_map(|folder| {
                let id = folder.file_name().to_str()?.to_string();
                let deleted_at: i64 = id.parse().ok()?;
                let mut files = Vec::new();
                for (sub, is_journal) in [("pages", false), ("journals", true)] {
                    let Ok(found) = fs::read_dir(folder.path().join(sub)) else {
                        continue;
                    };
                    for file in found.flatten() {
                        let path = file.path();
                        if path.extension().and_then(|e| e.to_str()) != Some("md") {
                            continue;
                        }
                        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                            continue;
                        };
                        if stem.starts_with('.') {
                            continue;
                        }
                        files.push(TrashEntry {
                            id: id.clone(),
                            title: title_from_filename(stem, is_journal),
                            is_journal,
                            deleted_at,
                            relative: Path::new(sub).join(file.file_name()),
                        });
                    }
                }
                (files.len() == 1).then(|| files.remove(0))
            })
            .collect();
        entries.sort_by(|a, b| (b.deleted_at, &b.id).cmp(&(a.deleted_at, &a.id)));
        entries
    }

    /// Move `entry`'s file back to where it was and return the page read
    /// from it. Fails with `AlreadyExists` (and changes nothing) if a file
    /// is at that path again; the caller also checks titles ignoring case.
    pub fn restore(&self, entry: &TrashEntry) -> io::Result<Page> {
        let file = self.trash_file(entry);
        let target = self.root.join(&entry.relative);
        if target.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} already exists", target.display()),
            ));
        }
        let markdown = fs::read_to_string(&file)?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(&file, &target)?;
        // The entry's folder only held that file.
        let _ = fs::remove_dir_all(self.trash_dir().join(&entry.id));
        Ok(Page::from_markdown(
            &entry.title,
            entry.is_journal,
            &markdown,
        ))
    }

    /// Delete `entry` for good: its whole trash folder. An entry that is
    /// already gone counts as deleted.
    pub fn delete_forever(&self, entry: &TrashEntry) -> io::Result<()> {
        match fs::remove_dir_all(self.trash_dir().join(&entry.id)) {
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }

    /// Delete every trash entry for good (only the folders `list_trash`
    /// recognises; anything else in `.trash/` is left alone). Returns how
    /// many were deleted.
    pub fn empty_trash(&self) -> io::Result<usize> {
        let entries = self.list_trash();
        for entry in &entries {
            self.delete_forever(entry)?;
        }
        Ok(entries.len())
    }

    /// Where "Export page to HTML" writes `page`: `<graph>/exports/` and
    /// the page file's name with `.html` (`Area___Sub.html`,
    /// `2026_10_08.html`), so the same page always lands on the same file.
    pub fn export_path(&self, page: &Page) -> PathBuf {
        let file = self.path_for(page).with_extension("html");
        let name = file.file_name().map(PathBuf::from).unwrap_or_default();
        self.root.join(EXPORTS_DIR).join(name)
    }

    /// Write `html` as `page`'s export (atomically, replacing an earlier
    /// export), creating `exports/` if needed. Returns the path written.
    pub fn write_export(&self, page: &Page, html: &str) -> io::Result<PathBuf> {
        let path = self.export_path(page);
        fs::create_dir_all(self.root.join(EXPORTS_DIR))?;
        write_atomic(&path, html)?;
        Ok(path)
    }

    /// The trashed file of `entry`.
    fn trash_file(&self, entry: &TrashEntry) -> PathBuf {
        self.trash_dir().join(&entry.id).join(&entry.relative)
    }

    /// The graph directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path_for(&self, page: &Page) -> PathBuf {
        if page.is_journal {
            // Logseq names journal files with underscores.
            self.root
                .join("journals")
                .join(format!("{}.md", page.title.replace('-', "_")))
        } else {
            self.root
                .join("pages")
                .join(format!("{}.md", filename_from_title(&page.title)))
        }
    }
}

/// The trash folder inside the graph. Hidden (a dot folder), and outside
/// `pages/` and `journals/`, so its files are never loaded as pages.
pub const TRASH_DIR: &str = ".trash";

/// Where page exports go, inside the graph. Not `pages/` or `journals/`,
/// so exports are never loaded as pages.
pub const EXPORTS_DIR: &str = "exports";

/// A deleted page in the trash (see the module docs for the layout).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrashEntry {
    /// The entry's folder name in `.trash/` (its deletion time).
    pub id: String,
    pub title: String,
    pub is_journal: bool,
    /// When it was deleted, in milliseconds since the Unix epoch.
    pub deleted_at: i64,
    /// Where its file was (and goes back to), relative to the graph:
    /// `pages/<file>.md` or `journals/<file>.md`.
    pub relative: PathBuf,
}

/// Longest file name most filesystems allow, in bytes.
const MAX_FILENAME_BYTES: usize = 255;

/// Check a new page title (from Rename) and return it trimmed, or say why
/// it can't be used. The rules follow how titles become file names here:
/// `/` is fine (namespaces, stored as `___`), but a literal `___` would read
/// back as `/`; control characters (newlines, NUL) don't belong in a file
/// name; and the name must fit the filesystem's limit including the `.md`
/// extension and the `.<name>.tmp` file that `write_atomic` writes first.
/// Name collisions are checked by the caller, which knows the other pages.
pub fn validate_title(title: &str) -> Result<String, String> {
    let title = title.trim();
    if title.is_empty() {
        return Err("The name can't be empty".into());
    }
    if title.chars().any(char::is_control) {
        return Err("The name can't contain line breaks or control characters".into());
    }
    if title.contains("___") {
        return Err("The name can't contain \"___\" (it stands for / in file names)".into());
    }
    let file_name = format!(".{}.md.tmp", filename_from_title(title));
    if file_name.len() > MAX_FILENAME_BYTES {
        return Err("The name is too long".into());
    }
    Ok(title.to_string())
}

/// `/` can't appear in a filename; Logseq encodes it as `___`.
fn filename_from_title(title: &str) -> String {
    title.replace('/', "___")
}

fn title_from_filename(stem: &str, is_journal: bool) -> String {
    if is_journal {
        stem.replace('_', "-")
    } else {
        stem.replace("___", "/")
    }
}

/// Today's date as `YYYY-MM-DD`, in local time.
pub fn today_title() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{cycle_task, task_split, TaskState};

    #[test]
    fn scheduled_and_deadline_lines_round_trip_through_storage() {
        use crate::agenda::parse_dates;
        let dir =
            std::env::temp_dir().join(format!("notesec-storage-dates-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let storage = Storage::open(dir.clone()).unwrap();
        // Logseq's layout: the markers are continuation lines of the task,
        // also under a nested task.
        let original = "- TODO report\n  SCHEDULED: <2026-10-09 Fri>\n  DEADLINE: <2026-10-12 Mon 17:00>\n  - DOING part\n    SCHEDULED: <2026-10-10 Sat>\n- plain\n";
        fs::write(dir.join("pages/Dates.md"), original).unwrap();

        let pages = storage.load_all();
        let page = &pages[0];
        assert_eq!(
            page.blocks[0].content,
            "TODO report\nSCHEDULED: <2026-10-09 Fri>\nDEADLINE: <2026-10-12 Mon 17:00>"
        );
        assert_eq!(page.depth_of(1), 1);
        let dates = parse_dates(&page.blocks[0].content);
        assert!(dates.scheduled.is_some() && dates.deadline.is_some());
        assert!(parse_dates(&page.blocks[1].content).scheduled.is_some());

        // Saved back byte for byte.
        storage.save(page).unwrap();
        assert_eq!(
            fs::read_to_string(dir.join("pages/Dates.md")).unwrap(),
            original
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn task_states_round_trip_through_storage() {
        let dir =
            std::env::temp_dir().join(format!("notesec-storage-tasks-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let storage = Storage::open(dir.clone()).unwrap();
        let original =
            "- TODO a\n  - DOING b\n- DONE **c**\n- ## LATER d\n- NOW e\n- TODO \n- plain\n";
        fs::write(dir.join("pages/Tasks.md"), original).unwrap();

        let mut pages = storage.load_all();
        let page = &mut pages[0];
        let states: Vec<_> = page
            .blocks
            .iter()
            .map(|b| task_split(&b.content).1)
            .collect();
        use TaskState::*;
        assert_eq!(
            states,
            [
                Some(Todo),
                Some(Doing),
                Some(Done),
                Some(Later),
                Some(Now),
                Some(Todo),
                None
            ]
        );
        // Saving unchanged pages writes the same bytes.
        storage.save(page).unwrap();
        let path = dir.join("pages/Tasks.md");
        assert_eq!(fs::read_to_string(&path).unwrap(), original);

        // A cycled state is written as Logseq's keyword and reads back.
        page.blocks[0].content = cycle_task(&page.blocks[0].content);
        page.blocks[6].content = cycle_task(&page.blocks[6].content);
        storage.save(page).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            original
                .replace("- TODO a", "- DOING a")
                .replace("- plain", "- TODO plain")
        );
        let reloaded = storage.load_all();
        assert_eq!(task_split(&reloaded[0].blocks[0].content).1, Some(Doing));
        assert_eq!(reloaded[0].blocks[6].content, "TODO plain");
        let _ = fs::remove_dir_all(dir);
    }

    fn temp_root(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("notesec-storage-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn new_graph_gets_the_example_template() {
        let root = temp_root("seed");
        let storage = Storage::open(root.clone()).unwrap();
        let templates = storage.load_templates();
        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].name, "Daily review");
        assert_eq!(templates[0].markdown, DEFAULT_TEMPLATE.1);
        assert!(root.join("templates/Daily review.md").is_file());
    }

    #[test]
    fn deleted_example_template_is_not_recreated() {
        let root = temp_root("no-reseed");
        Storage::open(root.clone()).unwrap();
        fs::remove_file(root.join("templates/Daily review.md")).unwrap();
        let storage = Storage::open(root).unwrap();
        assert!(storage.load_templates().is_empty());
    }

    #[test]
    fn templates_list_md_files_sorted_by_name() {
        let root = temp_root("list");
        let storage = Storage::open(root.clone()).unwrap();
        let dir = storage.templates_dir();
        fs::write(dir.join("meeting.md"), "- Agenda\n").unwrap();
        fs::write(dir.join("Book notes.md"), "- Author\n").unwrap();
        fs::write(dir.join("notes.txt"), "not a template").unwrap();
        fs::write(dir.join(".meeting.md.tmp"), "- half written").unwrap();
        let names: Vec<String> = storage
            .load_templates()
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert_eq!(names, ["Book notes", "Daily review", "meeting"]);
    }

    #[test]
    fn rename_moves_the_file_and_refuses_to_overwrite() {
        let root = temp_root("rename");
        let storage = Storage::open(root.clone()).unwrap();
        let page = Page::from_markdown("Old", false, "- body\n");
        storage.save(&page).unwrap();
        storage
            .save(&Page::from_markdown("Taken", false, "- other\n"))
            .unwrap();

        let err = storage.rename(&page, "Taken").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            fs::read_to_string(root.join("pages/Taken.md")).unwrap(),
            "- other\n"
        );

        // Namespaced titles use the same `___` mapping as saving.
        storage.rename(&page, "Area/New").unwrap();
        assert!(!root.join("pages/Old.md").exists());
        assert_eq!(
            fs::read_to_string(root.join("pages/Area___New.md")).unwrap(),
            "- body\n"
        );
        let titles: Vec<String> = storage.load_all().into_iter().map(|p| p.title).collect();
        assert!(titles.contains(&"Area/New".to_string()));

        // Case-only renames of the same file are allowed.
        let renamed = Page::from_markdown("Area/New", false, "- body\n");
        storage.rename(&renamed, "area/new").unwrap();
        assert!(root.join("pages/area___new.md").exists());
        let _ = fs::remove_dir_all(root);
    }

    fn ids(entries: &[TrashEntry]) -> Vec<&str> {
        entries.iter().map(|e| e.id.as_str()).collect()
    }

    #[test]
    fn trash_keeps_the_relative_path_and_restore_puts_the_page_back() {
        let root = temp_root("trash");
        let storage = Storage::open(root.clone()).unwrap();
        let page = Page::from_markdown(
            "Area/Sub",
            false,
            "- body
  - child
",
        );
        let journal = Page::from_markdown(
            "2026-10-08",
            true,
            "- day
",
        );
        storage.save(&page).unwrap();
        storage.save(&journal).unwrap();

        let entry = storage.trash_at(&page, 1_000).unwrap();
        assert_eq!(entry.id, "1000");
        assert_eq!(entry.relative, Path::new("pages/Area___Sub.md"));
        assert!(!root.join("pages/Area___Sub.md").exists());
        assert_eq!(
            fs::read_to_string(root.join(".trash/1000/pages/Area___Sub.md")).unwrap(),
            "- body\n  - child\n"
        );
        storage.trash_at(&journal, 2_000).unwrap();
        assert!(root.join(".trash/2000/journals/2026_10_08.md").exists());
        // Gone from the pages, listed in the trash newest first.
        assert!(storage.load_all().is_empty());
        let trash = storage.list_trash();
        assert_eq!(ids(&trash), ["2000", "1000"]);
        assert_eq!(
            (
                trash[0].title.as_str(),
                trash[0].is_journal,
                trash[0].deleted_at
            ),
            ("2026-10-08", true, 2_000)
        );
        assert_eq!(
            (trash[1].title.as_str(), trash[1].is_journal),
            ("Area/Sub", false)
        );
        assert_eq!(trash[1], entry);

        // Restore: same file, same content, and its trash folder is gone.
        let restored = storage.restore(&entry).unwrap();
        assert_eq!(restored.title, "Area/Sub");
        assert!(!restored.is_journal);
        assert_eq!(restored.to_markdown(), "- body\n  - child\n");
        assert_eq!(
            fs::read_to_string(root.join("pages/Area___Sub.md")).unwrap(),
            "- body\n  - child\n"
        );
        assert!(!root.join(".trash/1000").exists());
        assert_eq!(ids(&storage.list_trash()), ["2000"]);
        let journal = storage.restore(&storage.list_trash()[0]).unwrap();
        assert!(journal.is_journal);
        assert!(root.join("journals/2026_10_08.md").exists());
        assert_eq!(storage.load_all().len(), 2);
        assert!(storage.list_trash().is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn same_millisecond_deletions_get_their_own_folders() {
        let root = temp_root("trash-same-ms");
        let storage = Storage::open(root.clone()).unwrap();
        // The same title deleted twice (re-created in between), and another.
        for (title, body) in [("A", "- first\n"), ("A", "- second\n"), ("B", "- b\n")] {
            let page = Page::from_markdown(title, false, body);
            storage.save(&page).unwrap();
            storage.trash_at(&page, 5).unwrap();
        }
        let trash = storage.list_trash();
        assert_eq!(ids(&trash), ["7", "6", "5"]);
        assert_eq!(
            fs::read_to_string(root.join(".trash/5/pages/A.md")).unwrap(),
            "- first\n"
        );
        assert_eq!(
            fs::read_to_string(root.join(".trash/6/pages/A.md")).unwrap(),
            "- second\n"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_page_without_a_file_is_trashed_with_its_content() {
        let root = temp_root("trash-no-file");
        let storage = Storage::open(root.clone()).unwrap();
        let page = Page::from_markdown("Unsaved", false, "- only in memory\n");
        let entry = storage.trash_at(&page, 9).unwrap();
        assert_eq!(
            fs::read_to_string(root.join(".trash/9/pages/Unsaved.md")).unwrap(),
            "- only in memory\n"
        );
        assert_eq!(storage.list_trash(), [entry]);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn restore_refuses_when_the_file_exists_again() {
        let root = temp_root("trash-clash");
        let storage = Storage::open(root.clone()).unwrap();
        let old = Page::from_markdown("Notes", false, "- old\n");
        storage.save(&old).unwrap();
        let entry = storage.trash_at(&old, 1).unwrap();
        storage
            .save(&Page::from_markdown("Notes", false, "- new\n"))
            .unwrap();

        let err = storage.restore(&entry).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        // Nothing moved: the new page and the trashed one are both intact.
        assert_eq!(
            fs::read_to_string(root.join("pages/Notes.md")).unwrap(),
            "- new\n"
        );
        assert_eq!(storage.list_trash(), [entry]);
        assert_eq!(
            fs::read_to_string(root.join(".trash/1/pages/Notes.md")).unwrap(),
            "- old\n"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn delete_forever_and_empty_trash_leave_unknown_files_alone() {
        let root = temp_root("trash-empty");
        let storage = Storage::open(root.clone()).unwrap();
        for (i, title) in ["One", "Two", "Three"].into_iter().enumerate() {
            let page = Page::from_markdown(title, false, "- x\n");
            storage.save(&page).unwrap();
            storage.trash_at(&page, i as i64).unwrap();
        }
        // Things that aren't trash entries are neither listed nor deleted.
        let trash = root.join(".trash");
        fs::create_dir_all(trash.join("notes/pages")).unwrap();
        fs::write(trash.join("notes/pages/Keep.md"), "- keep\n").unwrap();
        fs::write(trash.join("README.txt"), "mine").unwrap();
        fs::create_dir_all(trash.join("42")).unwrap();
        assert_eq!(ids(&storage.list_trash()), ["2", "1", "0"]);

        let two = storage.list_trash()[0].clone();
        storage.delete_forever(&two).unwrap();
        assert!(!trash.join("2").exists());
        assert_eq!(ids(&storage.list_trash()), ["1", "0"]);
        // Already gone counts as deleted.
        storage.delete_forever(&two).unwrap();

        assert_eq!(storage.empty_trash().unwrap(), 2);
        assert!(storage.list_trash().is_empty());
        assert!(!trash.join("1").exists() && !trash.join("0").exists());
        assert!(trash.join("notes/pages/Keep.md").exists());
        assert!(trash.join("README.txt").exists() && trash.join("42").exists());
        assert_eq!(storage.empty_trash().unwrap(), 0);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn exports_go_to_one_file_per_page_and_are_not_pages() {
        let root = temp_root("exports");
        let storage = Storage::open(root.clone()).unwrap();
        let page = Page::from_markdown("Area/Sub", false, "- x\n");
        let journal = Page::from_markdown("2026-10-08", true, "- day\n");
        storage.save(&page).unwrap();
        assert_eq!(
            storage.export_path(&page),
            root.join("exports/Area___Sub.html")
        );
        assert_eq!(
            storage.export_path(&journal),
            root.join("exports/2026_10_08.html")
        );
        let path = storage.write_export(&page, "<p>one</p>").unwrap();
        assert_eq!(path, root.join("exports/Area___Sub.html"));
        // Exporting again replaces the file.
        storage.write_export(&page, "<p>two</p>").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "<p>two</p>");
        let titles: Vec<String> = storage.load_all().into_iter().map(|p| p.title).collect();
        assert_eq!(titles, ["Area/Sub"]);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn the_trash_is_never_loaded_as_pages() {
        let root = temp_root("trash-not-pages");
        let storage = Storage::open(root.clone()).unwrap();
        let page = Page::from_markdown("Gone", false, "- bye\n");
        storage.save(&page).unwrap();
        storage.trash_at(&page, 3).unwrap();
        // Even files laid out like a graph inside the trash folder.
        fs::create_dir_all(root.join(".trash/pages")).unwrap();
        fs::write(root.join(".trash/pages/Stray.md"), "- stray\n").unwrap();
        assert!(storage.load_all().is_empty());
        assert!(storage.load_templates().iter().all(|t| t.name != "Gone"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn titles_are_validated_like_file_names() {
        assert_eq!(validate_title("  New name "), Ok("New name".to_string()));
        assert_eq!(validate_title("Area/Sub"), Ok("Area/Sub".to_string()));
        assert!(validate_title("   ").is_err());
        assert!(validate_title("two\nlines").is_err());
        assert!(validate_title("a___b").is_err());
        // 255-byte names, counting the `.<name>.md.tmp` of an atomic save.
        assert!(validate_title(&"x".repeat(247)).is_ok());
        assert!(validate_title(&"x".repeat(248)).is_err());
        // A `/` takes three bytes in the file name.
        assert!(validate_title(&format!("{}/", "x".repeat(245))).is_err());
    }

    #[test]
    fn templates_are_not_loaded_as_pages() {
        let root = temp_root("not-pages");
        let storage = Storage::open(root).unwrap();
        assert!(storage.load_all().is_empty());
    }
}
