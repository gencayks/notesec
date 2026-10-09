//! Kanban board (roadmap v0.3.0 feature 4, docs/KANBAN.md): a per-page
//! Outline/Kanban toggle in the page header. Top-level blocks tagged
//! `#todo`, `#doing` or `#done` are cards in those columns (the rest in
//! Unsorted); dragging a card onto a column rewrites its status tags, so
//! the markdown stays the source of truth. Clicking a card edits it in
//! the outline; each column adds cards. The view choice persists per page
//! in `state.toml`. Drag and drop reuses the block-drag GPUI pattern
//! (`on_drag` / `on_drop`); no new GPUI APIs.

use super::*;
use crate::model::{kanban_status, set_kanban_status, KanbanStatus};

/// What a kanban card drag carries (see `BlockDragPreview` for what is
/// drawn under the mouse): the dragged block, by id so it is found again
/// even if indices shift.
#[derive(Clone, Debug)]
pub(in crate::app) struct KanbanDrag {
    id: Uuid,
}

impl NoteSec {
    /// This page shows the board rather than the outline.
    pub(in crate::app) fn is_kanban(&self, title: &str) -> bool {
        self.state.kanban.iter().any(|t| t == title)
    }

    /// The palette command (and the header toggle below): flip the current
    /// page between Outline and Kanban.
    pub(in crate::app) fn on_toggle_kanban(
        &mut self,
        _: &ToggleKanban,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.mode != Mode::Notes || self.pages.get(self.selected).is_none() {
            return;
        }
        let title = self.pages[self.selected].title.clone();
        self.set_kanban(&title, !self.is_kanban(&title), cx);
    }

    /// Show `title` as a board (`on`) or an outline, persisting the choice.
    fn set_kanban(&mut self, title: &str, on: bool, cx: &mut Context<Self>) {
        let has = self.state.kanban.iter().any(|t| t == title);
        if has != on {
            if on {
                self.state.kanban.push(title.to_string());
            } else {
                self.state.kanban.retain(|t| t != title);
            }
            self.save_state();
        }
        cx.notify();
    }

    /// A card was dropped on a column: rewrite its status tags (one undo
    /// step, saved like any edit). A block being edited keeps its editor
    /// in sync.
    fn drop_card(&mut self, dragged: Uuid, status: KanbanStatus, cx: &mut Context<Self>) {
        self.kanban_over = None;
        let found = self.pages.iter().enumerate().find_map(|(p, page)| {
            page.blocks
                .iter()
                .position(|b| b.id == dragged)
                .map(|b| (p, b))
        });
        let Some((p, b)) = found else {
            cx.notify();
            return;
        };
        let content = set_kanban_status(&self.pages[p].blocks[b].content, status);
        if content == self.pages[p].blocks[b].content {
            cx.notify();
            return;
        }
        self.record_edit();
        self.pages[p].blocks[b].content = content.clone();
        if self.selected == p
            && self.editing.is_some_and(|ix| {
                self.pages[p]
                    .blocks
                    .get(ix)
                    .is_some_and(|e| e.id == dragged)
            })
        {
            self.editor = EditorState::new(&content);
            self.editor.cursor = self.editor.text.len();
            self.sync_content();
        }
        let page = &self.pages[p];
        if let Err(err) = self.storage.save(page) {
            eprintln!("notesec: failed to save {}: {err}", page.title);
        }
        cx.notify();
    }

    /// Click on a card: back to the outline, editing that block.
    fn edit_kanban_card(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let title = match self.pages.get(self.selected) {
            Some(page) if self.mode == Mode::Notes => page.title.clone(),
            _ => return,
        };
        let Some(ix) = self.pages[self.selected]
            .blocks
            .iter()
            .position(|b| b.id == id)
        else {
            return;
        };
        self.set_kanban(&title, false, cx);
        self.start_edit(ix, window, cx);
    }

