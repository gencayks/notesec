//! Live embeds in the reading view (decision 47): under a block that embeds
//! a page (`![[Page]]`) or a block (`![[((id))]]`), a box with the source's
//! title and its blocks as an indented outline, or a note when the embed is
//! circular, nested too deeply or points at nothing. Resolved every frame
//! from the pages in memory (and the editor's unsaved text), so an edit to
//! the source shows in every embed of it at once, in either pane.
//!
//! Embedded content is read-only: the title opens the source page, and a
//! press on an embedded block opens its page and edits it there. Every
//! block of the source is shown (folding is not carried into embeds).

use gpui::{div, prelude::*, px, AnyElement, ClickEvent, Context, MouseButton};

use super::NoteSec;
use crate::display::DisplayBlock;
use crate::embed::{Resolved, Resolver};
use crate::model::find_block;

/// A row's embeds, drawn.
pub(super) struct Embeds {
    pub element: AnyElement,
    /// What the boxes show, one line per title, block or note, indented
    /// two spaces per level, titles in brackets (for tests).
    #[cfg(test)]
    pub lines: Vec<String>,
}

/// Running state while drawing one row's embeds.
struct Out {
    prefix: &'static str,
    /// The embedding row.
    ix: usize,
    /// Numbers the titles, blocks and notes of the row's embeds in order,
    /// for element ids and debug selectors (`embed-{ix}-block-{n}`).
    n: usize,
    lines: Vec<String>,
}

impl Out {
    fn next(&mut self) -> usize {
        self.n += 1;
        self.n - 1
    }
}

impl NoteSec {
    /// The embeds of block `ix` of page `page_ix`, whose text (live, if it
    /// is being edited in the other pane) is `content`; `None` if it has
    /// none. `prefix` is the pane's debug selector prefix.
    pub(super) fn render_embeds(
        &self,
        prefix: &'static str,
        page_ix: usize,
        ix: usize,
        content: &str,
        cx: &mut Context<Self>,
    ) -> Option<Embeds> {
        if !content.contains("![[") && !content.contains("{{") {
            return None;
        }
        let live = self
            .editing
            .and_then(|e| self.pages.get(self.selected)?.blocks.get(e))
            .map(|b| (b.id, self.editor.text.as_str()));
        let resolved = Resolver::new(&self.pages, live).resolve(content, Some(page_ix));
        if resolved.is_empty() {
            return None;
        }
        let mut out = Out {
            prefix,
            ix,
            n: 0,
            lines: Vec::new(),
        };
        let boxes: Vec<AnyElement> = resolved
            .iter()
            .map(|r| self.embed_box(&mut out, r, 0, cx))
            .collect();
        #[cfg(not(test))]
        let _ = out.lines;
        Some(Embeds {
            element: div()
                .id(("embeds", ix))
                .mt_1()
                .flex()
                .flex_col()
                .gap_1()
                .children(boxes)
                .into_any_element(),
            #[cfg(test)]
            lines: out.lines,
        })
    }

    /// One embed: a box with the title and blocks (and their own embeds,
    /// nested), or a note. `level` indents the test lines.
    fn embed_box(
        &self,
        out: &mut Out,
        embed: &Resolved,
        level: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = self.theme;
        let font_size = self.ui_size();
        let (prefix, ix) = (out.prefix, out.ix);
        let indent = |n: usize| "  ".repeat(n);
        let (title, rows) = match embed {
            Resolved::Page { title, rows } | Resolved::Block { title, rows } => (title, rows),
            _ => {
                let k = out.next();
                let note = embed.note().unwrap_or_default();
                out.lines.push(format!("{}{note}", indent(level)));
                return div()
                    .debug_selector(move || format!("{prefix}embed-{ix}-note-{k}"))
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .italic()
                    .text_color(theme.muted)
                    .text_size(px(font_size * 0.9))
                    .child(note)
                    .into_any_element();
            }
        };
        let k = out.next();
        out.lines.push(format!("{}[{title}]", indent(level)));
        let source = title.clone();
        let header = div()
            .id(("embed-title", k))
            .debug_selector(move || format!("{prefix}embed-{ix}-title-{k}"))
            .pb_1()
            .text_size(px(font_size * 0.85))
            .text_color(theme.accent)
            .cursor_pointer()
            // A press here never starts editing the embedding block; the
            // click opens the source page.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.open_page(&source, cx);
            }))
            .child(title.clone())
            .into_any_element();
        let mut items = vec![header];
        for row in rows {
            let k = out.next();
            let text = DisplayBlock::with_refs(&row.content, |id| {
                find_block(&self.pages, id).map(|(p, b)| self.pages[p].blocks[b].content.clone())
            })
            .text;
            out.lines
                .push(format!("{}{text}", indent(level + 1 + row.depth)));
            let id = row.id;
            items.push(
                div()
                    .id(("embed-block", k))
                    .debug_selector(move || format!("{prefix}embed-{ix}-block-{k}"))
                    .pl(px(16.0 * row.depth as f32))
                    .flex()
                    .flex_row()
                    .gap_2()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|d| d.bg(theme.selected_bg))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.open_block_ref(id, window, cx);
                    }))
                    .child(div().flex_none().text_color(theme.muted).child("\u{2022}"))
                    .child(div().flex_1().min_w_0().text_color(theme.text).child(text))
                    .into_any_element(),
            );
            for inner in &row.embeds {
                let inner = self.embed_box(out, inner, level + 2 + row.depth, cx);
                items.push(
                    div()
                        .pl(px(16.0 * (row.depth + 1) as f32))
                        .child(inner)
                        .into_any_element(),
                );
            }
        }
        div()
            .debug_selector(move || format!("{prefix}embed-{ix}-{k}"))
            .px_2()
            .py_1()
            .rounded_md()
            .border_1()
            .border_l_2()
            .border_color(theme.border)
            .bg(theme.sidebar_bg)
            .flex()
            .flex_col()
            .children(items)
            .into_any_element()
    }
}
