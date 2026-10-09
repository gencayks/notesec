//! Git-based sync across machines (roadmap v0.3.0 feature 6,
//! docs/SYNC.md): pull-then-push against a configured remote. Plain Rust,
//! no GPUI: everything runs the `git` program and blocks, so `app.rs`
//! calls it on GPUI's background executor.
//!
//! The vault folder must be its own git repository (`backup::prepare`
//! finds anything else, and sync refuses it rather than syncing a
//! stranger's files). Local changes are committed first (reusing
//! `backup::commit`), then the remote is fetched and merged, then pushed.
//! A merge conflict stops everything: both sides are backed up under
//! `.sync-conflicts/` and the conflict is reported, never auto-resolved.
//! Like backups, sync never touches `state.toml`, the trash, exports or
//! anything outside the graph folder.

use std::fs;
use std::path::{Path, PathBuf};

use crate::backup::{self, BackupError, Git};
use crate::storage::write_atomic;

/// The remote sync manages: added (or pointed at the configured URL) when
/// sync runs, so the user's own remotes are never renamed or moved. (If a
/// remote of this name already points elsewhere, sync refuses rather than
/// hijacking it.)
pub const REMOTE_NAME: &str = "notesec";

/// Where conflict backups live: `.sync-conflicts/<timestamp>/<path>.ours`
/// and `.theirs` (mirroring the vault's folders). Ignored by git.
pub const CONFLICT_DIR: &str = ".sync-conflicts";

/// What the UI shows for sync (clean / ahead / behind / conflicted, plus
/// the states around them).
#[derive(Clone, Debug, PartialEq)]
pub enum SyncStatus {
    /// No remote configured yet.
    NoRemote,
    /// Same commits as the remote (and nothing unmerged).
    Clean,
    /// Local commits the remote doesn't have.
    Ahead(usize),
    /// Remote commits this machine doesn't have.
    Behind(usize),
    /// Both sides moved: sync merges on the next run.
    Diverged { ahead: usize, behind: usize },
    /// A merge stopped on these files: pick a side for each.
    Conflicted(Vec<String>),
}

/// Which side of a conflict to keep.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Ours,
    Theirs,
}

/// The graph folder's own repository, or why sync won't use it.
fn own_repo(git: &Git, graph: &Path) -> Result<(), BackupError> {
    let repo = backup::prepare(git, graph)?;
    if repo.root != graph && canonical(&repo.root) != canonical(graph) {
        return Err(BackupError::Git(format!(
            "sync needs the vault folder to be its own git repository (it is inside {})",
            repo.root.display()
        )));
    }
    Ok(())
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Point the managed remote at `url` (adding it first), unless it already
/// points at another URL, which sync won't touch.
pub fn ensure_remote(git: &Git, graph: &Path, url: &str) -> Result<(), BackupError> {
    let existing = git.output(graph, &[], &["remote", "get-url", REMOTE_NAME])?;
    if existing.status.success() {
        let current = String::from_utf8_lossy(&existing.stdout).trim().to_string();
        if current == url.trim() {
            return Ok(());
        }
        return Err(BackupError::Git(format!(
            "the git remote {REMOTE_NAME} already points at {current}; rename it or point sync at it"
        )));
    }
    git.run(graph, &[], &["remote", "add", REMOTE_NAME, url.trim()])?;
    Ok(())
}

/// The checked-out branch, or why there is none to sync.
fn current_branch(git: &Git, graph: &Path) -> Result<String, BackupError> {
    let branch = git
        .run(graph, &[], &["branch", "--show-current"])?
        .trim()
        .to_string();
    if branch.is_empty() {
        return Err(BackupError::Git(
            "no branch is checked out (empty repository?)".into(),
        ));
    }
    Ok(branch)
}

/// Whether `notesec/<branch>` (the last fetch) exists here.
fn has_remote_branch(git: &Git, graph: &Path, branch: &str) -> bool {
    git.output(
        graph,
        &[],
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/remotes/{REMOTE_NAME}/{branch}"),
        ],
    )
    .is_ok_and(|out| out.status.success())
}

