//! The web clipper in the app (decision 51): starting and stopping the
//! listener as the setting changes, saving the clips it receives (on the
//! UI thread, through the normal storage path), the "Clipped" status with
//! its Open button, and Settings > Web clipper.

use std::sync::mpsc;
use std::time::Duration;

use gpui::{div, prelude::*, px, AnyElement, ClipboardItem, Context, Task};

use super::{create_journal, NoteSec, Status};
use crate::clipper::{self, Clip, Delivery, Server};
use crate::model::Block;
use crate::storage::{today_title, validate_title};

/// How often the UI thread looks for clips while the clipper is on.
const POLL: Duration = Duration::from_millis(200);

#[derive(Default)]
pub(super) struct ClipperState {
    server: Option<Server>,
    clips: Option<mpsc::Receiver<Delivery>>,
    poll: Option<Task<()>>,
    /// Why the listener isn't running although it is turned on.
    error: Option<String>,
    /// The clipped page the status message is about, and that message.
    last: Option<(String, String)>,
}

impl NoteSec {
    /// The token, made (and saved) on first use.
    fn clipper_token(&mut self) -> String {
        if self.state.clipper_token.is_empty() {
            self.state.clipper_token = clipper::new_token();
            self.save_state();
        }
        self.state.clipper_token.clone()
    }

    /// The port it listens on (the configured one until it runs).
    pub(super) fn clipper_port(&self) -> u16 {
        self.clipper
            .server
            .as_ref()
            .map_or(self.config.clipper_port, Server::port)
    }

    /// The port it listens on, if it does.
    #[cfg(test)]
    pub(super) fn clipper_listening(&self) -> Option<u16> {
        self.clipper.server.as_ref().map(Server::port)
    }

    /// (Re)start the listener with the current port and token. A failure
    /// (port in use) is shown, not fatal.
    pub(super) fn start_clipper(&mut self, cx: &mut Context<Self>) {
        self.stop_clipper();
        let token = self.clipper_token();
        let port = self.config.clipper_port;
        let (tx, rx) = mpsc::channel();
        match Server::start(port, token, tx) {
            Ok(server) => {
                self.clipper.server = Some(server);
                self.clipper.clips = Some(rx);
                self.clipper.error = None;
                self.clipper.poll = Some(cx.spawn(async move |this, cx| loop {
                    cx.background_executor().timer(POLL).await;
                    match this.update(cx, |this, cx| this.take_clips(cx)) {
                        Ok(true) => {}
                        _ => break,
                    }
                }));
            }
            Err(err) => {
                let text = if err.kind() == std::io::ErrorKind::AddrInUse {
                    format!("Web clipper: port {port} is in use; pick another in Settings")
                } else {
                    format!("Web clipper could not listen on 127.0.0.1:{port}: {err}")
                };
                self.clipper.error = Some(text.clone());
                self.show_status(Status { text, error: true }, cx);
            }
        }
        cx.notify();
    }

    /// Close the port (requests being answered get "NoteSec is closing").
    pub(super) fn stop_clipper(&mut self) {
        if let Some(mut server) = self.clipper.server.take() {
            server.stop();
        }
        self.clipper.clips = None;
        self.clipper.poll = None;
    }

    pub(super) fn set_web_clipper(&mut self, on: bool, cx: &mut Context<Self>) {
        self.config.web_clipper = on;
        self.save_config();
        if on {
            self.start_clipper(cx);
        } else {
            self.stop_clipper();
            self.clipper.error = None;
        }
        cx.notify();
    }

    /// The port buttons: `Some(step)` up or down, `None` the default.
    fn change_clipper_port(&mut self, step: Option<i32>, cx: &mut Context<Self>) {
        let port = match step {
            Some(step) => (i32::from(self.config.clipper_port) + step).clamp(1024, 65535) as u16,
            None => clipper::DEFAULT_PORT,
        };
        self.config.clipper_port = port;
        self.save_config();
        if self.config.web_clipper {
            self.start_clipper(cx);
        }
        cx.notify();
    }

