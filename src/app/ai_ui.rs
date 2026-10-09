//! The AI parts of the root view (decision 42): Settings > AI and the
//! "Ask my notes" panel. Kept in this submodule so `app.rs` only carries
//! one-line hooks (`active_editor`, `text_input_open`, `enter`, `escape`,
//! the key context, and the action / overlay lists).
//!
//! Both use the shared `EditorState` text field machinery: whichever AI
//! field is active is returned by `NoteSec::active_editor` and drawn with
//! `BlockText`, and the root's key context is "BlockEditor" while it has
//! the keyboard, so typing, Backspace, arrows, Enter and Esc work as in the
//! palette's query box.

use super::*;
use crate::ai::{self, AiError, AiProvider, Endpoint};
use crate::config::DEFAULT_API_BASE;

/// Semantic search (decision 43): its overlay and the indexing pipeline
/// Ask my notes shares.
mod semantic_ui;
use semantic_ui::Progress;
pub(super) use semantic_ui::SemanticState;

/// Tag suggestions and Related pages (decision 44).
mod related_ui;
mod tags_ui;
pub(super) use related_ui::RelatedState;
pub(super) use tags_ui::TagSuggestState;

/// A text field of Settings > AI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AiField {
    /// Local mode: the server's base URL.
    Endpoint,
    /// Local mode: chat model id.
    Model,
    /// Local mode: embedding model id (semantic search, decision 43).
    EmbeddingModel,
    /// API key mode: base URL.
    ApiBase,
    /// API key mode: the key (state.toml, plaintext).
    ApiKey,
    /// API key mode: chat model id.
    ApiModel,
    /// API key mode: embedding model id (decision 43).
    ApiEmbeddingModel,
}

impl AiField {
    pub(super) fn name(self) -> &'static str {
        match self {
            AiField::Endpoint => "endpoint",
            AiField::Model => "model",
            AiField::EmbeddingModel => "embedding-model",
            AiField::ApiBase => "api-base",
            AiField::ApiKey => "api-key",
            AiField::ApiModel => "api-model",
            AiField::ApiEmbeddingModel => "api-embedding-model",
        }
    }

    fn label(self) -> &'static str {
        match self {
            AiField::Endpoint => "Server URL",
            AiField::Model => "Chat model",
            AiField::EmbeddingModel => "Embedding model",
            AiField::ApiBase => "API base URL",
            AiField::ApiKey => "API key",
            AiField::ApiModel => "Chat model",
            AiField::ApiEmbeddingModel => "Embedding model",
        }
    }

    /// What an empty field means, shown muted.
    fn placeholder(self) -> &'static str {
        match self {
            AiField::Endpoint => ai::DEFAULT_ENDPOINT,
            AiField::Model => "Empty: the server's first chat model",
            AiField::EmbeddingModel => "Empty: use the chat model",
            AiField::ApiBase => DEFAULT_API_BASE,
            AiField::ApiKey => "Not set",
            AiField::ApiModel => "e.g. gpt-4o-mini, grok-4, claude-sonnet-4",
            AiField::ApiEmbeddingModel => "Empty: use the chat model",
        }
    }
}

/// Settings > AI's state while the panel is open.
#[derive(Default)]
pub(super) struct AiSettings {
    /// The field being edited and its editor. Enter saves, Esc cancels.
    pub(super) field: Option<(AiField, EditorState)>,
    /// The last "Check connection": the server's models, or why it failed.
    pub(super) models: Option<Result<Vec<String>, String>>,
    pub(super) checking: bool,
    /// The running check (dropping it cancels it).
    pub(super) check_task: Option<Task<()>>,
}

/// One block given to the model as a numbered source.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct AskSource {
    pub(super) title: String,
    pub(super) block: Uuid,
    pub(super) text: String,
}

/// One question and its (streaming) answer.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct AskTurn {
    pub(super) question: String,
    pub(super) answer: String,
    pub(super) sources: Vec<AskSource>,
    /// Why the answer failed (`AiError`'s wording; never contains a key).
    pub(super) error: Option<String>,
    /// The answer is complete (or failed).
    pub(super) done: bool,
    /// How the sources were found, or indexing progress (decision 43).
    pub(super) note: Option<String>,
}

impl AskTurn {
    /// The 1-based source numbers the answer cites, in citation order.
    pub(super) fn cited(&self) -> Vec<usize> {
        ai::cited(&self.answer, self.sources.len())
    }
}

/// The "Ask my notes" panel. Closing it keeps the history for the session.
#[derive(Default)]
pub(super) struct AskState {
    pub(super) open: bool,
    pub(super) input: EditorState,
    pub(super) turns: Vec<AskTurn>,
    /// The answer being streamed (dropping it cancels it).
    pub(super) task: Option<Task<()>>,
}

/// Which AI overlay's input has the keyboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AiInput {
    Ask,
    Semantic,
}

/// The AI editor that has the keyboard, if any, from the parts of
/// `NoteSec` it lives in (split so `active_editor_mut` can fall back to the
/// block editor without a borrow conflict).
pub(super) fn ai_editor_mut<'a>(
    settings: &'a mut Option<SettingsState>,
    ask: &'a mut AskState,
    semantic: &'a mut SemanticState,
    input: Option<AiInput>,
) -> Option<&'a mut EditorState> {
    if let Some(SettingsState {
        ai: AiSettings {
            field: Some((_, editor)),
            ..
        },
        ..
    }) = settings
    {
        return Some(editor);
    }
    match input {
        Some(AiInput::Ask) => Some(&mut ask.input),
        Some(AiInput::Semantic) => Some(&mut semantic.input),
        None => None,
    }
}

/// Longest snippet of a source shown in the panel.
const SNIPPET_CHARS: usize = 160;

impl NoteSec {
    // --- hooks used by app.rs -------------------------------------------------

    /// The open AI overlay (Ask or Semantic search; never both) whose
    /// input has the keyboard: no dialog, palette or menu covers it.
    pub(super) fn ai_overlay_input(&self) -> Option<AiInput> {
        let covered = self.settings.is_some()
            || self.search.is_some()
            || self.page_menu.is_some()
            || self.shortcuts_open
            || self.trash_confirm.is_some();
        if covered {
            None
        } else if self.ask.open {
            Some(AiInput::Ask)
        } else if self.semantic.open {
            Some(AiInput::Semantic)
        } else {
            None
        }
    }