/// Files with unresolved merge conflicts (empty: none).
fn unmerged(git: &Git, graph: &Path) -> Result<Vec<String>, BackupError> {
    let out = git.run(graph, &[], &["diff", "--name-only", "--diff-filter=U"])?;
    Ok(out
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

/// Whether a merge is in progress (conflicted or staged-but-uncommitted).
fn merge_head(graph: &Path) -> bool {
    git_dir(graph).is_some_and(|dir| dir.join("MERGE_HEAD").exists())
}

/// Whether HEAD and the remote branch share history.
fn related(git: &Git, graph: &Path, branch: &str) -> Result<bool, BackupError> {
    Ok(git
        .output(
            graph,
            &[],
            &["merge-base", "HEAD", &format!("{REMOTE_NAME}/{branch}")],
        )?
        .status
        .success())
}

/// A plain `.git` folder, or the target of a `gitdir:` pointer.
fn git_dir(graph: &Path) -> Option<PathBuf> {
    let dot = graph.join(".git");
    if dot.is_dir() {
        return Some(dot);
    }
    let text = fs::read_to_string(&dot).ok()?;
    text.strip_prefix("gitdir: ")
        .map(|target| graph.join(target.trim()))
        .filter(|dir| dir.is_dir())
}

/// Fetch the remote (updating `notesec/<branch>`), without merging.
/// Status checks and syncs both go through here.
pub fn fetch(git: &Git, graph: &Path, remote: &str) -> Result<(), BackupError> {
    let remote = remote.trim();
    if remote.is_empty() {
        return Ok(());
    }
    own_repo(git, graph)?;
    ensure_remote(git, graph, remote)?;
    git.run(graph, &[], &["fetch", "--quiet", "--prune", REMOTE_NAME])?;
    Ok(())
}

/// The sync status from local information only (no network): conflicts,
/// then how HEAD compares to the last fetch. No remote configured reads
/// as NoRemote.
pub fn compare(git: &Git, graph: &Path, remote: &str) -> Result<SyncStatus, BackupError> {
    if remote.trim().is_empty() {
        return Ok(SyncStatus::NoRemote);
    }
    own_repo(git, graph)?;
    let files = unmerged(git, graph)?;
    if !files.is_empty() || merge_head(graph) {
        return Ok(SyncStatus::Conflicted(files));
    }
    let head = git.output(graph, &[], &["rev-parse", "--verify", "--quiet", "HEAD"])?;
    let branch = current_branch(git, graph).ok();
    match (head.status.success(), branch) {
        (false, _) => Ok(SyncStatus::Clean), // nothing committed yet
        (true, None) => Err(BackupError::Git(
            "no branch is checked out (empty repository?)".into(),
        )),
        (true, Some(branch)) => {
            if !has_remote_branch(git, graph, &branch) {
                let count: usize = git
                    .run(graph, &[], &["rev-list", "--count", "HEAD"])?
                    .trim()
                    .parse()
                    .unwrap_or(0);
                return Ok(SyncStatus::Ahead(count));
            }
            let counts = git.run(
                graph,
                &[],
                &[
                    "rev-list",
                    "--left-right",
                    "--count",
                    &format!("HEAD...{REMOTE_NAME}/{branch}"),
                ],
            )?;
            let (ahead, behind) = parse_counts(&counts)?;
            Ok(match (ahead, behind) {
                (0, 0) => SyncStatus::Clean,
                (a, 0) => SyncStatus::Ahead(a),
                (0, b) => SyncStatus::Behind(b),
                (a, b) => SyncStatus::Diverged {
                    ahead: a,
                    behind: b,
                },
            })
        }
    }
}

/// The sync status, freshly fetched first so Behind is truthful. Fails
/// offline; the UI then keeps its last status and says why.
pub fn status(git: &Git, graph: &Path, remote: &str) -> Result<SyncStatus, BackupError> {
    fetch(git, graph, remote)?;
    compare(git, graph, remote)
}

fn parse_counts(out: &str) -> Result<(usize, usize), BackupError> {
    let mut parts = out.split_whitespace();
    match (parts.next(), parts.next()) {
        (Some(a), Some(b)) => match (a.parse(), b.parse()) {
            (Ok(a), Ok(b)) => Ok((a, b)),
            _ => Err(BackupError::Git(format!(
                "unexpected rev-list output: {out}"
            ))),
        },
        _ => Err(BackupError::Git(format!(
            "unexpected rev-list output: {out}"
        ))),
    }
}

/// Keep `.sync-conflicts/` out of git (appended once to the graph's
/// `.gitignore`, leaving the user's own lines alone).
fn ensure_conflict_ignore(graph: &Path) -> std::io::Result<()> {
    let path = graph.join(".gitignore");
    let text = fs::read_to_string(&path).unwrap_or_default();
    if text.lines().any(|line| {
        let line = line.trim().trim_start_matches('/').trim_end_matches('/');
        line == CONFLICT_DIR
    }) {
        return Ok(());
    }
    let mut out = text;
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&format!("{CONFLICT_DIR}/\n"));
    write_atomic(&path, &out)
}

/// Back up both sides of every file in `files` under
/// `.sync-conflicts/<timestamp>/` (`<path>.ours` from stage 2,
/// `<path>.theirs` from stage 3). Returns the folder.
fn back_up_conflicts(git: &Git, graph: &Path, files: &[String]) -> Result<PathBuf, BackupError> {
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let dir = graph.join(CONFLICT_DIR).join(&stamp);
    for file in files {
        for (stage, side) in [(2, "ours"), (3, "theirs")] {
            let bytes = git
                .output(graph, &[], &["show", &format!(":{stage}:{file}")])
                .map_err(|_| BackupError::Git(format!("can't read the {side} side of {file}")))?;
            if !bytes.status.success() {
                return Err(BackupError::Git(format!(
                    "can't read the {side} side of {file}"
                )));
            }
            let path = dir.join(format!("{file}.{side}"));
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(BackupError::Io)?;
            }
            fs::write(&path, &bytes.stdout).map_err(BackupError::Io)?;
        }
    }
    Ok(dir)
}