    /// A new token: the old one (and bookmarklets made with it) stops
    /// working at once.
    fn regenerate_clipper_token(&mut self, cx: &mut Context<Self>) {
        self.state.clipper_token = clipper::new_token();
        self.save_state();
        if self.config.web_clipper {
            self.start_clipper(cx);
        }
        let text = "New clipper token: copy the bookmarklet again".to_string();
        self.show_status(Status { text, error: false }, cx);
    }

    fn copy_clipper_text(&mut self, text: String, what: &str, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        let text = format!("Copied the {what}");
        self.show_status(Status { text, error: false }, cx);
    }

    /// Save the clips that arrived. False once the clipper is off (the
    /// polling stops).
    fn take_clips(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(rx) = &self.clipper.clips else {
            return false;
        };
        let arrived: Vec<Delivery> = rx.try_iter().collect();
        for delivery in arrived {
            let result = self.save_clip(&delivery.clip, cx);
            // The connection may have given up waiting: nothing to tell.
            let _ = delivery.reply.send(result);
        }
        true
    }

    /// Save `clip` as a new page, link it from today's journal (the
    /// inbox), and say so. Returns the page's title.
    pub(super) fn save_clip(
        &mut self,
        clip: &Clip,
        cx: &mut Context<Self>,
    ) -> Result<String, String> {
        // The block being edited is saved first; the clip is a new page.
        self.stop_edit(cx);
        let title = clipper::unique_title(&clip.title, |t| self.find_page(t).is_some());
        let title = validate_title(&title)?;
        let day = today_title();
        let page = clipper::clip_page(clip, &title, &day);
        self.storage
            .save(&page)
            .map_err(|err| format!("could not save the page: {err}"))?;
        self.add_page(page);

        // The inbox: "Clipped [[Title]]" at the end of today's journal.
        if self.find_journal(&day).is_none() {
            let journal = create_journal(&self.storage, &day);
            self.add_page(journal);
        }
        if let Some(ix) = self.find_journal(&day) {
            let journal = &mut self.pages[ix];
            let line = format!("Clipped [[{title}]]");
            if journal.blocks.len() == 1 && journal.blocks[0].content.trim().is_empty() {
                journal.blocks[0].content = line;
            } else {
                let order = journal
                    .blocks
                    .iter()
                    .filter(|b| b.parent_id.is_none())
                    .count();
                journal.blocks.push(Block {
                    id: uuid::Uuid::new_v4(),
                    content: line,
                    parent_id: None,
                    page_id: journal.id.clone(),
                    order,
                });
            }
            if let Err(err) = self.storage.save(&self.pages[ix]) {
                eprintln!("notesec: could not add the clip to {day}: {err}");
            }
        }
        // Undo snapshots hold every page: one from before the clip would
        // drop its page (as after an import or a restore).
        self.forget_history();
        let text = format!("Clipped \u{201c}{title}\u{201d}");
        self.clipper.last = Some((title.clone(), text.clone()));
        self.show_status(Status { text, error: false }, cx);
        Ok(title)
    }

    /// The clipped page, while the status message is about it.
    fn clip_shown(&self) -> Option<&String> {
        let (title, text) = self.clipper.last.as_ref()?;
        (self.status.as_ref()?.text == *text).then_some(title)
    }

    /// "Open", inside the status message after a clip.
    pub(super) fn render_clipper_actions(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.clip_shown()?;
        let theme = self.theme;
        Some(
            div()
                .flex()
                .flex_row()
                .mt_1()
                .child(
                    div()
                        .id("clipper-open")
                        .debug_selector(|| "clipper-open".to_string())
                        .px_2()
                        .rounded_md()
                        .border_1()
                        .border_color(theme.border)
                        .cursor_pointer()
                        .text_color(theme.accent)
                        .hover(|d| d.bg(theme.selected_bg))
                        .child("Open")
                        .on_click(cx.listener(|this, _e, _w, cx| {
                            if let Some(title) = this.clip_shown().cloned() {
                                this.open_page(&title, cx);
                            }
                        })),
                )
                .into_any_element(),
        )
    }

