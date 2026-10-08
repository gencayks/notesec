//! Theme colours and small stateless view helpers.
//!
//! Functions here return GPUI elements; they don't own any state. Anything that
//! needs click handlers is built in `app.rs`, where `cx.listener` is available.

use crate::config::ThemeKind;
use crate::model::{BlockKind, TaskState};
use gpui::{div, prelude::*, px, rgb, rgba, AnyElement, Div, FontWeight, Rgba};

/// Colours used across the UI. Every colour comes from here, so switching
/// theme is just swapping this struct.
#[derive(Clone, Copy)]
pub struct Theme {
    pub bg: Rgba,
    pub sidebar_bg: Rgba,
    pub text: Rgba,
    pub muted: Rgba,
    pub accent: Rgba,
    pub selected_bg: Rgba,
    pub border: Rgba,
    /// Translucent background behind selected text, so the text colour
    /// (including link and tag colours) shows through.
    pub selection: Rgba,
    /// Destructive actions (Delete) and error messages.
    pub danger: Rgba,
}

impl Theme {
    pub fn from_kind(kind: ThemeKind) -> Self {
        match kind {
            ThemeKind::Dark => Self::dark(),
            ThemeKind::Light => Self::light(),
        }
    }

    /// Catppuccin Mocha-inspired.
    pub fn dark() -> Self {
        Theme {
            bg: rgb(0x1e1e2e),
            sidebar_bg: rgb(0x181825),
            text: rgb(0xcdd6f4),
            muted: rgb(0x7f849c),
            accent: rgb(0x89b4fa),
            selected_bg: rgb(0x313244),
            border: rgb(0x313244),
            selection: rgba(0x89b4fa55),
            danger: rgb(0xf38ba8),
        }
    }
}

impl Theme {
    /// Catppuccin Latte-inspired.
    pub fn light() -> Self {
        Theme {
            bg: rgb(0xeff1f5),
            sidebar_bg: rgb(0xe6e9ef),
            text: rgb(0x4c4f69),
            muted: rgb(0x7c7f93),
            accent: rgb(0x1e66f5),
            selected_bg: rgb(0xccd0da),
            border: rgb(0xccd0da),
            selection: rgba(0x1e66f540),
            danger: rgb(0xd20f39),
        }
    }
}

/// GPUI's default line height is 1.618 (the golden ratio) times the font size.
/// Layout that depends on the line height (bullet position, row height) is
/// derived from the font size with this so it scales when the font does.
const LINE_HEIGHT_RATIO: f32 = 1.618;

/// Text size of a block of `kind`, relative to the configured font size.
/// Headings scale up from the user's font size so they follow Ctrl-+/-.
pub fn kind_scale(kind: BlockKind) -> f32 {
    match kind {
        BlockKind::Heading1 => 1.6,
        BlockKind::Heading2 => 1.35,
        BlockKind::Heading3 => 1.15,
        BlockKind::Text | BlockKind::Quote => 1.0,
    }
}

/// The checkbox drawn in place of a task keyword, sized and vertically
/// centred for a block of `kind`: an empty box for TODO/LATER, a half-filled
/// one for DOING/NOW, a filled box with a tick for DONE. `app.rs` adds the
/// click handler.
pub fn task_checkbox(theme: &Theme, font_size: f32, kind: BlockKind, state: TaskState) -> Div {
    let size = font_size * kind_scale(kind);
    let line_height = size * LINE_HEIGHT_RATIO;
    let side = (size * 0.9).round();
    let check = div()
        .flex_shrink_0()
        .mt(px((line_height - side) / 2.0))
        .size(px(side))
        .rounded_sm()
        .border_1()
        .overflow_hidden()
        .cursor_pointer();
    match state {
        TaskState::Done => check
            .border_color(theme.accent)
            .bg(theme.accent)
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(side * 0.8))
            .text_color(theme.bg)
            .font_weight(FontWeight::BOLD)
            .child("✓"),
        state if state.is_started() => check
            .border_color(theme.accent)
            .child(div().w(px(side / 2.0)).h_full().bg(theme.accent)),
        _ => check.border_color(theme.muted),
    }
}

/// The fold triangle for a block with children (`▸` collapsed, `▾`
/// expanded). `block_row` places it just left of the bullet, in the indent,
/// so it doesn't move the text. `app.rs` adds the click handler.
pub fn fold_arrow(theme: &Theme, collapsed: bool) -> Div {
    div()
        .absolute()
        .left(px(-17.0))
        .top(px(-5.0))
        .size(px(16.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded_sm()
        .cursor_pointer()
        .text_size(px(12.0))
        .text_color(theme.muted)
        .hover(|d| d.bg(theme.selected_bg))
        .child(if collapsed { "▸" } else { "▾" })
}

/// Badge after a collapsed block: how many blocks are hidden under it.
pub fn fold_badge(theme: &Theme, font_size: f32, count: usize) -> Div {
    let line_height = font_size * LINE_HEIGHT_RATIO;
    div()
        .flex_shrink_0()
        .mt(px((line_height - font_size) / 2.0 - 1.0))
        .px_2()
        .rounded_full()
        .bg(theme.selected_bg)
        .text_size(px(font_size * 0.75))
        .text_color(theme.muted)
        .child(count.to_string())
}

/// One block row: indent + bullet + `content`. The content is either static
/// text (display mode) or the live text-editing element (edit mode).
/// `font_size` (in px) is the configured size; `kind` scales it for headings
/// and styles quotes, in both modes, so editing a block doesn't make it jump.
/// `fold` is the fold arrow of a block with children (see [`fold_arrow`]).
pub fn block_row(
    theme: &Theme,
    depth: usize,
    font_size: f32,
    kind: BlockKind,
    content: impl IntoElement,
    fold: Option<AnyElement>,
) -> Div {
    let size = font_size * kind_scale(kind);
    let line_height = size * LINE_HEIGHT_RATIO;
    let body = div()
        .flex_1()
        .text_size(px(size))
        .text_color(theme.text)
        .when(
            matches!(
                kind,
                BlockKind::Heading1 | BlockKind::Heading2 | BlockKind::Heading3
            ),
            |d| d.font_weight(FontWeight::BOLD),
        )
        // Quotes: a bar on the left and muted italic text.
        .when(kind == BlockKind::Quote, |d| {
            d.border_l_2()
                .border_color(theme.muted)
                .pl_2()
                .text_color(theme.muted)
                .italic()
        })
        .child(content);
    div()
        .flex()
        .flex_row()
        .items_start()
        .gap_2()
        // 24px of indent per nesting level.
        .pl(px(24.0 * depth as f32))
        .py_1()
        .min_h(px(line_height + 8.0)) // text line + py_1 padding
        .child(
            // The bullet dot, vertically centred on the first text line.
            div()
                .relative()
                .mt(px((line_height - 6.0) / 2.0))
                .size(px(6.0))
                .flex_shrink_0()
                .rounded_full()
                .bg(theme.muted)
                .children(fold),
        )
        .child(body)
}

/// The star on a sidebar page row: a filled accent `★` for a favorite, an
/// outline `☆` otherwise. `app.rs` adds the click handler and hides the
/// outline star until its row is hovered.
pub fn favorite_star(theme: &Theme, filled: bool) -> Div {
    div()
        .flex_shrink_0()
        .px_1()
        .rounded_sm()
        .cursor_pointer()
        .text_color(if filled { theme.accent } else { theme.muted })
        .hover(|d| d.text_color(theme.accent))
        .child(if filled { "★" } else { "☆" })
}
