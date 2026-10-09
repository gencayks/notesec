//! Settings > Appearance (decision 57): the colour theme, and the two font
//! stacks: the *reading* font (the proportional UI font that outlines, page
//! titles and the sidebar are drawn in) and the *editor* font (the monospace
//! font a block is drawn in while it is being edited).
//!
//! Every control applies and saves at once, through `set_theme`,
//! `set_ui_font`, `set_mono_font` and the size setters in `app.rs`. Each font
//! gets a live preview line, drawn exactly the way that font is used.

use gpui::{div, prelude::*, px, AnyElement, Context, ElementId, FontWeight, Hsla, SharedString};

use super::{NoteSec, SettingsState, FONT_LIST_HEIGHT};
use crate::config::{
    ThemeKind, DEFAULT_FONT_SIZE, DEFAULT_MONO_FONT_SIZE, MAX_FONT_SIZE, MIN_FONT_SIZE,
};
use crate::ui::Theme;

/// The line each font previews.
const SAMPLE: &str = "The quick brown fox jumps over the lazy dog 0123456789";

/// What differs between the reading and the editor font controls.
struct FontControls {
    /// Prefix of the ids (`ui-size-inc`, `mono-font-3`...).
    id: &'static str,
    title: &'static str,
    /// The first list row: what "no choice" means for this font.
    default_label: &'static str,
    default_size: f32,
    size: f32,
    family: Option<String>,
    /// The family actually in use (`None`: not installed or not chosen).
    effective: Option<SharedString>,
    set_family: fn(&mut NoteSec, Option<String>, &mut Context<NoteSec>),
    change_size: fn(&mut NoteSec, f32, &mut Context<NoteSec>),
    reset_size: fn(&mut NoteSec, &mut Context<NoteSec>),
}

impl NoteSec {
    pub(super) fn render_appearance_settings(
        &self,
        state: &SettingsState,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = self.theme;

        // One button per built-in theme, each with a swatch drawn in that
        // theme's own colours (background, accent dot, text dot).
        let current_theme = self.theme_kind();
        let theme_row = div()
            .flex()
            .flex_row()
            // Wrap: three buttons don't fit the panel's width side by side.
            .flex_wrap()
            .gap_2()
            .children(ThemeKind::ALL.map(|kind| {
                let preview = Theme::from_kind(kind);
                let active = current_theme == kind;
                let id = match kind {
                    ThemeKind::TokyoNight => "theme-tokyo-night",
                    ThemeKind::CatppuccinMocha => "theme-catppuccin-mocha",
                    ThemeKind::Light => "theme-light",
                };
                let swatch = div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_center()
                    .gap_1()
                    .w(px(30.0))
                    .h(px(16.0))
                    .rounded_sm()
                    .border_1()
                    .border_color(preview.border)
                    .bg(preview.bg)
                    .child(div().size(px(6.0)).rounded_full().bg(preview.accent))
                    .child(div().size(px(6.0)).rounded_full().bg(preview.text));
                div()
                    .id(id)
                    .debug_selector(move || id.to_string())
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(if active { theme.accent } else { theme.border })
                    .cursor_pointer()
                    .text_color(if active { theme.accent } else { theme.text })
                    .when(active, |d| d.bg(theme.selected_bg))
                    .hover(|d| d.bg(theme.selected_bg))
                    .child(swatch)
                    .child(kind.label())
                    .on_click(cx.listener(move |this, _e, _window, cx| this.set_theme(kind, cx)))
            }));

        let reading = FontControls {
            id: "ui",
            title: "Reading font",
            default_label: "System default",
            default_size: DEFAULT_FONT_SIZE,
            size: self.ui_size(),
            family: self.state.ui_font.clone(),
            effective: self.font_family.clone(),
            set_family: NoteSec::set_ui_font,
            change_size: NoteSec::change_ui_size,
            reset_size: NoteSec::reset_ui_size,
        };
        let editor = FontControls {
            id: "mono",
            title: "Editor font (monospace)",
            default_label: "Automatic (first installed monospace font)",
            default_size: DEFAULT_MONO_FONT_SIZE,
            size: self.mono_size(),
            family: self.state.mono_font.clone(),
            effective: self.mono_font.clone(),
            set_family: NoteSec::set_mono_font,
            change_size: NoteSec::change_mono_size,
            reset_size: NoteSec::reset_mono_size,
        };

        div()
            .debug_selector(|| "settings-appearance".to_string())
            .flex()
            .flex_col()
            .gap_2()
            .child(div().text_color(theme.muted).child("Theme"))
            .child(theme_row)
            .child(self.font_controls(state, reading, cx))
            .child(self.font_controls(state, editor, cx))
            .into_any_element()
    }

