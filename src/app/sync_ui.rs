//! Git sync in the app (roadmap v0.3.0 feature 6, docs/SYNC.md): a
//! sidebar row showing the status (clean / ahead / behind / conflicted),
//! a dialog with Sync now, the remote and interval settings and conflict
//! resolution, and an auto-sync loop. The git work itself is `crate::sync`
//! on the background executor; this module only moves results into view.
//! Dialog fields reuse the vault dialog's editor pattern (`active_editor`
//! in `app.rs`), and every button here already exists as a GPUI shape
//! elsewhere in the app.

use super::*;
use crate::sync::{self, Side, SyncStatus};

/// Which sync dialog field is being edited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::app) enum SyncField {
    Remote,
    Interval,
}

/// Sync UI state: the cached status, the dialog and its field, and the
/// background work (dropping a task cancels it).
#[derive(Default)]
pub(in crate::app) struct SyncUi {
    pub(in crate::app) status: Option<SyncStatus>,
    pub(in crate::app) status_error: Option<String>,
    pub(in crate::app) working: bool,
    pub(in crate::app) task: Option<Task<()>>,
    pub(in crate::app) dialog: bool,
    pub(in crate::app) field: Option<(SyncField, EditorState)>,
    pub(in crate::app) last_run: Option<std::time::SystemTime>,
    pub(in crate::app) auto_task: Option<Task<()>>,
}

impl NoteSec {
    /// "Sync now" (palette): run a sync; conflicts open the dialog.
    pub(in crate::app) fn on_sync_now(
        &mut self,
        _: &SyncNow,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.run_sync(cx);
    }

    /// The sidebar row opens the dialog (and refreshes the status).
    pub(in crate::app) fn open_sync_dialog(&mut self, cx: &mut Context<Self>) {
        self.stop_edit(cx);
        self.sync.dialog = true;
        self.sync.field = None;
        self.refresh_sync_status(cx);
        cx.notify();
    }

    pub(in crate::app) fn close_sync_dialog(&mut self, cx: &mut Context<Self>) {
        self.sync.dialog = false;
        self.sync.field = None;
        cx.notify();
    }

    /// Enter in the dialog: commit the field being edited.
    pub(in crate::app) fn sync_enter(&mut self, cx: &mut Context<Self>) -> bool {
        let Some((field, editor)) = self.sync.field.take() else {
            return false;
        };
        let text = editor.text.trim().to_string();
        match field {
            SyncField::Remote => {
                self.config.sync_remote = text;
                self.save_config();
                self.refresh_sync_status(cx);
            }
            SyncField::Interval => match text.parse::<u64>() {
                Ok(minutes) => {
                    self.config.sync_interval_minutes = minutes;
                    self.save_config();
                    cx.notify();
                }
                Err(_) => {
                    self.sync.field = Some((field, editor));
                    let text = format!("Not a number of minutes: {text:?}");
                    self.show_status(Status { text, error: true }, cx);
                }
            },
        }
        cx.notify();
        true
    }

    /// Esc in the dialog: cancel the field, else close the dialog.
    pub(in crate::app) fn sync_escape(&mut self, cx: &mut Context<Self>) -> bool {
        if self.sync.field.is_some() {
            self.sync.field = None;
            cx.notify();
            return true;
        }
        if self.sync.dialog {
            self.close_sync_dialog(cx);
            return true;
        }
        false
    }

    /// Refresh the cached status off the UI thread (fetches, so this can
    /// fail offline: the last status stays, with the reason shown).
    pub(in crate::app) fn refresh_sync_status(&mut self, cx: &mut Context<Self>) {
        if self.sync.working {
            return;
        }
        self.sync.working = true;
        cx.notify();
        let job = cx.background_spawn({
            let git = self.git.clone();
            let root = self.storage.root().to_path_buf();
            let remote = self.config.sync_remote.clone();
            async move { sync::status(&git, &root, &remote) }
        });
        self.sync.task = Some(cx.spawn(async move |this, cx| {
            let result = job.await;
            let _ = this.update(cx, |this, cx| {
                this.sync.working = false;
                this.sync.task = None;
                match result {
                    Ok(status) => {
                        this.sync.status = Some(status);
                        this.sync.status_error = None;
                    }
                    Err(err) => this.sync.status_error = Some(err.to_string()),
                }
                cx.notify();
            });
        }));
    }

