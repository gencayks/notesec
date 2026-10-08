//! Git auto-backup: the graph folder as a local git repository, committed a
//! few seconds after each change.
//!
//! Plain Rust, no GPUI: every function here runs the `git` program
//! (`std::process::Command`) and blocks, so `app.rs` calls them on GPUI's
//! background executor and owns the timing (the debounce).
//!
//! Local only. The only git commands used are `rev-parse`, `check-ignore`,
//! `init`, `config --get`, `status`, `add` and `commit`: nothing that talks
//! to a remote, rewrites history or forces anything.
//!
//! Scope: everything is run with `-C <graph>` and the pathspec `.`, so only
//! files inside the graph folder are staged and committed, even when the
//! graph lives inside a bigger repository (see decision 39).

use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use crate::storage::write_atomic;

/// How long after the last change the commit happens. Every change in
/// between restarts the wait.
pub const BACKUP_AFTER: Duration = Duration::from_secs(5);

/// The start of every commit message.
pub const MESSAGE: &str = "notesec autosave";

/// Lines `prepare` makes sure the graph's `.gitignore` has: the trash
/// (deleted pages), exports (re-creatable HTML) and the temp files of
/// atomic saves.
pub const IGNORED: [&str; 3] = [".trash/", "exports/", ".*.tmp"];

/// The identity used when git has none configured, so a commit never fails
/// for lack of `user.name` / `user.email`.
const FALLBACK_NAME: &str = "notesec";
const FALLBACK_EMAIL: &str = "notesec@localhost";

/// Environment variables that would point git at another repository than
/// the graph's (set e.g. when the app is started from a git hook).
const REPO_ENV: [&str; 7] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
];

/// One git command at a time: a commit started by the timer and one at
/// quit must not race for `.git/index.lock`.
static LOCK: Mutex<()> = Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    // A panic while holding the lock leaves nothing to clean up.
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// How to run git: the program and extra environment variables. The app
/// uses `Git::default()` (`git` from `PATH`, the user's own git config);
/// tests point it at a missing program or an isolated config.
#[derive(Clone, Debug)]
pub struct Git {
    program: OsString,
    envs: Vec<(OsString, OsString)>,
}

impl Default for Git {
    fn default() -> Self {
        Git {
            program: "git".into(),
            envs: Vec::new(),
        }
    }
}

/// Why a backup step failed.
#[derive(Debug)]
pub enum BackupError {
    /// The `git` program isn't installed (not on `PATH`).
    GitMissing,
    /// The graph folder is inside a repository that ignores it, so nothing
    /// would ever be committed.
    Ignored(PathBuf),
    /// A git command failed; its first line of error output.
    Git(String),
    Io(io::Error),
}

impl fmt::Display for BackupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BackupError::GitMissing => write!(f, "git is not installed"),
            BackupError::Ignored(root) => write!(
                f,
                "the graph folder is ignored by the git repository at {}",
                root.display()
            ),
            BackupError::Git(message) => write!(f, "{message}"),
            BackupError::Io(err) => write!(f, "{err}"),
        }
    }
}

impl From<io::Error> for BackupError {
    fn from(err: io::Error) -> Self {
        BackupError::Io(err)
    }
}

/// The repository the graph is backed up to.
#[derive(Clone, Debug, PartialEq)]
pub struct Repo {
    /// Its top folder: the graph folder, or a folder above it.
    pub root: PathBuf,
    /// `prepare` just ran `git init`.
    pub created: bool,
}

impl Git {
    /// Run a different program instead of `git` (tests: a missing one).
    #[cfg(test)]
    pub fn with_program(mut self, program: impl Into<OsString>) -> Self {
        self.program = program.into();
        self
    }

