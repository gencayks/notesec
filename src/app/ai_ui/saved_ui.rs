//! Saved searches (decision 46): name a global or semantic search and keep
//! it in the sidebar as a smart folder. Unfolding one shows its results:
//! a global search re-runs on the current pages every frame (`search_text`
//! is cheap), so it is always live; a semantic search runs only when the
//! user unfolds it (first time this session) or presses ↻, with the
//! active AI mode only (decision 42), and Off / unreachable say so in
//! place. Stored in `state.toml` (`UiState::saved_searches`).

use super::*;
use crate::search::{search_text, snippet as text_snippet, Target};
use crate::state::{SavedSearch, SearchKind};
use semantic_ui::SemanticHit;
use std::collections::{HashMap, HashSet};

/// How many results an unfolded saved search lists.
const SAVED_RESULTS: usize = 20;
/// Longest snippet (in characters) a result shows.
const SAVED_SNIPPET: usize = 48;

/// The "Save search" name field.
pub(in crate::app) struct SavePrompt {
    pub(in crate::app) kind: SearchKind,
    pub(in crate::app) query: String,
    pub(in crate::app) input: EditorState,
    /// Renaming the saved search with this name (else saving a new one).
    pub(in crate::app) rename: Option<String>,
    pub(in crate::app) error: Option<String>,
}

/// One saved semantic search's last run (this session).
#[derive(Default)]
pub(in crate::app) struct SavedRun {
    pub(in crate::app) id: u64,
    pub(in crate::app) running: bool,
    pub(in crate::app) progress: Option<(usize, usize)>,
    pub(in crate::app) results: Vec<SemanticHit>,
    pub(in crate::app) error: Option<String>,
    pub(in crate::app) task: Option<Task<()>>,
}

#[derive(Default)]
pub(in crate::app) struct SavedUi {
    pub(in crate::app) prompt: Option<SavePrompt>,
    /// The sidebar section is folded.
    pub(in crate::app) collapsed: bool,
    /// Unfolded saved searches, by lowercase name.
    pub(in crate::app) expanded: HashSet<String>,
    /// Semantic runs, by lowercase name.
    pub(in crate::app) runs: HashMap<String, SavedRun>,
    /// The saved search whose × was pressed once (a second press deletes).
    pub(in crate::app) confirm_delete: Option<String>,
    /// The last global or semantic query seen, for the palette command.
    pub(in crate::app) last: Option<(SearchKind, String)>,
    seen_global: String,
    seen_semantic: String,
    next_run: u64,
}

impl NoteSec {
    /// Remember the latest global / semantic query (the palette's "Save
    /// search" saves it: opening the palette closes those overlays).
    fn track_last_search(&mut self) {
        if let Some(s) = self.search.as_ref().filter(|s| s.global) {
            let q = s.query.text.trim();
            if !q.is_empty() && q != self.saved.seen_global {
                self.saved.seen_global = q.to_string();
                self.saved.last = Some((SearchKind::Global, q.to_string()));
            }
        }
        let q = self.semantic.query.trim();
        if !q.is_empty() && q != self.saved.seen_semantic {
            self.saved.seen_semantic = q.to_string();
            self.saved.last = Some((SearchKind::Semantic, q.to_string()));
        }
    }

    /// The query an open overlay would save, if any.
    fn open_search_query(&self, kind: SearchKind) -> Option<String> {
        let q = match kind {
            SearchKind::Global => self
                .search
                .as_ref()
                .filter(|s| s.global)
                .map(|s| s.query.text.trim().to_string()),
            SearchKind::Semantic => self.semantic.open.then(|| {
                let typed = self.semantic.input.text.trim();
                if typed.is_empty() {
                    self.semantic.query.clone()
                } else {
                    typed.to_string()
                }
            }),
        };
        q.filter(|q| !q.is_empty())
    }