/// Sync now: commit local changes, fetch, merge, push. A conflict backs
/// up both sides and stops (reported, never resolved); anything else that
/// fails is an error. Returns the status afterwards.
pub fn sync(git: &Git, graph: &Path, remote: &str) -> Result<SyncStatus, BackupError> {
    let remote = remote.trim();
    if remote.is_empty() {
        return Ok(SyncStatus::NoRemote);
    }
    own_repo(git, graph)?;
    ensure_conflict_ignore(graph).map_err(BackupError::Io)?;
    // A merge left unfinished (e.g. resolved outside the app): finish it.
    if merge_head(graph) {
        let files = unmerged(git, graph)?;
        if !files.is_empty() {
            return Ok(SyncStatus::Conflicted(files));
        }
        return finalize_merge(git, graph);
    }
    // Local changes become a commit first, so the merge only ever sees
    // committed work on this side.
    backup::commit(git, graph)?;
    fetch(git, graph, remote)?;
    let branch = match current_branch(git, graph) {
        Ok(branch) => branch,
        // Nothing committed here yet: adopt the remote's default branch,
        // if it has one (a fresh vault joining an existing sync).
        Err(_) => return adopt_remote(git, graph, remote),
    };
    if !has_remote_branch(git, graph, &branch) {
        git.run(graph, &[], &["push", "--quiet", REMOTE_NAME, &branch])?;
        return compare(git, graph, remote);
    }
    // Histories that never met (a fresh vault joining in) only merge when
    // this side is still trivial; established vaults meeting strangers
    // stop with an error instead.
    let mut extra = Vec::new();
    if !related(git, graph, &branch)? {
        let local: usize = git
            .run(graph, &[], &["rev-list", "--count", "HEAD"])?
            .trim()
            .parse()
            .unwrap_or(usize::MAX);
        if local > 1 {
            return Err(BackupError::Git(
                "refusing to merge unrelated histories; check the sync remote, or join from a fresh vault".into(),
            ));
        }
        extra.push("--allow-unrelated-histories");
    }
    let identity = git.identity(graph)?;
    let remote_branch = format!("{REMOTE_NAME}/{branch}");
    let mut merged = vec!["merge", "--quiet", "--no-edit"];
    merged.extend(extra);
    merged.push(&remote_branch);
    let merged = git.output(graph, &identity, &merged)?;
    if !merged.status.success() {
        let files = unmerged(git, graph)?;
        if files.is_empty() {
            let stderr = String::from_utf8_lossy(&merged.stderr);
            let line = stderr.lines().map(str::trim).find(|l| !l.is_empty());
            return Err(BackupError::Git(
                line.unwrap_or("the merge failed").to_string(),
            ));
        }
        back_up_conflicts(git, graph, &files)?;
        return Ok(SyncStatus::Conflicted(files));
    }
    git.run(graph, &[], &["push", "--quiet", REMOTE_NAME, &branch])
        .map_err(|err| match err {
            BackupError::Git(message) => BackupError::Git(format!(
                "the push was rejected ({message}); sync again to merge first"
            )),
            other => other,
        })?;
    compare(git, graph, remote)
}

