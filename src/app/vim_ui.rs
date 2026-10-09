//! Vim mode in the app (decision 48): feeds keystrokes to the pure state
//! machine in `vim.rs`, applies the effects it returns to the outline, and
//! draws the mode indicator and the Settings row.
//!
//! Keys reach vim through the window's keystroke interceptor, which runs
//! before key bindings and before text input: a key vim takes (every key
//! in Normal and Visual mode except Ctrl / Alt / Super chords) stops
//! there, so it is never typed and runs no shortcut. Text that arrives
//! without a key press (an input method committing) is ignored outside
//! Insert mode too (`vim_takes_keys` in the input handler).

use gpui::{div, prelude::*, px, AnyElement, Context, Keystroke, Window};

use super::{NoteSec, Status, ToggleVimMode, Undo};
use crate::vim::{self, Effect, Key, Mode, Outcome, Register};

/// A keystroke as vim sees it.
fn vim_key(keystroke: &Keystroke) -> Key {
    let m = &keystroke.modifiers;
    if m.control && !m.alt && !m.platform && !m.function {
        let mut chars = keystroke.key.chars();
        return match (chars.next(), chars.next()) {
            (Some(c), None) => Key::Ctrl(c),
            _ => Key::Other,
        };
    }
    if m.control || m.alt || m.platform || m.function {
        return Key::Other;
    }
    match keystroke.key.as_str() {
        "escape" => return Key::Escape,
        "enter" => return Key::Enter,
        "backspace" => return Key::Backspace,
        "delete" => return Key::Delete,
        "tab" => return Key::Other,
        _ => {}
    }
    let text = keystroke
        .key_char
        .as_deref()
        .unwrap_or(match keystroke.key.as_str() {
            "space" => " ",
            key => key,
        });
    let mut chars = text.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if !c.is_control() => {
            if m.shift && keystroke.key_char.is_none() {
                Key::Char(c.to_ascii_uppercase())
            } else {
                Key::Char(c)
            }
        }
        _ => Key::Other,
    }
}

impl NoteSec {
    /// Vim is on and the block editor has the keyboard: not the palette,
    /// the rename field, Settings (and its key capture), a dialog (page
    /// menu, trash, import clash), or any other text field
    /// (`text_input_open`). Modal overlays always win.
    pub(super) fn vim_applies(&self) -> bool {
        self.config.vim_mode
            && self.editing.is_some()
            && !self.text_input_open()
            && self.settings.is_none()
            && !self.shortcuts_open
            && self.page_menu.is_none()
            && self.trash_confirm.is_none()
            && !self.import_dialog_open()
            // Typing in a whiteboard card is plain typing (decision 53).
            && !self.card_editing()
    }

    /// Vim takes every key (Normal, Visual, or the `:` line): nothing is
    /// typed into the block and the pickers stay closed.
    pub(super) fn vim_takes_keys(&self) -> bool {
        self.vim_applies() && (self.vim.mode != Mode::Insert || self.vim.cmdline().is_some())
    }

    /// The keystroke interceptor's hook: true if vim took the key (it then
    /// goes nowhere else).
    pub(super) fn vim_keystroke(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        // A lone modifier (Shift pressed and released) isn't a key.
        if !self.vim_applies()
            || keystroke.key.is_empty()
            || crate::hotkeys::is_modifier_key(&keystroke.key)
        {
            return false;
        }
        let key = vim_key(keystroke);
        // In Insert mode only Esc is vim's, and not while it has another
        // job: closing the "/" menu or a picker, or ending a composition.
        if self.vim.mode == Mode::Insert
            && (key != Key::Escape
                || self.slash.is_some()
                || self.ref_menu_open()
                || self.editor.marked.is_some())
        {
            return false;
        }
        let before = self.editor.clone();
        let Outcome::Handled(effect) = self.vim.key(key, &mut self.editor) else {
            return false;
        };
        // Each change made in Normal mode is one undo step, and the next
        // run of typing starts its own.
        self.text_history_active = false;
        if self.editor.text != before.text {
            let mut state = self.history_state();
            state.editor = before;
            self.record_state(state);
            self.text_changed();
        }
        if let Some(effect) = effect {
            self.apply_vim_effect(effect, window, cx);
        }
        cx.notify();
        true
    }

