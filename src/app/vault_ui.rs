//! Encrypted vault export/import in the app (decision 54): the passphrase
//! dialog and the file/folder pickers. The format and the crypto are in
//! `crate::vault`.
//!
//! The passphrase fields are ordinary `EditorState`s routed through the
//! app's text input (typing, paste, IME), drawn as bullets. Their text is
//! wiped when the dialog closes and the copy handed to the background
//! job is wiped when it's done; it is never logged or saved.

use std::path::PathBuf;

use gpui::{
    canvas, div, prelude::*, px, AnyElement, Context, ElementInputHandler, FontWeight,
    PathPromptOptions, Task, Window,
};

use super::{ExportVault, ImportVault, NoteSec, Status};
use crate::editor::EditorState;
use crate::vault::{self, Kdf, MIN_PASSPHRASE};

#[derive(Clone, Debug, PartialEq)]
pub(super) enum VaultMode {
    Export,
    Import { file: PathBuf, dest: PathBuf },
}

pub(super) struct VaultDialog {
    pub(super) mode: VaultMode,
    pass: EditorState,
    confirm: EditorState,
    /// 0: the passphrase, 1: its confirmation (export only).
    field: usize,
    pub(super) error: Option<String>,
    /// Encrypting or decrypting: keys are ignored.
    pub(super) busy: bool,
}

impl VaultDialog {
    fn new(mode: VaultMode) -> Self {
        VaultDialog {
            mode,
            pass: EditorState::new(""),
            confirm: EditorState::new(""),
            field: 0,
            error: None,
            busy: false,
        }
    }

    pub(super) fn field(&self) -> &EditorState {
        if self.field == 0 {
            &self.pass
        } else {
            &self.confirm
        }
    }

    pub(super) fn field_mut(&mut self) -> &mut EditorState {
        if self.field == 0 {
            &mut self.pass
        } else {
            &mut self.confirm
        }
    }

    fn wipe(&mut self) {
        vault::wipe_string(&mut self.pass.text);
        vault::wipe_string(&mut self.confirm.text);
        self.pass = EditorState::new("");
        self.confirm = EditorState::new("");
    }
}

impl Drop for VaultDialog {
    fn drop(&mut self) {
        self.wipe();
    }
}

#[derive(Default)]
pub(super) struct VaultState {
    pub(super) dialog: Option<VaultDialog>,
    task: Option<Task<()>>,
}

/// The Argon2 costs new vaults get (cheap in tests).
fn kdf() -> Kdf {
    #[cfg(test)]
    return vault::TEST_KDF;
    #[cfg(not(test))]
    vault::DEFAULT_KDF
}

/// Why a new passphrase isn't accepted yet (`None`: fine).
fn passphrase_problem(pass: &str, confirm: &str) -> Option<String> {
    let n = pass.chars().count();
    if n < MIN_PASSPHRASE {
        Some(format!(
            "At least {MIN_PASSPHRASE} characters ({n} so far); a few words is best"
        ))
    } else if pass != confirm {
        Some("The two passphrases don't match".into())
    } else {
        None
    }
}

impl NoteSec {
    pub(super) fn vault_dialog_open(&self) -> bool {
        self.vault.dialog.is_some()
    }

    pub(super) fn on_export_vault(
        &mut self,
        _: &ExportVault,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.vault.task.is_some() || self.overlay_open() {
            return;
        }
        self.stop_edit(cx);
        self.vault.dialog = Some(VaultDialog::new(VaultMode::Export));
        cx.notify();
    }

