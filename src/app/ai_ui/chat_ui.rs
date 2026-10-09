//! AI sidebar chat (roadmap v0.3.0 feature 3): a persistent panel docked
//! right of the page, toggled by its hotkey and palette command. It reuses
//! the 3-mode AI infra (`ai_endpoint`, `ai_chat_model`): Local / API key
//! stream answers, Off explains itself instead of erroring. "Include
//! current page" injects the current page's markdown as context. Answers
//! render as markdown (the reading view's emphasis, plus fenced code
//! blocks) with a copy button per answer.

use super::*;
use crate::ai::{self, AiProvider};
use crate::code::{self, Part};

/// The sidebar chat panel. Closing it keeps the turns for the session.
#[derive(Debug)]
pub(in crate::app) struct ChatState {
    pub(in crate::app) open: bool,
    pub(in crate::app) input: EditorState,
    pub(in crate::app) turns: Vec<ChatTurn>,
    /// Inject the current page's markdown as context (on by default).
    pub(in crate::app) include_page: bool,
    /// The answer being streamed (dropping it cancels it).
    pub(in crate::app) task: Option<Task<()>>,
}

impl Default for ChatState {
    fn default() -> Self {
        ChatState {
            open: false,
            input: EditorState::default(),
            turns: Vec::new(),
            include_page: true,
            task: None,
        }
    }
}

/// One question and its (streaming) answer.
#[derive(Clone, Debug, PartialEq)]
pub(in crate::app) struct ChatTurn {
    pub(in crate::app) question: String,
    pub(in crate::app) answer: String,
    /// Why the answer failed (`AiError`'s wording; never contains a key).
    pub(in crate::app) error: Option<String>,
    /// The answer is complete (or failed).
    pub(in crate::app) done: bool,
}

impl NoteSec {
    /// Toggle the sidebar chat (hotkey + palette): closing keeps the
    /// turns; opening stops editing so the input has the keyboard.
    pub(in crate::app) fn on_toggle_chat(
        &mut self,
        _: &ToggleChat,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.chat.open {
            self.close_chat(cx);
            return;
        }
        self.stop_edit(cx);
        self.close_search(cx);
        self.chat.open = true;
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    pub(super) fn close_chat(&mut self, cx: &mut Context<Self>) {
        self.chat.open = false;
        cx.notify();
    }

    /// The chat input has the keyboard: the panel is open, no dialog or
    /// overlay covers it, and no block is being edited (then the block
    /// wins, as with every other input).
    pub(super) fn chat_input_active(&self) -> bool {
        self.ai_overlay_input() == Some(AiInput::Chat)
    }

    /// Enter in the chat input: stream the model's answer into a new
    /// turn. The finished turns go back as history, and the current page
    /// (when toggled on) as context. A question while an answer is still
    /// streaming waits (Enter does nothing). In Off mode the panel
    /// explains itself, so there is nothing to send.
    pub(super) fn chat_send(&mut self, cx: &mut Context<Self>) {
        if self.config.ai_provider == AiProvider::Off {
            return;
        }
        let question = self.chat.input.text.trim().to_string();
        if question.is_empty() || self.chat.turns.last().is_some_and(|t| !t.done) {
            return;
        }
        self.chat.input = EditorState::default();
        let history: Vec<(String, String)> = self
            .chat
            .turns
            .iter()
            .filter(|t| t.done && t.error.is_none())
            .map(|t| (t.question.clone(), t.answer.clone()))
            .collect();
        let page = self.chat_page_context();
        let endpoint = self.ai_endpoint();
        let model = self.ai_chat_model();
        self.chat.turns.push(ChatTurn {
            question: question.clone(),
            answer: String::new(),
            error: None,
            done: false,
        });
        let ix = self.chat.turns.len() - 1;
        let endpoint = match endpoint {
            Ok(endpoint) => endpoint,
            Err(err) => {
                self.finish_chat(ix, Some(err), cx);
                return;
            }
        };
        let messages = ai::chat_messages(
            &history,
            page.as_ref().map(|(t, m)| (t.as_str(), m.as_str())),
            &question,
        );
        self.chat.task = Some(cx.spawn(async move |this, cx| {
            let started = cx
                .background_spawn(async move {
                    let model = ai::chat_model(&endpoint, &model)?;
                    ai::chat_stream(&endpoint, &model, messages)
                })
                .await;
            let mut stream = match started {
                Ok(stream) => stream,
                Err(err) => {
                    let _ = this.update(cx, |this, cx| this.finish_chat(ix, Some(err), cx));
                    return;
                }
            };
            // One background step per piece: each blocks until the server
            // sends more, then the piece is shown and the next step starts.
            loop {
                let (back, piece) = cx
                    .background_spawn(async move {
                        let piece = stream.next();
                        (stream, piece)
                    })
                    .await;
                stream = back;
                let keep_going = this.update(cx, |this, cx| match piece {
                    Ok(Some(piece)) => {
                        if let Some(turn) = this.chat.turns.get_mut(ix) {
                            turn.answer.push_str(&piece);
                        }
                        cx.notify();
                        true
                    }
                    Ok(None) => {
                        this.finish_chat(ix, None, cx);
                        false
                    }
                    Err(err) => {
                        this.finish_chat(ix, Some(err), cx);
                        false
                    }
                });
                if !keep_going.unwrap_or(false) {
                    return;
                }
            }
        }));
        cx.notify();
    }

    fn finish_chat(&mut self, ix: usize, error: Option<ai::AiError>, cx: &mut Context<Self>) {
        if let Some(turn) = self.chat.turns.get_mut(ix) {
            turn.done = true;
            turn.error = error.map(|e| e.to_string());
        }
        self.chat.task = None;
        cx.notify();
    }

    /// The current page's title and markdown, when the toggle is on and a
    /// page is showing.
    fn chat_page_context(&self) -> Option<(String, String)> {
        if !self.chat.include_page || self.mode != Mode::Notes {
            return None;
        }
        let page = self.pages.get(self.selected)?;
        Some((page.title.clone(), page.to_markdown()))
    }

    /// A turn answer onto the clipboard.
    fn copy_chat_answer(&mut self, turn: usize, cx: &mut Context<Self>) {
        let Some(answer) = self.chat.turns.get(turn).map(|t| t.answer.clone()) else {
            return;
        };
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(answer));
        self.show_status(
            Status {
                text: "Copied to clipboard".to_string(),
                error: false,
            },
            cx,
        );
    }

