//! "Mentioned in" (decision 45): below "Linked from", the blocks on other
//! pages that name this page without linking it, each with a "Link"
//! button (and "Link all" per page). Plain text matching, no AI, so it
//! works in every mode. Computed off the UI thread once per page title /
//! save count (`Storage::changes`), like Related.

use super::*;
use crate::mentions;

/// Longest snippet shown for a mention (in characters, around it).
const SNIPPET_BEFORE: usize = 60;
const SNIPPET_AFTER: usize = 90;

/// One page's mentions as computed: blocks by id, with the text they
/// had and the mention ranges in it.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct MentionPage {
    pub(super) title: String,
    pub(super) blocks: Vec<(Uuid, String, Vec<Range<usize>>)>,
}

impl MentionPage {
    fn count(&self) -> usize {
        self.blocks.iter().map(|b| b.2.len()).sum()
    }
}

#[derive(Default)]
pub(super) struct MentionsState {
    /// What `pages` are for: (page title, storage save count).
    pub(super) key: Option<(String, u64)>,
    pub(super) pages: Vec<MentionPage>,
    /// The section is expanded (it starts folded, showing the count).
    pub(super) open: bool,
    pub(super) task: Option<Task<()>>,
}

/// The snippet of `content` around `range` on one line, and where the
/// mention is in it.
fn snippet(content: &str, range: &Range<usize>) -> (String, Range<usize>) {
    let mut start = range.start;
    for _ in 0..SNIPPET_BEFORE {
        match content[..start].chars().next_back() {
            Some(c) => start -= c.len_utf8(),
            None => break,
        }
    }
    let mut end = range.end;
    for _ in 0..SNIPPET_AFTER {
        match content[end..].chars().next() {
            Some(c) => end += c.len_utf8(),
            None => break,
        }
    }
    let lead = if start > 0 { "\u{2026}" } else { "" };
    let tail = if end < content.len() { "\u{2026}" } else { "" };
    // `\n` and ` ` are both one byte: offsets stay valid.
    let text = format!("{lead}{}{tail}", content[start..end].replace('\n', " "));
    let at = lead.len() + range.start - start;
    (text, at..at + range.len())
}

impl NoteSec {
    /// Per-frame hook (`render`): recompute the focused page's mentions
    /// off the UI thread when the page or the notes changed.
    pub(super) fn sync_mentions(&mut self, cx: &mut Context<Self>) {
        let Some(title) = self.current_page() else {
            return;
        };
        let key = (title.clone(), self.storage.changes());
        if self.mentions.key.as_ref() == Some(&key) || self.mentions.task.is_some() {
            return;
        }
        if self.mentions.key.as_ref().is_some_and(|k| k.0 != title) {
            self.mentions.pages.clear();
        }
        let pages = self.pages.clone();
        self.mentions.task = Some(cx.spawn(async move |this, cx| {
            let found = cx
                .background_spawn(async move {
                    let Some(target) = pages.iter().position(|p| p.title == title) else {
                        return Vec::new();
                    };
                    mentions::unlinked_mentions(&pages, target)
                        .into_iter()
                        .map(|g| {
                            let page = &pages[g.page];
                            MentionPage {
                                title: page.title.clone(),
                                blocks: g
                                    .blocks
                                    .into_iter()
                                    .map(|b| {
                                        let block = &page.blocks[b.block];
                                        (block.id, block.content.clone(), b.ranges)
                                    })
                                    .collect(),
                            }
                        })
                        .collect()
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.mentions.task = None;
                this.mentions.key = Some(key);
                this.mentions.pages = found;
                cx.notify();
            });
        }));
    }

