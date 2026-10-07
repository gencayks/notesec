//! Theme colours and small stateless view helpers.
//!
//! Functions here return GPUI elements; they don't own any state. Anything that
//! needs click handlers is built in `app.rs`, where `cx.listener` is available.

use gpui::{div, prelude::*, px, rgb, Div, Rgba};

/// Colours used across the UI. Dark/light switching arrives in MVP feature 7;
/// putting colours in one struct now means that is a small change later.
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

/// One block row: indent + bullet + `content`. The content is either static
/// text (display mode) or the live text-editing element (edit mode).
pub fn block_row(theme: &Theme, depth: usize, content: impl IntoElement) -> Div {
    div()
        .flex()
        .flex_row()
        .items_start()
        .gap_2()
        // 24px of indent per nesting level.
        .pl(px(24.0 * depth as f32))
        .py_1()
        .min_h(px(30.0))
        .child(
            // The bullet dot.
            div()
                .mt(px(9.0))
                .size(px(6.0))
                .flex_shrink_0()
                .rounded_full()
                .bg(theme.muted),
        )
        .child(div().flex_1().text_color(theme.text).child(content))
}
