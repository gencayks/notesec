//! "Semantic search" (decision 43): an overlay like the palette that ranks
//! pages by meaning, and the indexing pipeline it shares with Ask my
//! notes. Embedding happens off the UI thread, one batch per background
//! step (so progress shows between batches); the cache stays in memory
//! between runs and is saved in the graph after each run.

use super::*;
use crate::semantic::{self, Cache, CacheKey, Item};
use crate::state::SearchKind;
use gpui::{AsyncApp, WeakEntity};
use std::path::PathBuf;
use std::sync::Arc;

/// How many pages a semantic search lists.
pub(in crate::app) const SEMANTIC_RESULTS: usize = 30;

/// One result: a page, by its best-matching block.
#[derive(Clone, Debug, PartialEq)]
pub(in crate::app) struct SemanticHit {
    pub(in crate::app) title: String,
    pub(in crate::app) block: Uuid,
    pub(in crate::app) content: String,
    /// Cosine similarity, -1..=1.
    pub(in crate::app) score: f32,
}

/// The Semantic search overlay. Closing it keeps the last results.
#[derive(Default)]
pub(in crate::app) struct SemanticState {
    pub(in crate::app) open: bool,
    pub(in crate::app) input: EditorState,
    /// The query the results are for (a saved search re-runs it,
    /// feature 5).
    pub(in crate::app) query: String,
    pub(in crate::app) results: Vec<SemanticHit>,
    pub(in crate::app) selected: usize,
    /// A search is running.
    pub(in crate::app) running: bool,
    /// Blocks embedded so far / to embed in this run.
    pub(in crate::app) progress: Option<(usize, usize)>,
    pub(in crate::app) error: Option<String>,
    /// The embedding cache, kept between runs (None: read it from the
    /// graph on the next run, or a run has it). Shared, so "Related"
    /// (decision 44) can read it off the UI thread without a copy; a run
    /// takes it back (copying only if a Related computation still holds
    /// it).
    pub(in crate::app) cache: Option<Arc<Cache>>,
    /// Bumped whenever a cache is put back (after a run, or read from the
    /// graph): Related recomputes when it changes.
    pub(in crate::app) generation: u64,
    /// Related has looked for a cache in the graph this session.
    pub(in crate::app) disk_checked: bool,
    pub(in crate::app) task: Option<Task<()>>,
}

/// Everything a run needs, captured on the UI thread.
pub(in crate::app) struct SemanticJob {
    endpoint: Endpoint,
    key_provider: &'static str,
    embedding: String,
    chat: String,
    root: PathBuf,
    items: Vec<Item>,
    query: String,
    cache: Option<Cache>,
}

/// Where a run reports indexing progress.
#[derive(Clone, Copy, Debug)]
pub(in crate::app) enum Progress {
    Search,
    Ask(usize),
    /// "Index notes" in the Related section (decision 44).
    Related,
    /// A saved semantic search being re-run, by its run id (decision 46).
    Saved(u64),
}

impl NoteSec {
    /// The provider name a cache is keyed by.
    pub(in crate::app) fn cache_key_provider(&self) -> &'static str {
        match self.config.ai_provider {
            AiProvider::Api => "api",
            _ => "local",
        }
    }

    pub(in crate::app) fn semantic_job(
        &mut self,
        endpoint: Endpoint,
        query: String,
    ) -> SemanticJob {
        SemanticJob {
            endpoint,
            key_provider: self.cache_key_provider(),
            embedding: self.ai_embedding_setting(),
            chat: self.ai_chat_model(),
            root: self.storage.root().to_path_buf(),
            items: semantic::items(&self.pages),
            query,
            cache: self.semantic.cache.take().map(Arc::unwrap_or_clone),
        }
    }

    fn report(&mut self, progress: Progress, done: usize, total: usize, cx: &mut Context<Self>) {
        match progress {
            Progress::Search => self.semantic.progress = Some((done, total)),
            Progress::Ask(ix) => {
                if let Some(turn) = self.ask.turns.get_mut(ix) {
                    turn.note = Some(format!("Indexing notes\u{2026} {done}/{total} blocks"));
                }
            }
            Progress::Related => self.related.progress = Some((done, total)),
            Progress::Saved(id) => {
                if let Some(run) = self.saved.runs.values_mut().find(|r| r.id == id) {
                    run.progress = Some((done, total));
                }
            }
        }
        cx.notify();
    }

    /// Put a cache back after a run or a read from the graph.
    pub(in crate::app) fn put_cache(&mut self, cache: Arc<Cache>) {
        self.semantic.cache = Some(cache);
        self.semantic.generation += 1;
        self.semantic.disk_checked = true;
    }
}

