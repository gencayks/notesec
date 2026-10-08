//! Theme colours and small stateless view helpers.
//!
//! Functions here return GPUI elements; they don't own any state. Anything that
//! needs click handlers is built in `app.rs`, where `cx.listener` is available.

use crate::config::ThemeKind;
use crate::model::BlockKind;
use gpui::{div, prelude::*, px, rgb, rgba, Div, FontWeight, Rgba};

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

/// One block row: indent + bullet + `content`. The content is either static
/// text (display mode) or the live text-editing element (edit mode).
/// `font_size` (in px) is the configured size; `kind` scales it for headings
/// and styles quotes, in both modes, so editing a block doesn't make it jump.
pub fn block_row(
    theme: &Theme,
    depth: usize,
    font_size: f32,
    kind: BlockKind,
    content: impl IntoElement,
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
                .mt(px((line_height - 6.0) / 2.0))
                .size(px(6.0))
                .flex_shrink_0()
                .rounded_full()
                .bg(theme.muted),
        )
        .child(body)
}