    /// Settings > Web clipper.
    pub(super) fn render_clipper_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let on = self.config.web_clipper;
        let port = self.clipper_port();
        let token = self.state.clipper_token.clone();
        let button = |id: &'static str, label: &'static str, active: bool| {
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
                .child(label)
        };
        let label = |text: &'static str| div().text_color(theme.muted).child(text);
        let status = match (&self.clipper.server, &self.clipper.error) {
            (Some(server), _) => (format!("Listening on 127.0.0.1:{}", server.port()), false),
            (None, Some(err)) => (err.clone(), true),
            (None, None) => ("Off".to_string(), false),
        };
        let masked = if token.is_empty() {
            "(made when you turn it on)".to_string()
        } else {
            format!(
                "\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}{}",
                &token[token.len().saturating_sub(4)..]
            )
        };
        div()
            .debug_selector(|| "settings-clipper".to_string())
            .flex()
            .flex_col()
            .gap_2()
            .child(label("Web clipper"))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(
                        button("clipper-off", "Off", !on).on_click(
                            cx.listener(|this, _e, _w, cx| this.set_web_clipper(false, cx)),
                        ),
                    )
                    .child(
                        button("clipper-on", "On", on).on_click(
                            cx.listener(|this, _e, _w, cx| this.set_web_clipper(true, cx)),
                        ),
                    ),
            )
            .child(
                div()
                    .debug_selector(|| "clipper-status".to_string())
                    .text_color(if status.1 { theme.danger } else { theme.text })
                    .child(status.0),
            )
            .child(label("Port (on 127.0.0.1 only)"))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .debug_selector(|| "clipper-port".to_string())
                            .min_w(px(64.0))
                            .child(port.to_string()),
                    )
                    .child(button("clipper-port-down", "\u{2212}", false).on_click(
                        cx.listener(|this, _e, _w, cx| this.change_clipper_port(Some(-1), cx)),
                    ))
                    .child(button("clipper-port-up", "+", false).on_click(
                        cx.listener(|this, _e, _w, cx| this.change_clipper_port(Some(1), cx)),
                    ))
                    .child(button("clipper-port-reset", "Default", false).on_click(
                        cx.listener(|this, _e, _w, cx| this.change_clipper_port(None, cx)),
                    )),
            )
            .child(label("Token (the bookmarklet carries it)"))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .debug_selector(|| "clipper-token".to_string())
                            .min_w(px(120.0))
                            .child(masked),
                    )
                    .when(!token.is_empty(), |d| {
                        d.child(
                            button("clipper-copy-token", "Copy", false).on_click(cx.listener(
                                |this, _e, _w, cx| {
                                    let token = this.state.clipper_token.clone();
                                    this.copy_clipper_text(token, "clipper token", cx)
                                },
                            )),
                        )
                    })
                    .child(button("clipper-regenerate", "Regenerate", false).on_click(
                        cx.listener(|this, _e, _w, cx| this.regenerate_clipper_token(cx)),
                    )),
            )
            .child(div().flex().flex_row().child(
                button("clipper-copy-bookmarklet", "Copy bookmarklet", false).on_click(
                    cx.listener(|this, _e, _w, cx| {
                        let token = this.clipper_token();
                        let bookmarklet = clipper::bookmarklet(this.clipper_port(), &token);
                        this.copy_clipper_text(
                            bookmarklet,
                            "bookmarklet: add it as a bookmark's address",
                            cx,
                        );
                        cx.notify();
                    }),
                ),
            ))
            .child(div().text_color(theme.muted).child(
                "Clips become pages tagged #clipped, linked from today's journal. \
                 Anything on this computer that knows the token can add pages; \
                 regenerate it if it leaks. See docs/WEB_CLIPPER.md.",
            ))
            .into_any_element()
    }
}