/// Index what isn't embedded yet, embed `job.query`, and rank pages by
/// their best block (an empty query only indexes). The cache goes back to
/// `SemanticState` (and to disk) even when a batch fails, so the next run
/// continues where this stopped.
pub(in crate::app) async fn run_job(
    this: &WeakEntity<NoteSec>,
    cx: &mut AsyncApp,
    job: SemanticJob,
    progress: Progress,
) -> Result<Vec<SemanticHit>, AiError> {
    let SemanticJob {
        endpoint,
        key_provider,
        embedding,
        chat,
        root,
        items,
        query,
        cache,
    } = job;
    let (ep, root2) = (endpoint.clone(), root.clone());
    let (model, mut cache) = cx
        .background_spawn(async move {
            let mut cache = cache.unwrap_or_else(|| Cache::load(&root2));
            let model = ai::embedding_model(&ep, &embedding, &chat);
            if let Ok(model) = &model {
                cache.use_key(&CacheKey {
                    provider: key_provider.to_string(),
                    base: ep.url().to_string(),
                    model: model.clone(),
                });
            }
            (model, cache)
        })
        .await;
    let model = match model {
        Ok(model) => model,
        Err(err) => {
            let _ = this.update(cx, |this, _| this.put_cache(Arc::new(cache)));
            return Err(err);
        }
    };
    let missing: Vec<(String, String)> = cache
        .missing(&items)
        .into_iter()
        .map(|item| (item.hash.clone(), item.text.clone()))
        .collect();
    let total = missing.len();
    let _ = this.update(cx, |this, cx| this.report(progress, 0, total, cx));
    let mut done = 0;
    for batch in missing.chunks(ai::EMBED_BATCH) {
        let texts: Vec<String> = batch.iter().map(|(_, text)| text.clone()).collect();
        let (ep, model2) = (endpoint.clone(), model.clone());
        let result = cx
            .background_spawn(async move { ai::embed(&ep, &model2, &texts) })
            .await;
        match result {
            Ok(vectors) => {
                for ((hash, _), vector) in batch.iter().zip(vectors) {
                    cache.vectors.insert(hash.clone(), vector);
                }
            }
            Err(err) => {
                // Keep what was embedded so far.
                let root = root.clone();
                let cache = cx
                    .background_spawn(async move {
                        save_cache(&cache, &root);
                        cache
                    })
                    .await;
                let _ = this.update(cx, |this, _| this.put_cache(Arc::new(cache)));
                return Err(err);
            }
        }
        done += batch.len();
        let _ = this.update(cx, |this, cx| this.report(progress, done, total, cx));
    }
    let items2 = items.clone();
    let cache = cx
        .background_spawn(async move {
            cache.prune(&items2);
            save_cache(&cache, &root);
            Arc::new(cache)
        })
        .await;
    let _ = this.update(cx, |this, _| this.put_cache(cache.clone()));
    if query.is_empty() {
        return Ok(Vec::new());
    }
    let query_vector = cx
        .background_spawn(async move { ai::embed(&endpoint, &model, &[query]) })
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| AiError::BadResponse("no embedding for the query".into()))?;
    let hits = semantic::rank_pages(&query_vector, &items, &cache, SEMANTIC_RESULTS)
        .into_iter()
        .map(|(score, i)| SemanticHit {
            title: items[i].title.clone(),
            block: items[i].id,
            content: items[i].content.clone(),
            score,
        })
        .collect();
    Ok(hits)
}

fn save_cache(cache: &Cache, root: &std::path::Path) {
    if let Err(err) = cache.save(root) {
        eprintln!("notesec: could not save the embedding cache: {err}");
    }
}