/// No local history: check out the remote's default branch (tracking
/// it), or report clean when the remote is empty too.
fn adopt_remote(git: &Git, graph: &Path, remote: &str) -> Result<SyncStatus, BackupError> {
    let symref = git.run(graph, &[], &["ls-remote", "--symref", remote, "HEAD"])?;
    let branch = symref.lines().find_map(|line| {
        line.strip_prefix("ref: refs/heads/")
            .and_then(|rest| rest.split_whitespace().next())
            .map(str::to_string)
    });
    let Some(branch) = branch else {
        return Ok(SyncStatus::Clean); // nothing anywhere yet
    };
    git.run(graph, &[], &["checkout", "--quiet", &branch])?;
    compare(git, graph, remote)
}

/// Commit a finished merge (nothing unmerged) and push it.
fn finalize_merge(git: &Git, graph: &Path) -> Result<SyncStatus, BackupError> {
    let identity = git.identity(graph)?;
    git.run(
        graph,
        &identity,
        &[
            "commit",
            "--quiet",
            "--message",
            "notesec sync: resolved conflicts",
        ],
    )?;
    let branch = current_branch(git, graph)?;
    git.run(graph, &[], &["push", "--quiet", REMOTE_NAME, &branch])?;
    let remote = git.run(graph, &[], &["remote", "get-url", REMOTE_NAME])?;
    status(git, graph, &remote)
}

