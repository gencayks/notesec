//! Reading and writing the on-disk graph.
//!
//! Layout (same as Logseq, so a graph can be opened by either app):
//!
//! ```text
//! <graph>/pages/<title>.md
//! <graph>/journals/YYYY_MM_DD.md      (page title is shown as YYYY-MM-DD)
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

pub struct Storage {
    root: PathBuf,
}

impl Storage {
    /// Open (creating if needed) a graph directory.
    pub fn open(root: PathBuf) -> io::Result<Self> {
        fs::create_dir_all(root.join("pages"))?;
        fs::create_dir_all(root.join("journals"))?;
        Ok(Storage { root })
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
