//! Theme colours and small stateless view helpers.
//!
//! Functions here return GPUI elements; they don't own any state. Anything that
//! needs click handlers is built in `app.rs`, where `cx.listener` is available.

use crate::config::ThemeKind;
use gpui::{div, prelude::*, px, rgb, Div, Rgba};

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
        }
    }
}

/// GPUI's default line height is 1.618 (the golden ratio) times the font size.
/// Layout that depends on the line height (bullet position, row height) is
/// derived from the font size with this so it scales when the font does.
const LINE_HEIGHT_RATIO: f32 = 1.618;

/// One block row: indent + bullet + `content`. The content is either static
/// text (display mode) or the live text-editing element (edit mode).
/// `font_size` (in px) positions the bullet and sets the minimum row height.
pub fn block_row(theme: &Theme, depth: usize, font_size: f32, content: impl IntoElement) -> Div {
    let line_height = font_size * LINE_HEIGHT_RATIO;
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
        .child(div().flex_1().text_color(theme.text).child(content))
}
