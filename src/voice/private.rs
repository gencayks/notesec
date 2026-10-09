//! Private scratch folders for recordings, whisper's output and program
//! logs. The system temp folder is shared: files there get the creating
//! program's umask (often 0644), so other users of this computer could
//! read a recording or a transcript. Each use gets a fresh folder with
//! mode 0700 (only us), removed with everything in it when dropped.
//! `mkdir` fails if the name exists (a symlink planted there included),
//! and the name is random, so nobody can prepare it for us.

use std::fs::{self, DirBuilder, Permissions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub struct PrivateDir(PathBuf);

impl PrivateDir {
    /// A new private folder under `$XDG_RUNTIME_DIR/notesec-voice` (per
    /// user, 0700, in memory) if that is usable, else under the system
    /// temp folder.
    pub fn new() -> io::Result<PrivateDir> {
        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .and_then(|runtime| runtime_base(&runtime).ok());
        PrivateDir::new_in(&base.unwrap_or_else(std::env::temp_dir))
    }

    /// A new folder `notesec-voice-<random>` in `base`, mode 0700.
    pub fn new_in(base: &Path) -> io::Result<PrivateDir> {
        let path = base.join(format!("notesec-voice-{}", uuid::Uuid::new_v4().simple()));
        DirBuilder::new().mode(0o700).create(&path)?;
        let dir = PrivateDir(path);
        // The umask can only take bits away; set it exactly anyway.
        fs::set_permissions(&dir.0, Permissions::from_mode(0o700))?;
        check_private(&dir.0, None)?;
        Ok(dir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for PrivateDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// `<runtime>/notesec-voice`, made 0700 if missing. An existing one must
/// be a real folder (not a symlink), owned by whoever owns the runtime
/// folder (us), and closed to others.
pub fn runtime_base(runtime: &Path) -> io::Result<PathBuf> {
    let owner = fs::metadata(runtime)?.uid();
    let base = runtime.join("notesec-voice");
    match DirBuilder::new().mode(0o700).create(&base) {
        Ok(()) => fs::set_permissions(&base, Permissions::from_mode(0o700))?,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    check_private(&base, Some(owner))?;
    Ok(base)
}

/// `path` is a folder (not a symlink), only its owner can enter it, and
/// that owner is `owner` (when given).
fn check_private(path: &Path, owner: Option<u32>) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    let bad = |why: &str| {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} {why}", path.display()),
        ))
    };
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return bad("is not a folder");
    }
    if owner.is_some_and(|o| o != meta.uid()) {
        return bad("belongs to another user");
    }
    if meta.mode() & 0o077 != 0 {
        return bad("is open to other users");
    }
    Ok(())
}