    /// Recompute the status from local information only (no network):
    /// called after a backup commit, silently.
    pub(in crate::app) fn note_committed_for_sync(&mut self, cx: &mut Context<Self>) {
        if self.sync.working || self.config.sync_remote.trim().is_empty() {
            return;
        }
        self.sync.working = true;
        let job = cx.background_spawn({
            let git = self.git.clone();
            let root = self.storage.root().to_path_buf();
            let remote = self.config.sync_remote.clone();
            async move { sync::compare(&git, &root, &remote) }
        });
        self.sync.task = Some(cx.spawn(async move |this, cx| {
            let result = job.await;
            let _ = this.update(cx, |this, cx| {
                this.sync.working = false;
                this.sync.task = None;
                if let Ok(status) = result {
                    this.sync.status = Some(status);
                }
                cx.notify();
            });
        }));
    }

    /// Run a full sync off the UI thread. Conflicts open the dialog;
    /// anything else is a status message.
    pub(in crate::app) fn run_sync(&mut self, cx: &mut Context<Self>) {
        if self.sync.working {
            return;
        }
        self.sync.working = true;
        self.sync.last_run = Some(std::time::SystemTime::now());
        cx.notify();
        let job = cx.background_spawn({
            let git = self.git.clone();
            let root = self.storage.root().to_path_buf();
            let remote = self.config.sync_remote.clone();
            async move { sync::sync(&git, &root, &remote) }
        });
        self.sync.task = Some(cx.spawn(async move |this, cx| {
            let result = job.await;
            let _ = this.update(cx, |this, cx| {
                this.sync.working = false;
                this.sync.task = None;
                match result {
                    Ok(status) => {
                        let conflicted = matches!(status, SyncStatus::Conflicted(_));
                        this.sync.status = Some(status);
                        this.sync.status_error = None;
                        if conflicted {
                            this.sync.dialog = true;
                            this.sync.field = None;
                            let text = "Sync conflict: pick a version for each file".to_string();
                            this.show_status(Status { text, error: true }, cx);
                        } else {
                            this.show_status(
                                Status {
                                    text: "Synced".to_string(),
                                    error: false,
                                },
                                cx,
                            );
                        }
                    }
                    Err(err) => {
                        this.sync.status_error = Some(err.to_string());
                        let text = format!("Sync failed: {err}");
                        this.show_status(Status { text, error: true }, cx);
                    }
                }
                cx.notify();
            });
        }));
    }

    /// Keep one side of a conflicted file, then show where that leaves us.
    pub(in crate::app) fn resolve_sync_conflict(
        &mut self,
        file: String,
        side: Side,
        cx: &mut Context<Self>,
    ) {
        if self.sync.working {
            return;
        }
        self.sync.working = true;
        cx.notify();
        let job = cx.background_spawn({
            let git = self.git.clone();
            let root = self.storage.root().to_path_buf();
            async move { sync::resolve(&git, &root, &file, side) }
        });
        self.sync.task = Some(cx.spawn(async move |this, cx| {
            let result = job.await;
            let _ = this.update(cx, |this, cx| {
                this.sync.working = false;
                this.sync.task = None;
                match result {
                    Ok(status) => {
                        let done = !matches!(status, SyncStatus::Conflicted(_));
                        this.sync.status = Some(status);
                        this.sync.status_error = None;
                        if done {
                            this.show_status(
                                Status {
                                    text: "Conflicts resolved and synced".to_string(),
                                    error: false,
                                },
                                cx,
                            );
                        }
                    }
                    Err(err) => {
                        let text = format!("Sync failed: {err}");
                        this.show_status(Status { text, error: true }, cx);
                    }
                }
                cx.notify();
            });
        }));
    }