    fn apply_vim_effect(&mut self, effect: Effect, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.editing else {
            return;
        };
        match effect {
            Effect::BlockUp { count, col } => self.vim_move(ix, count, false, col, cx),
            Effect::BlockDown { count, col } => self.vim_move(ix, count, true, col, cx),
            Effect::FirstBlock => self.vim_jump(0, false, cx),
            Effect::LastBlock => {
                let visible = self.pages[self.selected].visible_blocks(&self.collapsed);
                let last = visible.iter().rposition(|&v| v).unwrap_or(0);
                self.vim_jump(last, true, cx);
            }
            Effect::OpenBelow => {
                self.record_edit();
                self.sync_content();
                let page = &mut self.pages[self.selected];
                // Like Enter at the end of the block: on a folded block the
                // new one goes after its hidden children.
                let folded =
                    self.collapsed.contains(&page.blocks[ix].id) && page.descendant_count(ix) > 0;
                let new_ix = if folded {
                    page.insert_after_subtree(ix, String::new())
                } else {
                    page.insert_after(ix, String::new())
                };
                self.save_page();
                self.load_editor(new_ix, true);
            }
            Effect::OpenAbove => {
                self.record_edit();
                self.sync_content();
                let new_ix = self.pages[self.selected].insert_before(ix, String::new());
                self.save_page();
                self.load_editor(new_ix, true);
            }
            Effect::DeleteBlocks(n) => self.vim_take_blocks(ix, n, true),
            Effect::YankBlocks(n) => self.vim_take_blocks(ix, n, false),
            Effect::PasteBlocks { before } => {
                let Register::Blocks(source) = &self.vim.register else {
                    return;
                };
                let source = source.clone();
                let state = self.history_state();
                self.sync_content();
                let page = &mut self.pages[self.selected];
                let range = if before {
                    page.insert_blocks_before(ix, &source)
                } else {
                    page.insert_blocks_from(Some(ix), &source)
                };
                if !range.is_empty() {
                    self.record_state(state);
                    self.save_page();
                    self.load_editor(range.start, true);
                }
            }
            Effect::Undo(n) | Effect::Redo(n) => {
                let redo = matches!(effect, Effect::Redo(_));
                for _ in 0..n {
                    if redo {
                        self.redo(&super::Redo, window, cx);
                    } else {
                        self.undo(&Undo, window, cx);
                    }
                }
                vim::clamp_normal(&mut self.editor);
            }
            Effect::Indent(n) => {
                for _ in 0..n {
                    self.restructure(cx, |page, ix| page.indent(ix));
                }
            }
            Effect::Outdent(n) => {
                for _ in 0..n {
                    self.restructure(cx, |page, ix| page.outdent(ix));
                }
            }
            Effect::Write => self.vim_write(cx),
            Effect::WriteQuit => {
                self.vim_write(cx);
                self.stop_edit(cx);
            }
            Effect::Quit | Effect::StopEditing => self.stop_edit(cx),
            Effect::Error(text) => self.show_status(Status { text, error: true }, cx),
        }
    }

    /// `j` / `k` past the block's edge: `count` visible blocks on.
    fn vim_move(
        &mut self,
        ix: usize,
        count: usize,
        down: bool,
        col: usize,
        cx: &mut Context<Self>,
    ) {
        let mut target = ix;
        for _ in 0..count {
            match self.visible_neighbor(target, down) {
                Some(next) => target = next,
                None => break,
            }
        }
        if target != ix {
            self.move_edit(target, cx);
            vim::place_cursor(&mut self.editor, !down, col);
        }
    }

    /// `gg` / `G`: edit block `target`, at the start of its first (last)
    /// line.
    fn vim_jump(&mut self, target: usize, last_line: bool, cx: &mut Context<Self>) {
        if Some(target) != self.editing {
            self.move_edit(target, cx);
        }
        vim::place_cursor(&mut self.editor, last_line, 0);
    }