    /// One markdown answer as elements: fenced code blocks as mono boxes,
    /// other lines with the reading view's emphasis (bold, italic, links,
    /// tags). `prefix` scopes the debug selectors.
    fn render_chat_markdown(&self, text: &str, prefix: &str) -> Vec<AnyElement> {
        let (link_style, tag_style, ref_style) = markdown_styles(self.theme);
        let mut out = Vec::new();
        for (i, part) in code::split_code(text).iter().enumerate() {
            match part {
                Part::Code(block) => {
                    out.push(
                        div()
                            .debug_selector(move || format!("{prefix}code-{i}"))
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .border_1()
                            .border_color(self.theme.border)
                            .when_some(self.mono_font.clone(), |d, font| d.font_family(font))
                            .child(block.code.clone())
                            .into_any_element(),
                    );
                }
                Part::Text(prose) => {
                    for (j, line) in prose.lines().enumerate() {
                        if line.trim().is_empty() {
                            continue;
                        }
                        let display = DisplayBlock::inline(line, |_| None);
                        out.push(
                            div()
                                .debug_selector(move || format!("{prefix}line-{i}-{j}"))
                                .child(StyledText::new(display.text.clone()).with_highlights(
                                    reading_highlights(&display, link_style, tag_style, ref_style),
                                ))
                                .into_any_element(),
                        );
                    }
                }
            }
        }
        out
    }