    /// The Ask panel's input has the keyboard.
    pub(super) fn ask_input_active(&self) -> bool {
        self.ai_overlay_input() == Some(AiInput::Ask)
    }

    /// The open AI overlay, if any (`app.rs` adds it under the palette).
    pub(super) fn render_ai_overlay(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.ask.open {
            Some(self.render_ask(cx))
        } else if self.semantic.open {
            Some(self.render_semantic(cx))
        } else {
            None
        }
    }

    /// A Settings > AI field is being edited.
    pub(super) fn ai_settings_editing(&self) -> bool {
        self.settings.as_ref().is_some_and(|s| s.ai.field.is_some())
    }

    /// The AI editor that has the keyboard, if any.
    pub(super) fn ai_editor(&self) -> Option<&EditorState> {
        if let Some((_, editor)) = self.settings.as_ref().and_then(|s| s.ai.field.as_ref()) {
            return Some(editor);
        }
        match self.ai_overlay_input()? {
            AiInput::Ask => Some(&self.ask.input),
            AiInput::Semantic => Some(&self.semantic.input),
        }
    }

    /// Enter in an AI field: save the Settings field, or ask the question.
    /// Returns whether it was handled.
    pub(super) fn ai_enter(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.ai_settings_editing() {
            self.commit_ai_field(cx);
            return true;
        }
        match self.ai_overlay_input() {
            Some(AiInput::Ask) => self.ask_question(cx),
            Some(AiInput::Semantic) => self.semantic_enter(window, cx),
            None => return false,
        }
        true
    }

    /// Up / Down in the semantic search results. Returns whether handled.
    pub(super) fn ai_move_selection(&mut self, delta: isize, cx: &mut Context<Self>) -> bool {
        if self.ai_overlay_input() != Some(AiInput::Semantic) {
            return false;
        }
        self.move_semantic_selection(delta, cx);
        true
    }

    /// Esc: cancel the Settings field being edited (the panel stays), or
    /// close the Ask panel (its history stays). Returns whether handled.
    pub(super) fn ai_escape(&mut self, cx: &mut Context<Self>) -> bool {
        if self.ai_settings_editing() {
            if let Some(settings) = &mut self.settings {
                settings.ai.field = None;
            }
            cx.notify();
            return true;
        }
        match self.ai_overlay_input() {
            Some(AiInput::Ask) => self.close_ask(cx),
            Some(AiInput::Semantic) => self.close_semantic(cx),
            None => return false,
        }
        true
    }

    // --- settings -----------------------------------------------------------------

    /// The endpoint the active provider uses (never another mode's).
    pub(super) fn ai_endpoint(&self) -> Result<Endpoint, AiError> {
        Endpoint::from_settings(
            self.config.ai_provider,
            &self.config.ai_endpoint,
            &self.config.ai_api_base,
            &self.state.ai_api_key,
        )
    }

    /// The chat model setting of the active provider (empty: ask the
    /// server).
    fn ai_chat_model(&self) -> String {
        match self.config.ai_provider {
            AiProvider::Api => self.config.ai_api_model.clone(),
            _ => self.config.ai_model.clone(),
        }
    }

    /// The embedding model setting of the active provider (empty: use the
    /// chat model, decision 43).
    fn ai_embedding_setting(&self) -> String {
        match self.config.ai_provider {
            AiProvider::Api => self.config.ai_api_embedding_model.clone(),
            _ => self.config.ai_embedding_model.clone(),
        }
    }

    fn ai_field_value(&self, field: AiField) -> String {
        match field {
            AiField::Endpoint => self.config.ai_endpoint.clone(),
            AiField::Model => self.config.ai_model.clone(),
            AiField::EmbeddingModel => self.config.ai_embedding_model.clone(),
            AiField::ApiBase => self.config.ai_api_base.clone(),
            AiField::ApiKey => self.state.ai_api_key.clone(),
            AiField::ApiModel => self.config.ai_api_model.clone(),
            AiField::ApiEmbeddingModel => self.config.ai_api_embedding_model.clone(),
        }
    }

    /// Switch provider (the only way the mode ever changes).
    pub(super) fn set_ai_provider(&mut self, provider: AiProvider, cx: &mut Context<Self>) {
        if let Some(settings) = &mut self.settings {
            settings.ai = AiSettings::default();
        }
        if self.config.ai_provider != provider {
            self.config.ai_provider = provider;
            self.save_config();
        }
        cx.notify();
    }

