//! "Publish page" in the app (decision 49): runs `publish.rs` on the page on
//! screen, off the UI thread (re-encoding photos takes a moment), and says
//! where the bundle is, with "Open folder" and "Copy path" buttons in the
//! status message.

use std::path::PathBuf;

use gpui::{div, prelude::*, AnyElement, ClipboardItem, Context, Task, Window};

use super::{NoteSec, PublishPage, PublishPageWithLinks, Status};

/// The last publish, for the status message's buttons.
#[derive(Default)]
pub(super) struct PublishState {
    /// The bundle folder and the status text that announced it (the
    /// buttons show only while that status is on screen).
    last: Option<(PathBuf, String)>,
    /// The publish running in the background; another waits for it.
    task: Option<Task<()>>,
    /// Folders "Open folder" handed to the system (tests can't open one).
    #[cfg(test)]
    pub(super) opened: Vec<PathBuf>,
}

impl NoteSec {
    pub(super) fn on_publish_page(
        &mut self,
        _: &PublishPage,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.current_page().is_some() {
            self.publish_page(false, cx);
        }
    }

    pub(super) fn on_publish_page_with_links(
        &mut self,
        _: &PublishPageWithLinks,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.current_page().is_some() {
            self.publish_page(true, cx);
        }
    }

    /// Publish the page on screen (and, `with_linked`, the pages it links
    /// to) to `<graph>/published/<slug>/`, after saving the block being
    /// edited, and say where. The work runs on a background thread over a
    /// copy of the pages (as they are now); one publish at a time.
    fn publish_page(&mut self, with_linked: bool, cx: &mut Context<Self>) {
        if self.publish.task.is_some() {
            let text = "Still publishing; try again in a moment".to_string();
            self.show_status(Status { text, error: true }, cx);
            return;
        }
        self.stop_edit(cx);
        let title = self.pages[self.selected].title.clone();
        let pages = self.pages.clone();
        let root = self.storage.root().to_path_buf();
        let main = self.selected;
        self.publish.last = None;
        let text = format!("Publishing \u{201c}{title}\u{201d}\u{2026}");
        self.show_status(Status { text, error: false }, cx);
        self.publish.task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    crate::publish::publish(&root, &pages, main, with_linked)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.publish.task = None;
                this.publish_done(&title, result, cx);
            });
        }));
    }

    fn publish_done(
        &mut self,
        title: &str,
        result: Result<crate::publish::Published, String>,
        cx: &mut Context<Self>,
    ) {
        let status = match result {
            Ok(published) => {
                let others = published.pages - 1;
                let with = match others {
                    0 => String::new(),
                    1 => " and 1 linked page".to_string(),
                    n => format!(" and {n} linked pages"),
                };
                let text = format!(
                    "Published \u{201c}{title}\u{201d}{with} to {}",
                    published.dir.display()
                );
                self.publish.last = Some((published.dir, text.clone()));
                Status { text, error: false }
            }
            Err(text) => {
                self.publish.last = None;
                Status { text, error: true }
            }
        };
        self.show_status(status, cx);
    }

    /// The published folder, while the status message is about it.
    fn published_shown(&self) -> Option<&PathBuf> {
        let (dir, text) = self.publish.last.as_ref()?;
        (self.status.as_ref()?.text == *text).then_some(dir)
    }

    fn open_published(&mut self, cx: &mut Context<Self>) {
        let Some(dir) = self.published_shown().cloned() else {
            return;
        };
        #[cfg(not(test))]
        cx.open_with_system(&dir);
        #[cfg(test)]
        self.publish.opened.push(dir);
        cx.notify();
    }

    fn copy_published_path(&mut self, cx: &mut Context<Self>) {
        if let Some(dir) = self.published_shown() {
            cx.write_to_clipboard(ClipboardItem::new_string(dir.display().to_string()));
        }
    }

    /// "Open folder" and "Copy path", inside the status message after a
    /// publish.
    pub(super) fn render_publish_actions(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.published_shown()?;
        let theme = self.theme;
        let button = |id: &'static str, label: &'static str| {
            div()
                .id(id)
                .debug_selector(move || id.to_string())
                .px_2()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .cursor_pointer()
                .text_color(theme.accent)
                .hover(|d| d.bg(theme.selected_bg))
                .child(label)
        };
        Some(
            div()
                .flex()
                .flex_row()
                .gap_2()
                .mt_1()
                .child(
                    button("publish-open-folder", "Open folder")
                        .on_click(cx.listener(|this, _e, _w, cx| this.open_published(cx))),
                )
                .child(
                    button("publish-copy-path", "Copy path")
                        .on_click(cx.listener(|this, _e, _w, cx| this.copy_published_path(cx))),
                )
                .into_any_element(),
        )
    }
}
