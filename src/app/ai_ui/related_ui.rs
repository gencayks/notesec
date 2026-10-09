//! "Related" (decision 44): the pages most similar to the one on screen,
//! from the embedding cache semantic search fills (decision 43). Shown
//! just above "Linked from"; it never calls the model on its own. The
//! ranking is computed off the UI thread from the cache already there,
//! once per page / cache generation / save count; "Index notes" is the
//! one explicit way to embed what's missing.

use super::*;
use crate::semantic::{self, Cache};
use std::sync::Arc;

/// How many related pages are listed.
pub(in crate::app) const RELATED_PAGES: usize = 5;

#[derive(Clone, Debug, PartialEq)]
pub(in crate::app) struct RelatedHit {
    pub(in crate::app) title: String,
    /// Cosine similarity of the two pages' mean vectors.
    pub(in crate::app) score: f32,
}

#[derive(Default)]
pub(in crate::app) struct RelatedState {
    /// What `results` are for: (page title, cache generation, storage
    /// save count). Anything else changing recomputes them.
    pub(in crate::app) key: Option<(String, u64, u64)>,
    pub(in crate::app) results: Vec<RelatedHit>,
    /// The page has no embedded block with the active model yet.
    pub(in crate::app) unindexed: bool,
    /// A ranking being computed (dropping it cancels it).
    pub(in crate::app) computing: Option<Task<()>>,
    /// The cache being read from the graph.
    pub(in crate::app) loading: Option<Task<()>>,
    /// "Index notes" is running: blocks embedded / to embed.
    pub(in crate::app) indexing: bool,
    pub(in crate::app) progress: Option<(usize, usize)>,
    pub(in crate::app) error: Option<String>,
    pub(in crate::app) index_task: Option<Task<()>>,
}

impl NoteSec {
    /// The cache, if it was made by the active mode's endpoint with the
    /// model it would use (any model when neither model is set: the
    /// server picks, decision 43). Never another provider's vectors.
    pub(in crate::app) fn related_cache(&self) -> Option<Arc<Cache>> {
        let cache = self.semantic.cache.as_ref()?;
        let endpoint = self.ai_endpoint().ok()?;
        let want = [self.ai_embedding_setting(), self.ai_chat_model()]
            .into_iter()
            .map(|m| m.trim().to_string())
            .find(|m| !m.is_empty())
            .unwrap_or_default();
        let key = &cache.key;
        let matches = key.provider == self.cache_key_provider()
            && key.base == endpoint.url()
            && !key.model.is_empty()
            && (want.is_empty() || key.model == want);
        matches.then(|| cache.clone())
    }

    /// The section shows: a mode is on with a usable endpoint, and either
    /// an embedding model is set for it or there is a matching cache.
    pub(in crate::app) fn related_visible(&self) -> bool {
        self.config.ai_provider != AiProvider::Off
            && self.ai_endpoint().is_ok()
            && (!self.ai_embedding_setting().trim().is_empty() || self.related_cache().is_some())
    }

