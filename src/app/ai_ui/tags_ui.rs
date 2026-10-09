//! "Suggest tags" (decision 44): ask the active mode's chat model for tags
//! for the page on screen, show them as chips under the title, and add a
//! chosen one (or all) to the page's `tags::` line as an undoable edit.
//! Also the page's AI row above "Linked from" (Related + the button) and
//! the per-frame hook that drops suggestions for a page no longer shown.

use super::*;
use crate::autotag;

#[derive(Default)]
pub(in crate::app) struct TagSuggestState {
    /// The page the suggestions are for (None: nothing to show).
    pub(in crate::app) page: Option<String>,
    pub(in crate::app) running: bool,
    pub(in crate::app) tags: Vec<String>,
    /// Why it failed (`AiError`'s wording; never contains a key).
    pub(in crate::app) error: Option<String>,
    /// The model answered (so an empty list means nothing new to add).
    pub(in crate::app) done: bool,
    /// The request (dropping it cancels it).
    pub(in crate::app) task: Option<Task<()>>,
}

impl NoteSec {
    /// Per-frame hook (`render`, before the focused page is drawn): forget
    /// suggestions (cancelling a running request) once their page isn't
    /// the one shown, and keep Related current.
    pub(in crate::app) fn sync_page_ai(&mut self, cx: &mut Context<Self>) {
        let page = self.current_page();
        if self.tag_suggest.page.is_some() && self.tag_suggest.page != page {
            self.tag_suggest = TagSuggestState::default();
        }
        self.sync_related(page, cx);
    }

    pub(in crate::app) fn on_suggest_tags(
        &mut self,
        _: &SuggestTags,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.suggest_tags(cx);
    }

