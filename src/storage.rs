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
use std::io;
use std::path::{Path, PathBuf};

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

    pub fn root(&self) -> &Path {
        &self.root
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
        fs::write(self.path_for(page), page.to_markdown())
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