    /// "Save search" (palette): the global or semantic search that is
    /// open, else the last one run.
    pub(in crate::app) fn on_save_search(
        &mut self,
        _: &SaveSearch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.track_last_search();
        let found = [SearchKind::Global, SearchKind::Semantic]
            .into_iter()
            .find_map(|k| self.open_search_query(k).map(|q| (k, q)))
            .or_else(|| self.saved.last.clone());
        match found {
            Some((kind, query)) => self.open_save_prompt(kind, query, None, window, cx),
            None => {
                let status = Status {
                    text: "Run a global or semantic search first, then save it".to_string(),
                    error: true,
                };
                self.show_status(status, cx);
            }
        }
    }

    /// Ask for a name (default: the query, or the old name when renaming).
    pub(in crate::app) fn open_save_prompt(
        &mut self,
        kind: SearchKind,
        query: String,
        rename: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.stop_edit(cx);
        self.close_search(cx);
        self.semantic.open = false;
        self.ask.open = false;
        self.page_menu = None;
        let name = rename.clone().unwrap_or_else(|| query.clone());
        self.saved.prompt = Some(SavePrompt {
            kind,
            query,
            input: EditorState::new(&name),
            rename,
            error: None,
        });
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    /// Enter in the name field: save (or rename), refusing an empty or
    /// taken name.
    pub(in crate::app) fn confirm_save_prompt(&mut self, cx: &mut Context<Self>) {
        let Some(prompt) = &self.saved.prompt else {
            return;
        };
        let name = prompt.input.text.trim().to_string();
        let own = prompt
            .rename
            .as_deref()
            .and_then(|old| self.state.saved_search(old));
        let error = if name.is_empty() {
            Some("Give the search a name".to_string())
        } else if self
            .state
            .saved_search(&name)
            .is_some_and(|i| Some(i) != own)
        {
            Some(format!(
                "There is already a saved search called \u{201c}{name}\u{201d}"
            ))
        } else {
            None
        };
        if let Some(error) = error {
            if let Some(prompt) = &mut self.saved.prompt {
                prompt.error = Some(error);
            }
            cx.notify();
            return;
        }
        let Some(prompt) = self.saved.prompt.take() else {
            return;
        };
        let key = name.to_lowercase();
        match own {
            Some(i) => {
                let old = self.state.saved_searches[i].name.to_lowercase();
                self.state.saved_searches[i].name = name.clone();
                if self.saved.expanded.remove(&old) {
                    self.saved.expanded.insert(key.clone());
                }
                if let Some(run) = self.saved.runs.remove(&old) {
                    self.saved.runs.insert(key, run);
                }
            }
            None => {
                // A semantic search saved from its overlay keeps the
                // results it shows (no second request).
                if prompt.kind == SearchKind::Semantic
                    && self.semantic.query == prompt.query
                    && !self.semantic.results.is_empty()
                {
                    self.saved.next_run += 1;
                    let run = SavedRun {
                        id: self.saved.next_run,
                        results: self.semantic.results.clone(),
                        ..SavedRun::default()
                    };
                    self.saved.runs.insert(key.clone(), run);
                }
                self.saved.expanded.insert(key);
                self.state.saved_searches.push(SavedSearch {
                    name: name.clone(),
                    kind: prompt.kind,
                    query: prompt.query,
                });
            }
        }
        self.saved.collapsed = false;
        self.save_state();
        let status = Status {
            text: format!("Saved search \u{201c}{name}\u{201d}"),
            error: false,
        };
        self.show_status(status, cx);
        cx.notify();
    }

    /// × on a saved search: the first press asks, the second deletes.
    fn delete_saved_search(&mut self, name: &str, cx: &mut Context<Self>) {
        let key = name.to_lowercase();
        if self.saved.confirm_delete.as_deref() != Some(key.as_str()) {
            self.saved.confirm_delete = Some(key);
            cx.notify();
            return;
        }
        self.saved.confirm_delete = None;
        if let Some(i) = self.state.saved_search(name) {
            self.state.saved_searches.remove(i);
            self.save_state();
        }
        self.saved.expanded.remove(&key);
        self.saved.runs.remove(&key);
        cx.notify();
    }

    /// Click on a saved search: fold or unfold it. Unfolding a semantic
    /// search that hasn't run this session runs it (the user asked).
    fn toggle_saved_search(&mut self, name: &str, cx: &mut Context<Self>) {
        let key = name.to_lowercase();
        self.saved.confirm_delete = None;
        if !self.saved.expanded.remove(&key) {
            self.saved.expanded.insert(key.clone());
            if let Some(i) = self.state.saved_search(name) {
                let s = self.state.saved_searches[i].clone();
                if s.kind == SearchKind::Semantic && !self.saved.runs.contains_key(&key) {
                    self.run_saved_semantic(&s, cx);
                }
            }
        }
        cx.notify();
    }

    /// Re-run a saved semantic search with the active mode (never another;
    /// Off and connection errors are shown under it).
    fn run_saved_semantic(&mut self, search: &SavedSearch, cx: &mut Context<Self>) {
        let key = search.name.to_lowercase();
        self.saved.next_run += 1;
        let id = self.saved.next_run;
        let endpoint = self.ai_endpoint();
        let run = self.saved.runs.entry(key).or_default();
        *run = SavedRun {
            id,
            results: std::mem::take(&mut run.results),
            ..SavedRun::default()
        };
        let endpoint = match endpoint {
            Ok(endpoint) => endpoint,
            Err(err) => {
                run.error = Some(err.to_string());
                cx.notify();
                return;
            }
        };
        run.running = true;
        let job = self.semantic_job(endpoint, search.query.clone());
        let task = cx.spawn(async move |this, cx| {
            let result = semantic_ui::run_job(&this, cx, job, Progress::Saved(id)).await;
            let _ = this.update(cx, |this, cx| {
                if let Some(run) = this.saved.runs.values_mut().find(|r| r.id == id) {
                    run.running = false;
                    run.progress = None;
                    run.task = None;
                    match result {
                        Ok(hits) => {
                            run.results = hits;
                            run.error = None;
                        }
                        Err(err) => run.error = Some(err.to_string()),
                    }
                }
                cx.notify();
            });
        });
        if let Some(run) = self.saved.runs.values_mut().find(|r| r.id == id) {
            run.task = Some(task);
        }
        cx.notify();
    }

    /// Go to a semantic result's block (it may be gone since the run).
    fn open_saved_semantic_hit(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        if find_block(&self.pages, id).is_none() {
            let status = Status {
                text: "That block no longer exists; refresh the search".to_string(),
                error: true,
            };
            self.show_status(status, cx);
            return;
        }
        self.open_block_ref(id, window, cx);
    }

    /// "Save search" in the global search or semantic search overlay
    /// (None while there is nothing to save).
    pub(in crate::app) fn save_search_button(
        &self,
        kind: SearchKind,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let query = self.open_search_query(kind)?;
        let theme = self.theme;
        let selector = match kind {
            SearchKind::Global => "global-save-search",
            SearchKind::Semantic => "semantic-save-search",
        };
        Some(
            div()
                .id(selector)
                .debug_selector(move || selector.to_string())
                .px_3()
                .text_color(theme.accent)
                .cursor_pointer()
                .child("Save search")
                .on_click(cx.listener(move |this, _e, window, cx| {
                    this.open_save_prompt(kind, query.clone(), None, window, cx)
                }))
                .into_any_element(),
        )
    }

    /// The name dialog.
    pub(in crate::app) fn render_save_prompt(
        &self,
        prompt: &SavePrompt,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = self.theme;
        let active = self.ai_overlay_input() == Some(AiInput::SaveName);
        let what = match prompt.kind {
            SearchKind::Global => "Global search",
            SearchKind::Semantic => "Semantic search",
        };
        let title = if prompt.rename.is_some() {
            "Rename saved search"
        } else {
            "Save search"
        };
        div()
            .id("save-search-backdrop")
            .debug_selector(|| "save-search-backdrop".to_string())
            .absolute()
            .inset_0()
            .occlude()
            .bg(gpui::black().opacity(0.45))
            .flex()
            .flex_col()
            .items_center()
            .pt(px(120.0))
            .on_click(cx.listener(|this, _e, _w, cx| {
                this.saved.prompt = None;
                cx.notify();
            }))
            .child(
                div()
                    .id("save-search-panel")
                    .debug_selector(|| "save-search-panel".to_string())
                    .occlude()
                    .w(px(420.0))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_3()
                    .rounded_lg()
                    .bg(theme.sidebar_bg)
                    .border_1()
                    .border_color(theme.border)
                    .shadow_lg()
                    .child(div().font_weight(FontWeight::BOLD).child(title))
                    .child(
                        div()
                            .text_color(theme.muted)
                            .child(format!("{what}: \u{201c}{}\u{201d}", prompt.query)),
                    )
                    .child(
                        div()
                            .debug_selector(|| "save-search-input".to_string())
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .border_1()
                            .border_color(theme.accent)
                            .bg(theme.bg)
                            .map(|d| {
                                if active {
                                    d.child(BlockText { app: cx.entity() })
                                } else {
                                    d.child(prompt.input.text.clone())
                                }
                            }),
                    )
                    .when_some(prompt.error.clone(), |d, err| {
                        d.child(
                            div()
                                .debug_selector(|| "save-search-error".to_string())
                                .text_color(theme.danger)
                                .child(err),
                        )
                    })
                    .child(
                        div()
                            .text_color(theme.muted)
                            .child("Enter saves it in the sidebar, Esc cancels."),
                    ),
            )
            .into_any_element()
    }

    /// The sidebar's "SAVED SEARCHES" section (None without any). Called
    /// once per frame from `render` (so it also notes the last search).
    pub(in crate::app) fn render_saved_searches(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        self.track_last_search();
        if self.state.saved_searches.is_empty() {
            return None;
        }
        let theme = self.theme;
        let mut section = div().flex().flex_col().gap_1().child(
            div()
                .id("saved-header")
                .debug_selector(|| "saved-header".to_string())
                .mt_2()
                .px_3()
                .py_2()
                .cursor_pointer()
                .text_color(theme.muted)
                .child(if self.saved.collapsed {
                    "\u{25b8} SAVED SEARCHES"
                } else {
                    "\u{25be} SAVED SEARCHES"
                })
                .on_click(cx.listener(|this, _e, _w, cx| {
                    this.saved.collapsed = !this.saved.collapsed;
                    cx.notify();
                })),
        );
        if self.saved.collapsed {
            return Some(section.into_any_element());
        }
        for (i, search) in self.state.saved_searches.iter().enumerate() {
            let key = search.name.to_lowercase();
            let expanded = self.saved.expanded.contains(&key);
            let confirming = self.saved.confirm_delete.as_deref() == Some(key.as_str());
            let semantic = search.kind == SearchKind::Semantic;
            let (toggle, rename, delete, refresh) = (
                search.name.clone(),
                search.clone(),
                search.name.clone(),
                search.clone(),
            );
            let small = |id: (&'static str, usize), selector: String, label: &'static str| {
                div()
                    .id(id)
                    .debug_selector(move || selector)
                    .px_1()
                    .text_color(theme.muted)
                    .cursor_pointer()
                    .hover(|d| d.text_color(theme.accent))
                    .child(label)
            };
            section = section.child(
                div()
                    .id(("saved", i))
                    .debug_selector(move || format!("saved-{i}"))
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .cursor_pointer()
                    .text_color(theme.text)
                    .hover(|d| d.bg(theme.selected_bg))
                    .on_click(
                        cx.listener(move |this, _e, _w, cx| this.toggle_saved_search(&toggle, cx)),
                    )
                    .child(div().text_color(theme.muted).child(if semantic {
                        "\u{2248}"
                    } else {
                        "\u{2315}"
                    }))
                    .child(div().flex_1().overflow_hidden().child(search.name.clone()))
                    .when(semantic && expanded, |d| {
                        d.child(
                            small(
                                ("saved-refresh", i),
                                format!("saved-refresh-{i}"),
                                "\u{21bb}",
                            )
                            .on_click(cx.listener(
                                move |this, _e, _w, cx| {
                                    cx.stop_propagation();
                                    this.run_saved_semantic(&refresh, cx)
                                },
                            )),
                        )
                    })
                    .child(
                        small(("saved-rename", i), format!("saved-rename-{i}"), "\u{270e}")
                            .on_click(cx.listener(move |this, _e, window, cx| {
                                cx.stop_propagation();
                                this.open_save_prompt(
                                    rename.kind,
                                    rename.query.clone(),
                                    Some(rename.name.clone()),
                                    window,
                                    cx,
                                )
                            })),
                    )
                    .child(
                        small(
                            ("saved-delete", i),
                            format!("saved-delete-{i}"),
                            if confirming { "Delete?" } else { "\u{d7}" },
                        )
                        .when(confirming, |d| d.text_color(theme.danger))
                        .on_click(cx.listener(move |this, _e, _w, cx| {
                            cx.stop_propagation();
                            this.delete_saved_search(&delete, cx)
                        })),
                    ),
            );
            if expanded {
                section = section.children(self.saved_results(i, search, cx));
            }
        }
        Some(section.into_any_element())
    }

    /// An unfolded saved search's rows.
    fn saved_results(
        &self,
        i: usize,
        search: &SavedSearch,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let theme = self.theme;
        let note = |suffix: &'static str, text: String, error: bool| {
            div()
                .debug_selector(move || format!("saved-{i}-{suffix}"))
                .pl_6()
                .pr_2()
                .text_color(if error { theme.danger } else { theme.muted })
                .child(text)
                .into_any_element()
        };
        let row = |j: usize, title: String, text: Option<String>| {
            div()
                .id(ElementId::from((
                    SharedString::from(format!("saved-{i}-result")),
                    j,
                )))
                .debug_selector(move || format!("saved-{i}-result-{j}"))
                .pl_6()
                .pr_2()
                .py_px()
                .rounded_md()
                .flex()
                .flex_col()
                .cursor_pointer()
                .hover(|d| d.bg(theme.selected_bg))
                .child(div().text_color(theme.accent).truncate().child(title))
                .when_some(text, |d, text| {
                    d.child(div().text_color(theme.muted).truncate().child(text))
                })
        };
        let mut out = Vec::new();
        match search.kind {
            SearchKind::Global => {
                let hits = search_text(&self.pages, &search.query, SAVED_RESULTS);
                if hits.is_empty() {
                    out.push(note("empty", "No results".to_string(), false));
                }
                for (j, hit) in hits.into_iter().enumerate() {
                    let (title, text) = match hit.target {
                        Target::Page(p) => (self.pages[p].title.clone(), None),
                        Target::Match {
                            page,
                            block,
                            start,
                            end,
                        } => {
                            let content = &self.pages[page].blocks[block].content;
                            let (text, _) = text_snippet(content, start..end, SAVED_SNIPPET);
                            (self.pages[page].title.clone(), Some(text))
                        }
                        _ => continue,
                    };
                    out.push(
                        row(j, title, text)
                            .on_click(cx.listener(move |this, _e, window, cx| {
                                this.open_hit(&hit, window, cx)
                            }))
                            .into_any_element(),
                    );
                }
            }
            SearchKind::Semantic => {
                let run = self.saved.runs.get(&search.name.to_lowercase());
                if self.config.ai_provider == AiProvider::Off {
                    out.push(note("error", AiError::Off.to_string(), true));
                } else if let Some(err) = run.and_then(|r| r.error.clone()) {
                    out.push(note("error", err, true));
                }
                if let Some(run) = run.filter(|r| r.running) {
                    let text = match run.progress {
                        Some((done, total)) if total > done => {
                            format!("Indexing notes\u{2026} {done}/{total} blocks")
                        }
                        _ => "Searching\u{2026}".to_string(),
                    };
                    out.push(note("progress", text, false));
                }
                let results = run.map(|r| r.results.as_slice()).unwrap_or_default();
                if run.is_some_and(|r| !r.running && r.error.is_none()) && results.is_empty() {
                    out.push(note("empty", "No results".to_string(), false));
                }
                for (j, hit) in results.iter().take(SAVED_RESULTS).enumerate() {
                    let text: String = hit.content.chars().take(SAVED_SNIPPET).collect();
                    let id = hit.block;
                    out.push(
                        row(j, hit.title.clone(), Some(text))
                            .on_click(cx.listener(move |this, _e, window, cx| {
                                this.open_saved_semantic_hit(id, window, cx)
                            }))
                            .into_any_element(),
                    );
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{click_on, has, local, setup, PAGES};
    use super::*;
    use crate::ai::test_server::{dead_endpoint, embeddings_reply, serve_fn};
    use gpui::{TestAppContext, VisualTestContext};

    const VOCAB: &[&str] = &["weather", "rust", "book", "ownership", "hello"];

    fn saved(view: &Entity<NoteSec>, cx: &mut VisualTestContext) -> Vec<SavedSearch> {
        view.update(cx, |app, _| app.state.saved_searches.clone())
    }

    fn search(name: &str, kind: SearchKind, query: &str) -> SavedSearch {
        SavedSearch {
            name: name.into(),
            kind,
            query: query.into(),
        }
    }

    fn prompt_text(view: &Entity<NoteSec>, cx: &mut VisualTestContext) -> Option<String> {
        view.update(cx, |app, _| {
            app.saved.prompt.as_ref().map(|p| p.input.text.clone())
        })
    }

    fn clear_prompt(cx: &mut VisualTestContext, chars: usize) {
        for _ in 0..chars {
            cx.simulate_keystrokes("backspace");
        }
    }

    fn palette(cx: &mut VisualTestContext, command: &str) {
        cx.simulate_keystrokes("ctrl-k");
        cx.simulate_input(command);
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
    }

    #[gpui::test]
    fn a_global_search_saved_from_its_overlay_stays_live(cx: &mut TestAppContext) {
        let ep = dead_endpoint();
        let (view, cx, dir) = setup(cx, "saved-global", PAGES, local(ep.url()), "");
        assert!(
            !has(cx, "saved-header"),
            "no section without saved searches"
        );
        cx.simulate_keystrokes("ctrl-shift-f");
        assert!(!has(cx, "global-save-search"), "nothing to save yet");
        cx.simulate_input("rust");
        cx.run_until_parked();
        click_on(cx, "global-save-search");
        cx.run_until_parked();
        assert!(view.update(cx, |app, _| app.search.is_none()));
        assert_eq!(
            prompt_text(&view, cx).as_deref(),
            Some("rust"),
            "the query by default"
        );
        assert!(has(cx, "save-search-panel"));
        cx.simulate_input(" notes");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(
            saved(&view, cx),
            vec![search("rust notes", SearchKind::Global, "rust")]
        );
        let file = std::fs::read_to_string(crate::state::UiState::path(&dir)).unwrap();
        assert!(file.contains("[[saved_searches]]") && file.contains("name = \"rust notes\""));

        // Unfolded: the Rust page and Diary's line.
        assert!(has(cx, "saved-0-result-1") && !has(cx, "saved-0-result-2"));
        // Live: another page starts mentioning rust.
        view.update(cx, |app, cx| {
            let p = app.find_page("Test").unwrap();
            app.pages[p].blocks[0].content = "rust is fun".into();
            cx.notify();
        });
        cx.run_until_parked();
        assert!(has(cx, "saved-0-result-2"));
        // A line result opens its block.
        click_on(cx, "saved-0-result-1");
        cx.run_until_parked();
        view.update(cx, |app, _| {
            assert_eq!(app.pages[app.selected].title, "Diary");
            assert_eq!(app.editing, Some(0));
        });
        // Folding hides the rows.
        click_on(cx, "saved-0");
        assert!(!has(cx, "saved-0-result-0"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn a_saved_semantic_search_reruns_only_when_asked(cx: &mut TestAppContext) {
        let (ep, requests) = serve_fn(3, |_, body| embeddings_reply(body, VOCAB));
        let (view, cx, dir) = setup(cx, "saved-semantic", PAGES, local(ep.url()), "");
        palette(cx, "semantic search");
        cx.simulate_input("weather");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(requests.lock().unwrap().len(), 2, "index, then the query");
        click_on(cx, "semantic-save-search");
        cx.run_until_parked();
        assert!(view.update(cx, |app, _| !app.semantic.open));
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(
            saved(&view, cx),
            vec![search("weather", SearchKind::Semantic, "weather")]
        );
        // The overlay's results are shown as they are: no new request.
        assert!(has(cx, "saved-0-result-0"));
        assert_eq!(requests.lock().unwrap().len(), 2);
        // Folding and unfolding doesn't call the server either.
        click_on(cx, "saved-0");
        click_on(cx, "saved-0");
        cx.run_until_parked();
        assert_eq!(requests.lock().unwrap().len(), 2);
        // ↻ re-runs it: everything is indexed, so only the query.
        click_on(cx, "saved-refresh-0");
        cx.run_until_parked();
        assert_eq!(requests.lock().unwrap().len(), 3);
        let top = view.update(cx, |app, _| {
            app.saved.runs["weather"].results[0].title.clone()
        });
        assert_eq!(top, "Test");
        click_on(cx, "saved-0-result-0");
        cx.run_until_parked();
        view.update(cx, |app, _| {
            assert_eq!(app.pages[app.selected].title, "Test");
            assert_eq!(app.editing, Some(1));
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn the_palette_saves_the_last_search_and_rows_rename_and_delete(cx: &mut TestAppContext) {
        let ep = dead_endpoint();
        let (view, cx, dir) = setup(cx, "saved-manage", PAGES, local(ep.url()), "");
        palette(cx, "save search");
        assert!(prompt_text(&view, cx).is_none(), "nothing searched yet");
        cx.simulate_keystrokes("ctrl-shift-f");
        cx.simulate_input("weather");
        cx.run_until_parked();
        cx.simulate_keystrokes("escape");
        palette(cx, "save search");
        assert_eq!(prompt_text(&view, cx).as_deref(), Some("weather"));
        // Esc cancels; an empty name is refused.
        cx.simulate_keystrokes("escape");
        assert!(prompt_text(&view, cx).is_none() && saved(&view, cx).is_empty());
        palette(cx, "save search");
        clear_prompt(cx, 7);
        cx.simulate_keystrokes("enter");
        assert!(has(cx, "save-search-error") && saved(&view, cx).is_empty());
        cx.simulate_input("Forecast");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(
            saved(&view, cx),
            vec![search("Forecast", SearchKind::Global, "weather")]
        );

        // A second one, then renaming it onto the first's name is refused.
        cx.simulate_keystrokes("ctrl-shift-f");
        cx.simulate_input("hello");
        cx.run_until_parked();
        click_on(cx, "global-save-search");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        click_on(cx, "saved-rename-1");
        assert_eq!(prompt_text(&view, cx).as_deref(), Some("hello"));
        clear_prompt(cx, 5);
        cx.simulate_input("forecast");
        cx.simulate_keystrokes("enter");
        assert!(has(cx, "save-search-error"));
        clear_prompt(cx, 8);
        cx.simulate_input("Greetings");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(saved(&view, cx)[1].name, "Greetings");
        assert_eq!(saved(&view, cx)[1].query, "hello");

        // × asks first; the second press deletes (and saves state.toml).
        click_on(cx, "saved-delete-0");
        assert_eq!(saved(&view, cx).len(), 2);
        click_on(cx, "saved-delete-0");
        assert_eq!(
            saved(&view, cx),
            vec![search("Greetings", SearchKind::Global, "hello")]
        );
        let reloaded = crate::state::UiState::load(&dir);
        assert_eq!(reloaded.saved_searches, saved(&view, cx));
        // The section folds.
        click_on(cx, "saved-header");
        assert!(!has(cx, "saved-0"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn off_mode_explains_semantic_ones_and_global_ones_still_work(cx: &mut TestAppContext) {
        let off = Config {
            ai_provider: AiProvider::Off,
            ..Config::default()
        };
        let state =
            "[[saved_searches]]\nname = \"Ideas\"\nkind = \"semantic\"\nquery = \"ideas\"\n\n\
                     [[saved_searches]]\nname = \"Rusty\"\nquery = \"rust\"\n";
        let (view, cx, dir) = setup(cx, "saved-off", PAGES, off, state);
        assert_eq!(saved(&view, cx).len(), 2);
        click_on(cx, "saved-0");
        cx.run_until_parked();
        assert!(has(cx, "saved-0-error"));
        let run_error = view.update(cx, |app, _| app.saved.runs["ideas"].error.clone());
        assert_eq!(run_error, Some(AiError::Off.to_string()));
        click_on(cx, "saved-1");
        assert!(has(cx, "saved-1-result-0"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