    /// Called each frame for the focused page: read the cache from the
    /// graph once, then (re)rank related pages off the UI thread when the
    /// page, the cache or the notes changed. No network.
    pub(in crate::app) fn sync_related(&mut self, page: Option<String>, cx: &mut Context<Self>) {
        let Some(title) = page.filter(|_| self.config.ai_provider != AiProvider::Off) else {
            return;
        };
        if self.semantic.cache.is_none() && !self.semantic.disk_checked {
            self.semantic.disk_checked = true;
            let root = self.storage.root().to_path_buf();
            if Cache::path(&root).exists() {
                self.related.loading = Some(cx.spawn(async move |this, cx| {
                    let cache = cx.background_spawn(async move { Cache::load(&root) }).await;
                    let _ = this.update(cx, |this, cx| {
                        this.related.loading = None;
                        // A run may have put a newer one back meanwhile.
                        if this.semantic.cache.is_none() {
                            this.put_cache(Arc::new(cache));
                        }
                        cx.notify();
                    });
                }));
                return;
            }
        }
        if !self.related_visible() || self.related.computing.is_some() {
            return;
        }
        let key = (
            title.clone(),
            self.semantic.generation,
            self.storage.changes(),
        );
        if self.related.key.as_ref() == Some(&key) {
            return;
        }
        let Some(cache) = self.related_cache() else {
            self.related.key = Some(key);
            self.related.results.clear();
            self.related.unindexed = true;
            return;
        };
        let pages = self.pages.clone();
        self.related.computing = Some(cx.spawn(async move |this, cx| {
            let found = cx
                .background_spawn(async move {
                    let ix = pages.iter().position(|p| p.title == title)?;
                    let items = semantic::items(&pages);
                    let indexed = items
                        .iter()
                        .any(|i| i.page == ix && cache.vectors.contains_key(&i.hash));
                    indexed.then(|| {
                        semantic::related_pages(ix, &items, &cache, RELATED_PAGES)
                            .into_iter()
                            .map(|(score, p)| RelatedHit {
                                title: pages[p].title.clone(),
                                score,
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.related.computing = None;
                this.related.key = Some(key);
                this.related.unindexed = found.is_none();
                this.related.results = found.unwrap_or_default();
                cx.notify();
            });
        }));
    }

    /// "Index notes": embed what's missing with the active mode (the same
    /// run as semantic search, without a query).
    pub(in crate::app) fn index_related(&mut self, cx: &mut Context<Self>) {
        if self.related.indexing {
            return;
        }
        self.related.error = None;
        self.related.progress = None;
        let endpoint = match self.ai_endpoint() {
            Ok(endpoint) => endpoint,
            Err(err) => {
                self.related.error = Some(err.to_string());
                cx.notify();
                return;
            }
        };
        let job = self.semantic_job(endpoint, String::new());
        self.related.indexing = true;
        self.related.index_task = Some(cx.spawn(async move |this, cx| {
            let result = semantic_ui::run_job(&this, cx, job, Progress::Related).await;
            let _ = this.update(cx, |this, cx| {
                this.related.indexing = false;
                this.related.progress = None;
                if let Err(err) = result {
                    this.related.error = Some(err.to_string());
                }
                this.related.index_task = None;
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// The Related section's body for page `title` (the header and the
    /// "Suggest tags" button are drawn by `render_page_ai`).
    pub(in crate::app) fn render_related(
        &self,
        title: &str,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let theme = self.theme;
        let state = &self.related;
        let note = |selector: &'static str, text: String, error: bool| {
            div()
                .debug_selector(move || selector.to_string())
                .mt_1()
                .text_color(if error { theme.danger } else { theme.muted })
                .child(text)
                .into_any_element()
        };
        let index_button = || {
            div()
                .id("related-index")
                .debug_selector(|| "related-index".to_string())
                .mt_1()
                .text_color(theme.accent)
                .cursor_pointer()
                .child("Index notes")
                .on_click(cx.listener(|this, _e, _w, cx| this.index_related(cx)))
                .into_any_element()
        };
        let mut out = Vec::new();
        if state.indexing {
            let text = match state.progress {
                Some((done, total)) if total > done => {
                    format!("Indexing notes\u{2026} {done}/{total} blocks")
                }
                _ => "Indexing notes\u{2026}".to_string(),
            };
            out.push(note("related-progress", text, false));
            return out;
        }
        if let Some(err) = &state.error {
            out.push(note(
                "related-error",
                format!("Couldn't index: {err}"),
                true,
            ));
            out.push(index_button());
            return out;
        }
        if state.key.as_ref().is_none_or(|k| k.0 != title) {
            return out; // being computed
        }
        if state.unindexed {
            out.push(note(
                "related-unindexed",
                "This page isn't indexed for similarity yet.".to_string(),
                false,
            ));
            out.push(index_button());
        } else if state.results.is_empty() {
            out.push(note(
                "related-empty",
                "No similar pages yet.".to_string(),
                false,
            ));
        }
        for (i, hit) in state.results.iter().enumerate() {
            let open = hit.title.clone();
            out.push(
                div()
                    .id(("related", i))
                    .debug_selector(move || format!("related-{i}"))
                    .flex()
                    .flex_row()
                    .justify_between()
                    .gap_2()
                    .pl_4()
                    .py_1()
                    .cursor_pointer()
                    .hover(|d| d.bg(theme.selected_bg))
                    .child(div().text_color(theme.accent).child(hit.title.clone()))
                    .child(
                        div()
                            .text_color(theme.muted)
                            .child(format!("{:.0}%", hit.score.max(0.0) * 100.0)),
                    )
                    .on_click(cx.listener(move |this, _e, _w, cx| this.open_page(&open, cx)))
                    .into_any_element(),
            );
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{click_on, has, local, setup, PAGES};
    use super::*;
    use crate::ai::test_server::{dead_endpoint, embeddings_reply, fake_embedding, serve_fn};
    use crate::semantic::CacheKey;
    use gpui::{TestAppContext, VisualTestContext};

    const VOCAB: &[&str] = &["weather", "rust", "book", "ownership", "hello"];

    fn related(view: &Entity<NoteSec>, cx: &mut VisualTestContext) -> Vec<String> {
        view.update(cx, |app, _| {
            app.related
                .results
                .iter()
                .map(|r| r.title.clone())
                .collect()
        })
    }

    fn open(view: &Entity<NoteSec>, cx: &mut VisualTestContext, title: &str) {
        view.update(cx, |app, cx| app.open_page(title, cx));
        cx.run_until_parked();
    }

    #[gpui::test]
    fn index_notes_then_related_lists_similar_pages_without_more_requests(cx: &mut TestAppContext) {
        let (ep, requests) = serve_fn(1, |_, body| embeddings_reply(body, VOCAB));
        let config = Config {
            ai_embedding_model: "emb".into(),
            ..local(ep.url())
        };
        let (view, cx, dir) = setup(cx, "related-index", PAGES, config, "");
        // An embedding model is set but nothing is indexed: a note and the
        // button, and no request on its own.
        assert!(has(cx, "related") && has(cx, "related-unindexed"));
        assert!(requests.lock().unwrap().is_empty());
        click_on(cx, "related-index");
        cx.run_until_parked();
        assert_eq!(requests.lock().unwrap().len(), 1, "one batch, no query");
        assert!(crate::semantic::Cache::path(&dir).exists());

        open(&view, cx, "Rust");
        // Diary mentions rust; Test shares nothing but the constant.
        assert_eq!(related(&view, cx), vec!["Diary", "Test"]);
        let scores: Vec<f32> = view.update(cx, |app, _| {
            app.related.results.iter().map(|r| r.score).collect()
        });
        assert!(scores[0] > scores[1]);
        assert!(has(cx, "related-0") && has(cx, "related-1"));
        assert!(!has(cx, "related-unindexed"));
        click_on(cx, "related-0");
        cx.run_until_parked();
        view.update(cx, |app, _| {
            assert_eq!(app.pages[app.selected].title, "Diary")
        });
        // Diary's own list comes from the same cache.
        assert_eq!(related(&view, cx)[0], "Rust");
        assert_eq!(requests.lock().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn related_reads_the_saved_cache_and_hides_for_other_modes(cx: &mut TestAppContext) {
        // Nothing listens: Related must never call the server.
        let ep = dead_endpoint();
        let (view, cx, dir) = setup(cx, "related-saved", PAGES, local(ep.url()), "");
        // No embedding model, no cache: only the button.
        assert!(has(cx, "suggest-tags-button") && !has(cx, "related"));

        // A cache made with this endpoint and its chat model (the model
        // semantic search falls back to), as left by an earlier session.
        let pages = view.update(cx, |app, _| app.pages.clone());
        let mut cache = Cache::default();
        cache.key = CacheKey {
            provider: "local".into(),
            base: ep.url().to_string(),
            model: "test-model".into(),
        };
        for item in semantic::items(&pages) {
            cache
                .vectors
                .insert(item.hash.clone(), fake_embedding(&item.text, VOCAB));
        }
        cache.save(&dir).unwrap();
        view.update(cx, |app, cx| {
            app.semantic.disk_checked = false;
            cx.notify();
        });
        cx.run_until_parked();
        open(&view, cx, "Rust");
        assert!(has(cx, "related"));
        assert_eq!(related(&view, cx)[0], "Diary");

        // Another mode never uses these vectors: API key mode hides it.
        view.update(cx, |app, cx| {
            app.config.ai_provider = AiProvider::Api;
            app.config.ai_api_base = "https://api.example.com/v1".into();
            app.state.ai_api_key = "sk-test".into();
            cx.notify();
        });
        cx.run_until_parked();
        assert!(!has(cx, "related") && has(cx, "suggest-tags-button"));
        // Off: no AI row at all.
        view.update(cx, |app, cx| {
            app.config.ai_provider = AiProvider::Off;
            cx.notify();
        });
        cx.run_until_parked();
        assert!(!has(cx, "page-ai"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn a_failed_index_says_why_and_offers_to_try_again(cx: &mut TestAppContext) {
        let ep = dead_endpoint();
        let config = Config {
            ai_embedding_model: "emb".into(),
            ..local(ep.url())
        };
        let (view, cx, dir) = setup(cx, "related-dead", PAGES, config, "");
        click_on(cx, "related-index");
        cx.run_until_parked();
        assert!(has(cx, "related-error") && has(cx, "related-index"));
        let error = view.update(cx, |app, _| app.related.error.clone()).unwrap();
        assert!(error.contains(ep.url()), "{error}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
