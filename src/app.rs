//! The root view: sidebar of pages on the left, selected page on the right.

use crate::model::Page;
use crate::storage::{today_title, Storage};
use crate::ui::{page_blocks, Theme};
use gpui::{div, prelude::*, px, Context, Window};

pub struct NoteSec {
    storage: Storage,
    /// All pages, kept sorted for the sidebar (journals first, newest first).
    pages: Vec<Page>,
    /// Index into `pages` of the page shown in the main pane.
    selected: usize,
    theme: Theme,
}

impl NoteSec {
    pub fn new(storage: Storage) -> Self {
        let mut pages = storage.load_all();

        // Auto-create today's journal if it doesn't exist yet.
        let today = today_title();
        if !pages.iter().any(|p| p.is_journal && p.title == today) {
            let mut page = Page::from_markdown(&today, true, "- \n");
            page.blocks[0].content.clear();
            if let Err(err) = storage.save(&page) {
                eprintln!("notesec: could not create journal {today}: {err}");
            }
            pages.push(page);
        }

        // First run: give the user something to look at.
        if pages
            .iter()
            .all(|p| p.is_journal && p.blocks.iter().all(|b| b.content.is_empty()))
        {
            let welcome = Page::from_markdown(
                "Welcome",
                false,
                "- Welcome to notesec\n  - Notes are plain markdown files in your graph folder\n  - Blocks can be nested\n",
            );
            if let Err(err) = storage.save(&welcome) {
                eprintln!("notesec: could not create Welcome page: {err}");
            }
            pages.push(welcome);
        }

        sort_pages(&mut pages);
        // Open on today's journal.
        let selected = pages
            .iter()
            .position(|p| p.is_journal && p.title == today)
            .unwrap_or(0);

        NoteSec {
            storage,
            pages,
            selected,
            theme: Theme::dark(),
        }
    }
}

/// Journals first (newest first, since `YYYY-MM-DD` sorts lexically), then
/// regular pages alphabetically.
fn sort_pages(pages: &mut [Page]) {
    pages.sort_by(|a, b| match (a.is_journal, b.is_journal) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        (true, true) => b.title.cmp(&a.title),
        (false, false) => a.title.to_lowercase().cmp(&b.title.to_lowercase()),
    });
}

impl Render for NoteSec {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;

        // --- Sidebar: one clickable row per page ---------------------------
        let sidebar_items = self.pages.iter().enumerate().map(|(ix, page)| {
            let is_selected = ix == self.selected;
            div()
                // Interactive elements need a stable id; (name, index) is the idiom.
                .id(("page", ix))
                .px_3()
                .py_1()
                .rounded_md()
                .cursor_pointer()
                .text_color(if is_selected {
                    theme.accent
                } else {
                    theme.text
                })
                .when(is_selected, |d| d.bg(theme.selected_bg))
                .hover(|d| d.bg(theme.selected_bg))
                // `cx.listener` turns a closure over `&mut Self` into a GPUI handler.
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    this.selected = ix;
                    cx.notify(); // ask GPUI to re-render this view
                }))
                .child(page.title.clone())
        });

        let sidebar = div()
            .id("sidebar")
            .w(px(240.0))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .bg(theme.sidebar_bg)
            .border_r_1()
            .border_color(theme.border)
            .overflow_y_scroll()
            .child(div().px_3().py_2().text_color(theme.muted).child("PAGES"))
            .children(sidebar_items);

        // --- Main pane: title + blocks --------------------------------------
        let page = &self.pages[self.selected];
        let main = div()
            .id("main")
            .flex_1()
            .h_full()
            .overflow_y_scroll()
            .p_8()
            .flex()
            .flex_col()
            .child(
                div()
                    .mb_4()
                    .text_3xl()
                    .text_color(theme.text)
                    .child(page.title.clone()),
            )
            .children(page_blocks(&theme, page));

        div()
            .size_full()
            .flex()
            .flex_row()
            .bg(theme.bg)
            .text_color(theme.text)
            .child(sidebar)
            .child(main)
    }
}