    /// Pick the vault file, then a new empty folder, then ask the passphrase.
    pub(super) fn on_import_vault(
        &mut self,
        _: &ImportVault,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.vault.task.is_some() || self.overlay_open() {
            return;
        }
        self.stop_edit(cx);
        let file = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Open this vault".into()),
        });
        self.vault.task = Some(cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = file.await else {
                let _ = this.update(cx, |this, _| this.vault.task = None);
                return;
            };
            let Some(file) = paths.into_iter().next() else {
                return;
            };
            let Ok(folder) = this.update(cx, |_, cx| {
                cx.prompt_for_paths(PathPromptOptions {
                    files: false,
                    directories: true,
                    multiple: false,
                    prompt: Some("Import into this new, empty folder".into()),
                })
            }) else {
                return;
            };
            let dest = match folder.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                _ => None,
            };
            let _ = this.update(cx, |this, cx| {
                this.vault.task = None;
                if let Some(dest) = dest {
                    this.ask_import_passphrase(file, dest, cx);
                }
            });
        }));
    }

    fn ask_import_passphrase(&mut self, file: PathBuf, dest: PathBuf, cx: &mut Context<Self>) {
        let problem = if dest.starts_with(self.storage.root())
            || self.storage.root().starts_with(&dest)
        {
            Some("That folder is (or holds) the open notes: pick a new, empty folder".to_string())
        } else {
            match std::fs::read_dir(&dest).map(|mut items| items.next().is_some()) {
                Ok(true) => Some(format!(
                    "{} isn't empty: pick a new, empty folder",
                    dest.display()
                )),
                Ok(false) => None,
                Err(err) => Some(format!("Can't use {}: {err}", dest.display())),
            }
        };
        match problem {
            Some(text) => self.show_status(Status { text, error: true }, cx),
            None => {
                self.vault.dialog = Some(VaultDialog::new(VaultMode::Import { file, dest }));
                cx.notify();
            }
        }
    }

    pub(super) fn close_vault_dialog(&mut self, cx: &mut Context<Self>) {
        if self.vault.dialog.as_ref().is_some_and(|d| d.busy) {
            return; // the job finishes, then it closes
        }
        self.vault.dialog = None; // wiped on drop
        cx.notify();
    }

    /// Tab / Shift-Tab in the export dialog: the other field.
    pub(super) fn vault_next_field(&mut self, cx: &mut Context<Self>) {
        if let Some(d) = &mut self.vault.dialog {
            if d.mode == VaultMode::Export {
                d.field = 1 - d.field;
                cx.notify();
            }
        }
    }

    /// Enter in the dialog.
    pub(super) fn confirm_vault(&mut self, cx: &mut Context<Self>) {
        let Some(d) = &mut self.vault.dialog else {
            return;
        };
        if d.busy {
            return;
        }
        match d.mode.clone() {
            VaultMode::Export => {
                if d.field == 0 && d.confirm.text.is_empty() {
                    d.field = 1;
                    return cx.notify();
                }
                if let Some(problem) = passphrase_problem(&d.pass.text, &d.confirm.text) {
                    d.error = Some(problem);
                    return cx.notify();
                }
                d.busy = true;
                d.error = None;
                let pass = d.pass.text.clone();
                self.pick_vault_destination(pass, cx);
            }
            VaultMode::Import { file, dest } => {
                if d.pass.text.is_empty() {
                    return;
                }
                d.busy = true;
                d.error = None;
                let mut pass = d.pass.text.clone();
                let job = cx.background_spawn(async move {
                    let result = vault::import(&file, &dest, &pass);
                    vault::wipe_string(&mut pass);
                    result.map(|n| (n, dest))
                });
                self.vault.task = Some(cx.spawn(async move |this, cx| {
                    let result = job.await;
                    let _ = this.update(cx, |this, cx| this.import_vault_done(result, cx));
                }));
                cx.notify();
            }
        }
    }

    fn pick_vault_destination(&mut self, mut pass: String, cx: &mut Context<Self>) {
        let dir = dirs::home_dir().unwrap_or_else(|| self.storage.root().to_path_buf());
        let name = format!(
            "notesec-{}.{}",
            chrono::Local::now().format("%Y-%m-%d"),
            vault::EXTENSION
        );
        let picked = cx.prompt_for_new_path(&dir, Some(&name));
        let root = self.storage.root().to_path_buf();
        self.vault.task = Some(cx.spawn(async move |this, cx| {
            let dest = match picked.await {
                Ok(Ok(Some(path))) => path,
                _ => {
                    vault::wipe_string(&mut pass);
                    let _ = this.update(cx, |this, cx| {
                        this.vault.task = None;
                        if let Some(d) = &mut this.vault.dialog {
                            d.busy = false;
                        }
                        cx.notify();
                    });
                    return;
                }
            };
            let dest = if dest.extension().is_some_and(|e| e == vault::EXTENSION) {
                dest
            } else {
                let mut name = dest.file_name().unwrap_or_default().to_os_string();
                name.push(format!(".{}", vault::EXTENSION));
                dest.with_file_name(name)
            };
            let job = cx.background_spawn(async move {
                let result = vault::export(&root, &dest, &pass, kdf());
                vault::wipe_string(&mut pass);
                result.map(|n| (n, dest))
            });
            let result = job.await;
            let _ = this.update(cx, |this, cx| this.export_vault_done(result, cx));
        }));
        cx.notify();
    }

    fn export_vault_done(
        &mut self,
        result: Result<(usize, PathBuf), String>,
        cx: &mut Context<Self>,
    ) {
        self.vault.task = None;
        match result {
            Ok((n, path)) => {
                self.vault.dialog = None;
                let text = format!("Encrypted {n} files into {}", path.display());
                self.show_status(Status { text, error: false }, cx);
            }
            Err(err) => {
                if let Some(d) = &mut self.vault.dialog {
                    d.busy = false;
                    d.error = Some(err);
                }
            }
        }
        cx.notify();
    }

    fn import_vault_done(
        &mut self,
        result: Result<(usize, PathBuf), String>,
        cx: &mut Context<Self>,
    ) {
        self.vault.task = None;
        match result {
            Ok((n, dest)) => {
                self.vault.dialog = None;
                // NoteSec opens one graph per run (no switching yet).
                let text = format!(
                    "Imported {n} files into {}. To open them, start NoteSec with NOTESEC_DIR={}",
                    dest.display(),
                    dest.display()
                );
                self.show_status(Status { text, error: false }, cx);
            }
            Err(err) => {
                if let Some(d) = &mut self.vault.dialog {
                    d.busy = false;
                    d.error = Some(err);
                    vault::wipe_string(&mut d.pass.text);
                    d.pass = EditorState::new("");
                }
            }
        }
        cx.notify();
    }

    pub(super) fn render_vault_dialog(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let d = self.vault.dialog.as_ref()?;
        let theme = self.theme;
        let export = d.mode == VaultMode::Export;
        let entity = cx.entity();
        let focus = self.focus_handle.clone();
        let field = |ix: usize, label: &'static str, editor: &EditorState| {
            let active = d.field == ix;
            let n = editor.text.chars().count();
            let before = editor.text[..editor.cursor.min(editor.text.len())]
                .chars()
                .count();
            let id = if ix == 0 {
                "vault-pass"
            } else {
                "vault-confirm"
            };
            let entity = entity.clone();
            let focus = focus.clone();
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(div().text_color(theme.muted).child(label))
                .child(
                    div()
                        .id(id)
                        .debug_selector(move || id.to_string())
                        .relative()
                        .h(px(30.0))
                        .px_2()
                        .flex()
                        .flex_row()
                        .items_center()
                        .rounded_md()
                        .border_1()
                        .border_color(if active { theme.accent } else { theme.border })
                        .bg(theme.bg)
                        .cursor_text()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(d) = &mut this.vault.dialog {
                                d.field = ix;
                                cx.notify();
                            }
                        }))
                        // Masked: one bullet per character, the caret between.
                        .child("\u{2022}".repeat(before))
                        .when(active, |el| {
                            el.child(div().w(px(1.5)).h(px(18.0)).bg(theme.accent))
                        })
                        .child("\u{2022}".repeat(n - before))
                        // Typing goes to the app's input handler while shown.
                        .when(active, |el| {
                            el.child(
                                canvas(
                                    |_, _, _| (),
                                    move |bounds, (), window, cx| {
                                        window.handle_input(
                                            &focus,
                                            ElementInputHandler::new(bounds, entity),
                                            cx,
                                        );
                                    },
                                )
                                .absolute()
                                .size_full(),
                            )
                        }),
                )
        };
        let (title, body) = if export {
            (
                "Export encrypted vault",
                "Your pages, journals, whiteboards, assets and settings in one encrypted file \
                 (no API keys or tokens). The passphrase is never stored: if it's lost, the \
                 vault can't be opened by anyone, you included.",
            )
        } else {
            (
                "Import encrypted vault",
                "The notes go into the new folder you picked; the open notes aren't touched.",
            )
        };
        let hint = if export {
            passphrase_problem(&d.pass.text, &d.confirm.text)
                .unwrap_or_else(|| "Looks good. Long passphrases (several words) are best.".into())
        } else {
            String::new()
        };
        Some(
            div()
                .id("vault-backdrop")
                .debug_selector(|| "vault-backdrop".to_string())
                .absolute()
                .inset_0()
                .occlude()
                .bg(gpui::black().opacity(0.45))
                .flex()
                .flex_col()
                .items_center()
                .pt(px(140.0))
                .child(
                    div()
                        .id("vault-dialog")
                        .debug_selector(|| "vault-dialog".to_string())
                        .occlude()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .p_4()
                        .w(px(460.0))
                        .rounded_lg()
                        .bg(theme.sidebar_bg)
                        .border_1()
                        .border_color(theme.border)
                        .shadow_lg()
                        .child(div().font_weight(FontWeight::BOLD).child(title))
                        .child(div().text_color(theme.muted).child(body))
                        .child(field(0, "Passphrase", &d.pass))
                        .when(export, |el| {
                            el.child(field(1, "Passphrase again", &d.confirm))
                        })
                        .when(!hint.is_empty(), |el| {
                            el.child(
                                div()
                                    .debug_selector(|| "vault-hint".to_string())
                                    .text_color(theme.muted)
                                    .child(hint),
                            )
                        })
                        .when_some(d.error.clone(), |el, err| {
                            el.child(
                                div()
                                    .debug_selector(|| "vault-error".to_string())
                                    .text_color(theme.danger)
                                    .child(err),
                            )
                        })
                        .child(div().text_color(theme.muted).child(if d.busy {
                            if export {
                                "Encrypting\u{2026}"
                            } else {
                                "Decrypting\u{2026}"
                            }
                        } else {
                            "Enter to continue \u{00b7} Tab: next field \u{00b7} Esc to cancel"
                        })),
                )
                .into_any_element(),
        )
    }
}