    /// Ask the active provider (never another) for tags for the current
    /// page, offering the vault's existing tags to reuse.
    pub(in crate::app) fn suggest_tags(&mut self, cx: &mut Context<Self>) {
        let Some(title) = self.current_page() else {
            return;
        };
        self.tag_suggest = TagSuggestState {
            page: Some(title.clone()),
            ..Default::default()
        };
        let endpoint = match self.ai_endpoint() {
            Ok(endpoint) => endpoint,
            Err(err) => {
                self.tag_suggest.error = Some(err.to_string());
                cx.notify();
                return;
            }
        };
        let vault = autotag::vault_tags(&self.pages);
        let messages = autotag::tag_messages(&self.pages[self.selected], &vault);
        let chat = self.ai_chat_model();
        self.tag_suggest.running = true;
        self.tag_suggest.task = Some(cx.spawn(async move |this, cx| {
            let reply = cx
                .background_spawn(async move {
                    let model = ai::chat_model(&endpoint, &chat)?;
                    ai::complete(&endpoint, &model, messages)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.tag_suggest.page.as_deref() != Some(title.as_str()) {
                    return;
                }
                let state = &mut this.tag_suggest;
                state.running = false;
                state.task = None;
                match reply {
                    Ok(reply) => {
                        // Read against the page as it is now.
                        if let Some(page) = this.pages.iter().find(|p| p.title == title) {
                            state.tags = autotag::suggestions(&reply, page, &vault);
                        }
                        state.done = true;
                    }
                    Err(err) => state.error = Some(err.to_string()),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Add `names` to the current page's `tags::` line: one undo step,
    /// saved like any edit (which also creates the tags' pages, as typing
    /// them would).
    pub(in crate::app) fn apply_suggested_tags(
        &mut self,
        names: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(title) = self.tag_suggest.page.clone() else {
            return;
        };
        if self.current_page().as_deref() != Some(title.as_str()) {
            return;
        }
        self.stop_edit(cx);
        let before = self.history_state();
        let mut page = self.pages[self.selected].clone();
        if autotag::apply_tags(&mut page, &names) {
            self.record_state(before);
            self.pages[self.selected] = page;
            self.save_page();
        }
        let applied: Vec<String> = names.iter().map(|n| n.to_lowercase()).collect();
        self.tag_suggest
            .tags
            .retain(|t| !applied.contains(&t.to_lowercase()));
        if self.tag_suggest.tags.is_empty() {
            self.tag_suggest = TagSuggestState::default();
        }
        cx.notify();
    }

    /// The chips under the title of the focused page (None when there is
    /// nothing for it).
    pub(in crate::app) fn render_tag_suggestions(
        &self,
        page_ix: usize,
        focused: bool,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let state = &self.tag_suggest;
        if !focused || state.page.as_deref() != Some(self.pages[page_ix].title.as_str()) {
            return None;
        }
        let theme = self.theme;
        let mut row = div()
            .debug_selector(|| "tag-suggestions".to_string())
            .mb_4()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap_2()
            .text_color(theme.muted)
            .child("Suggested tags:");
        if state.running {
            row = row.child(
                div()
                    .debug_selector(|| "tags-running".to_string())
                    .child("Suggesting tags\u{2026}"),
            );
        } else if let Some(err) = &state.error {
            row = row.child(
                div()
                    .debug_selector(|| "tags-error".to_string())
                    .text_color(theme.danger)
                    .child(err.clone()),
            );
        } else if state.done && state.tags.is_empty() {
            row = row.child(
                div()
                    .debug_selector(|| "tags-none".to_string())
                    .child("No new tags to suggest"),
            );
        }
        for (i, name) in state.tags.iter().enumerate() {
            let apply = name.clone();
            row = row.child(
                div()
                    .id(("tag-chip", i))
                    .debug_selector(move || format!("tag-chip-{i}"))
                    .px_2()
                    .rounded_md()
                    .bg(theme.selected_bg)
                    .text_color(theme.accent)
                    .cursor_pointer()
                    .hover(|d| d.border_1().border_color(theme.accent))
                    .child(format!("+ #{name}"))
                    .on_click(cx.listener(move |this, _e, _w, cx| {
                        this.apply_suggested_tags(vec![apply.clone()], cx)
                    })),
            );
        }
        if state.tags.len() > 1 {
            row = row.child(
                div()
                    .id("tags-add-all")
                    .debug_selector(|| "tags-add-all".to_string())
                    .text_color(theme.accent)
                    .cursor_pointer()
                    .child("Add all")
                    .on_click(cx.listener(|this, _e, _w, cx| {
                        let all = this.tag_suggest.tags.clone();
                        this.apply_suggested_tags(all, cx)
                    })),
            );
        }
        row = row.child(
            div()
                .id("tags-dismiss")
                .debug_selector(|| "tags-dismiss".to_string())
                .cursor_pointer()
                .child("\u{d7}")
                .on_click(cx.listener(|this, _e, _w, cx| {
                    this.tag_suggest = TagSuggestState::default();
                    cx.notify();
                })),
        );
        Some(row.into_any_element())
    }

    /// The focused page's AI row, just above "Linked from": the Related
    /// section (when the active mode can have one) and a small "Suggest
    /// tags" button. Nothing at all when AI is off.
    pub(in crate::app) fn render_page_ai(
        &self,
        page_ix: usize,
        focused: bool,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !focused || self.config.ai_provider == AiProvider::Off {
            return None;
        }
        let theme = self.theme;
        let related = self.related_visible();
        let title = self.pages[page_ix].title.clone();
        let button = div()
            .id("suggest-tags-button")
            .debug_selector(|| "suggest-tags-button".to_string())
            .text_color(theme.muted)
            .cursor_pointer()
            .hover(|d| d.text_color(theme.accent))
            .child("Suggest tags")
            .on_click(cx.listener(|this, _e, _w, cx| this.suggest_tags(cx)));
        let body = if related {
            self.render_related(&title, cx)
        } else {
            Vec::new()
        };
        Some(
            div()
                .id("page-ai")
                .debug_selector(|| "page-ai".to_string())
                .mt_8()
                .flex()
                .flex_col()
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .justify_between()
                        .gap_2()
                        .child(div().when(related, |d| {
                            d.debug_selector(|| "related".to_string())
                                .text_color(theme.text)
                                .child("Related")
                        }))
                        .child(button),
                )
                .children(body)
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{click_on, has, local, setup};
    use super::*;
    use crate::ai::test_server::{dead_endpoint, serve, stream};
    use gpui::{TestAppContext, VisualTestContext};

    const PAGES: &[(&str, &str)] = &[
        ("Rust", "- ownership and borrowing\n- see #programming\n"),
        ("Books", "- tagged #reading\n"),
    ];
    const REPLY: &str = "[\"Programming\", \"memory safety\", \"reading\", \"rust\"]";

    fn suggest(cx: &mut VisualTestContext) {
        cx.simulate_keystrokes("ctrl-k");
        cx.simulate_input("suggest tags");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
    }

    fn chips(view: &Entity<NoteSec>, cx: &mut VisualTestContext) -> Vec<String> {
        view.update(cx, |app, _| app.tag_suggest.tags.clone())
    }

    fn first_block(view: &Entity<NoteSec>, cx: &mut VisualTestContext) -> String {
        view.update(cx, |app, _| {
            let p = app.find_page("Rust").unwrap();
            app.pages[p].blocks[0].content.clone()
        })
    }

    #[gpui::test]
    fn suggested_tags_are_chips_and_a_click_adds_one_as_an_undoable_edit(cx: &mut TestAppContext) {
        let (ep, requests) = serve(vec![stream(&[REPLY])]);
        let (view, cx, dir) = setup(cx, "tags-chip", PAGES, local(ep.url()), "");
        suggest(cx);
        // The page's own tag, its title and repeats are dropped; the
        // vault's spelling is kept.
        assert_eq!(chips(&view, cx), vec!["memory safety", "reading"]);
        assert!(has(cx, "tag-chip-0") && has(cx, "tag-chip-1") && has(cx, "tags-add-all"));
        {
            let requests = requests.lock().unwrap();
            assert_eq!(requests[0].0, "POST /v1/chat/completions HTTP/1.1");
            let body: serde_json::Value = serde_json::from_str(&requests[0].2).unwrap();
            assert_eq!(body["model"], "test-model");
            let user = body["messages"][1]["content"].as_str().unwrap();
            assert!(
                user.contains("programming") && user.contains("reading"),
                "{user}"
            );
            assert!(user.contains("- ownership and borrowing"), "{user}");
        }

        click_on(cx, "tag-chip-0");
        assert_eq!(first_block(&view, cx), "tags:: #[[memory safety]]");
        let file = std::fs::read_to_string(dir.join("pages/Rust.md")).unwrap();
        assert!(file.starts_with("- tags:: #[[memory safety]]\n"), "{file}");
        assert_eq!(chips(&view, cx), vec!["reading"]);
        assert!(!has(cx, "tags-add-all"));
        // The tag is a real reference: its page exists, as when typed.
        assert!(view.update(cx, |app, _| app.find_page("memory safety").is_some()));

        cx.simulate_keystrokes("ctrl-z");
        cx.run_until_parked();
        assert_eq!(first_block(&view, cx), "ownership and borrowing");
        let file = std::fs::read_to_string(dir.join("pages/Rust.md")).unwrap();
        assert!(file.starts_with("- ownership and borrowing\n"), "{file}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn add_all_then_leaving_the_page_drops_a_running_request(cx: &mut TestAppContext) {
        let (ep, requests) = serve(vec![stream(&[REPLY]), stream(&[REPLY])]);
        let (view, cx, dir) = setup(cx, "tags-all", PAGES, local(ep.url()), "");
        // The button on the page does the same as the command.
        assert!(has(cx, "suggest-tags-button"));
        click_on(cx, "suggest-tags-button");
        cx.run_until_parked();
        click_on(cx, "tags-add-all");
        assert_eq!(
            first_block(&view, cx),
            "tags:: #[[memory safety]], #reading"
        );
        assert!(!has(cx, "tag-suggestions"), "nothing left to offer");

        // Ask again, and go to another page before the answer is read.
        view.update(cx, |app, cx| {
            app.suggest_tags(cx);
            app.open_page("Books", cx);
        });
        cx.run_until_parked();
        view.update(cx, |app, _| {
            assert_eq!(app.pages[app.selected].title, "Books");
            assert_eq!(app.tag_suggest.page, None);
            assert!(app.tag_suggest.task.is_none());
        });
        assert!(!has(cx, "tag-suggestions"));
        assert!(requests.lock().unwrap().len() <= 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn off_and_unreachable_are_explained_and_change_nothing(cx: &mut TestAppContext) {
        let off = Config {
            ai_provider: AiProvider::Off,
            ..Config::default()
        };
        let (view, cx, dir) = setup(cx, "tags-off", PAGES, off, "");
        // No AI row on the page when AI is off.
        assert!(!has(cx, "page-ai") && !has(cx, "suggest-tags-button"));
        suggest(cx);
        assert!(has(cx, "tags-error"));
        let error = view
            .update(cx, |app, _| app.tag_suggest.error.clone())
            .unwrap();
        assert_eq!(error, AiError::Off.to_string());
        click_on(cx, "tags-dismiss");
        assert!(!has(cx, "tag-suggestions"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn an_unreachable_server_is_named(cx: &mut TestAppContext) {
        let ep = dead_endpoint();
        let (view, cx, dir) = setup(cx, "tags-dead", PAGES, local(ep.url()), "");
        suggest(cx);
        let error = view
            .update(cx, |app, _| app.tag_suggest.error.clone())
            .unwrap();
        assert!(error.contains(ep.url()), "{error}");
        assert!(has(cx, "tags-error"));
        assert_eq!(first_block(&view, cx), "ownership and borrowing");
        let _ = std::fs::remove_dir_all(dir);
    }
}