impl NoteSec {
    /// "Semantic search" (palette): open the overlay (the last results are
    /// still there).
    pub(in crate::app) fn on_semantic_search(
        &mut self,
        _: &SemanticSearch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.stop_edit(cx);
        self.close_search(cx);
        self.settings = None;
        self.page_menu = None;
        self.shortcuts_open = false;
        self.trash_confirm = None;
        self.ask.open = false;
        self.semantic.open = true;
        if self.config.ai_provider == AiProvider::Off {
            let status = Status {
                text: AiError::Off.to_string(),
                error: true,
            };
            self.show_status(status, cx);
        }
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    pub(in crate::app) fn close_semantic(&mut self, cx: &mut Context<Self>) {
        self.semantic.open = false;
        self.last_layout = None;
        self.last_bounds = None;
        cx.notify();
    }

    /// Enter: search for a new query, else open the highlighted result.
    pub(in crate::app) fn semantic_enter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let query = self.semantic.input.text.trim().to_string();
        if self.semantic.running {
            return;
        }
        if query != self.semantic.query || self.semantic.results.is_empty() {
            self.run_semantic(query, cx);
        } else {
            self.open_semantic_hit(self.semantic.selected, window, cx);
        }
    }

    /// Rank pages by meaning for `query` with the active provider (never
    /// another), indexing new and changed blocks first.
    pub(in crate::app) fn run_semantic(&mut self, query: String, cx: &mut Context<Self>) {
        if query.is_empty() {
            return;
        }
        self.semantic.query = query.clone();
        self.semantic.results.clear();
        self.semantic.selected = 0;
        self.semantic.error = None;
        self.semantic.progress = None;
        let endpoint = match self.ai_endpoint() {
            Ok(endpoint) => endpoint,
            Err(err) => {
                self.semantic.error = Some(err.to_string());
                self.semantic.running = false;
                cx.notify();
                return;
            }
        };
        let job = self.semantic_job(endpoint, query);
        self.semantic.running = true;
        self.semantic.task = Some(cx.spawn(async move |this, cx| {
            let result = run_job(&this, cx, job, Progress::Search).await;
            let _ = this.update(cx, |this, cx| {
                this.semantic.running = false;
                match result {
                    Ok(hits) => this.semantic.results = hits,
                    Err(err) => this.semantic.error = Some(err.to_string()),
                }
                this.semantic.task = None;
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(in crate::app) fn move_semantic_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let count = self.semantic.results.len();
        if count > 0 {
            let next = self.semantic.selected as isize + delta;
            self.semantic.selected = next.clamp(0, count as isize - 1) as usize;
        }
        cx.notify();
    }

    /// Close the overlay and go to the result's block.
    fn open_semantic_hit(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.semantic.results.get(ix).map(|hit| hit.block) else {
            return;
        };
        if find_block(&self.pages, id).is_none() {
            let status = Status {
                text: "That block no longer exists; search again".to_string(),
                error: true,
            };
            self.show_status(status, cx);
            return;
        }
        self.close_semantic(cx);
        self.open_block_ref(id, window, cx);
    }

    /// The overlay: input, a status line (mode, indexing progress, errors)
    /// and the ranked pages with their best block and similarity.
    pub(in crate::app) fn render_semantic(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let state = &self.semantic;
        let (mode, mode_warn) = self.ai_mode_line();
        let off = self.config.ai_provider == AiProvider::Off;
        let active = self.ai_overlay_input() == Some(AiInput::Semantic);

        let status: Option<(String, bool, &'static str)> = if let Some(err) = &state.error {
            Some((err.clone(), true, "semantic-error"))
        } else if off {
            Some((AiError::Off.to_string(), true, "semantic-off"))
        } else if state.running {
            Some(match state.progress {
                Some((done, total)) if total > done => (
                    format!("Indexing notes\u{2026} {done}/{total} blocks"),
                    false,
                    "semantic-progress",
                ),
                _ => ("Searching\u{2026}".to_string(), false, "semantic-progress"),
            })
        } else if !state.query.is_empty() && state.results.is_empty() {
            Some(("No results".to_string(), false, "semantic-empty"))
        } else if state.query.is_empty() {
            Some((
                "Type what you're looking for and press Enter. Pages are ranked by meaning, not exact words.".to_string(),
                false,
                "semantic-hint",
            ))
        } else {
            None
        };

        let rows: Vec<AnyElement> = state
            .results
            .iter()
            .enumerate()
            .map(|(i, hit)| {
                let selected = i == state.selected;
                let snippet: String = hit.content.chars().take(SNIPPET_CHARS).collect();
                div()
                    .id(ElementId::from(("semantic-result", i)))
                    .debug_selector(move || format!("semantic-result-{i}"))
                    .flex_shrink_0()
                    .flex()
                    .flex_col()
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .when(selected, |d| d.bg(theme.selected_bg))
                    .hover(|d| d.bg(theme.selected_bg))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_between()
                            .gap_2()
                            .child(
                                div()
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(if selected { theme.accent } else { theme.text })
                                    .child(hit.title.clone()),
                            )
                            .child(
                                div()
                                    .text_color(theme.muted)
                                    .child(format!("{:.0}%", hit.score.max(0.0) * 100.0)),
                            ),
                    )
                    .child(div().text_color(theme.muted).child(snippet))
                    .on_click(cx.listener(move |this, _e, window, cx| {
                        this.open_semantic_hit(i, window, cx)
                    }))
                    .into_any_element()
            })
            .collect();

        div()
            .id("semantic-backdrop")
            .debug_selector(|| "semantic-backdrop".to_string())
            .absolute()
            .inset_0()
            .occlude()
            .bg(theme.scrim)
            .flex()
            .flex_col()
            .items_center()
            .pt(px(60.0))
            .pb(px(30.0))
            .on_click(cx.listener(|this, _e, _w, cx| this.close_semantic(cx)))
            .child(
                div()
                    .id("semantic-panel")
                    .debug_selector(|| "semantic-panel".to_string())
                    .occlude()
                    .w(px(620.0))
                    .max_h_full()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_3()
                    .rounded_lg()
                    .bg(theme.sidebar_bg)
                    .border_1()
                    .border_color(theme.border)
                    .shadow_lg()
                    .child(
                        div()
                            .flex_shrink_0()
                            .flex()
                            .flex_row()
                            .justify_between()
                            .child(div().font_weight(FontWeight::BOLD).child("Semantic search"))
                            .children(self.save_search_button(SearchKind::Semantic, cx))
                            .child(
                                div()
                                    .text_color(if mode_warn { theme.danger } else { theme.muted })
                                    .child(mode),
                            ),
                    )
                    .child(
                        div()
                            .debug_selector(|| "semantic-input".to_string())
                            .flex_shrink_0()
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
                                    d.child(state.input.text.clone())
                                }
                            }),
                    )
                    .when_some(status, |d, (text, error, selector)| {
                        d.child(
                            div()
                                .debug_selector(move || selector.to_string())
                                .flex_shrink_0()
                                .text_color(if error { theme.danger } else { theme.muted })
                                .child(text),
                        )
                    })
                    .child(
                        div()
                            .id("semantic-results")
                            .min_h_0()
                            .overflow_y_scroll()
                            .flex()
                            .flex_col()
                            .children(rows),
                    ),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{ask, has, local, open_ask, setup, turns, PAGES};
    use super::*;
    use crate::ai::test_server::{dead_endpoint, embeddings_reply, json, serve_fn, stream};
    use gpui::{TestAppContext, VisualTestContext};

    const VOCAB: &[&str] = &["weather", "rust", "book", "ownership", "hello"];
    /// The non-empty blocks of `PAGES` (today's journal is empty).
    const BLOCKS: usize = 5;

    fn open_semantic(cx: &mut VisualTestContext) {
        cx.simulate_keystrokes("ctrl-k");
        cx.simulate_input("semantic search");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
    }

    fn embeddings_server(count: usize) -> (Endpoint, crate::ai::test_server::Requests) {
        serve_fn(count, |_, body| embeddings_reply(body, VOCAB))
    }

    fn inputs(body: &str) -> Vec<String> {
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        v["input"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap().to_string())
            .collect()
    }

    fn model_of(body: &str) -> String {
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        v["model"].as_str().unwrap().to_string()
    }

    fn run(view: &Entity<NoteSec>, cx: &mut VisualTestContext, query: &str) {
        let query = query.to_string();
        view.update(cx, |app, cx| app.run_semantic(query, cx));
        cx.run_until_parked();
    }

    fn top(view: &Entity<NoteSec>, cx: &mut VisualTestContext) -> SemanticHit {
        view.update(cx, |app, _| app.semantic.results[0].clone())
    }

    #[gpui::test]
    fn semantic_search_ranks_pages_by_meaning_and_opens_the_block(cx: &mut TestAppContext) {
        let (ep, requests) = embeddings_server(2);
        // No embedding model set: the chat model embeds (model fallback).
        let (view, cx, dir) = setup(cx, "sem-open", PAGES, local(ep.url()), "");
        open_semantic(cx);
        assert!(has(cx, "semantic-panel") && has(cx, "semantic-hint"));
        cx.simulate_input("weather");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        let hit = top(&view, cx);
        assert_eq!(
            (hit.title.as_str(), hit.content.as_str()),
            ("Test", "weather today")
        );
        let results = view.update(cx, |app, _| app.semantic.results.clone());
        assert_eq!(results.len(), 3, "one row per page");
        assert!(results[0].score > results[1].score);
        assert!(has(cx, "semantic-result-0") && has(cx, "semantic-result-2"));
        {
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert_eq!(requests[0].0, "POST /v1/embeddings HTTP/1.1");
            assert_eq!(inputs(&requests[0].2).len(), BLOCKS, "one batch");
            assert!(inputs(&requests[0].2).contains(&"Test\nweather today".to_string()));
            assert_eq!(model_of(&requests[0].2), "test-model");
            assert_eq!(inputs(&requests[1].2), vec!["weather"]);
        }
        assert!(semantic::Cache::path(&dir).exists());
        assert!(dir.join(".notesec/.gitignore").exists());

        cx.simulate_keystrokes("down");
        assert_eq!(view.update(cx, |app, _| app.semantic.selected), 1);
        cx.simulate_keystrokes("up");
        // Same query: Enter opens the highlighted result.
        cx.simulate_keystrokes("enter");
        view.update(cx, |app, _| {
            assert!(!app.semantic.open);
            assert_eq!(app.pages[app.selected].title, "Test");
            assert_eq!(app.editing, Some(1));
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn the_cache_embeds_only_new_text_and_a_new_model_starts_over(cx: &mut TestAppContext) {
        let (ep, requests) = embeddings_server(8);
        let config = Config {
            ai_embedding_model: "emb".into(),
            ..local(ep.url())
        };
        let (view, cx, dir) = setup(cx, "sem-cache", PAGES, config, "");
        let count = |_: &mut VisualTestContext| requests.lock().unwrap().len();
        run(&view, cx, "rust");
        assert_eq!(count(cx), 2);
        assert_eq!(model_of(&requests.lock().unwrap()[0].2), "emb");
        // Everything is cached: only the query is embedded.
        run(&view, cx, "book");
        assert_eq!(count(cx), 3);
        assert_eq!(top(&view, cx).title, "Rust");

        // An edited block is embedded again, alone.
        view.update(cx, |app, _| {
            let p = app.find_page("Test").unwrap();
            app.pages[p].blocks[0].content = "a book about weather".into();
        });
        run(&view, cx, "book");
        assert_eq!(count(cx), 5);
        assert_eq!(
            inputs(&requests.lock().unwrap()[3].2),
            vec!["Test\na book about weather"]
        );

        // From disk (as after a restart): still nothing to embed.
        view.update(cx, |app, _| app.semantic.cache = None);
        run(&view, cx, "rust");
        assert_eq!(count(cx), 6);

        // Another model: its vectors aren't comparable, so all are redone.
        view.update(cx, |app, _| app.config.ai_embedding_model = "emb2".into());
        run(&view, cx, "rust");
        assert_eq!(count(cx), 8);
        let requests = requests.lock().unwrap();
        assert_eq!(inputs(&requests[6].2).len(), BLOCKS);
        assert_eq!(model_of(&requests[6].2), "emb2");
        drop(requests);
        assert_eq!(semantic::Cache::load(&dir).key.model, "emb2");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn off_and_unreachable_are_explained(cx: &mut TestAppContext) {
        let config = Config {
            ai_provider: AiProvider::Off,
            ..Config::default()
        };
        let (view, cx, dir) = setup(cx, "sem-off", PAGES, config, "");
        open_semantic(cx);
        assert!(has(cx, "semantic-off"));
        cx.simulate_input("rust");
        cx.simulate_keystrokes("enter");
        assert_eq!(
            view.update(cx, |app, _| app.semantic.error.clone()),
            Some(AiError::Off.to_string())
        );
        cx.simulate_keystrokes("escape");
        assert!(!view.update(cx, |app, _| app.semantic.open));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn an_unreachable_server_is_named(cx: &mut TestAppContext) {
        let dead = dead_endpoint();
        let (view, cx, dir) = setup(cx, "sem-dead", PAGES, local(dead.url()), "");
        open_semantic(cx);
        cx.simulate_input("rust");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        let error = view
            .update(cx, |app, _| app.semantic.error.clone())
            .unwrap();
        assert!(
            error.contains("Can't reach") && error.contains(dead.url()),
            "{error}"
        );
        assert!(has(cx, "semantic-error"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn api_mode_embeds_with_its_own_model_and_the_bearer_key(cx: &mut TestAppContext) {
        let (ep, requests) = embeddings_server(2);
        let config = Config {
            ai_provider: AiProvider::Api,
            ai_api_base: ep.url().to_string(),
            ai_api_model: "chat".into(),
            ai_api_embedding_model: "text-embedding-3-small".into(),
            ai_embedding_model: "local-only".into(),
            ..Config::default()
        };
        let (view, cx, dir) = setup(cx, "sem-api", PAGES, config, "ai_api_key = \"sk-x\"\n");
        run(&view, cx, "weather");
        assert_eq!(top(&view, cx).title, "Test");
        let requests = requests.lock().unwrap();
        assert_eq!(model_of(&requests[0].2), "text-embedding-3-small");
        assert_eq!(requests[0].1.as_deref(), Some("Bearer sk-x"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn ask_my_notes_finds_sources_by_meaning_when_an_embedding_model_is_set(
        cx: &mut TestAppContext,
    ) {
        let (ep, requests) = serve_fn(3, |line, body| {
            if line.contains("/embeddings") {
                embeddings_reply(body, VOCAB)
            } else {
                stream(&["Mild [1]."])
            }
        });
        let config = Config {
            ai_embedding_model: "emb".into(),
            ..local(ep.url())
        };
        let (view, cx, dir) = setup(cx, "sem-ask", PAGES, config, "");
        open_ask(cx);
        // No keyword of the question is in the block; its embedding is.
        ask(cx, "forecast? weather");
        let turn = &turns(&view, cx)[0];
        assert_eq!(turn.sources[0].text, "weather today");
        assert!(turn.note.as_deref().unwrap().contains("meaning"));
        assert_eq!(turn.answer, "Mild [1].");
        assert!(has(cx, "ask-note-0"));
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[2]
            .2
            .contains("[1] (page \\\"Test\\\")\\nweather today"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn ask_falls_back_to_keywords_when_embedding_fails(cx: &mut TestAppContext) {
        let (ep, _) = serve_fn(2, |line, _| {
            if line.contains("/embeddings") {
                json(404, r#"{"error":{"message":"no embeddings here"}}"#)
            } else {
                stream(&["Yes [1]."])
            }
        });
        let config = Config {
            ai_embedding_model: "emb".into(),
            ..local(ep.url())
        };
        let (view, cx, dir) = setup(cx, "sem-ask-fallback", PAGES, config, "");
        open_ask(cx);
        ask(cx, "rust ownership");
        let turn = &turns(&view, cx)[0];
        let note = turn.note.clone().unwrap();
        assert!(
            note.contains("keywords") && note.contains("no embeddings here"),
            "{note}"
        );
        assert_eq!(turn.sources[0].title, "Rust");
        assert_eq!(turn.answer, "Yes [1].");
        assert!(turn.error.is_none());
        let _ = std::fs::remove_dir_all(dir);
    }
}
