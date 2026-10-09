//! The vault's plaintext: a tiny archive of the graph's files (decision 54).
//!
//! Format: `ARCHIVE_MAGIC`, then per file `u32` path length (big endian),
//! the path (UTF-8, `/`-separated, relative), `u64` length, the bytes; a
//! zero path length ends it. Paths are checked when packing and again
//! when unpacking (`check_path`): relative, no `.`/`..`/empty parts, no
//! backslashes or NULs, under an allowed top folder, no duplicates.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub const ARCHIVE_MAGIC: &[u8; 8] = b"NSARCH1\n";
/// Top-level folders taken, whole (whiteboards are pages).
pub const DIRS: [&str; 3] = ["pages", "journals", "assets"];
/// Top-level files taken (`state.toml` with its secrets removed).
pub const FILES: [&str; 2] = ["config.toml", "state.toml"];
/// Keys never put in a vault, wherever they are in `state.toml`.
pub const SECRET_KEYS: [&str; 2] = ["ai_api_key", "clipper_token"];
const MAX_PATH: usize = 4096;

/// One file: its path in the graph and its bytes.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub path: String,
    pub bytes: Vec<u8>,
}

/// Why a path can't be in a vault.
pub fn check_path(path: &str) -> Result<(), String> {
    let bad = |why: &str| Err(format!("Refused path \u{201c}{path}\u{201d}: {why}"));
    if path.is_empty() || path.len() > MAX_PATH {
        return bad("empty or too long");
    }
    if path.contains(['\\', '\0']) || path.starts_with('/') || path.contains(':') {
        return bad("not a plain relative path");
    }
    let parts: Vec<&str> = path.split('/').collect();
    if parts
        .iter()
        .any(|p| p.is_empty() || *p == "." || *p == "..")
    {
        return bad("empty, . or .. part");
    }
    let allowed = match parts.as_slice() {
        [file] => FILES.contains(file),
        [dir, ..] => DIRS.contains(dir),
        [] => false,
    };
    if !allowed {
        return bad("not a notes folder or settings file");
    }
    Ok(())
}

/// Skipped when collecting: hidden files (atomic-save temps like
/// `.name.tmp`, `.DS_Store`) and other `*.tmp` files.
fn skipped(name: &str) -> bool {
    name.starts_with('.') || name.ends_with(".tmp")
}

/// The graph's files that go in a vault, sorted by path. Symlinks are
/// skipped (never followed), as is everything outside `DIRS`/`FILES`
/// (`.git/`, `.trash/`, `.notesec/`, exports, published sites).
pub fn collect(root: &Path) -> io::Result<Vec<Entry>> {
    let mut out = Vec::new();
    for name in FILES {
        let path = root.join(name);
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_file() => {
                let bytes = fs::read(&path)?;
                let bytes = if name == "state.toml" {
                    strip_secrets(&bytes)
                } else {
                    bytes
                };
                out.push(Entry {
                    path: name.to_string(),
                    bytes,
                });
            }
            _ => {}
        }
    }
    for dir in DIRS {
        walk(root, &root.join(dir), &mut out)?;
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<Entry>) -> io::Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if skipped(name) {
            continue;
        }
        let kind = entry.file_type()?; // not followed: a symlink is a symlink
        let path = entry.path();
        if kind.is_dir() {
            walk(root, &path, out)?;
        } else if kind.is_file() {
            let Ok(rel) = path.strip_prefix(root) else {
                continue;
            };
            let rel: Vec<&str> = rel.iter().filter_map(|c| c.to_str()).collect();
            let rel = rel.join("/");
            if check_path(&rel).is_ok() {
                out.push(Entry {
                    path: rel,
                    bytes: fs::read(&path)?,
                });
            }
        }
    }
    Ok(())
}