    /// `dd` / `yy`: block `ix` and the `n - 1` after it (with their
    /// children) into the register, removed if `delete`. Editing goes on
    /// in the block that takes their place.
    fn vim_take_blocks(&mut self, ix: usize, n: usize, delete: bool) {
        let state = self.history_state();
        self.sync_content();
        let page = &self.pages[self.selected];
        let end = page.subtrees_end(ix, n);
        self.vim.register = Register::Blocks(page.copy_blocks(ix..end));
        if !delete {
            return;
        }
        self.record_state(state);
        let page = &mut self.pages[self.selected];
        page.remove_blocks(ix..end);
        let target = ix.min(page.blocks.len() - 1);
        self.save_page();
        self.load_editor(target, true);
        vim::clamp_normal(&mut self.editor);
    }

    /// `:w`: save the page now (it is saved on every change anyway) and
    /// say so.
    fn vim_write(&mut self, cx: &mut Context<Self>) {
        self.sync_content();
        self.save_page();
        let title = self.pages[self.selected].title.clone();
        self.show_status(
            Status {
                text: format!("Saved \u{201c}{title}\u{201d}"),
                error: false,
            },
            cx,
        );
    }

    /// Settings' Vim row and the palette command.
    pub(super) fn set_vim_mode(&mut self, on: bool, cx: &mut Context<Self>) {
        self.config.vim_mode = on;
        self.save_config();
        self.vim.reset();
        cx.notify();
    }

    pub(super) fn on_toggle_vim_mode(
        &mut self,
        _: &ToggleVimMode,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let on = !self.config.vim_mode;
        self.set_vim_mode(on, cx);
        let text = if on {
            "Vim mode is on"
        } else {
            "Vim mode is off"
        };
        self.show_status(
            Status {
                text: text.to_string(),
                error: false,
            },
            cx,
        );
    }

    /// The mode indicator's text: the mode (and an unfinished command),
    /// or the `:` line being typed. `None` when vim isn't in use.
    pub(super) fn vim_label(&self) -> Option<String> {
        if !self.vim_applies() {
            return None;
        }
        Some(match self.vim.cmdline() {
            Some(line) => format!(":{line}"),
            None => {
                let pending = self.vim.pending();
                let label = self.vim.mode.label();
                if pending.is_empty() {
                    label.to_string()
                } else {
                    format!("{label}  {pending}")
                }
            }
        })
    }

    /// The mode indicator: a small pill at the bottom left of the page.
    pub(super) fn render_vim_pill(&self) -> Option<AnyElement> {
        let label = self.vim_label()?;
        let theme = self.theme;
        let accent = self.vim.mode != Mode::Normal || self.vim.cmdline().is_some();
        Some(
            div()
                .debug_selector(|| "vim-mode".to_string())
                .absolute()
                .bottom(px(16.0))
                // Just right of the sidebar (240 px wide).
                .left(px(256.0))
                .px_2()
                .py(px(2.0))
                .rounded_md()
                .border_1()
                .border_color(if accent { theme.accent } else { theme.border })
                .bg(theme.sidebar_bg)
                .text_size(px(self.config.font_size * 0.8))
                .text_color(if accent { theme.accent } else { theme.muted })
                .when_some(self.mono_font.clone(), |d, font| d.font_family(font))
                .child(label)
                .into_any_element(),
        )
    }

    /// Settings > General, "Editor": vim keybindings off / on.
    pub(super) fn render_vim_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let on = self.config.vim_mode;
        let button = |id: &'static str, label: &'static str, active: bool| {
            div()
                .id(id)
                .debug_selector(move || id.to_string())
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
        div()
            .debug_selector(|| "settings-editor".to_string())
            .flex()
            .flex_col()
            .gap_2()
            .pt_2()
            .mt_1()
            .border_t_1()
            .border_color(theme.border)
            .child(div().text_color(theme.text).child("Editor"))
            .child(div().text_color(theme.muted).child("Vim keybindings"))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(button("vim-mode-off", "Off", !on).on_click(
                        cx.listener(|this, _e, _window, cx| this.set_vim_mode(false, cx)),
                    ))
                    .child(button("vim-mode-on", "On", on).on_click(
                        cx.listener(|this, _e, _window, cx| this.set_vim_mode(true, cx)),
                    )),
            )
            .child(div().text_color(theme.muted).child(
                "Blocks open in Normal mode: i, a or o to type, Esc back to Normal, \
                 Esc again to stop editing. :w saves.",
            ))
            .into_any_element()
    }
}