    /// Turn mentions of the current page into links: the one starting at
    /// `start` in block `id`, or (`start` None) every mention on block
    /// `id`'s page. Checked against the text as it is now; one undo step,
    /// saved like any edit.
    pub(super) fn link_mentions(&mut self, id: Uuid, start: Option<usize>, cx: &mut Context<Self>) {
        let Some(title) = self.current_page() else {
            return;
        };
        self.stop_edit(cx);
        let Some(target) = self.find_page(&title) else {
            return;
        };
        let Some((page, _)) = find_block(&self.pages, id) else {
            return;
        };
        let group = mentions::unlinked_mentions(&self.pages, target)
            .into_iter()
            .find(|g| g.page == page);
        let mut edits: Vec<(usize, String)> = Vec::new();
        for b in group.map(|g| g.blocks).unwrap_or_default() {
            let block = &self.pages[page].blocks[b.block];
            let ranges: Vec<Range<usize>> = match start {
                None => b.ranges,
                Some(start) if block.id == id => {
                    b.ranges.into_iter().filter(|r| r.start == start).collect()
                }
                Some(_) => Vec::new(),
            };
            if !ranges.is_empty() {
                let text = mentions::link_ranges(&self.pages, target, &block.content, &ranges);
                edits.push((b.block, text));
            }
        }
        if edits.is_empty() {
            let status = Status {
                text: "That mention has changed; the list is updated".to_string(),
                error: true,
            };
            self.show_status(status, cx);
            self.mentions.key = None;
            cx.notify();
            return;
        }
        let before = self.history_state();
        self.record_state(before);
        for (block, text) in edits {
            self.pages[page].blocks[block].content = text;
        }
        if let Err(err) = self.storage.save(&self.pages[page]) {
            eprintln!("notesec: failed to save {}: {err}", self.pages[page].title);
        }
        cx.notify();
    }