    /// Startup: one status refresh, plus the auto-sync loop (every
    /// minute it syncs when an interval is set, the remote is set, the
    /// interval has passed since the last run, and nothing else runs).
    pub(in crate::app) fn start_sync(&mut self, cx: &mut Context<Self>) {
        self.refresh_sync_status(cx);
        if self.sync.auto_task.is_some() {
            return;
        }
        self.sync.auto_task = Some(cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(60))
                .await;
            let run = this.update(cx, |this, _| {
                let minutes = this.config.sync_interval_minutes;
                let due = this.sync.last_run.is_none_or(|at| {
                    at.elapsed()
                        .map_or(true, |age| age.as_secs() >= minutes * 60)
                });
                minutes > 0
                    && !this.config.sync_remote.trim().is_empty()
                    && !this.sync.working
                    && due
            });
            let Ok(run) = run else {
                return;
            };
            if run {
                let _ = this.update(cx, |this, cx| this.run_sync(cx));
            }
        }));
    }

    /// The sidebar row: the cached status, or that sync isn't set up.
    /// Clicking opens the dialog.
    pub(in crate::app) fn render_sync_sidebar_item(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let (text, danger) = match &self.sync.status {
            None => (
                self.sync
                    .status_error
                    .as_deref()
                    .map_or("Sync: not set up".to_string(), |err| format!("Sync: {err}")),
                self.sync.status_error.is_some(),
            ),
            Some(SyncStatus::NoRemote) => ("Sync: not set up".to_string(), false),
            Some(SyncStatus::Clean) => ("Sync: clean".to_string(), false),
            Some(SyncStatus::Ahead(n)) => (format!("Sync: ahead {n}"), false),
            Some(SyncStatus::Behind(n)) => (format!("Sync: behind {n}"), false),
            Some(SyncStatus::Diverged { ahead, behind }) => {
                (format!("Sync: diverged {ahead}/{behind}"), false)
            }
            Some(SyncStatus::Conflicted(files)) => {
                (format!("Sync: conflict ({})", files.len()), true)
            }
        };
        div()
            .id("sidebar-sync")
            .debug_selector(|| "sidebar-sync".to_string())
            .px_3()
            .py_1()
            .rounded_md()
            .cursor_pointer()
            .text_color(if danger { theme.danger } else { theme.text })
            .when(self.sync.dialog, |d| d.bg(theme.selected_bg))
            .hover(|d| d.bg(theme.selected_bg))
            .on_click(cx.listener(|this, _e, _window, cx| this.open_sync_dialog(cx)))
            .child(text)
            .into_any_element()
    }

    /// The sync dialog: status, Sync now, the remote and interval fields,
    /// and conflict resolution. Backups of both sides of every conflict
    /// live under `.sync-conflicts/` (never committed).
    pub(in crate::app) fn render_sync_dialog(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.sync.dialog {
            return None;
        }
        let theme = self.theme;
        let working = self.sync.working;
        let status_line = match (&self.sync.status, &self.sync.status_error) {
            (Some(SyncStatus::NoRemote), _) => {
                "Set a remote below to sync this vault with another machine.".to_string()
            }
            (Some(SyncStatus::Clean), _) => "Everything is synced.".to_string(),
            (Some(SyncStatus::Ahead(n)), _) => {
                format!("{n} local commit{} the remote doesn't have.", plural(*n))
            }
            (Some(SyncStatus::Behind(n)), _) => {
                format!("{n} remote commit{} to pull.", plural(*n))
            }
            (Some(SyncStatus::Diverged { ahead, behind }), _) => {
                format!("Both sides moved ({ahead} ahead, {behind} behind): sync merges.")
            }
            (Some(SyncStatus::Conflicted(files)), _) => {
                format!(
                    "A merge stopped on {}: pick a version for each. Both sides are backed up under .sync-conflicts/.",
                    files.join(", ")
                )
            }
            (None, Some(err)) => format!("Couldn't check: {err}"),
            (None, None) => "Checking…".to_string(),
        };
        let remote_value = self.config.sync_remote.trim().to_string();
        let remote_set = !remote_value.is_empty();
        let remote_shown = if remote_set {
            remote_value.clone()
        } else {
            "Not set".to_string()
        };
        let interval = self.config.sync_interval_minutes;
        let interval_shown = if interval == 0 {
            "Off".to_string()
        } else {
            format!("Every {interval} min")
        };

        let field_row =
            |field: SyncField, id: &'static str, label: &str, shown: String, active: bool| {
                let editing = self.sync.field.as_ref().is_some_and(|(f, _)| *f == field);
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(div().text_color(theme.muted).child(label.to_string()))
                    .child(if editing {
                        div()
                            .debug_selector(move || format!("{id}-input"))
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .border_1()
                            .border_color(theme.accent)
                            .bg(theme.bg)
                            .child(BlockText { app: cx.entity() })
                            .into_any_element()
                    } else {
                        div()
                            .id(id)
                            .debug_selector(move || id.to_string())
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.bg)
                            .cursor_pointer()
                            .text_color(if active { theme.text } else { theme.muted })
                            .child(shown)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                let text = match field {
                                    SyncField::Remote => this.config.sync_remote.clone(),
                                    SyncField::Interval => {
                                        this.config.sync_interval_minutes.to_string()
                                    }
                                };
                                this.sync.field = Some((field, EditorState::new(&text)));
                                window.focus(&this.focus_handle, cx);
                                cx.notify();
                            }))
                            .into_any_element()
                    })
                    .into_any_element()
            };

        let conflicts: Vec<AnyElement> = match &self.sync.status {
            Some(SyncStatus::Conflicted(files)) => files
                .iter()
                .enumerate()
                .map(|(i, file)| {
                    let mine = file.clone();
                    let theirs = file.clone();
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .child(div().flex_1().min_w_0().child(file.clone()))
                        .child(
                            div()
                                .id(SharedString::from(format!("sync-ours-{i}")))
                                .debug_selector(move || format!("sync-ours-{i}"))
                                .px_2()
                                .rounded_md()
                                .border_1()
                                .border_color(theme.border)
                                .cursor_pointer()
                                .hover(|d| d.bg(theme.selected_bg))
                                .child("Keep mine")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.resolve_sync_conflict(mine.clone(), Side::Ours, cx);
                                })),
                        )
                        .child(
                            div()
                                .id(SharedString::from(format!("sync-theirs-{i}")))
                                .debug_selector(move || format!("sync-theirs-{i}"))
                                .px_2()
                                .rounded_md()
                                .border_1()
                                .border_color(theme.border)
                                .cursor_pointer()
                                .hover(|d| d.bg(theme.selected_bg))
                                .child("Keep theirs")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.resolve_sync_conflict(theirs.clone(), Side::Theirs, cx);
                                })),
                        )
                        .into_any_element()
                })
                .collect(),
            _ => Vec::new(),
        };

        div()
            .id("sync-backdrop")
            .debug_selector(|| "sync-backdrop".to_string())
            .absolute()
            .inset_0()
            .occlude()
            .bg(theme.scrim)
            .flex()
            .flex_col()
            .items_center()
            .pt(px(120.0))
            .on_click(cx.listener(|this, _e, _window, cx| this.close_sync_dialog(cx)))
            .absolute()
            .inset_0()
            .occlude()
            .bg(theme.scrim)
            .flex()
            .flex_col()
            .items_center()
            .pt(px(120.0))
            .child(
                div()
                    .id("sync-dialog")
                    .debug_selector(|| "sync-dialog".to_string())
                    .occlude()
                    .w(px(460.0))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_4()
                    .rounded_lg()
                    .bg(theme.sidebar_bg)
                    .border_1()
                    .border_color(theme.border)
                    .shadow_lg()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_between()
                            .child(div().font_weight(FontWeight::BOLD).child("Sync"))
                            .child(div().text_color(theme.muted).child("Esc to close")),
                    )
                    .child(div().text_color(theme.muted).child(status_line))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .child(
                                div()
                                    .id("sync-now")
                                    .debug_selector(|| "sync-now".to_string())
                                    .px_3()
                                    .py_1()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(theme.accent)
                                    .text_color(theme.accent)
                                    .cursor_pointer()
                                    .child(if working { "Syncing…" } else { "Sync now" })
                                    .on_click(cx.listener(|this, _, _, cx| this.run_sync(cx))),
                            )
                            .child(
                                div()
                                    .id("sync-refresh")
                                    .debug_selector(|| "sync-refresh".to_string())
                                    .px_3()
                                    .py_1()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(theme.border)
                                    .cursor_pointer()
                                    .child(if working { "Working…" } else { "Refresh" })
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.refresh_sync_status(cx);
                                    })),
                            ),
                    )
                    .child(field_row(
                        SyncField::Remote,
                        "sync-remote",
                        "Remote (SSH URL, https URL or local path)",
                        remote_shown,
                        remote_set,
                    ))
                    .child(field_row(
                        SyncField::Interval,
                        "sync-interval",
                        "Auto-sync (minutes, 0 = only on demand)",
                        interval_shown,
                        interval > 0,
                    ))
                    .children(conflicts)
                    .child(div().text_color(theme.muted).child(
                        "Pull, then push. Conflicts are never auto-resolved. See docs/SYNC.md.",
                    )),
            )
            .into_any_element()
            .into()
    }
}