/// Keep one side of `file` (which must still be conflicted), stage it,
/// and finish the merge if that was the last one.
pub fn resolve(git: &Git, graph: &Path, file: &str, side: Side) -> Result<SyncStatus, BackupError> {
    if !unmerged(git, graph)?.iter().any(|f| f == file) {
        return Err(BackupError::Git(format!("{file} is not conflicted")));
    }
    let flag = match side {
        Side::Ours => "--ours",
        Side::Theirs => "--theirs",
    };
    git.run(graph, &[], &["checkout", flag, "--", file])?;
    git.run(graph, &[], &["add", "--", file])?;
    if unmerged(git, graph)?.is_empty() {
        return finalize_merge(git, graph);
    }
    Ok(SyncStatus::Conflicted(unmerged(git, graph)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backup::{git_available, isolated_git};

    fn temp_graph(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("notesec-sync-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("pages")).unwrap();
        fs::create_dir_all(dir.join("journals")).unwrap();
        dir
    }

    /// A graph with one committed page, ready to sync.
    fn graph(name: &str, page: &str, text: &str) -> PathBuf {
        let dir = temp_graph(name);
        let git = isolated_git();
        backup::prepare(&git, &dir).unwrap();
        fs::write(dir.join(format!("pages/{page}.md")), text).unwrap();
        backup::commit(&git, &dir).unwrap();
        dir
    }

    fn bare(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("notesec-bare-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        isolated_git()
            .run(&dir, &[], &["init", "--quiet", "--bare"])
            .unwrap();
        dir
    }

    fn url(dir: &Path) -> String {
        dir.to_string_lossy().into_owned()
    }

    fn read(dir: &Path, page: &str) -> String {
        fs::read_to_string(dir.join(format!("pages/{page}.md"))).unwrap()
    }

    #[test]
    fn without_a_remote_there_is_nothing_to_do() {
        if !git_available() {
            return;
        }
        let dir = graph("noremote", "A", "- a\n");
        let git = isolated_git();
        assert_eq!(status(&git, &dir, "").unwrap(), SyncStatus::NoRemote);
        assert_eq!(sync(&git, &dir, "").unwrap(), SyncStatus::NoRemote);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_push_then_a_pull_moves_pages_between_clones() {
        if !git_available() {
            return;
        }
        let remote = bare("flow");
        let git = isolated_git();
        let left = graph("flow-left", "A", "- a\n");
        assert_eq!(sync(&git, &left, &url(&remote)).unwrap(), SyncStatus::Clean);

        // A second machine joins and sees the page (uniting histories).
        let right = temp_graph("flow-right");
        backup::prepare(&git, &right).unwrap();
        assert_eq!(
            sync(&git, &right, &url(&remote)).unwrap(),
            SyncStatus::Clean
        );
        assert_eq!(read(&right, "A"), "- a\n");
        // The join merged on the right, so the left pulls it back.
        assert_eq!(sync(&git, &left, &url(&remote)).unwrap(), SyncStatus::Clean);

        // Edits on the left arrive on the right.
        fs::write(left.join("pages/A.md"), "- a2\n").unwrap();
        assert_eq!(
            status(&git, &left, &url(&remote)).unwrap(),
            SyncStatus::Clean
        );
        assert_eq!(sync(&git, &left, &url(&remote)).unwrap(), SyncStatus::Clean);
        assert_eq!(
            status(&git, &right, &url(&remote)).unwrap(),
            SyncStatus::Behind(1)
        );
        assert_eq!(
            sync(&git, &right, &url(&remote)).unwrap(),
            SyncStatus::Clean
        );
        assert_eq!(read(&right, "A"), "- a2\n");
        for dir in [&left, &right, &remote] {
            let _ = fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn divergent_edits_conflict_loudly_and_resolve_without_loss() {
        if !git_available() {
            return;
        }
        let remote = bare("conflict");
        let git = isolated_git();
        let left = graph("conflict-left", "A", "- base\n");
        sync(&git, &left, &url(&remote)).unwrap();
        let right = temp_graph("conflict-right");
        backup::prepare(&git, &right).unwrap();
        sync(&git, &right, &url(&remote)).unwrap();

        // Both sides edit the same line and commit (as backup would).
        fs::write(left.join("pages/A.md"), "- left\n").unwrap();
        backup::commit(&git, &left).unwrap();
        fs::write(right.join("pages/A.md"), "- right\n").unwrap();
        backup::commit(&git, &right).unwrap();
        assert_eq!(sync(&git, &left, &url(&remote)).unwrap(), SyncStatus::Clean);

        // The second sync stops on the conflict: markers in the file,
        // both sides backed up, nothing pushed.
        let status = sync(&git, &right, &url(&remote)).unwrap();
        eprintln!(
            "LOG-R:\n{}",
            git.run(&right, &[], &["log", "--oneline", "--all", "--graph"])
                .unwrap()
        );
        eprintln!(
            "PARENTS-R:\n{}",
            git.run(&right, &[], &["log", "--format=%h %p %s"]).unwrap()
        );
        let m2 = git
            .run(&right, &[], &["rev-parse", "notesec/master"])
            .unwrap();
        eprintln!(
            "MBASE: {}",
            git.run(&right, &[], &["merge-base", "HEAD", m2.trim()])
                .unwrap_or_default()
        );
        eprintln!(
            "M1TREE:\n{}",
            git.run(&right, &[], &["ls-tree", "-r", "--name-only", "HEAD~1"])
                .unwrap_or_default()
        );
        eprintln!(
            "STATUS-R:\n{}",
            git.run(&right, &[], &["status", "--short"])
                .unwrap_or_default()
        );
        eprintln!(
            "LOG-L:\n{}",
            git.run(&left, &[], &["log", "--oneline", "--all", "--graph"])
                .unwrap()
        );
        assert_eq!(status, SyncStatus::Conflicted(vec!["pages/A.md".into()]));
        let conflicted = read(&right, "A");
        assert!(conflicted.contains("<<<<<<<"), "{conflicted}");
        let backups: Vec<PathBuf> = fs::read_dir(right.join(CONFLICT_DIR))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(backups.len(), 1);
        let ours = fs::read_to_string(backups[0].join("pages/A.md.ours")).unwrap();
        let theirs = fs::read_to_string(backups[0].join("pages/A.md.theirs")).unwrap();
        assert_eq!((ours.as_str(), theirs.as_str()), ("- right\n", "- left\n"));
        assert!(
            right.join(".gitignore").exists()
                && fs::read_to_string(right.join(".gitignore"))
                    .unwrap()
                    .contains(".sync-conflicts/"),
            ".sync-conflicts stays out of git"
        );

        // Picking theirs takes the left side everywhere, with no loss.
        assert_eq!(
            resolve(&git, &right, "pages/A.md", Side::Theirs).unwrap(),
            SyncStatus::Clean
        );
        assert_eq!(read(&right, "A"), "- left\n");
        assert_eq!(sync(&git, &left, &url(&remote)).unwrap(), SyncStatus::Clean);
        assert_eq!(read(&left, "A"), "- left\n");
        for dir in [&left, &right, &remote] {
            let _ = fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn resolving_a_missing_conflict_is_an_error() {
        if !git_available() {
            return;
        }
        let dir = graph("resolve-clean", "A", "- a\n");
        let err = resolve(&isolated_git(), &dir, "pages/A.md", Side::Ours).unwrap_err();
        assert!(err.to_string().contains("not conflicted"), "{err}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_repo_above_the_graph_is_refused() {
        if !git_available() {
            return;
        }
        let outer = std::env::temp_dir().join(format!("notesec-sync-outer-{}", std::process::id()));
        let _ = fs::remove_dir_all(&outer);
        let graph = outer.join("notes");
        fs::create_dir_all(graph.join("pages")).unwrap();
        let git = isolated_git();
        git.run(&outer, &[], &["init", "--quiet"]).unwrap();
        let err = sync(&git, &graph, "/tmp/notesec-no-remote").unwrap_err();
        assert!(err.to_string().contains("its own git repository"), "{err}");
        let _ = fs::remove_dir_all(outer);
    }

    #[test]
    fn an_existing_remote_with_another_url_is_left_alone() {
        if !git_available() {
            return;
        }
        let dir = graph("hijack", "A", "- a\n");
        let git = isolated_git();
        git.run(&dir, &[], &["remote", "add", REMOTE_NAME, "other"])
            .unwrap();
        let err = sync(&git, &dir, "/tmp/notesec-no-remote").unwrap_err();
        assert!(err.to_string().contains("already points"), "{err}");
        let _ = fs::remove_dir_all(dir);
    }
}
