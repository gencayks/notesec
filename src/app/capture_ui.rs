//! Quick capture in the app (design pass, Feature 3): a small box that
//! appends its text to today's journal.
//!
//! The box reuses the shared text-input plumbing: while it is open it is
//! the active editor (`NoteSec::active_editor`), so typing, IME, paste and
//! cursor movement work exactly as in the palette's query box, and the root
//! view's "BlockEditor" key context delivers Enter/Escape to it. No new GPUI
//! APIs: `div`, `occlude`, `absolute` and `EditorState` are the same ones
//! the search overlay uses.

use gpui::{div, prelude::*, px, AnyElement, Context, Window};

use super::{NoteSec, QuickCapture, Status};
use crate::capture::push_block;
use crate::editor::EditorState;
use crate::storage::today_title;

impl NoteSec {
    /// Open the capture box, or close it if it is open (the `QuickCapture`
    /// action / command). Other overlays are closed first, like the palette.
    pub(super) fn on_quick_capture(
        &mut self,
        _: &QuickCapture,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.vault_dialog_open() {
            return;
        }
        if self.capture.is_some() {
            self.close_quick_capture(cx);
            return;
        }
        self.stop_edit(cx);
        self.search = None;
        self.settings = None;
        self.page_menu = None;
        self.shortcuts_open = false;
        self.trash_confirm = None;
        self.capture = Some(EditorState::default());
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    /// Close the capture box without capturing.
    pub(super) fn close_quick_capture(&mut self, cx: &mut Context<Self>) {
        self.capture = None;
        self.last_layout = None;
        self.last_bounds = None;
        cx.notify();
    }

    /// Enter in the capture box: append its text to today's journal.
    pub(super) fn confirm_quick_capture(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.capture.take() else {
            return;
        };
        let content = editor.text.trim().to_string();
        if content.is_empty() {
            self.capture = None;
            self.show_status(
                Status {
                    text: "Nothing to capture".to_string(),
                    error: true,
                },
                cx,
            );
            return;
        }
        self.stop_edit(cx);
        let today = today_title();
        if self.find_journal(&today).is_none() {
            let journal = super::create_journal(&self.storage, &today);
            self.add_page(journal);
        }
        if let Some(ix) = self.find_journal(&today) {
            push_block(&mut self.pages[ix], content);
            if let Err(err) = self.storage.save(&self.pages[ix]) {
                eprintln!("notesec: could not capture to {today}: {err}");
            }
        }
        // One capture is one undo step boundary, like a clip.
        self.forget_history();
        self.capture = None;
        self.last_layout = None;
        self.last_bounds = None;
        self.show_status(
            Status {
                text: format!("Captured to {today}"),
                error: false,
            },
            cx,
        );
    }

    /// The capture overlay: a dimmed backdrop (click closes) with a small
    /// centered box. The box shows the capture editor through `BlockText`
    /// (it draws the active editor, which is the capture box while open).
    pub(super) fn render_capture_overlay(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.capture.as_ref()?;
        let theme = self.theme;
        let today = today_title();
        Some(
            div()
                .id("capture-backdrop")
                .debug_selector(|| "capture-backdrop".to_string())
                .absolute()
                .inset_0()
                .occlude()
                .bg(theme.scrim)
                .flex()
                .flex_col()
                .items_center()
                .pt(px(120.0))
                .on_click(cx.listener(|this, _e, _window, cx| this.close_quick_capture(cx)))
                .child(
                    div()
                        .id("capture-panel")
                        .debug_selector(|| "capture-panel".to_string())
                        .occlude()
                        .w(px(480.0))
                        .flex()
                        .flex_col()
                        .gap_1()
                        .p_2()
                        .rounded_lg()
                        .bg(theme.sidebar_bg)
                        .border_1()
                        .border_color(theme.border)
                        .shadow_lg()
                        .child(
                            div()
                                .px_3()
                                .pt_1()
                                .text_color(theme.muted)
                                .child(format!("Quick capture — {today}")),
                        )
                        .child(
                            div()
                                .id("capture-input")
                                .debug_selector(|| "capture-input".to_string())
                                .px_3()
                                .py_2()
                                .rounded_md()
                                .bg(theme.bg)
                                .child(super::BlockText { app: cx.entity() }),
                        )
                        .child(
                            div()
                                .px_3()
                                .pb_1()
                                .text_color(theme.muted)
                                .child("Enter to capture, Esc to cancel"),
                        ),
                )
                .into_any_element(),
        )
    }
}