/// `state.toml` without `SECRET_KEYS` (at any depth). A file that doesn't
/// parse is left out entirely rather than risk a secret in it.
pub fn strip_secrets(bytes: &[u8]) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Vec::new();
    };
    let Ok(mut table) = text.parse::<toml::Table>() else {
        return Vec::new();
    };
    fn strip(table: &mut toml::Table) {
        for key in SECRET_KEYS {
            table.remove(key);
        }
        for (_, value) in table.iter_mut() {
            match value {
                toml::Value::Table(t) => strip(t),
                toml::Value::Array(items) => {
                    for item in items {
                        if let toml::Value::Table(t) = item {
                            strip(t);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    strip(&mut table);
    toml::to_string(&table).unwrap_or_default().into_bytes()
}

pub fn pack(entries: &[Entry]) -> Result<Vec<u8>, String> {
    let size: usize = entries
        .iter()
        .map(|e| e.path.len() + e.bytes.len() + 12)
        .sum();
    let mut out = Vec::with_capacity(ARCHIVE_MAGIC.len() + size + 4);
    out.extend_from_slice(ARCHIVE_MAGIC);
    for e in entries {
        check_path(&e.path)?;
        out.extend_from_slice(&(e.path.len() as u32).to_be_bytes());
        out.extend_from_slice(e.path.as_bytes());
        out.extend_from_slice(&(e.bytes.len() as u64).to_be_bytes());
        out.extend_from_slice(&e.bytes);
    }
    out.extend_from_slice(&0u32.to_be_bytes());
    Ok(out)
}

/// Read and check a whole archive (nothing is written anywhere).
pub fn unpack(data: &[u8]) -> Result<Vec<Entry>, String> {
    let damaged = || "The vault's contents are damaged".to_string();
    let rest = data
        .strip_prefix(ARCHIVE_MAGIC.as_slice())
        .ok_or_else(damaged)?;
    let mut at = 0usize;
    let mut take = |n: usize| -> Result<&[u8], String> {
        let end = at
            .checked_add(n)
            .filter(|&e| e <= rest.len())
            .ok_or_else(damaged)?;
        let s = &rest[at..end];
        at = end;
        Ok(s)
    };
    let mut out = Vec::new();
    let mut files: HashSet<String> = HashSet::new();
    let mut dirs: HashSet<String> = HashSet::new();
    loop {
        let len = u32::from_be_bytes(take(4)?.try_into().map_err(|_| damaged())?) as usize;
        if len == 0 {
            break;
        }
        if len > MAX_PATH {
            return Err(damaged());
        }
        let path = std::str::from_utf8(take(len)?)
            .map_err(|_| damaged())?
            .to_string();
        check_path(&path)?;
        // The same file twice, or a file and a folder of the same name
        // (compared without case: case-insensitive file systems).
        let lower = path.to_lowercase();
        let folders: Vec<&str> = lower.match_indices('/').map(|(i, _)| &lower[..i]).collect();
        if files.contains(&lower)
            || dirs.contains(&lower)
            || folders.iter().any(|f| files.contains(*f))
        {
            return Err(format!("Refused path \u{201c}{path}\u{201d}: duplicate"));
        }
        dirs.extend(folders.iter().map(|f| f.to_string()));
        files.insert(lower);
        let size = u64::from_be_bytes(take(8)?.try_into().map_err(|_| damaged())?);
        let size = usize::try_from(size).map_err(|_| damaged())?;
        let bytes = take(size)?.to_vec();
        out.push(Entry { path, bytes });
    }
    if at != rest.len() {
        return Err(damaged());
    }
    Ok(out)
}

/// Write checked entries under `dest` (which must exist). No symlink is
/// followed: every folder is created here, and files are created new.
pub fn write_all(dest: &Path, entries: &[Entry]) -> io::Result<()> {
    for e in entries {
        let path: PathBuf = e.path.split('/').fold(dest.to_path_buf(), |p, c| p.join(c));
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        io::Write::write_all(&mut file, &e.bytes)?;
        file.sync_all()?;
    }
    Ok(())
}
