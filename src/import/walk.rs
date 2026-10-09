//! Walk a source folder safely: every file under it, by path relative to
//! it, in a stable order. Hidden files and folders (`.obsidian`, `.trash`,
//! `.git`) and app folders (`logseq/` with its `bak/`, `bak`,
//! `node_modules`) are skipped. A symbolic link is followed only when it
//! points inside the folder (and each folder is visited once, so a link
//! loop ends); one pointing outside is listed as skipped.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Most files one import reads; the rest are skipped.
pub const MAX_FILES: usize = 50_000;

/// Folders never imported (any case), besides hidden ones.
const SKIP_DIRS: [&str; 5] = ["logseq", "bak", "node_modules", "version-files", "__macosx"];

/// One file found.
#[derive(Clone, Debug)]
pub struct Entry {
    /// Relative to the source folder, `/`-separated (`Folder/Note.md`).
    pub rel: String,
    pub path: PathBuf,
    pub size: u64,
}

/// What a walk found.
#[derive(Debug, Default)]
pub struct Walk {
    pub files: Vec<Entry>,
    /// (relative path, why) for what was left out on purpose.
    pub skipped: Vec<(String, String)>,
}

fn skip_dir(name: &str) -> bool {
    name.starts_with('.') || SKIP_DIRS.iter().any(|d| d.eq_ignore_ascii_case(name))
}

/// Every file under `root` (see the module docs).
pub fn walk(root: &Path) -> io::Result<Walk> {
    let real_root = root.canonicalize()?;
    let mut out = Walk::default();
    let mut seen: HashSet<PathBuf> = HashSet::from([real_root.clone()]);
    let mut stack: Vec<(PathBuf, String)> = vec![(real_root.clone(), String::new())];
    while let Some((dir, prefix)) = stack.pop() {
        let mut entries: Vec<_> = match fs::read_dir(&dir) {
            Ok(entries) => entries.flatten().collect(),
            Err(err) => {
                out.skipped
                    .push((prefix.clone(), format!("unreadable folder: {err}")));
                continue;
            }
        };
        entries.sort_by_key(|e| e.file_name());
        let mut subdirs = Vec::new();
        for entry in entries {
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                out.skipped.push((
                    format!("{prefix}{}", entry.file_name().to_string_lossy()),
                    "name is not UTF-8".into(),
                ));
                continue;
            };
            let rel = format!("{prefix}{name}");
            if name.starts_with('.') {
                continue;
            }
            let Ok(meta) = entry.path().symlink_metadata() else {
                continue;
            };
            let (path, meta) = if meta.is_symlink() {
                let target = match entry.path().canonicalize() {
                    Ok(t) if t.starts_with(&real_root) => t,
                    _ => {
                        out.skipped.push((rel, "link to outside the folder".into()));
                        continue;
                    }
                };
                let Ok(meta) = target.metadata() else {
                    continue;
                };
                (target, meta)
            } else {
                (entry.path(), meta)
            };
            if meta.is_dir() {
                if skip_dir(&name) {
                    continue;
                }
                let real = path.canonicalize().unwrap_or_else(|_| path.clone());
                if seen.insert(real.clone()) {
                    subdirs.push((real, format!("{rel}/")));
                }
            } else if meta.is_file() {
                if out.files.len() >= MAX_FILES {
                    out.skipped
                        .push((rel, format!("more than {MAX_FILES} files")));
                    continue;
                }
                out.files.push(Entry {
                    rel,
                    path,
                    size: meta.len(),
                });
            }
        }
        // Depth first, folders in name order.
        stack.extend(subdirs.into_iter().rev());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("notesec-walk-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn rels(walk: &Walk) -> Vec<&str> {
        walk.files.iter().map(|e| e.rel.as_str()).collect()
    }

    #[test]
    fn hidden_and_app_folders_are_skipped() {
        let dir = temp("skip");
        for f in [
            "a.md",
            "Sub/b.md",
            "Sub/Deeper/c.md",
            ".obsidian/app.json",
            ".trash/old.md",
            "logseq/bak/pages/x.md",
            "Logseq/config.edn",
            "bak/y.md",
            ".hidden.md",
            "z.png",
        ] {
            let p = dir.join(f);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, "x").unwrap();
        }
        let w = walk(&dir).unwrap();
        assert_eq!(rels(&w), ["a.md", "z.png", "Sub/b.md", "Sub/Deeper/c.md"]);
        assert_eq!(w.files[0].size, 1);
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn links_inside_are_followed_once_and_outside_never() {
        use std::os::unix::fs::symlink;
        let dir = temp("links");
        let outside = temp("links-outside");
        fs::write(outside.join("secret.md"), "s").unwrap();
        fs::create_dir_all(dir.join("Real")).unwrap();
        fs::write(dir.join("Real/n.md"), "n").unwrap();
        symlink(&outside, dir.join("Out")).unwrap();
        symlink(outside.join("secret.md"), dir.join("secret.md")).unwrap();
        symlink(dir.join("Real"), dir.join("Alias")).unwrap();
        symlink(&dir, dir.join("Real/Loop")).unwrap();
        let w = walk(&dir).unwrap();
        // "Alias" is the same folder as "Real": visited once.
        assert_eq!(rels(&w), ["Alias/n.md"]);
        let skipped: Vec<&str> = w.skipped.iter().map(|(r, _)| r.as_str()).collect();
        assert_eq!(skipped, ["Out", "secret.md"]);
        let _ = fs::remove_dir_all(dir);
        let _ = fs::remove_dir_all(outside);
    }
}