    /// Set an environment variable for every git command (tests).
    #[cfg(test)]
    pub fn with_env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.envs.push((key.into(), value.into()));
        self
    }

    /// Run `git -C <dir> <config...> <args...>` and wait for it.
    fn output(&self, dir: &Path, config: &[String], args: &[&str]) -> Result<Output, BackupError> {
        let mut command = Command::new(&self.program);
        command.arg("-C").arg(dir);
        for setting in config {
            command.arg("-c").arg(setting);
        }
        command
            .args(args)
            .stdin(Stdio::null())
            // Never wait for a password or an editor.
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_EDITOR", "true");
        for var in REPO_ENV {
            command.env_remove(var);
        }
        for (key, value) in &self.envs {
            command.env(key, value);
        }
        command.output().map_err(|err| match err.kind() {
            io::ErrorKind::NotFound => BackupError::GitMissing,
            _ => BackupError::Io(err),
        })
    }

    /// Like `output`, but a failure is an error and stdout is returned.
    fn run(&self, dir: &Path, config: &[String], args: &[&str]) -> Result<String, BackupError> {
        let output = self.output(dir, config, args)?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            Err(failure(&output, args))
        }
    }

    /// `-c user.name=... -c user.email=...` for whichever of the two git
    /// has no value for (none when the user configured both).
    fn identity(&self, dir: &Path) -> Result<Vec<String>, BackupError> {
        let mut config = Vec::new();
        for (key, fallback) in [("user.name", FALLBACK_NAME), ("user.email", FALLBACK_EMAIL)] {
            let output = self.output(dir, &[], &["config", "--get", key])?;
            let value = String::from_utf8_lossy(&output.stdout);
            if !output.status.success() || value.trim().is_empty() {
                config.push(format!("{key}={fallback}"));
            }
        }
        Ok(config)
    }
}

/// The error for a git command that failed: its first line of stderr.
fn failure(output: &Output, args: &[&str]) -> BackupError {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let line = stderr
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string);
    let command = args.first().copied().unwrap_or("");
    BackupError::Git(line.unwrap_or_else(|| format!("git {command} failed")))
}

/// Get the graph folder ready for backups: find the repository it is in,
/// or `git init` one there; check that repository doesn't ignore the graph;
/// make sure the graph's `.gitignore` lists `IGNORED` (`merge_gitignore`).
pub fn prepare(git: &Git, graph: &Path) -> Result<Repo, BackupError> {
    let _lock = lock();
    let found = git.output(graph, &[], &["rev-parse", "--show-toplevel"])?;
    let repo = if found.status.success() {
        let root = String::from_utf8_lossy(&found.stdout).trim().to_string();
        Repo {
            root: PathBuf::from(root),
            created: false,
        }
    } else {
        git.run(graph, &[], &["init", "--quiet"])?;
        Repo {
            root: graph.to_path_buf(),
            created: true,
        }
    };
    // `pages/` always exists (`Storage::open`). Exit code 0 means ignored.
    let ignored = git.output(graph, &[], &["check-ignore", "--quiet", "--", "pages"])?;
    if ignored.status.success() {
        return Err(BackupError::Ignored(repo.root));
    }
    merge_gitignore(graph)?;
    Ok(repo)
}

/// Add the `IGNORED` lines missing from `<graph>/.gitignore` at its end
/// (creating it if needed). The user's own lines are kept as they are;
/// `.trash`, `/.trash` and `/.trash/` count as `.trash/`. Returns whether
/// the file changed.
pub fn merge_gitignore(graph: &Path) -> io::Result<bool> {
    let path = graph.join(".gitignore");
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err),
    };
    let normalize = |line: &str| {
        line.trim()
            .trim_start_matches('/')
            .trim_end_matches('/')
            .to_string()
    };
    let present: Vec<String> = text.lines().map(normalize).collect();
    let missing: Vec<&str> = IGNORED
        .iter()
        .copied()
        .filter(|entry| !present.contains(&normalize(entry)))
        .collect();
    if missing.is_empty() {
        return Ok(false);
    }
    let mut out = text;
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("# notesec: not backed up\n");
    for entry in missing {
        out.push_str(entry);
        out.push('\n');
    }
    write_atomic(&path, &out)?;
    Ok(true)
}

/// Commit everything changed inside the graph folder, if anything did.
/// Returns the number of changed files committed, or `None` when there was
/// nothing to commit (no empty commits). The message is
/// `notesec autosave: N file(s) changed`.
pub fn commit(git: &Git, graph: &Path) -> Result<Option<usize>, BackupError> {
    let _lock = lock();
    let status = git.run(
        graph,
        &[],
        &["status", "--porcelain", "--untracked-files=all", "--", "."],
    )?;
    let changed = status
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();
    if changed == 0 {
        return Ok(None);
    }
    git.run(graph, &[], &["add", "--all", "--", "."])?;
    let identity = git.identity(graph)?;
    let plural = if changed == 1 { "" } else { "s" };
    let message = format!("{MESSAGE}: {changed} file{plural} changed");
    // With a pathspec, `commit` takes only those paths: anything the user
    // staged outside the graph folder stays staged and out of this commit.
    git.run(
        graph,
        &identity,
        &["commit", "--quiet", "--message", &message, "--", "."],
    )?;
    Ok(Some(changed))
}

