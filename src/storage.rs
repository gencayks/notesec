//! Reading and writing the on-disk graph.
//!
//! Layout (same as Logseq, so a graph can be opened by either app):
//!
//! ```text
//! <graph>/pages/<title>.md
//! <graph>/journals/YYYY_MM_DD.md      (page title is shown as YYYY-MM-DD)
//! <graph>/templates/<name>.md         (notesec only; Logseq ignores it)
//! ```
//!
//! The markdown files are the source of truth; nothing is cached elsewhere.

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

    /// Delete `page`'s file. A file that is already gone counts as deleted.
    pub fn delete(&self, page: &Page) -> io::Result<()> {
        match fs::remove_file(self.path_for(page)) {
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
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

    #[test]
    fn delete_removes_the_file_and_tolerates_a_missing_one() {
        let root = temp_root("delete");
        let storage = Storage::open(root.clone()).unwrap();
        let journal = Page::from_markdown("2026-10-08", true, "- day\n");
        storage.save(&journal).unwrap();
        assert!(root.join("journals/2026_10_08.md").exists());
        storage.delete(&journal).unwrap();
        assert!(!root.join("journals/2026_10_08.md").exists());
        storage.delete(&journal).unwrap();
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
