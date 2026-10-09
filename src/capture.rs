//! Quick capture (design pass, Feature 3): jot a note from anywhere into
//! today's journal.
//!
//! Two entry points share the block-appending helper here:
//!
//! - `notesec --capture "some text"`: [`append_to_journal`] appends the text
//!   as a new block to today's journal file and exits (no window; see
//!   `main.rs`).
//! - The in-app capture box (`app/capture_ui.rs`) appends to the in-memory
//!   pages instead, so undo/tabs stay consistent, and saves through the
//!   normal storage path.
//!
//! The vault format is unchanged: a plain `- ` bullet in the journal file.

use crate::model::Page;
use crate::storage::{today_title, Storage};
use std::io;
use std::path::PathBuf;

/// Append `text` (trimmed) as a new top-level block to `page`. A journal
/// that is just one blank bullet (a fresh journal) gets that bullet
/// replaced, so capturing into a new day doesn't leave a stray empty bullet.
/// This mirrors the clipper's inbox append (`app/clipper_ui.rs`).
pub fn push_block(page: &mut Page, content: String) {
    if page.blocks.len() == 1 && page.blocks[0].content.trim().is_empty() {
        page.blocks[0].content = content;
        return;
    }
    let order = page.blocks.iter().filter(|b| b.parent_id.is_none()).count();
    page.blocks.push(crate::model::Block {
        id: uuid::Uuid::new_v4(),
        content,
        parent_id: None,
        page_id: page.id.clone(),
        order,
    });
}

/// Append `text` as a new block to today's journal on disk, creating the
/// journal if needed. Returns the journal's title (`YYYY-MM-DD`).
/// Empty/blank text is refused rather than writing an empty bullet.
pub fn append_to_journal(storage: &Storage, text: &str) -> io::Result<String> {
    let content = text.trim().to_string();
    if content.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "nothing to capture: the text is empty",
        ));
    }
    let today = today_title();
    let mut pages = storage.load_all();
    match pages.iter_mut().find(|p| p.is_journal && p.title == today) {
        Some(journal) => {
            push_block(journal, content);
            storage.save(journal)?;
        }
        None => {
            let mut journal = Page::from_markdown(&today, true, "- \n");
            journal.blocks[0].content.clear();
            push_block(&mut journal, content);
            storage.save(&journal)?;
        }
    }
    Ok(today)
}

/// What the CLI parser found.
#[derive(Debug, PartialEq)]
pub struct CaptureCli {
    /// The graph directory: `--notes-dir <dir>`, else `NOTESEC_DIR`/`~/notesec`
    /// via [`Storage::default_root`].
    pub root: PathBuf,
    /// The text after `--capture`, if given.
    pub capture: Option<String>,
}

/// Parse `args` (without the program name): `--capture <text>` captures
/// headlessly, `--notes-dir <dir>` overrides the graph location. Both
/// `--flag value` and `--flag=value` forms work.
pub fn parse_cli(args: &[String]) -> Result<CaptureCli, String> {
    let mut root = Storage::default_root();
    let mut capture: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v.to_string())),
            _ => (arg, None),
        };
        match flag {
            "--capture" => {
                let value = match inline {
                    Some(v) => v,
                    None => {
                        i += 1;
                        args.get(i)
                            .cloned()
                            .ok_or("--capture needs some text".to_string())?
                    }
                };
                capture = Some(value);
            }
            "--notes-dir" => {
                let value = match inline {
                    Some(v) => v,
                    None => {
                        i += 1;
                        args.get(i)
                            .cloned()
                            .ok_or("--notes-dir needs a directory".to_string())?
                    }
                };
                root = PathBuf::from(value);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
        i += 1;
    }
    Ok(CaptureCli { root, capture })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_root(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("notesec-capture-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn journal_file(dir: &std::path::Path) -> PathBuf {
        dir.join(format!("journals/{}.md", today_title().replace('-', "_")))
    }

    #[test]
    fn capture_creates_todays_journal() {
        let root = temp_root("create");
        let storage = Storage::open(root.clone()).unwrap();
        let title = append_to_journal(&storage, "hello").unwrap();
        assert_eq!(title, today_title());
        assert_eq!(
            fs::read_to_string(journal_file(&root)).unwrap(),
            "- hello\n"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn capture_appends_below_existing_blocks() {
        let root = temp_root("append");
        let storage = Storage::open(root.clone()).unwrap();
        fs::write(journal_file(&root), "- first\n").unwrap();
        append_to_journal(&storage, "  second  ").unwrap();
        assert_eq!(
            fs::read_to_string(journal_file(&root)).unwrap(),
            "- first\n- second\n"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn blank_capture_is_refused() {
        let root = temp_root("blank");
        let storage = Storage::open(root.clone()).unwrap();
        let err = append_to_journal(&storage, "   ").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(!journal_file(&root).exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cli_parses_capture_and_notes_dir() {
        let args = vec!["--capture".to_string(), "hi".to_string()];
        let cli = parse_cli(&args).unwrap();
        assert_eq!(cli.capture.as_deref(), Some("hi"));
        assert_eq!(cli.root, Storage::default_root());

        let args = vec![
            "--notes-dir=/tmp/x".to_string(),
            "--capture=hey".to_string(),
        ];
        let cli = parse_cli(&args).unwrap();
        assert_eq!(cli.capture.as_deref(), Some("hey"));
        assert_eq!(cli.root, PathBuf::from("/tmp/x"));

        assert!(parse_cli(&["--capture".to_string()]).is_err());
        assert!(parse_cli(&["--bogus".to_string()]).is_err());
        assert!(parse_cli(&[]).unwrap().capture.is_none());
    }
}
