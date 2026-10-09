//! Theme colours and small stateless view helpers.
//!
//! Functions here return GPUI elements; they don't own any state. Anything that
//! needs click handlers is built in `app.rs`, where `cx.listener` is available.

use crate::config::ThemeKind;
use crate::model::{BlockKind, TaskState};
use gpui::{
    div, prelude::*, px, rgb, rgba, AnyElement, Context, Div, FontWeight, Rgba, SharedString,
    Window,
};

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
    /// The translucent layer that dims the app behind a modal (palette,
    /// settings, dialogs). Heavier on light themes would look muddy, so each
    /// theme picks its own strength.
    pub scrim: Rgba,
    /// Text drawn on the whiteboard's pastel card fills. Those fills are the
    /// same in every theme (they are user data), so the text on them must be
    /// dark in every theme too, unlike `text`.
    pub ink: Rgba,
}

impl Theme {
    pub fn from_kind(kind: ThemeKind) -> Self {
        match kind {
            ThemeKind::TokyoNight => Self::tokyo_night(),
            ThemeKind::CatppuccinMocha => Self::catppuccin_mocha(),
            ThemeKind::Light => Self::light(),
        }
    }

    /// Tokyo Night (the default). The background is the app icon's, so the
    /// window and the launcher icon read as one thing.
    pub fn tokyo_night() -> Self {
        Theme {
            bg: rgb(0x16161e),
            sidebar_bg: rgb(0x101015),
            text: rgb(0xc0caf5),
            muted: rgb(0x7982a9),
            accent: rgb(0x7aa2f7),
            selected_bg: rgb(0x292e42),
            border: rgb(0x292e42),
            selection: rgba(0x7aa2f755),
            danger: rgb(0xf7768e),
            scrim: rgba(0x0a0a0f99),
            ink: rgb(0x1f2328),
        }
    }