    /// Size stepper, preview line and family list for one font.
    fn font_controls(
        &self,
        state: &SettingsState,
        font: FontControls,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = self.theme;
        let id = font.id;
        let button = |id: String, label: String, active: bool| {
            div()
                .id(SharedString::from(id.clone()))
                .debug_selector(move || id)
                .px_3()
                .py_1()
                .rounded_md()
                .border_1()
                .border_color(if active { theme.accent } else { theme.border })
                .cursor_pointer()
                .text_color(if active { theme.accent } else { theme.text })
                .when(active, |d| d.bg(theme.selected_bg))
                .hover(|d| d.bg(theme.selected_bg))
                .child(label)
        };

        // The − / + buttons are dimmed at the limits (clicking them then
        // does nothing: the size setters clamp).
        let size = font.size;
        let (change, reset) = (font.change_size, font.reset_size);
        let size_row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(
                button(format!("{id}-size-dec"), "\u{2212}".into(), false)
                    .when(size <= MIN_FONT_SIZE, |d| d.text_color(theme.muted))
                    .on_click(cx.listener(move |this, _e, _w, cx| change(this, -1.0, cx))),
            )
            .child(
                div()
                    .debug_selector(move || format!("{id}-size-value"))
                    .min_w(px(40.0))
                    .flex()
                    .justify_center()
                    .child(format!("{size}")),
            )
            .child(
                button(format!("{id}-size-inc"), "+".into(), false)
                    .when(size >= MAX_FONT_SIZE, |d| d.text_color(theme.muted))
                    .on_click(cx.listener(move |this, _e, _w, cx| change(this, 1.0, cx))),
            )
            .child(
                button(
                    format!("{id}-size-reset"),
                    "Reset".into(),
                    size == font.default_size,
                )
                .on_click(cx.listener(move |this, _e, _w, cx| reset(this, cx))),
            );

        // The preview is drawn the way this font is really used: the editor
        // preview in the editor's font and size, with a tinted box like a
        // block being edited.
        let preview = div()
            .debug_selector(move || format!("{id}-preview"))
            .px_3()
            .py_2()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .bg(theme.bg)
            .text_color(theme.text)
            .text_size(px(size))
            .when_some(font.effective.clone(), |d, family| d.font_family(family))
            .child(SAMPLE);

        // Family list: the default first, then every installed family.
        let row = |element_id: ElementId, selector: String, text: String, active: bool| {
            div()
                .id(element_id)
                .debug_selector(move || selector)
                .flex_shrink_0()
                .px_3()
                .py_1()
                .rounded_md()
                .cursor_pointer()
                .text_color(if active { theme.accent } else { theme.text })
                .when(active, |d| d.bg(theme.selected_bg))
                .hover(|d| d.bg(theme.selected_bg))
                .child(text)
        };
        let current = font.family.as_deref();
        let set_family = font.set_family;
        let rows: Vec<AnyElement> = state
            .fonts
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let family = name.clone();
                row(
                    ElementId::from((SharedString::from(format!("{id}-font")), i)),
                    format!("{id}-font-{i}"),
                    name.clone(),
                    current == Some(name.as_str()),
                )
                .on_click(
                    cx.listener(move |this, _e, _w, cx| set_family(this, Some(family.clone()), cx)),
                )
                .into_any_element()
            })
            .collect();
        let no_fonts = rows.is_empty();
        // A chosen family that isn't installed (e.g. a typo in state.toml) is
        // kept in the file but not used; say so.
        let missing = current.filter(|f| !state.fonts.iter().any(|name| name == f));
        let list = div()
            .id(SharedString::from(format!("{id}-font-list")))
            .debug_selector(move || format!("{id}-font-list"))
            .h(px(FONT_LIST_HEIGHT))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .p_1()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .bg(theme.bg)
            .child(
                row(
                    ElementId::from(SharedString::from(format!("{id}-font-default"))),
                    format!("{id}-font-default"),
                    font.default_label.to_string(),
                    current.is_none(),
                )
                .on_click(cx.listener(move |this, _e, _w, cx| set_family(this, None, cx))),
            )
            .children(rows)
            .when(no_fonts, |d| {
                d.child(
                    div()
                        .px_3()
                        .py_1()
                        .text_color(theme.muted)
                        .child("No installed fonts were found"),
                )
            });

        div()
            .flex()
            .flex_col()
            .gap_2()
            .pt_2()
            .mt_1()
            .border_t_1()
            .border_color(theme.border)
            .child(
                div()
                    .font_weight(FontWeight::BOLD)
                    .text_color(Hsla::from(theme.text))
                    .child(font.title),
            )
            .child(div().text_color(theme.muted).child("Size"))
            .child(size_row)
            .child(preview)
            .child(div().text_color(theme.muted).child("Family"))
            .child(list)
            .when_some(missing, |d, family| {
                d.child(div().text_color(theme.muted).child(format!(
                    "\u{201c}{family}\u{201d} is not installed; using the default"
                )))
            })
            .into_any_element()
    }
}