    /// The "Mentioned in" section for the focused page (the other pane of
    /// a split shows only "Linked from").
    pub(super) fn render_mentions(
        &self,
        page_ix: usize,
        focused: bool,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !focused {
            return None;
        }
        let theme = self.theme;
        let state = &self.mentions;
        let ready = state
            .key
            .as_ref()
            .is_some_and(|k| k.0 == self.pages[page_ix].title);
        let total: usize = state.pages.iter().map(MentionPage::count).sum();
        let mark = HighlightStyle {
            color: Some(theme.accent.into()),
            background_color: Some(theme.selected_bg.into()),
            font_weight: Some(FontWeight::BOLD),
            ..Default::default()
        };
        let mut items: Vec<AnyElement> = Vec::new();
        if state.open && ready {
            let mut n: usize = 0;
            for (g, group) in state.pages.iter().enumerate() {
                let open = group.title.clone();
                let first = group.blocks[0].0;
                items.push(
                    div()
                        .mt_2()
                        .flex()
                        .flex_row()
                        .gap_3()
                        .child(
                            div()
                                .id(("mention-page", g))
                                .debug_selector(move || format!("mention-page-{g}"))
                                .text_color(theme.accent)
                                .cursor_pointer()
                                .child(group.title.clone())
                                .on_click(
                                    cx.listener(move |this, _e, _w, cx| this.open_page(&open, cx)),
                                ),
                        )
                        .when(group.count() > 1, |d| {
                            d.child(
                                div()
                                    .id(("mention-link-all", g))
                                    .debug_selector(move || format!("mention-link-all-{g}"))
                                    .text_color(theme.muted)
                                    .cursor_pointer()
                                    .hover(|d| d.text_color(theme.accent))
                                    .child("Link all")
                                    .on_click(cx.listener(move |this, _e, _w, cx| {
                                        this.link_mentions(first, None, cx)
                                    })),
                            )
                        })
                        .into_any_element(),
                );
                for (id, content, ranges) in &group.blocks {
                    for range in ranges {
                        let (text, at) = snippet(content, range);
                        let (id, start, i) = (*id, range.start, n);
                        items.push(
                            div()
                                .flex()
                                .flex_row()
                                .gap_2()
                                .pl_4()
                                .py_1()
                                .child(
                                    div()
                                        .debug_selector(move || format!("mention-{i}"))
                                        .flex_1()
                                        .text_color(theme.text)
                                        .child(StyledText::new(text).with_highlights([(at, mark)])),
                                )
                                .child(
                                    div()
                                        .id(("mention-link", i))
                                        .debug_selector(move || format!("mention-link-{i}"))
                                        .flex_shrink_0()
                                        .text_color(theme.accent)
                                        .cursor_pointer()
                                        .child("Link")
                                        .on_click(cx.listener(move |this, _e, _w, cx| {
                                            this.link_mentions(id, Some(start), cx)
                                        })),
                                )
                                .into_any_element(),
                        );
                        n += 1;
                    }
                }
            }
            if total == 0 {
                items.push(
                    div()
                        .debug_selector(|| "mentions-empty".to_string())
                        .mt_1()
                        .text_color(theme.muted)
                        .child("No unlinked mentions.")
                        .into_any_element(),
                );
            }
        }
        let pages_count = state.pages.len();
        Some(
            div()
                .id("mentions")
                .debug_selector(|| "mentioned-in".to_string())
                .mt_6()
                .flex()
                .flex_col()
                .child(
                    div()
                        .id("mentions-toggle")
                        .debug_selector(|| "mentions-toggle".to_string())
                        .flex()
                        .flex_row()
                        .gap_2()
                        .cursor_pointer()
                        .child(div().text_color(theme.text).child(if state.open {
                            "\u{25be} Mentioned in"
                        } else {
                            "\u{25b8} Mentioned in"
                        }))
                        .when(ready && total > 0, |d| {
                            d.child(
                                div()
                                    .debug_selector(|| "mentions-count".to_string())
                                    .text_color(theme.muted)
                                    .child(format!(
                                        "{pages_count} page{}, {total} mention{}",
                                        if pages_count == 1 { "" } else { "s" },
                                        if total == 1 { "" } else { "s" }
                                    )),
                            )
                        })
                        .on_click(cx.listener(|this, _e, _w, cx| {
                            this.mentions.open = !this.mentions.open;
                            cx.notify();
                        })),
                )
                .children(items)
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippets_cut_around_the_mention_on_one_line() {
        let (text, at) = snippet("a\nRust b", &(2..6));
        assert_eq!((text.as_str(), &text[at]), ("a Rust b", "Rust"));
        let long = format!("{}Rust{}", "é".repeat(100), "x".repeat(200));
        let start = "é".len() * 100;
        let (text, at) = snippet(&long, &(start..start + 4));
        assert_eq!(&text[at], "Rust");
        assert!(text.starts_with('\u{2026}') && text.ends_with('\u{2026}'));
        assert_eq!(text.chars().count(), 1 + 60 + 4 + 90 + 1);
    }

    use crate::ai::AiProvider;
    use gpui::{Modifiers, TestAppContext, VisualTestContext};
    use std::path::PathBuf;

    const PAGES: &[(&str, &str)] = &[
        ("JavaScript", "- alias:: JS\n- JavaScript on its own page\n"),
        (
            "Notes",
            "- I use javascript and JS daily\n- [[JavaScript]] is linked here, js too\n",
        ),
        ("Diary", "- js again\n- see `js` code at https://js.org\n"),
    ];

    fn setup<'a>(
        cx: &'a mut TestAppContext,
        name: &str,
        config: Config,
    ) -> (Entity<NoteSec>, &'a mut VisualTestContext, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("notesec-mentions-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let storage = Storage::open(dir.clone()).unwrap();
        for (title, markdown) in PAGES {
            std::fs::write(dir.join(format!("pages/{title}.md")), markdown).unwrap();
        }
        cx.update(bind_keys);
        let (view, cx) = cx.add_window_view(|window, cx| NoteSec::new(storage, config, window, cx));
        view.update(cx, |app, cx| {
            app.selected = app.find_page("JavaScript").unwrap();
            app.tabs = Tabs::new(TabTarget::Page("JavaScript".into()));
            cx.notify();
        });
        cx.run_until_parked();
        (view, cx, dir)
    }

    fn has(cx: &mut VisualTestContext, selector: &'static str) -> bool {
        cx.debug_bounds(selector).is_some()
    }

    fn click(cx: &mut VisualTestContext, selector: &'static str) {
        let bounds = cx.debug_bounds(selector).expect(selector);
        cx.simulate_click(bounds.center(), Modifiers::none());
        cx.run_until_parked();
    }

    /// (page title, mention texts) as listed.
    fn listed(view: &Entity<NoteSec>, cx: &mut VisualTestContext) -> Vec<(String, Vec<String>)> {
        view.update(cx, |app, _| {
            app.mentions
                .pages
                .iter()
                .map(|p| {
                    let texts = p
                        .blocks
                        .iter()
                        .flat_map(|(_, content, ranges)| {
                            ranges.iter().map(|r| content[r.clone()].to_string())
                        })
                        .collect();
                    (p.title.clone(), texts)
                })
                .collect()
        })
    }

    fn content(
        view: &Entity<NoteSec>,
        cx: &mut VisualTestContext,
        page: &str,
        block: usize,
    ) -> String {
        view.update(cx, |app, _| {
            let p = app.find_page(page).unwrap();
            app.pages[p].blocks[block].content.clone()
        })
    }

    fn s(list: &[&str]) -> Vec<String> {
        list.iter().map(|x| x.to_string()).collect()
    }

    #[gpui::test]
    fn mentions_are_listed_folded_and_linking_one_is_one_undo_step(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "link", Config::default());
        // Not the page itself, not the block that links it, not code or
        // URLs; folded, with the count.
        assert_eq!(
            listed(&view, cx),
            vec![
                ("Diary".to_string(), s(&["js"])),
                ("Notes".to_string(), s(&["javascript", "JS"])),
            ]
        );
        assert!(has(cx, "mentioned-in") && has(cx, "mentions-count"));
        assert!(!has(cx, "mention-0"));
        click(cx, "mentions-toggle");
        assert!(has(cx, "mention-0") && has(cx, "mention-2") && has(cx, "mention-link-all-1"));
        assert!(!has(cx, "mention-link-all-0"), "one mention: no Link all");

        // Diary's mention: written as typed (an alias, any case, resolves).
        click(cx, "mention-link-0");
        assert_eq!(content(&view, cx, "Diary", 0), "[[js]] again");
        let file = std::fs::read_to_string(dir.join("pages/Diary.md")).unwrap();
        assert!(file.starts_with("- [[js]] again\n"), "{file}");
        assert_eq!(
            listed(&view, cx),
            vec![("Notes".to_string(), s(&["javascript", "JS"]))]
        );
        // It is in "Linked from" now (with Notes' linking block).
        assert_eq!(view.update(cx, |app, _| app.backlink_texts.len()), 2);

        cx.simulate_keystrokes("ctrl-z");
        cx.run_until_parked();
        assert_eq!(content(&view, cx, "Diary", 0), "js again");
        assert_eq!(listed(&view, cx).len(), 2);
        assert_eq!(view.update(cx, |app, _| app.backlink_texts.len()), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn link_all_works_with_ai_off_and_a_changed_block_is_not_touched(cx: &mut TestAppContext) {
        let off = Config {
            ai_provider: AiProvider::Off,
            ..Config::default()
        };
        let (view, cx, dir) = setup(cx, "all", off);
        click(cx, "mentions-toggle");
        click(cx, "mention-link-all-1");
        assert_eq!(
            content(&view, cx, "Notes", 0),
            "I use [[javascript]] and [[JS]] daily"
        );
        assert_eq!(listed(&view, cx), vec![("Diary".to_string(), s(&["js"]))]);
        cx.simulate_keystrokes("ctrl-z");
        cx.run_until_parked();
        assert_eq!(
            content(&view, cx, "Notes", 0),
            "I use javascript and JS daily"
        );

        // The block changed since the list was made: nothing is linked,
        // the user is told, and the list is redone.
        view.update(cx, |app, _| {
            let p = app.find_page("Diary").unwrap();
            app.pages[p].blocks[0].content = "nothing here".into();
        });
        click(cx, "mention-link-0");
        assert_eq!(content(&view, cx, "Diary", 0), "nothing here");
        let status = view.update(cx, |app, _| app.status.as_ref().map(|s| s.text.clone()));
        assert!(status.unwrap().contains("changed"));
        assert_eq!(
            listed(&view, cx),
            vec![("Notes".to_string(), s(&["javascript", "JS"]))]
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