    /// A column's "+" button: append a top-level block with that column's
    /// tag (none for Unsorted) and edit it in the outline.
    fn add_kanban_card(
        &mut self,
        page_ix: usize,
        status: KanbanStatus,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.mode != Mode::Notes || self.pages.get(page_ix).is_none() {
            return;
        }
        let content = status.tag().map_or(String::new(), |t| format!("#{t}"));
        self.record_edit();
        let ix = self.pages[page_ix].push_block(content);
        let page = &self.pages[page_ix];
        if let Err(err) = self.storage.save(page) {
            eprintln!("notesec: failed to save {}: {err}", page.title);
        }
        // Editing lives in the outline on the focused pane's page.
        if self.selected == page_ix {
            let title = self.pages[page_ix].title.clone();
            self.set_kanban(&title, false, cx);
            self.start_edit(ix, window, cx);
        } else {
            cx.notify();
        }
    }

    /// The Outline/Kanban toggle for the page header.
    pub(in crate::app) fn render_view_toggle(
        &self,
        title: &str,
        prefix: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = self.theme;
        let kanban = self.is_kanban(title);
        let button = |id: String, label: &str, active: bool| {
            let selector = id.clone();
            div()
                .id(SharedString::from(id))
                .debug_selector(move || selector.clone())
                .px_2()
                .rounded_md()
                .border_1()
                .border_color(if active { theme.accent } else { theme.border })
                .text_color(if active { theme.accent } else { theme.muted })
                .cursor_pointer()
                .hover(|d| d.bg(theme.selected_bg))
                .child(label.to_string())
        };
        let outline_id = format!("{prefix}kanban-toggle-outline");
        let board_id = format!("{prefix}kanban-toggle-board");
        let owned = title.to_string();
        div()
            .flex()
            .flex_row()
            .gap_1()
            .child(button(outline_id, "Outline", !kanban).on_click(cx.listener(
                move |this, _, _, cx| {
                    this.set_kanban(&owned, false, cx);
                },
            )))
            .child({
                let owned = title.to_string();
                button(board_id, "Kanban", kanban).on_click(cx.listener(move |this, _, _, cx| {
                    this.set_kanban(&owned, true, cx);
                }))
            })
            .into_any_element()
    }