/// "1 commit" / "2 commits".
fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backup::{git_available, isolated_git};
    use gpui::{Modifiers, TestAppContext, VisualTestContext};

    fn setup<'a>(
        cx: &'a mut TestAppContext,
        name: &str,
    ) -> (
        Entity<NoteSec>,
        &'a mut VisualTestContext,
        std::path::PathBuf,
    ) {
        let dir =
            std::env::temp_dir().join(format!("notesec-sync-ui-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let storage = Storage::open(dir.clone()).unwrap();
        std::fs::write(dir.join("pages/Home.md"), "- hello\n").unwrap();
        cx.update(bind_keys);
        let (view, cx) =
            cx.add_window_view(|window, cx| NoteSec::new(storage, Config::default(), window, cx));
        view.update(cx, |app, cx| {
            app.selected = app.find_page("Home").unwrap();
            app.tabs = Tabs::new(TabTarget::Page("Home".to_string()));
            cx.notify();
        });
        cx.run_until_parked();
        (view, cx, dir)
    }

    fn has(cx: &mut VisualTestContext, selector: &str) -> bool {
        let selector: &'static str = Box::leak(selector.to_string().into_boxed_str());
        cx.debug_bounds(selector).is_some()
    }

    fn click_on(cx: &mut VisualTestContext, selector: &str) {
        let selector: &'static str = Box::leak(selector.to_string().into_boxed_str());
        let bounds = cx.debug_bounds(selector).expect(selector);
        cx.simulate_click(bounds.center(), Modifiers::none());
    }

    fn bare(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "notesec-sync-ui-bare-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        isolated_git()
            .run(&dir, &[], &["init", "--quiet", "--bare"])
            .unwrap();
        dir
    }

    #[gpui::test]
    fn dialog_sets_the_remote_and_syncs_clean(cx: &mut TestAppContext) {
        if !git_available() {
            return;
        }
        let remote = bare("dialog");
        let (view, cx, dir) = setup(cx, "dialog");
        // No remote: the row says so, and the dialog explains itself.
        assert!(has(cx, "sidebar-sync"));
        click_on(cx, "sidebar-sync");
        assert!(has(cx, "sync-dialog") && has(cx, "sync-remote"));
        // Type the remote into its row and save with Enter.
        click_on(cx, "sync-remote");
        cx.simulate_input(&remote.to_string_lossy());
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        view.update(cx, |app, _| {
            assert_eq!(app.config.sync_remote, remote.to_string_lossy());
        });
        // Sync now pushes the page; the status comes back clean and the
        // dialog stays open on the result.
        click_on(cx, "sync-now");
        cx.run_until_parked();
        view.update(cx, |app, _| {
            assert_eq!(app.sync.status, Some(SyncStatus::Clean));
            assert!(app.sync.dialog);
        });
        // The remote really has the page.
        let shown = isolated_git()
            .run(&remote, &[], &["show", "HEAD:pages/Home.md"])
            .unwrap();
        assert_eq!(shown, "- hello\n");
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(remote);
    }

    #[gpui::test]
    fn a_conflict_surfaces_and_resolving_keeps_both_sides_safe(cx: &mut TestAppContext) {
        if !git_available() {
            return;
        }
        let remote = bare("fight");
        let (view, cx, dir) = setup(cx, "fight");
        let url = remote.to_string_lossy().into_owned();
        view.update(cx, |app, cx| {
            app.config.sync_remote = url.clone();
            app.save_config();
            cx.notify();
        });
        click_on(cx, "sidebar-sync");
        click_on(cx, "sync-now");
        cx.run_until_parked();

        // Another machine edits the same line and pushes (plain git).
        let other =
            std::env::temp_dir().join(format!("notesec-sync-ui-other-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&other);
        isolated_git()
            .run(
                &std::env::temp_dir(),
                &[],
                &["clone", "--quiet", &url, &other.to_string_lossy()],
            )
            .unwrap();
        std::fs::write(other.join("pages/Home.md"), "- theirs\n").unwrap();
        let git = isolated_git();
        crate::backup::prepare(&git, &other).unwrap();
        crate::backup::commit(&git, &other).unwrap();
        git.run(&other, &[], &["push", "--quiet", "origin", "HEAD"])
            .unwrap();

        // Refresh sees the remote move; our own edit then conflicts.
        click_on(cx, "sync-refresh");
        cx.run_until_parked();
        view.update(cx, |app, _| {
            assert!(matches!(app.sync.status, Some(SyncStatus::Behind(_))));
        });
        std::fs::write(dir.join("pages/Home.md"), "- mine\n").unwrap();
        click_on(cx, "sync-now");
        cx.run_until_parked();
        view.update(cx, |app, _| {
            assert!(matches!(app.sync.status, Some(SyncStatus::Conflicted(_))));
            assert!(app.sync.dialog, "conflicts open the dialog");
        });
        assert!(has(cx, "sync-ours-0") && has(cx, "sync-theirs-0"));

        // Keep theirs: their line wins here and on the remote, and the
        // backup still holds both sides.
        click_on(cx, "sync-theirs-0");
        cx.run_until_parked();
        view.update(cx, |app, _| {
            assert_eq!(app.sync.status, Some(SyncStatus::Clean));
        });
        assert_eq!(
            std::fs::read_to_string(dir.join("pages/Home.md")).unwrap(),
            "- theirs\n"
        );
        let backups: Vec<_> = std::fs::read_dir(dir.join(".sync-conflicts"))
            .unwrap()
            .collect();
        assert_eq!(backups.len(), 1);
        for dir in [dir, other, remote] {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[gpui::test]
    fn interval_field_takes_minutes(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "interval");
        click_on(cx, "sidebar-sync");
        click_on(cx, "sync-interval");
        cx.simulate_input("30");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        view.update(cx, |app, _| {
            assert_eq!(app.config.sync_interval_minutes, 30);
        });
        // Nonsense keeps the field open with an error, not a panic.
        click_on(cx, "sync-interval");
        cx.simulate_input("hourly");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        view.update(cx, |app, _| {
            assert_eq!(app.config.sync_interval_minutes, 30);
            assert!(app.sync.field.is_some());
            assert!(app.status.as_ref().is_some_and(|s| s.error));
        });
        // Esc cancels the field, then closes the dialog.
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        view.update(cx, |app, _| {
            assert!(app.sync.field.is_none() && app.sync.dialog);
        });
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(!view.update(cx, |app, _| app.sync.dialog));
        let _ = std::fs::remove_dir_all(dir);
    }
}