    /// The sidebar chat panel: header, mode line, the page toggle, the
    /// session's turns with streamed markdown answers and copy buttons,
    /// and the input at the bottom. In Off mode it explains itself (with
    /// a way to Settings > AI) instead of erroring.
    pub(super) fn render_chat(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let (mode, mode_warn) = self.ai_mode_line();
        let off = self.config.ai_provider == AiProvider::Off;
        let include = self.chat.include_page;
        let page_title = (self.mode == Mode::Notes)
            .then(|| self.pages.get(self.selected))
            .flatten()
            .map(|p| p.title.clone());

        let turns: Vec<AnyElement> = self
            .chat
            .turns
            .iter()
            .enumerate()
            .map(|(t, turn)| {
                let thinking = !turn.done && turn.answer.is_empty();
                let prefix = format!("chat-{t}-");
                let body = self.render_chat_markdown(&turn.answer, &prefix);
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .debug_selector(move || format!("chat-question-{t}"))
                            .font_weight(FontWeight::BOLD)
                            .child(turn.question.clone()),
                    )
                    .when(thinking, |d| {
                        d.child(div().text_color(theme.muted).child("Thinking\u{2026}"))
                    })
                    .when(!turn.answer.is_empty(), |d| {
                        d.child(
                            div()
                                .debug_selector(move || format!("chat-answer-{t}"))
                                .flex()
                                .flex_col()
                                .gap_1()
                                .children(body),
                        )
                    })
                    .when_some(turn.error.clone(), |d, error| {
                        d.child(
                            div()
                                .debug_selector(move || format!("chat-error-{t}"))
                                .text_color(theme.danger)
                                .child(error),
                        )
                    })
                    .when(turn.done && turn.error.is_none(), |d| {
                        d.child(
                            div().flex().flex_row().child(
                                div()
                                    .id(ElementId::from(("chat-copy", t)))
                                    .debug_selector(move || format!("chat-copy-{t}"))
                                    .px_2()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(theme.border)
                                    .text_color(theme.muted)
                                    .cursor_pointer()
                                    .hover(|d| d.bg(theme.selected_bg))
                                    .child("Copy")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.copy_chat_answer(t, cx);
                                    })),
                            ),
                        )
                    })
                    .into_any_element()
            })
            .collect();
        let empty = turns.is_empty();

        div()
            .id("chat-panel")
            .debug_selector(|| "chat-panel".to_string())
            .flex_shrink_0()
            .w(px(340.0))
            .h_full()
            .flex()
            .flex_col()
            .gap_2()
            .p_4()
            .bg(theme.sidebar_bg)
            .border_l_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .flex_row()
                    .justify_between()
                    .child(div().font_weight(FontWeight::BOLD).child("AI chat"))
                    .child(div().text_color(theme.muted).child("Esc to close")),
            )
            .child(
                div()
                    .debug_selector(|| "chat-mode".to_string())
                    .flex_shrink_0()
                    .text_color(if mode_warn { theme.danger } else { theme.muted })
                    .child(mode),
            )
            .when(off, |d| {
                d.child(
                    div()
                        .debug_selector(|| "chat-off".to_string())
                        .flex_shrink_0()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(ai::AiError::Off.to_string())
                        .child(
                            div().flex().flex_row().child(
                                div()
                                    .id("chat-open-settings")
                                    .debug_selector(|| "chat-open-settings".to_string())
                                    .px_3()
                                    .py_1()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(theme.accent)
                                    .text_color(theme.accent)
                                    .cursor_pointer()
                                    .child("Open Settings > AI")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.show_settings_section(SettingsSection::Ai, cx)
                                    })),
                            ),
                        ),
                )
            })
            .when(!off, |d| {
                d.child(
                    div()
                        .id("chat-include-page")
                        .debug_selector(|| "chat-include-page".to_string())
                        .flex_shrink_0()
                        .flex()
                        .flex_row()
                        .gap_2()
                        .cursor_pointer()
                        .text_color(if include { theme.text } else { theme.muted })
                        .child(if include { "\u{2611}" } else { "\u{2610}" })
                        .child(match page_title {
                            Some(title) => format!("Include current page ({title})"),
                            None => "Include current page (no page open)".to_string(),
                        })
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.chat.include_page = !this.chat.include_page;
                            cx.notify();
                        })),
                )
            })
            .child(
                div()
                    .id("chat-history")
                    .debug_selector(|| "chat-history".to_string())
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .when(empty && !off, |d| {
                        d.child(div().text_color(theme.muted).child(
                            "Talk to the model. It remembers this conversation; toggle the current page in as context.",
                        ))
                    })
                    .children(turns),
            )
            .when(!off, |d| {
                // Only the editor with the keyboard is drawn live (a
                // dialog over the panel takes it).
                let input: AnyElement = if self.chat_input_active() {
                    BlockText { app: cx.entity() }.into_any_element()
                } else {
                    div().child(self.chat.input.text.clone()).into_any_element()
                };
                d.child(
                    div()
                        .id("chat-input")
                        .debug_selector(|| "chat-input".to_string())
                        .flex_shrink_0()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .border_1()
                        .border_color(theme.accent)
                        .bg(theme.bg)
                        .cursor_text()
                        .child(input)
                        .on_click(cx.listener(|this, _, _, cx| {
                            if this.editing.is_some() {
                                this.stop_edit(cx);
                            }
                        })),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_color(theme.muted)
                        .child("Enter to send"),
                )
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{click_on, has, local, open_ask, setup, PAGES};
    use super::*;
    use crate::ai::test_server::{serve, stream};
    use crate::config::Config;
    use gpui::TestAppContext;

    /// Open the chat with its hotkey.
    fn open_chat(cx: &mut gpui::VisualTestContext) {
        cx.simulate_keystrokes("ctrl-shift-a");
        cx.run_until_parked();
    }

    fn chat_turns(view: &gpui::Entity<NoteSec>, cx: &mut gpui::VisualTestContext) -> Vec<ChatTurn> {
        view.update(cx, |app, _| app.chat.turns.clone())
    }

    #[gpui::test]
    fn hotkey_palette_and_esc_toggle_the_chat(cx: &mut TestAppContext) {
        let (ep, _) = serve(vec![stream(&["Hi."])]);
        let (view, cx, dir) = setup(cx, "chat-toggle", PAGES, local(ep.url()), "");
        open_chat(cx);
        assert!(view.update(cx, |app, _| app.chat.open));
        assert!(has(cx, "chat-panel") && has(cx, "chat-input"));
        assert!(view.update(cx, |app, _| app.text_input_open()));
        // Esc closes it; the hotkey reopens it.
        cx.simulate_keystrokes("escape");
        assert!(!view.update(cx, |app, _| app.chat.open));
        assert!(!has(cx, "chat-panel"));
        open_chat(cx);
        assert!(has(cx, "chat-panel"));
        // The palette command does the same (and Ask still opens its own panel).
        cx.simulate_keystrokes("escape");
        cx.simulate_keystrokes("ctrl-k");
        cx.simulate_input("ai chat");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert!(view.update(cx, |app, _| app.chat.open));
        open_ask(cx);
        assert!(view.update(cx, |app, _| app.ask.open && app.chat.open));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn chat_sends_page_context_and_streams_markdown_with_copy(cx: &mut TestAppContext) {
        let (ep, requests) = serve(vec![stream(&["**Bold** answer."])]);
        let (view, cx, dir) = setup(cx, "chat-send", PAGES, local(ep.url()), "");
        open_chat(cx);
        assert!(has(cx, "chat-include-page"));
        cx.simulate_input("What is here?");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        let turn = &chat_turns(&view, cx)[0];
        assert_eq!(turn.answer, "**Bold** answer.");
        assert!(turn.done && turn.error.is_none());
        assert!(has(cx, "chat-answer-0") && has(cx, "chat-copy-0"));
        let requests = requests.lock().unwrap();
        assert_eq!(requests[0].0, "POST /v1/chat/completions HTTP/1.1");
        let body: serde_json::Value = serde_json::from_str(&requests[0].2).unwrap();
        let messages = body["messages"].as_array().unwrap();
        let roles: Vec<&str> = messages
            .iter()
            .map(|m| m["role"].as_str().unwrap())
            .collect();
        assert_eq!(roles, ["system", "system", "user"]);
        let context = messages[1]["content"].as_str().unwrap();
        assert!(context.contains("Current page \"Test\"") && context.contains("- hello"));
        assert_eq!(messages[2]["content"], serde_json::json!("What is here?"));
        drop(requests);
        // The copy button puts the raw answer on the clipboard.
        click_on(cx, "chat-copy-0");
        let item = cx.read_from_clipboard().expect("clipboard has the answer");
        assert_eq!(item.text().as_deref(), Some("**Bold** answer."));
        let status = view.update(cx, |app, _| app.status.clone()).unwrap();
        assert!(!status.error && status.text.contains("Copied"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn untoggling_the_page_leaves_it_out_of_the_prompt(cx: &mut TestAppContext) {
        let (ep, requests) = serve(vec![stream(&["OK."])]);
        let (view, cx, dir) = setup(cx, "chat-no-page", PAGES, local(ep.url()), "");
        open_chat(cx);
        click_on(cx, "chat-include-page");
        cx.simulate_input("Hi");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(chat_turns(&view, cx).len(), 1);
        let requests = requests.lock().unwrap();
        let body: serde_json::Value = serde_json::from_str(&requests[0].2).unwrap();
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2, "system prompt + question, no page");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn off_mode_explains_and_sends_nothing(cx: &mut TestAppContext) {
        let config = Config {
            ai_provider: AiProvider::Off,
            ..Config::default()
        };
        let (view, cx, dir) = setup(cx, "chat-off", PAGES, config, "");
        open_chat(cx);
        assert!(has(cx, "chat-off"));
        assert!(
            !has(cx, "chat-input"),
            "no input when there is nothing to send"
        );
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert!(
            chat_turns(&view, cx).is_empty(),
            "nothing sent, no failed turn"
        );
        assert!(
            view.update(cx, |app, _| app.status.is_none()),
            "explains instead of erroring"
        );
        click_on(cx, "chat-open-settings");
        view.update(cx, |app, _| {
            assert_eq!(
                app.settings.as_ref().map(|s| s.section),
                Some(SettingsSection::Ai)
            )
        });
        let _ = std::fs::remove_dir_all(dir);
    }
}