    /// The page as a board: one column per status with its top-level
    /// blocks as cards. Only the focused pane takes drops (like block
    /// drops); other panes render read-only.
    pub(in crate::app) fn render_kanban(
        &self,
        page_ix: usize,
        focused: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = self.theme;
        let prefix = if focused { "" } else { OTHER_PANE };
        let (link_style, tag_style, ref_style) = markdown_styles(theme);
        let page = &self.pages[page_ix];
        let tops: Vec<usize> = page
            .blocks
            .iter()
            .enumerate()
            .filter(|(_, b)| b.parent_id.is_none())
            .map(|(ix, _)| ix)
            .collect();

        let columns: Vec<AnyElement> = KanbanStatus::ALL
            .iter()
            .map(|&status| {
                let cards: Vec<AnyElement> = tops
                    .iter()
                    .filter(|&&ix| kanban_status(&page.blocks[ix].content) == status)
                    .map(|&ix| {
                        let block = &page.blocks[ix];
                        let id = block.id;
                        let kids = page.descendant_count(ix);
                        let preview: SharedString = block
                            .content
                            .lines()
                            .next()
                            .unwrap_or("")
                            .to_string()
                            .into();
                        let display = DisplayBlock::inline(&block.content, |_| None);
                        div()
                            .id(ElementId::from(("kanban-card", ix as u64)))
                            .debug_selector(move || format!("{prefix}kanban-card-{id}"))
                            .relative()
                            .p_2()
                            .rounded_md()
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.bg)
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(StyledText::new(display.text.clone()).with_highlights(
                                reading_highlights(&display, link_style, tag_style, ref_style),
                            ))
                            .when(kids > 0, |d| {
                                d.child(div().text_color(theme.muted).child(format!(
                                    "{} sub-block{}",
                                    kids,
                                    if kids == 1 { "" } else { "s" }
                                )))
                            })
                            .child(
                                drag_handle()
                                    .id(ElementId::from(("kanban-handle", ix as u64)))
                                    .debug_selector(move || format!("{prefix}kanban-handle-{id}"))
                                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                        cx.stop_propagation()
                                    })
                                    .on_drag(KanbanDrag { id }, move |_, _, _, cx| {
                                        cx.new(|_| BlockDragPreview {
                                            text: preview.clone(),
                                            theme,
                                        })
                                    }),
                            )
                            .when(focused, |d| {
                                d.cursor_pointer().on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        this.edit_kanban_card(id, window, cx);
                                    },
                                ))
                            })
                            .into_any_element()
                    })
                    .collect();
                let n = cards.len();
                let hovered = focused && self.kanban_over == Some(status);
                let status_name = format!("{status:?}").to_lowercase();
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_2()
                    .rounded_md()
                    .debug_selector({
                        let column = status_name.clone();
                        move || format!("{prefix}kanban-col-{column}")
                    })
                    .when(hovered, |d| d.bg(theme.selected_bg))
                    .child(
                        div().flex().flex_row().justify_between().child(
                            div()
                                .font_weight(FontWeight::BOLD)
                                .child(format!("{} ({n})", status.title())),
                        ),
                    )
                    .child(
                        div().flex().flex_col().gap_2().children(cards).child(
                            div()
                                .id(SharedString::from(format!(
                                    "{prefix}kanban-add-{status_name}"
                                )))
                                .debug_selector(move || format!("{prefix}kanban-add-{status_name}"))
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .border_1()
                                .border_color(theme.border)
                                .text_color(theme.muted)
                                .cursor_pointer()
                                .hover(|d| d.bg(theme.selected_bg))
                                .child("+ New card")
                                .when(focused, |d| {
                                    d.on_click(cx.listener(move |this, _, window, cx| {
                                        this.add_kanban_card(page_ix, status, window, cx);
                                    }))
                                }),
                        ),
                    )
                    .when(focused, |d| {
                        d.on_drag_move(cx.listener(
                            move |this, event: &DragMoveEvent<KanbanDrag>, _, cx| {
                                let over = event.bounds.contains(&event.event.position);
                                let next = over.then_some(status);
                                if this.kanban_over != next {
                                    this.kanban_over = next;
                                    cx.notify();
                                }
                            },
                        ))
                        .on_drop(cx.listener(
                            move |this, dragged: &KanbanDrag, _, cx| {
                                this.drop_card(dragged.id, status, cx);
                            },
                        ))
                    })
                    .into_any_element()
            })
            .collect();
        div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_row()
            .gap_2()
            .children(columns)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Modifiers, MouseButton, TestAppContext, VisualTestContext};

    /// A window on a graph with `pages` (the first is shown), like the
    /// root view's tests.
    fn setup<'a>(
        cx: &'a mut TestAppContext,
        name: &str,
        pages: &[(&str, &str)],
    ) -> (
        Entity<NoteSec>,
        &'a mut VisualTestContext,
        std::path::PathBuf,
    ) {
        let dir =
            std::env::temp_dir().join(format!("notesec-kanban-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let storage = Storage::open(dir.clone()).unwrap();
        for (title, markdown) in pages {
            std::fs::write(dir.join(format!("pages/{title}.md")), markdown).unwrap();
        }
        cx.update(bind_keys);
        let (view, cx) =
            cx.add_window_view(|window, cx| NoteSec::new(storage, Config::default(), window, cx));
        let first = pages[0].0.to_string();
        view.update(cx, |app, cx| {
            app.selected = app.find_page(&first).unwrap();
            app.tabs = Tabs::new(TabTarget::Page(first.clone()));
            cx.notify();
        });
        cx.run_until_parked();
        (view, cx, dir)
    }

    fn has(cx: &mut VisualTestContext, selector: &str) -> bool {
        let selector: &'static str = Box::leak(selector.to_string().into_boxed_str());
        cx.debug_bounds(selector).is_some()
    }

    fn at(cx: &mut VisualTestContext, selector: &str) -> gpui::Point<gpui::Pixels> {
        let selector: &'static str = Box::leak(selector.to_string().into_boxed_str());
        cx.debug_bounds(selector).expect(selector).center()
    }

    fn click_on(cx: &mut VisualTestContext, selector: &str) {
        let selector: &'static str = Box::leak(selector.to_string().into_boxed_str());
        let bounds = cx.debug_bounds(selector).expect(selector);
        cx.simulate_click(bounds.center(), Modifiers::none());
    }

    fn block_id(view: &Entity<NoteSec>, cx: &mut VisualTestContext, ix: usize) -> Uuid {
        view.update(cx, |app, _| app.pages[app.selected].blocks[ix].id)
    }

    /// Drag the card's grip onto a column, like a pointer would.
    fn drag_card(cx: &mut VisualTestContext, handle: &str, column: &str) {
        let from = at(cx, handle);
        let to = at(cx, column);
        let mid = gpui::point((from.x + to.x) / 2.0, (from.y + to.y) / 2.0);
        cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(mid, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(to, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
    }

    const PAGES: &[(&str, &str)] =
        &[("Board", "- write tests #todo\n- review #doing\n- ship it\n")];

    #[gpui::test]
    fn the_header_toggle_shows_the_board_and_persists_it(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "toggle", PAGES);
        click_on(cx, "kanban-toggle-board");
        assert!(has(cx, "kanban-col-doing") && has(cx, "kanban-add-todo"));
        assert!(!has(cx, "block-0"), "cards replace the outline rows");
        view.update(cx, |app, _| assert_eq!(app.state.kanban, vec!["Board"]));
        assert_eq!(UiState::load(&dir).kanban, vec!["Board"]);
        click_on(cx, "kanban-toggle-outline");
        assert!(has(cx, "block-0") && !has(cx, "kanban-col-doing"));
        view.update(cx, |app, _| assert!(app.state.kanban.is_empty()));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn the_palette_toggles_the_board(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "palette", PAGES);
        cx.simulate_keystrokes("ctrl-k");
        cx.simulate_input("kanban view");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert!(view.update(cx, |app, _| app.is_kanban("Board")));
        assert!(has(cx, "kanban-col-done"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn dragging_a_card_retags_its_block_in_the_file(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "drag", PAGES);
        click_on(cx, "kanban-toggle-board");
        let id = block_id(&view, cx, 0);
        drag_card(cx, &format!("kanban-handle-{id}"), "kanban-col-doing");
        view.update(cx, |app, _| {
            assert_eq!(
                app.pages[app.selected].blocks[0].content,
                "write tests #doing"
            );
        });
        let file = std::fs::read_to_string(dir.join("pages/Board.md")).unwrap();
        assert!(file.contains("- write tests #doing"), "{file}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn clicking_a_card_edits_it_in_the_outline(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "edit", PAGES);
        click_on(cx, "kanban-toggle-board");
        let id = block_id(&view, cx, 1);
        click_on(cx, &format!("kanban-card-{id}"));
        view.update(cx, |app, _| {
            assert!(!app.is_kanban("Board"));
            assert_eq!(app.editing, Some(1));
            assert_eq!(app.editor.text, "review #doing");
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn plus_adds_a_tagged_card_and_edits_it(cx: &mut TestAppContext) {
        let (view, cx, dir) = setup(cx, "add", PAGES);
        click_on(cx, "kanban-toggle-board");
        click_on(cx, "kanban-add-doing");
        view.update(cx, |app, _| {
            assert!(!app.is_kanban("Board"), "editing happens in the outline");
            let last = app.pages[app.selected].blocks.len() - 1;
            assert_eq!(app.editing, Some(last));
            assert_eq!(app.pages[app.selected].blocks[last].content, "#doing");
        });
        let _ = std::fs::remove_dir_all(dir);
    }
}