    /// Click on a field: edit it (saving another one being edited). The key
    /// field starts empty, so the stored key is never shown.
    fn start_ai_field(&mut self, field: AiField, window: &mut Window, cx: &mut Context<Self>) {
        if self.ai_settings_editing() {
            self.commit_ai_field(cx);
        }
        let text = if field == AiField::ApiKey {
            String::new()
        } else {
            self.ai_field_value(field)
        };
        if let Some(settings) = &mut self.settings {
            settings.ai.field = Some((field, EditorState::new(&text)));
        }
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    /// Enter: save the edited field (trimmed; an empty URL means its
    /// default, an empty key keeps the stored one).
    fn commit_ai_field(&mut self, cx: &mut Context<Self>) {
        let Some((field, editor)) = self.settings.as_mut().and_then(|s| s.ai.field.take()) else {
            return;
        };
        let text = editor.text.trim().to_string();
        match field {
            AiField::ApiKey => {
                if !text.is_empty() {
                    self.state.ai_api_key = text;
                    self.save_state();
                    let status = Status {
                        text: "API key saved (plaintext, in state.toml)".to_string(),
                        error: false,
                    };
                    self.show_status(status, cx);
                }
            }
            _ => {
                let config = &mut self.config;
                match field {
                    AiField::Endpoint if text.is_empty() => {
                        config.ai_endpoint = ai::DEFAULT_ENDPOINT.to_string()
                    }
                    AiField::Endpoint => config.ai_endpoint = text,
                    AiField::Model => config.ai_model = text,
                    AiField::EmbeddingModel => config.ai_embedding_model = text,
                    AiField::ApiBase if text.is_empty() => {
                        config.ai_api_base = DEFAULT_API_BASE.to_string()
                    }
                    AiField::ApiBase => config.ai_api_base = text,
                    AiField::ApiModel => config.ai_api_model = text,
                    AiField::ApiEmbeddingModel => config.ai_api_embedding_model = text,
                    AiField::ApiKey => {}
                }
                self.save_config();
            }
        }
        if let Some(settings) = &mut self.settings {
            // The model list may belong to the old server.
            if matches!(
                field,
                AiField::Endpoint | AiField::ApiBase | AiField::ApiKey
            ) {
                settings.ai.models = None;
            }
        }
        cx.notify();
    }

    fn clear_ai_key(&mut self, cx: &mut Context<Self>) {
        if self.state.ai_api_key.is_empty() {
            return;
        }
        self.state.ai_api_key.clear();
        self.save_state();
        if let Some(settings) = &mut self.settings {
            settings.ai.models = None;
        }
        cx.notify();
    }

    /// "Check connection": list the active server's models off the UI
    /// thread.
    pub(super) fn check_ai_connection(&mut self, cx: &mut Context<Self>) {
        let endpoint = self.ai_endpoint();
        let Some(settings) = &mut self.settings else {
            return;
        };
        let endpoint = match endpoint {
            Ok(endpoint) => endpoint,
            Err(err) => {
                settings.ai.models = Some(Err(err.to_string()));
                settings.ai.checking = false;
                settings.ai.check_task = None;
                cx.notify();
                return;
            }
        };
        settings.ai.checking = true;
        settings.ai.models = None;
        settings.ai.check_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { ai::list_models(&endpoint) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Some(settings) = &mut this.settings {
                    settings.ai.checking = false;
                    settings.ai.models = Some(result.map_err(|e| e.to_string()));
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// A listed model's "Chat" / "Embeddings" button.
    fn use_ai_model(&mut self, model: String, embeddings: bool, cx: &mut Context<Self>) {
        if embeddings && self.config.ai_provider == AiProvider::Api {
            self.config.ai_api_embedding_model = model;
        } else if embeddings {
            self.config.ai_embedding_model = model;
        } else if self.config.ai_provider == AiProvider::Api {
            self.config.ai_api_model = model;
        } else {
            self.config.ai_model = model;
        }
        self.save_config();
        cx.notify();
    }

    // --- ask my notes ---------------------------------------------------------

    /// "Ask my notes" (palette): open the panel over everything else. The
    /// history of this session is still there.
    pub(super) fn on_ask_my_notes(
        &mut self,
        _: &AskMyNotes,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.stop_edit(cx);
        self.close_search(cx);
        self.settings = None;
        self.page_menu = None;
        self.shortcuts_open = false;
        self.trash_confirm = None;
        self.semantic.open = false;
        self.ask.open = true;
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

    pub(super) fn close_ask(&mut self, cx: &mut Context<Self>) {
        self.ask.open = false;
        self.last_layout = None;
        self.last_bounds = None;
        cx.notify();
    }

    /// Enter in the panel: find the best blocks, then stream the model's
    /// answer into a new turn. A question while an answer is still
    /// streaming waits (Enter does nothing). With an embedding model set
    /// for the active mode, blocks are found by meaning (decision 43,
    /// indexing what's new first); without one, or if that fails, by
    /// keywords (`ai::retrieve`). That is a choice of retrieval method on
    /// the same provider, never a switch of provider.
    pub(super) fn ask_question(&mut self, cx: &mut Context<Self>) {
        let question = self.ask.input.text.trim().to_string();
        if question.is_empty() || self.ask.turns.last().is_some_and(|t| !t.done) {
            return;
        }
        self.ask.input = EditorState::default();
        let lexical: Vec<AskSource> = ai::retrieve(&self.pages, &question, ai::ASK_SOURCES)
            .into_iter()
            .map(|(p, b)| AskSource {
                title: self.pages[p].title.clone(),
                block: self.pages[p].blocks[b].id,
                text: self.pages[p].blocks[b].content.clone(),
            })
            .collect();
        let endpoint = self.ai_endpoint();
        let model = self.ai_chat_model();
        let semantic = !self.ai_embedding_setting().is_empty();
        self.ask.turns.push(AskTurn {
            question: question.clone(),
            answer: String::new(),
            sources: if semantic {
                Vec::new()
            } else {
                lexical.clone()
            },
            error: None,
            done: false,
            note: None,
        });
        let ix = self.ask.turns.len() - 1;
        let endpoint = match endpoint {
            Ok(endpoint) => endpoint,
            Err(err) => {
                self.ask.turns[ix].sources = lexical;
                self.finish_turn(ix, Some(err), cx);
                return;
            }
        };
        let job = semantic.then(|| self.semantic_job(endpoint.clone(), question.clone()));
        self.ask.task = Some(cx.spawn(async move |this, cx| {
            if let Some(job) = job {
                let found = semantic_ui::run_job(&this, cx, job, Progress::Ask(ix)).await;
                let ok = this.update(cx, |this, cx| {
                    let (sources, note) = match found {
                        Ok(hits) => (
                            hits.into_iter()
                                .take(ai::ASK_SOURCES)
                                .map(|hit| AskSource {
                                    title: hit.title,
                                    block: hit.block,
                                    text: hit.content,
                                })
                                .collect(),
                            "Sources found by meaning (embeddings)".to_string(),
                        ),
                        Err(err) => (
                            lexical,
                            format!("Sources found by keywords: semantic retrieval failed ({err})"),
                        ),
                    };
                    if let Some(turn) = this.ask.turns.get_mut(ix) {
                        turn.sources = sources;
                        turn.note = Some(note);
                    }
                    cx.notify();
                });
                if ok.is_err() {
                    return;
                }
            }
            let Ok(pairs) = this.update(cx, |this, _| {
                this.ask.turns.get(ix).map_or(Vec::new(), |turn| {
                    turn.sources
                        .iter()
                        .map(|s| (s.title.clone(), s.text.clone()))
                        .collect::<Vec<_>>()
                })
            }) else {
                return;
            };
            let messages = ai::ask_messages(&question, &pairs);
            let started = cx
                .background_spawn(async move {
                    let model = ai::chat_model(&endpoint, &model)?;
                    ai::chat_stream(&endpoint, &model, messages)
                })
                .await;
            let mut stream = match started {
                Ok(stream) => stream,
                Err(err) => {
                    let _ = this.update(cx, |this, cx| this.finish_turn(ix, Some(err), cx));
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
                        if let Some(turn) = this.ask.turns.get_mut(ix) {
                            turn.answer.push_str(&piece);
                        }
                        cx.notify();
                        true
                    }
                    Ok(None) => {
                        this.finish_turn(ix, None, cx);
                        false
                    }
                    Err(err) => {
                        this.finish_turn(ix, Some(err), cx);
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

    fn finish_turn(&mut self, ix: usize, error: Option<AiError>, cx: &mut Context<Self>) {
        if let Some(turn) = self.ask.turns.get_mut(ix) {
            turn.done = true;
            turn.error = error.map(|e| e.to_string());
        }
        self.ask.task = None;
        cx.notify();
    }

    /// Click on a source: close the panel and go to its block (as clicking
    /// a block reference does).
    fn open_ask_source(
        &mut self,
        turn: usize,
        source: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self
            .ask
            .turns
            .get(turn)
            .and_then(|t| t.sources.get(source))
            .map(|s| s.block)
        else {
            return;
        };
        if find_block(&self.pages, id).is_none() {
            let status = Status {
                text: "That block no longer exists".to_string(),
                error: true,
            };
            self.show_status(status, cx);
            return;
        }
        self.close_ask(cx);
        self.open_block_ref(id, window, cx);
    }

    // --- rendering ------------------------------------------------------------

    /// A one-line description of where questions go.
    fn ai_mode_line(&self) -> (String, bool) {
        match self.config.ai_provider {
            AiProvider::Off => ("AI is off".to_string(), true),
            AiProvider::Local => (format!("Local \u{b7} {}", self.config.ai_endpoint), false),
            AiProvider::Api => (
                format!(
                    "API key \u{b7} {} \u{b7} your notes are sent to this provider",
                    self.config.ai_api_base
                ),
                true,
            ),
        }
    }

    /// Settings > AI: provider selector, the active mode's fields, the API
    /// key warning, and "Check connection" with the server's models.
    pub(super) fn render_ai_settings(
        &self,
        state: &SettingsState,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = self.theme;
        let provider = self.config.ai_provider;
        let label = |text: &'static str| div().text_color(theme.muted).child(text);
        let button = |id: &'static str, text: SharedString, active: bool| {
            div()
                .id(id)
                .debug_selector(move || id.to_string())
                .px_3()
                .py_1()
                .rounded_md()
                .border_1()
                .border_color(if active { theme.accent } else { theme.border })
                .cursor_pointer()
                .text_color(if active { theme.accent } else { theme.text })
                .when(active, |d| d.bg(theme.selected_bg))
                .hover(|d| d.bg(theme.selected_bg))
                .child(text)
        };

        let mut providers = div().flex().flex_row().gap_2();
        for (id, p) in [
            ("ai-provider-local", AiProvider::Local),
            ("ai-provider-api", AiProvider::Api),
            ("ai-provider-off", AiProvider::Off),
        ] {
            providers =
                providers
                    .child(button(id, p.label().into(), provider == p).on_click(
                        cx.listener(move |this, _e, _w, cx| this.set_ai_provider(p, cx)),
                    ));
        }

        let field = |field: AiField, cx: &mut Context<Self>| -> AnyElement {
            let name = field.name();
            let editing = state.ai.field.as_ref().is_some_and(|(f, _)| *f == field);
            let value = self.ai_field_value(field);
            let input = if editing {
                div()
                    .debug_selector(move || format!("ai-input-{name}"))
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.accent)
                    .bg(theme.bg)
                    .child(BlockText { app: cx.entity() })
                    .into_any_element()
            } else {
                let (shown, muted) = if field == AiField::ApiKey {
                    if value.is_empty() {
                        (field.placeholder().to_string(), true)
                    } else {
                        // Masked: the key is never drawn.
                        ("\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022} (set)".to_string(), false)
                    }
                } else if value.is_empty() {
                    (field.placeholder().to_string(), true)
                } else {
                    (value, false)
                };
                div()
                    .id(name)
                    .debug_selector(move || format!("ai-field-{name}"))
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.bg)
                    .cursor_pointer()
                    .text_color(if muted { theme.muted } else { theme.text })
                    .child(shown)
                    .on_click(cx.listener(move |this, _e, window, cx| {
                        this.start_ai_field(field, window, cx)
                    }))
                    .into_any_element()
            };
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(label(field.label()))
                .child(input)
                .into_any_element()
        };

        let mut body = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(label("Provider"))
            .child(providers);

        match provider {
            AiProvider::Off => {
                body = body.child(
                    div()
                        .debug_selector(|| "ai-off-note".to_string())
                        .text_color(theme.muted)
                        .child("AI features are off. Nothing is sent anywhere. Pick Local or API key to use Ask my notes."),
                );
            }
            AiProvider::Local => {
                body = body
                    .child(
                        div()
                            .text_color(theme.muted)
                            .child("An OpenAI-compatible server on this computer (LM Studio, Ollama, llama.cpp, vLLM). Only localhost is allowed; your notes never leave this computer."),
                    )
                    .child(field(AiField::Endpoint, cx))
                    .child(field(AiField::Model, cx))
                    .child(field(AiField::EmbeddingModel, cx));
            }
            AiProvider::Api => {
                body = body
                    .child(
                        div()
                            .debug_selector(|| "ai-cloud-warning".to_string())
                            .p_2()
                            .rounded_md()
                            .border_1()
                            .border_color(theme.danger)
                            .text_color(theme.danger)
                            .font_weight(FontWeight::BOLD)
                            .child("Warning: in this mode the text of your notes (the blocks used to answer a question) is sent to the provider at the API base URL below. Only use a provider you trust with your notes."),
                    )
                    .child(field(AiField::ApiBase, cx))
                    .child(field(AiField::ApiKey, cx))
                    .when(!self.state.ai_api_key.is_empty(), |d| {
                        d.child(
                            div().flex().flex_row().child(
                                button("ai-clear-key", "Remove key".into(), false).on_click(
                                    cx.listener(|this, _e, _w, cx| this.clear_ai_key(cx)),
                                ),
                            ),
                        )
                    })
                    .child(
                        div()
                            .debug_selector(|| "ai-key-note".to_string())
                            .text_color(theme.muted)
                            .child("The key is stored in plaintext in state.toml in your graph folder. If git auto-backup is on, it is also committed to the graph's local git repository."),
                    )
                    .child(field(AiField::ApiModel, cx))
                    .child(field(AiField::ApiEmbeddingModel, cx));
            }
        }

        if provider != AiProvider::Off {
            // A setting the active mode can't use, said right away.
            if let Err(err) = self.ai_endpoint() {
                body = body.child(
                    div()
                        .debug_selector(|| "ai-endpoint-error".to_string())
                        .text_color(theme.danger)
                        .child(err.to_string()),
                );
            }
            let checking = state.ai.checking;
            body = body.child(
                div().flex().flex_row().child(
                    button(
                        "ai-check",
                        if checking {
                            "Checking\u{2026}".into()
                        } else {
                            "Check connection".into()
                        },
                        false,
                    )
                    .on_click(cx.listener(|this, _e, _w, cx| this.check_ai_connection(cx))),
                ),
            );
            match &state.ai.models {
                Some(Err(err)) => {
                    body = body.child(
                        div()
                            .debug_selector(|| "ai-check-error".to_string())
                            .text_color(theme.danger)
                            .child(err.clone()),
                    );
                }
                Some(Ok(models)) => {
                    let chat = self.ai_chat_model();
                    let embed = self.ai_embedding_setting();
                    body = body.child(
                        div()
                            .debug_selector(|| "ai-check-ok".to_string())
                            .text_color(theme.muted)
                            .child(if models.is_empty() {
                                "Connected, but the server lists no models".to_string()
                            } else {
                                format!("Connected: {} model(s)", models.len())
                            }),
                    );
                    for (i, model) in models.iter().enumerate() {
                        let (m1, m2) = (model.clone(), model.clone());
                        let small =
                            |id: ElementId, selector: String, text: &'static str, on: bool| {
                                div()
                                    .id(id)
                                    .debug_selector(move || selector)
                                    .px_2()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(if on { theme.accent } else { theme.border })
                                    .text_color(if on { theme.accent } else { theme.text })
                                    .cursor_pointer()
                                    .hover(|d| d.bg(theme.selected_bg))
                                    .child(text)
                            };
                        body = body.child(
                            div()
                                .debug_selector(move || format!("ai-model-{i}"))
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .child(model.clone()),
                                )
                                .child(
                                    small(
                                        ElementId::from(("ai-use-chat", i)),
                                        format!("ai-use-chat-{i}"),
                                        "Chat",
                                        *model == chat,
                                    )
                                    .on_click(cx.listener(
                                        move |this, _e, _w, cx| {
                                            this.use_ai_model(m1.clone(), false, cx)
                                        },
                                    )),
                                )
                                .child(
                                    small(
                                        ElementId::from(("ai-use-embed", i)),
                                        format!("ai-use-embed-{i}"),
                                        "Embeddings",
                                        *model == embed,
                                    )
                                    .on_click(cx.listener(
                                        move |this, _e, _w, cx| {
                                            this.use_ai_model(m2.clone(), true, cx)
                                        },
                                    )),
                                ),
                        );
                    }
                }
                None => {}
            }
            body = body.child(
                div()
                    .text_color(theme.muted)
                    .child("Click a field to edit it. Enter saves, Esc cancels."),
            );
        }
        body.into_any_element()
    }

    /// The Ask my notes panel: docked right over a dimmed backdrop (a click
    /// there, or Esc, closes it), the session's questions with streamed
    /// answers and their sources, and the input at the bottom.
    pub(super) fn render_ask(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let (mode, mode_warn) = self.ai_mode_line();
        let off = self.config.ai_provider == AiProvider::Off;

        let turns: Vec<AnyElement> = self
            .ask
            .turns
            .iter()
            .enumerate()
            .map(|(t, turn)| {
                let cited = turn.cited();
                let sources: Vec<AnyElement> = turn
                    .sources
                    .iter()
                    .enumerate()
                    .map(|(j, source)| {
                        let is_cited = cited.contains(&(j + 1));
                        let snippet: String = source.text.chars().take(SNIPPET_CHARS).collect();
                        let more = source.text.chars().count() > SNIPPET_CHARS;
                        div()
                            .id(ElementId::from(("ask-source", t * 1000 + j)))
                            .debug_selector(move || format!("ask-source-{t}-{j}"))
                            .flex()
                            .flex_col()
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .border_1()
                            .border_color(if is_cited { theme.accent } else { theme.border })
                            .when(is_cited, |d| d.bg(theme.selected_bg))
                            .cursor_pointer()
                            .hover(|d| d.bg(theme.selected_bg))
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_color(if is_cited {
                                                theme.accent
                                            } else {
                                                theme.muted
                                            })
                                            .child(format!("[{}]", j + 1)),
                                    )
                                    .child(
                                        div()
                                            .font_weight(FontWeight::BOLD)
                                            .child(source.title.clone()),
                                    )
                                    .when(is_cited, |d| {
                                        d.child(
                                            div()
                                                .debug_selector(move || {
                                                    format!("ask-cited-{t}-{j}")
                                                })
                                                .text_color(theme.accent)
                                                .child("cited"),
                                        )
                                    }),
                            )
                            .child(div().text_color(theme.muted).child(if more {
                                format!("{snippet}\u{2026}")
                            } else {
                                snippet
                            }))
                            .on_click(cx.listener(move |this, _e, window, cx| {
                                this.open_ask_source(t, j, window, cx)
                            }))
                            .into_any_element()
                    })
                    .collect();
                let thinking = !turn.done && turn.answer.is_empty();
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .debug_selector(move || format!("ask-question-{t}"))
                            .font_weight(FontWeight::BOLD)
                            .child(turn.question.clone()),
                    )
                    .when_some(turn.note.clone(), |d, note| {
                        d.child(
                            div()
                                .debug_selector(move || format!("ask-note-{t}"))
                                .text_color(theme.muted)
                                .child(note),
                        )
                    })
                    .when(thinking, |d| {
                        d.child(div().text_color(theme.muted).child("Thinking\u{2026}"))
                    })
                    .when(!turn.answer.is_empty(), |d| {
                        d.child(
                            div()
                                .debug_selector(move || format!("ask-answer-{t}"))
                                .child(turn.answer.clone()),
                        )
                    })
                    .when_some(turn.error.clone(), |d, error| {
                        d.child(
                            div()
                                .debug_selector(move || format!("ask-error-{t}"))
                                .text_color(theme.danger)
                                .child(error),
                        )
                    })
                    .when(!sources.is_empty(), |d| {
                        d.child(div().text_color(theme.muted).child("Sources"))
                            .children(sources)
                    })
                    .when(
                        turn.sources.is_empty() && turn.done && turn.error.is_none(),
                        |d| {
                            d.child(
                                div()
                                    .text_color(theme.muted)
                                    .child("No notes matched the question."),
                            )
                        },
                    )
                    .into_any_element()
            })
            .collect();
        let empty = turns.is_empty();

        div()
            .id("ask-backdrop")
            .debug_selector(|| "ask-backdrop".to_string())
            .absolute()
            .inset_0()
            .occlude()
            .bg(gpui::black().opacity(0.3))
            .flex()
            .flex_row()
            .justify_end()
            .on_click(cx.listener(|this, _e, _w, cx| this.close_ask(cx)))
            .child(
                div()
                    .id("ask-panel")
                    .debug_selector(|| "ask-panel".to_string())
                    .occlude()
                    .w(px(460.0))
                    .h_full()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_4()
                    .bg(theme.sidebar_bg)
                    .border_l_1()
                    .border_color(theme.border)
                    .shadow_lg()
                    .child(
                        div()
                            .flex_shrink_0()
                            .flex()
                            .flex_row()
                            .justify_between()
                            .child(div().font_weight(FontWeight::BOLD).child("Ask my notes"))
                            .child(div().text_color(theme.muted).child("Esc to close")),
                    )
                    .child(
                        div()
                            .debug_selector(|| "ask-mode".to_string())
                            .flex_shrink_0()
                            .text_color(if mode_warn { theme.danger } else { theme.muted })
                            .child(mode),
                    )
                    .when(off, |d| {
                        d.child(
                            div()
                                .debug_selector(|| "ask-off".to_string())
                                .flex_shrink_0()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(AiError::Off.to_string())
                                .child(
                                    div().flex().flex_row().child(
                                        div()
                                            .id("ask-open-settings")
                                            .debug_selector(|| "ask-open-settings".to_string())
                                            .px_3()
                                            .py_1()
                                            .rounded_md()
                                            .border_1()
                                            .border_color(theme.accent)
                                            .text_color(theme.accent)
                                            .cursor_pointer()
                                            .child("Open Settings > AI")
                                            .on_click(cx.listener(|this, _e, _w, cx| {
                                                this.show_settings_section(SettingsSection::Ai, cx)
                                            })),
                                    ),
                                ),
                        )
                    })
                    .child(
                        div()
                            .id("ask-history")
                            .debug_selector(|| "ask-history".to_string())
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .flex()
                            .flex_col()
                            .gap_4()
                            .when(empty, |d| {
                                d.child(div().text_color(theme.muted).child(
                                    "Ask a question about your notes. The best-matching blocks are sent to the model as context, and the answer cites them.",
                                ))
                            })
                            .children(turns),
                    )
                    .child(
                        div()
                            .debug_selector(|| "ask-input".to_string())
                            .flex_shrink_0()
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .border_1()
                            .border_color(theme.accent)
                            .bg(theme.bg)
                            // Only the editor with the keyboard is drawn
                            // live (a dialog over the panel takes it).
                            .map(|d| {
                                if self.ask_input_active() {
                                    d.child(BlockText { app: cx.entity() })
                                } else {
                                    d.child(self.ask.input.text.clone())
                                }
                            }),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_color(theme.muted)
                            .child("Enter to ask"),
                    ),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::test_server::{dead_endpoint, json, serve, stream};
    use gpui::{Modifiers, TestAppContext, VisualTestContext};
    use std::path::PathBuf;

    /// A window on a graph with `pages` (the first is selected), `config`
    /// and `state_toml` in place before it opens.
    pub(super) fn setup<'a>(
        cx: &'a mut TestAppContext,
        name: &str,
        pages: &[(&str, &str)],
        config: Config,
        state_toml: &str,
    ) -> (Entity<NoteSec>, &'a mut VisualTestContext, PathBuf) {
        let dir = std::env::temp_dir().join(format!("notesec-ai-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let storage = Storage::open(dir.clone()).unwrap();
        for (title, markdown) in pages {
            std::fs::write(dir.join(format!("pages/{title}.md")), markdown).unwrap();
        }
        std::fs::write(UiState::path(&dir), state_toml).unwrap();
        cx.update(bind_keys);
        let (view, cx) = cx.add_window_view(|window, cx| NoteSec::new(storage, config, window, cx));
        let first = pages[0].0.to_string();
        view.update(cx, |app, cx| {
            app.selected = app.find_page(&first).unwrap();
            app.tabs = Tabs::new(TabTarget::Page(first.clone()));
            cx.notify();
        });
        cx.run_until_parked();
        (view, cx, dir)
    }

    pub(super) fn local(url: &str) -> Config {
        Config {
            ai_provider: AiProvider::Local,
            ai_endpoint: url.to_string(),
            ai_model: "test-model".into(),
            ..Config::default()
        }
    }

    pub(super) fn has(cx: &mut VisualTestContext, selector: &str) -> bool {
        let selector: &'static str = Box::leak(selector.to_string().into_boxed_str());
        cx.debug_bounds(selector).is_some()
    }

    pub(super) fn click_on(cx: &mut VisualTestContext, selector: &str) {
        let selector: &'static str = Box::leak(selector.to_string().into_boxed_str());
        let bounds = cx.debug_bounds(selector).expect(selector);
        cx.simulate_click(bounds.center(), Modifiers::none());
    }

    pub(super) fn open_ask(cx: &mut VisualTestContext) {
        cx.simulate_keystrokes("ctrl-k");
        cx.simulate_input("ask my notes");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
    }

    pub(super) fn ask(cx: &mut VisualTestContext, question: &str) {
        cx.simulate_input(question);
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
    }

    pub(super) fn turns(view: &Entity<NoteSec>, cx: &mut VisualTestContext) -> Vec<AskTurn> {
        view.update(cx, |app, _| app.ask.turns.clone())
    }

    pub(super) const PAGES: &[(&str, &str)] = &[
        ("Test", "- hello\n- weather today\n"),
        ("Rust", "- ownership rules every value\n- the book\n"),
        ("Diary", "- read about rust today\n"),
    ];

    #[gpui::test]
    fn the_palette_opens_ask_my_notes_and_esc_keeps_the_history(cx: &mut TestAppContext) {
        let (ep, _) = serve(vec![stream(&["Done."])]);
        let (view, cx, dir) = setup(cx, "open", PAGES, local(ep.url()), "");
        open_ask(cx);
        assert!(view.update(cx, |app, _| app.ask.open));
        assert!(has(cx, "ask-panel") && has(cx, "ask-input"));
        assert!(view.update(cx, |app, _| app.text_input_open()));
        ask(cx, "rust ownership");
        assert_eq!(turns(&view, cx).len(), 1);
        cx.simulate_keystrokes("escape");
        assert!(!view.update(cx, |app, _| app.ask.open));
        assert!(!has(cx, "ask-panel"));
        open_ask(cx);
        assert_eq!(turns(&view, cx).len(), 1, "history kept for the session");
        assert!(has(cx, "ask-question-0"));
        // A click on the backdrop (left of the panel) closes it too.
        let backdrop = cx.debug_bounds("ask-backdrop").unwrap();
        cx.simulate_click(
            gpui::point(backdrop.origin.x + px(20.), backdrop.center().y),
            Modifiers::none(),
        );
        assert!(!view.update(cx, |app, _| app.ask.open));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn asking_streams_the_answer_and_lists_the_cited_sources(cx: &mut TestAppContext) {
        let (ep, requests) = serve(vec![stream(&["Every value ", "has an owner [1]", "."])]);
        let (view, cx, dir) = setup(cx, "stream", PAGES, local(ep.url()), "");
        open_ask(cx);
        ask(cx, "How does Rust ownership work?");
        let turn = &turns(&view, cx)[0];
        assert_eq!(turn.answer, "Every value has an owner [1].");
        assert!(turn.done && turn.error.is_none());
        assert_eq!(turn.sources[0].title, "Rust", "best match first");
        assert!(turn.sources.iter().any(|s| s.title == "Diary"));
        assert!(!turn.sources.iter().any(|s| s.text == "weather today"));
        assert_eq!(turn.cited(), vec![1]);
        assert!(has(cx, "ask-answer-0"));
        assert!(has(cx, "ask-cited-0-0"));
        assert!(!has(cx, "ask-cited-0-1"), "only cited sources are marked");
        assert!(view.update(cx, |app, _| app.ask.input.text.is_empty()));
        let requests = requests.lock().unwrap();
        assert_eq!(requests[0].0, "POST /v1/chat/completions HTTP/1.1");
        assert_eq!(requests[0].1, None, "local mode sends no key");
        let body: serde_json::Value = serde_json::from_str(&requests[0].2).unwrap();
        assert_eq!(body["model"], "test-model");
        let prompt = body["messages"][1]["content"].as_str().unwrap();
        assert!(prompt.contains("[1] (page \"Rust\")\nownership rules every value"));
        assert!(prompt.ends_with("Question: How does Rust ownership work?"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn clicking_a_source_opens_its_page_at_the_block(cx: &mut TestAppContext) {
        let (ep, _) = serve(vec![stream(&["See [2]."])]);
        let (view, cx, dir) = setup(cx, "source", PAGES, local(ep.url()), "");
        open_ask(cx);
        ask(cx, "rust ownership");
        let target = turns(&view, cx)[0].sources[1].clone();
        click_on(cx, "ask-source-0-1");
        view.update(cx, |app, _| {
            assert!(!app.ask.open);
            let page = &app.pages[app.selected];
            assert_eq!(page.title, target.title);
            let ix = page.blocks.iter().position(|b| b.id == target.block);
            assert_eq!(app.editing, ix);
            assert!(ix.is_some());
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn off_mode_says_where_to_turn_ai_on_and_sends_nothing(cx: &mut TestAppContext) {
        let config = Config {
            ai_provider: AiProvider::Off,
            ..Config::default()
        };
        let (view, cx, dir) = setup(cx, "off", PAGES, config, "");
        open_ask(cx);
        assert!(has(cx, "ask-off"));
        let status = view.update(cx, |app, _| app.status.clone()).unwrap();
        assert!(status.error && status.text.contains("Settings > AI"));
        ask(cx, "rust ownership");
        let turn = &turns(&view, cx)[0];
        assert!(turn.done);
        assert_eq!(
            turn.error.as_deref(),
            Some(AiError::Off.to_string().as_str())
        );
        click_on(cx, "ask-open-settings");
        view.update(cx, |app, _| {
            assert_eq!(
                app.settings.as_ref().map(|s| s.section),
                Some(SettingsSection::Ai)
            )
        });
        assert!(has(cx, "ai-off-note"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn an_unreachable_server_is_named_and_nothing_falls_back(cx: &mut TestAppContext) {
        let dead = dead_endpoint();
        // A key is stored, but Local mode must never use the API instead.
        let (view, cx, dir) = setup(
            cx,
            "dead",
            PAGES,
            local(dead.url()),
            "ai_api_key = \"sk-never-used\"\n",
        );
        open_ask(cx);
        ask(cx, "rust ownership");
        let turn = &turns(&view, cx)[0];
        let error = turn.error.clone().unwrap();
        assert!(error.contains("Can't reach the model server"), "{error}");
        assert!(error.contains(dead.url()), "{error}");
        assert!(!error.contains("sk-"));
        assert!(has(cx, "ask-error-0"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn api_key_mode_sends_the_key_as_a_bearer_token(cx: &mut TestAppContext) {
        let (ep, requests) = serve(vec![stream(&["Yes [1]."])]);
        let config = Config {
            ai_provider: AiProvider::Api,
            ai_api_base: ep.url().to_string(),
            ai_api_model: "cloud-model".into(),
            ..Config::default()
        };
        let (view, cx, dir) = setup(cx, "api", PAGES, config, "ai_api_key = \"sk-test\"\n");
        open_ask(cx);
        ask(cx, "rust ownership");
        assert_eq!(turns(&view, cx)[0].answer, "Yes [1].");
        let requests = requests.lock().unwrap();
        assert_eq!(requests[0].1.as_deref(), Some("Bearer sk-test"));
        let body: serde_json::Value = serde_json::from_str(&requests[0].2).unwrap();
        assert_eq!(body["model"], "cloud-model");
        let _ = std::fs::remove_dir_all(dir);
    }

    fn open_ai_settings(view: &Entity<NoteSec>, cx: &mut VisualTestContext) {
        cx.simulate_keystrokes("ctrl-,");
        click_on(cx, "settings-tab-ai");
        view.update(cx, |app, _| {
            assert_eq!(
                app.settings.as_ref().map(|s| s.section),
                Some(SettingsSection::Ai)
            )
        });
    }

    fn editing_field(view: &Entity<NoteSec>, cx: &mut VisualTestContext) -> Option<AiField> {
        view.update(cx, |app, _| {
            app.settings
                .as_ref()
                .and_then(|s| s.ai.field.as_ref().map(|(f, _)| *f))
        })
    }

    #[gpui::test]
    fn settings_fields_edit_save_on_enter_and_cancel_on_escape(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "fields", PAGES, Config::default(), "");
        open_ai_settings(&view, cx);
        assert!(has(cx, "ai-provider-local"), "local is the default");
        assert!(has(cx, "ai-field-endpoint") && !has(cx, "ai-cloud-warning"));

        click_on(cx, "ai-field-endpoint");
        assert_eq!(editing_field(&view, cx), Some(AiField::Endpoint));
        assert!(has(cx, "ai-input-endpoint"));
        // The field types: the default URL is there, cursor at its end.
        for _ in 0..ai::DEFAULT_ENDPOINT.len() {
            cx.simulate_keystrokes("backspace");
        }
        cx.simulate_input("http://127.0.0.1:8080/v1");
        cx.simulate_keystrokes("enter");
        assert_eq!(editing_field(&view, cx), None);
        assert_eq!(
            Config::load(&dir).ai_endpoint,
            "http://127.0.0.1:8080/v1",
            "saved to config.toml"
        );
        assert!(view.update(cx, |app, _| app.settings.is_some()));

        click_on(cx, "ai-field-model");
        cx.simulate_input("typed");
        cx.simulate_keystrokes("escape");
        assert_eq!(editing_field(&view, cx), None, "Esc cancels the field");
        assert!(
            view.update(cx, |app, _| app.settings.is_some()),
            "…not settings"
        );
        assert_eq!(view.update(cx, |app, _| app.config.ai_model.clone()), "");
        cx.simulate_keystrokes("escape");
        assert!(view.update(cx, |app, _| app.settings.is_none()));

        // A remote host in Local mode is refused right away.
        open_ai_settings(&view, cx);
        click_on(cx, "ai-field-endpoint");
        for _ in 0.."http://127.0.0.1:8080/v1".len() {
            cx.simulate_keystrokes("backspace");
        }
        cx.simulate_input("http://192.168.1.5:1234/v1");
        cx.simulate_keystrokes("enter");
        assert!(has(cx, "ai-endpoint-error"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn switching_providers_shows_each_modes_fields_and_stores_the_key(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "providers", PAGES, Config::default(), "");
        open_ai_settings(&view, cx);
        click_on(cx, "ai-provider-api");
        assert_eq!(Config::load(&dir).ai_provider, AiProvider::Api);
        assert!(has(cx, "ai-cloud-warning") && has(cx, "ai-key-note"));
        assert!(has(cx, "ai-field-api-base") && has(cx, "ai-field-api-key"));
        assert!(!has(cx, "ai-field-endpoint"));
        assert!(has(cx, "ai-endpoint-error"), "no key yet");

        click_on(cx, "ai-field-api-key");
        let start = view.update(cx, |app, _| app.active_editor().text.clone());
        assert_eq!(start, "", "the stored key is never put in the field");
        cx.simulate_input("sk-secret");
        cx.simulate_keystrokes("enter");
        assert_eq!(UiState::load(&dir).ai_api_key, "sk-secret");
        assert!(!std::fs::read_to_string(Config::path(&dir))
            .unwrap()
            .contains("sk-secret"));
        assert!(!has(cx, "ai-endpoint-error"));
        click_on(cx, "ai-clear-key");
        assert_eq!(UiState::load(&dir).ai_api_key, "");

        click_on(cx, "ai-provider-off");
        assert_eq!(Config::load(&dir).ai_provider, AiProvider::Off);
        assert!(has(cx, "ai-off-note") && !has(cx, "ai-check"));
        click_on(cx, "ai-provider-local");
        assert_eq!(Config::load(&dir).ai_provider, AiProvider::Local);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn check_connection_lists_models_to_pick_from(cx: &mut TestAppContext) {
        let models = r#"{"data":[{"id":"nomic-embed-text"},{"id":"qwen2.5-7b"}]}"#;
        let (ep, _) = serve(vec![json(200, models)]);
        let config = Config {
            ai_endpoint: ep.url().to_string(),
            ..Config::default()
        };
        let (view, cx, dir) = setup(cx, "check", PAGES, config, "");
        open_ai_settings(&view, cx);
        click_on(cx, "ai-check");
        cx.run_until_parked();
        assert!(has(cx, "ai-check-ok"));
        assert!(has(cx, "ai-model-0") && has(cx, "ai-model-1"));
        click_on(cx, "ai-use-chat-1");
        click_on(cx, "ai-use-embed-0");
        let saved = Config::load(&dir);
        assert_eq!(saved.ai_model, "qwen2.5-7b");
        assert_eq!(saved.ai_embedding_model, "nomic-embed-text");
        // Nothing listens any more: the failure is shown.
        click_on(cx, "ai-check");
        cx.run_until_parked();
        assert!(has(cx, "ai-check-error"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