    /// Catppuccin Mocha, the dark alternative. (This is the palette the app
    /// shipped with before themes were selectable, so anyone who had "dark"
    /// selected keeps exactly this look.)
    pub fn catppuccin_mocha() -> Self {
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
            scrim: rgba(0x00000073),
            ink: rgb(0x1f2328),
        }
    }

    /// A clean light theme: paper-white background, near-black text.
    pub fn light() -> Self {
        Theme {
            bg: rgb(0xfbfaf7),
            sidebar_bg: rgb(0xf1efe9),
            text: rgb(0x24292f),
            muted: rgb(0x6b727c),
            accent: rgb(0x2f5fd0),
            selected_bg: rgb(0xe5e2d9),
            border: rgb(0xdad6ca),
            selection: rgba(0x2f5fd038),
            danger: rgb(0xc62a3c),
            scrim: rgba(0x2b2b3340),
            ink: rgb(0x1f2328),
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
/// `handle` is the drag handle laid over the bullet (see [`drag_handle`]).
pub fn block_row(
    theme: &Theme,
    depth: usize,
    font_size: f32,
    kind: BlockKind,
    content: impl IntoElement,
    fold: Option<AnyElement>,
    handle: AnyElement,
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
                .children(fold)
                .child(handle),
        )
        .child(body)
}

/// An invisible square laid over a block's bullet that you drag to move the
/// block. It is a bit bigger than the 6px dot so it is easy to grab, but
/// stays clear of the fold arrow on its left and the text on its right.
/// `app.rs` adds the drag handlers.
pub fn drag_handle() -> Div {
    div()
        .absolute()
        .left(px(-4.0))
        .top(px(-4.0))
        .size(px(14.0))
        .rounded_sm()
        .cursor_grab()
}

/// The accent line showing where a dragged block will land, laid across
/// the top of a row (or under the last one), indented to `depth`.
pub fn drop_line(theme: &Theme, depth: usize) -> Div {
    div()
        .absolute()
        .top(px(-1.0))
        .left(px(24.0 * depth as f32))
        .right_0()
        .h(px(2.0))
        .rounded_full()
        .bg(theme.accent)
}

/// What follows the mouse while a block is dragged: its text in a small
/// translucent card.
pub struct BlockDragPreview {
    pub text: SharedString,
    pub theme: Theme,
}

impl Render for BlockDragPreview {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .max_w(px(320.0))
            .px_2()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(self.theme.border)
            .bg(self.theme.sidebar_bg)
            .text_color(self.theme.text)
            .opacity(0.85)
            .overflow_hidden()
            .whitespace_nowrap()
            .child(self.text.clone())
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG relative luminance of a colour.
    fn luminance(c: Rgba) -> f32 {
        let lin = |v: f32| {
            if v <= 0.03928 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * lin(c.r) + 0.7152 * lin(c.g) + 0.0722 * lin(c.b)
    }

    /// WCAG contrast ratio between two colours (1 to 21).
    fn contrast(a: Rgba, b: Rgba) -> f32 {
        let (la, lb) = (luminance(a), luminance(b));
        (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
    }

    #[test]
    fn from_kind_picks_each_palette() {
        assert_eq!(
            Theme::from_kind(ThemeKind::TokyoNight).bg,
            Theme::tokyo_night().bg
        );
        assert_eq!(
            Theme::from_kind(ThemeKind::CatppuccinMocha).bg,
            Theme::catppuccin_mocha().bg
        );
        assert_eq!(Theme::from_kind(ThemeKind::Light).bg, Theme::light().bg);
    }

    #[test]
    fn tokyo_night_uses_the_icon_background_and_the_specified_accent() {
        let t = Theme::tokyo_night();
        assert_eq!(t.bg, rgb(0x16161e), "matches assets/notesec.svg");
        assert_eq!(t.accent, rgb(0x7aa2f7));
    }

    #[test]
    fn catppuccin_mocha_is_the_palette_the_old_dark_theme_used() {
        let t = Theme::catppuccin_mocha();
        assert_eq!(
            (t.bg, t.text, t.accent),
            (rgb(0x1e1e2e), rgb(0xcdd6f4), rgb(0x89b4fa))
        );
    }

    #[test]
    fn the_light_theme_is_paper_white_with_dark_text() {
        let t = Theme::light();
        assert!(luminance(t.bg) > 0.9, "paper-white background");
        assert!(luminance(t.text) < 0.05, "dark text");
        // And the dark themes are the other way round.
        for dark in [Theme::tokyo_night(), Theme::catppuccin_mocha()] {
            assert!(luminance(dark.bg) < 0.05 && luminance(dark.text) > 0.5);
        }
    }

    #[test]
    fn every_theme_is_readable() {
        for kind in ThemeKind::ALL {
            let t = Theme::from_kind(kind);
            let name = kind.label();
            // Body text: WCAG AAA.
            assert!(contrast(t.text, t.bg) >= 7.0, "{name}: text on bg");
            assert!(
                contrast(t.text, t.sidebar_bg) >= 7.0,
                "{name}: text on sidebar"
            );
            assert!(
                contrast(t.text, t.selected_bg) >= 5.0,
                "{name}: text on selection row"
            );
            // Secondary text, links and tags (accent), errors.
            assert!(contrast(t.muted, t.bg) >= 4.0, "{name}: muted on bg");
            assert!(contrast(t.accent, t.bg) >= 4.5, "{name}: accent on bg");
            assert!(
                contrast(t.accent, t.selected_bg) >= 3.5,
                "{name}: accent on selected row"
            );
            assert!(contrast(t.danger, t.bg) >= 3.5, "{name}: danger on bg");
            // The whiteboard's pastel cards use dark ink in every theme.
            assert!(
                contrast(t.ink, rgb(0xffd966)) >= 7.0,
                "{name}: ink on a yellow card"
            );
            // The scrim must actually dim: translucent, not fully clear or opaque.
            assert!(
                t.scrim.a > 0.1 && t.scrim.a < 0.9,
                "{name}: scrim alpha {}",
                t.scrim.a
            );
        }
    }

    #[test]
    fn the_themes_are_all_different() {
        let bgs: Vec<_> = ThemeKind::ALL
            .iter()
            .map(|k| Theme::from_kind(*k).bg)
            .collect();
        assert!(bgs[0] != bgs[1] && bgs[1] != bgs[2] && bgs[0] != bgs[2]);
    }

    #[test]
    fn toggling_goes_between_light_and_the_default_dark() {
        assert_eq!(ThemeKind::TokyoNight.toggled(), ThemeKind::Light);
        assert_eq!(ThemeKind::CatppuccinMocha.toggled(), ThemeKind::Light);
        assert_eq!(ThemeKind::Light.toggled(), ThemeKind::TokyoNight);
    }
}