/// Wait until no backup git command is running (e.g. one started by the
/// timer just before quitting).
pub fn wait_idle() {
    drop(lock());
}

/// A `Git` with no system or global config, for tests: the same result on
/// any machine, and no hooks or identity of the person running them.
#[cfg(test)]
pub fn isolated_git() -> Git {
    Git::default()
        .with_env("GIT_CONFIG_NOSYSTEM", "1")
        .with_env("GIT_CONFIG_GLOBAL", "/dev/null")
        // Never find a repository above the temp folder tests work in.
        .with_env("GIT_CEILING_DIRECTORIES", std::env::temp_dir())
}

/// Whether `git` can be run here (tests skip, not fail, without it).
#[cfg(test)]
pub fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_graph(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("notesec-backup-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("pages")).unwrap();
        fs::create_dir_all(dir.join("journals")).unwrap();
        dir
    }

    /// `git <args>` in `dir`, isolated like the code under test.
    fn run_git(dir: &Path, args: &[&str]) -> String {
        isolated_git().run(dir, &[], args).unwrap()
    }

    /// Commit subjects, newest first.
    fn log(dir: &Path) -> Vec<String> {
        match isolated_git().run(dir, &[], &["log", "--format=%s"]) {
            Ok(out) => out.lines().map(str::to_string).collect(),
            Err(_) => Vec::new(), // no commits yet
        }
    }

    /// Files in the last commit.
    fn committed(dir: &Path) -> Vec<String> {
        run_git(dir, &["show", "--name-only", "--format=", "HEAD"])
            .lines()
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn prepare_inits_a_repo_and_writes_the_gitignore() {
        if !git_available() {
            return;
        }
        let dir = temp_graph("init");
        let repo = prepare(&isolated_git(), &dir).unwrap();
        assert!(repo.created);
        assert!(dir.join(".git").is_dir());
        assert_eq!(
            fs::read_to_string(dir.join(".gitignore")).unwrap(),
            "# notesec: not backed up\n.trash/\nexports/\n.*.tmp\n"
        );
        // Again: the same repository, nothing added twice.
        let again = prepare(&isolated_git(), &dir).unwrap();
        assert!(!again.created);
        assert_eq!(again.root, repo.root);
        assert_eq!(
            fs::read_to_string(dir.join(".gitignore"))
                .unwrap()
                .matches(".trash/")
                .count(),
            1
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn an_existing_gitignore_is_kept_and_completed() {
        let dir = temp_graph("gitignore");
        fs::write(dir.join(".gitignore"), "*.bak\n/.trash\n# mine").unwrap();
        assert!(merge_gitignore(&dir).unwrap());
        assert_eq!(
            fs::read_to_string(dir.join(".gitignore")).unwrap(),
            "*.bak\n/.trash\n# mine\n# notesec: not backed up\nexports/\n.*.tmp\n"
        );
        assert!(!merge_gitignore(&dir).unwrap(), "complete: left alone");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn commits_say_notesec_autosave_and_are_never_empty() {
        if !git_available() {
            return;
        }
        let dir = temp_graph("commit");
        let git = isolated_git();
        prepare(&git, &dir).unwrap();
        fs::write(dir.join("pages/A.md"), "- a\n").unwrap();
        fs::write(dir.join("pages/B.md"), "- b\n").unwrap();
        // .gitignore + A + B.
        assert_eq!(commit(&git, &dir).unwrap(), Some(3));
        assert_eq!(log(&dir), ["notesec autosave: 3 files changed"]);
        // Nothing changed: no commit.
        assert_eq!(commit(&git, &dir).unwrap(), None);
        assert_eq!(log(&dir).len(), 1);

        // Edits and deletions are committed too.
        fs::write(dir.join("pages/A.md"), "- a2\n").unwrap();
        fs::remove_file(dir.join("pages/B.md")).unwrap();
        assert_eq!(commit(&git, &dir).unwrap(), Some(2));
        assert_eq!(log(&dir)[0], "notesec autosave: 2 files changed");
        assert_eq!(run_git(&dir, &["show", "HEAD:pages/A.md"]), "- a2\n");
        assert!(isolated_git()
            .run(&dir, &[], &["cat-file", "-e", "HEAD:pages/B.md"])
            .is_err());
        // Without a configured identity, the fallback one is used.
        assert_eq!(
            run_git(&dir, &["log", "-1", "--format=%an <%ae>|%cn <%ce>"]).trim(),
            "notesec <notesec@localhost>|notesec <notesec@localhost>"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_configured_identity_is_used() {
        if !git_available() {
            return;
        }
        let dir = temp_graph("identity");
        let git = isolated_git();
        prepare(&git, &dir).unwrap();
        run_git(&dir, &["config", "user.name", "Kazo"]);
        run_git(&dir, &["config", "user.email", "kazo@example.org"]);
        fs::write(dir.join("pages/A.md"), "- a\n").unwrap();
        commit(&git, &dir).unwrap();
        assert_eq!(
            run_git(&dir, &["log", "-1", "--format=%an <%ae>"]).trim(),
            "Kazo <kazo@example.org>"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn trash_exports_and_temp_files_are_not_committed() {
        if !git_available() {
            return;
        }
        let dir = temp_graph("ignored");
        let git = isolated_git();
        prepare(&git, &dir).unwrap();
        commit(&git, &dir).unwrap(); // the .gitignore
        fs::create_dir_all(dir.join(".trash/123/pages")).unwrap();
        fs::write(dir.join(".trash/123/pages/Old.md"), "- old\n").unwrap();
        fs::create_dir_all(dir.join("exports")).unwrap();
        fs::write(dir.join("exports/A.html"), "<p>").unwrap();
        fs::write(dir.join("pages/.A.md.tmp"), "- half").unwrap();
        assert_eq!(commit(&git, &dir).unwrap(), None);
        fs::write(dir.join("pages/A.md"), "- a\n").unwrap();
        assert_eq!(commit(&git, &dir).unwrap(), Some(1));
        assert_eq!(committed(&dir), ["pages/A.md"]);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_repo_above_the_graph_gets_only_the_graph_committed() {
        if !git_available() {
            return;
        }
        let outer = temp_graph("outer");
        let graph = outer.join("notes");
        fs::create_dir_all(graph.join("pages")).unwrap();
        run_git(&outer, &["init", "--quiet"]);
        // The user's own work: one file staged, one not.
        fs::write(outer.join("staged.txt"), "mine").unwrap();
        fs::write(outer.join("loose.txt"), "mine too").unwrap();
        run_git(&outer, &["add", "staged.txt"]);

        let repo = prepare(&isolated_git(), &graph).unwrap();
        assert!(!repo.created);
        assert_eq!(
            repo.root.canonicalize().unwrap(),
            outer.canonicalize().unwrap()
        );
        assert!(!graph.join(".git").exists(), "no nested repository");
        fs::write(graph.join("pages/A.md"), "- a\n").unwrap();
        assert_eq!(commit(&isolated_git(), &graph).unwrap(), Some(2));
        assert_eq!(committed(&outer), ["notes/.gitignore", "notes/pages/A.md"]);
        // The user's staged file is still staged, the other untouched.
        let status = run_git(&outer, &["status", "--porcelain"]);
        assert!(status.contains("A  staged.txt"), "{status}");
        assert!(status.contains("?? loose.txt"), "{status}");
        let _ = fs::remove_dir_all(outer);
    }

    #[test]
    fn a_repo_that_ignores_the_graph_is_refused() {
        if !git_available() {
            return;
        }
        let outer = temp_graph("ignoring");
        let graph = outer.join("notes");
        fs::create_dir_all(graph.join("pages")).unwrap();
        run_git(&outer, &["init", "--quiet"]);
        fs::write(outer.join(".gitignore"), "*\n").unwrap();
        let err = prepare(&isolated_git(), &graph).unwrap_err();
        assert!(matches!(err, BackupError::Ignored(_)), "{err}");
        assert!(!graph.join(".gitignore").exists());
        let _ = fs::remove_dir_all(outer);
    }

    #[test]
    fn missing_git_is_an_error_not_a_panic() {
        let dir = temp_graph("missing");
        let git = Git::default().with_program("notesec-no-such-git");
        assert!(matches!(prepare(&git, &dir), Err(BackupError::GitMissing)));
        assert!(matches!(commit(&git, &dir), Err(BackupError::GitMissing)));
        assert_eq!(BackupError::GitMissing.to_string(), "git is not installed");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn nothing_ever_adds_a_remote() {
        if !git_available() {
            return;
        }
        let dir = temp_graph("remote");
        let git = isolated_git();
        prepare(&git, &dir).unwrap();
        fs::write(dir.join("pages/A.md"), "- a\n").unwrap();
        commit(&git, &dir).unwrap();
        assert_eq!(run_git(&dir, &["remote"]), "");
        let config = fs::read_to_string(dir.join(".git/config")).unwrap();
        assert!(!config.contains("[remote"), "{config}");
        let _ = fs::remove_dir_all(dir);
    }
}
