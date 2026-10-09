//! "Import from Obsidian / Logseq / Notion…" in the app (decision 50): a
//! folder picker, then `import::plan` and `import::apply` on a background
//! thread, a dialog when page names are taken, and the summary as a status
//! (the import log page opens).

use gpui::{div, prelude::*, px, AnyElement, Context, FontWeight, PathPromptOptions, Task, Window};

use super::{ImportLogseq, ImportNotion, ImportObsidian, NoteSec, Status};
use crate::import::{self, Existing, OnClash, Plan, Source, Summary};
use crate::model::page_aliases;

/// The import in progress.
#[derive(Default)]
pub(super) struct ImportState {
    /// Reading or writing in the background; another import waits.
    task: Option<Task<()>>,
    /// Read, with names that are taken: the dialog asks what to do.
    pub(super) pending: Option<Plan>,
}

impl NoteSec {
    pub(super) fn on_import_obsidian(
        &mut self,
        _: &ImportObsidian,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pick_import_folder(Source::Obsidian, cx);
    }

    pub(super) fn on_import_logseq(
        &mut self,
        _: &ImportLogseq,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pick_import_folder(Source::Logseq, cx);
    }

    pub(super) fn on_import_notion(
        &mut self,
        _: &ImportNotion,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pick_import_folder(Source::Notion, cx);
    }

    fn import_busy(&mut self, cx: &mut Context<Self>) -> bool {
        let busy = self.import.task.is_some() || self.import.pending.is_some();
        if busy {
            let text = "An import is already running".to_string();
            self.show_status(Status { text, error: true }, cx);
        }
        busy
    }

    /// Ask for the export folder, then read it.
    fn pick_import_folder(&mut self, source: Source, cx: &mut Context<Self>) {
        if self.import_busy(cx) {
            return;
        }
        let prompt = match source {
            Source::Obsidian => "Import this Obsidian vault",
            Source::Logseq => "Import this Logseq graph",
            Source::Notion => "Import this unzipped Notion export",
        };
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(prompt.into()),
        });
        self.import.task = Some(cx.spawn(async move |this, cx| {
            let folder = match picked.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                Ok(Err(err)) => {
                    let _ = this.update(cx, |this, cx| {
                        this.import.task = None;
                        let text = format!("Could not open the folder picker: {err}");
                        this.show_status(Status { text, error: true }, cx);
                    });
                    return;
                }
                _ => None,
            };
            let _ = this.update(cx, |this, cx| {
                this.import.task = None;
                if let Some(folder) = folder {
                    this.read_import(source, folder, cx);
                }
            });
        }));
    }

    /// What the graph has, for clashes and unresolved links.
    fn import_existing(&self) -> Existing {
        Existing {
            titles: self.pages.iter().map(|p| p.title.clone()).collect(),
            aliases: self.pages.iter().flat_map(page_aliases).collect(),
            ids: self
                .pages
                .iter()
                .flat_map(|p| p.saved_ids.iter().copied())
                .collect(),
        }
    }

    /// Read `folder` in the background; then import, or ask about clashes.
    pub(super) fn read_import(
        &mut self,
        source: Source,
        folder: std::path::PathBuf,
        cx: &mut Context<Self>,
    ) {
        self.stop_edit(cx);
        let text = format!("Reading {}\u{2026}", folder.display());
        self.show_status(Status { text, error: false }, cx);
        let existing = self.import_existing();
        let root = self.storage.root().to_path_buf();
        self.import.task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { import::plan(source, &folder, &root, &existing) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.import.task = None;
                match result {
                    Err(text) => this.show_status(Status { text, error: true }, cx),
                    Ok(plan) if plan.clashes.is_empty() => {
                        this.run_import(plan, OnClash::Rename, cx)
                    }
                    Ok(plan) => {
                        this.import.pending = Some(plan);
                        cx.notify();
                    }
                }
            });
        }));
    }

    /// The clash dialog's buttons (`None`: cancel).
    pub(super) fn answer_import_clash(&mut self, choice: Option<OnClash>, cx: &mut Context<Self>) {
        let Some(plan) = self.import.pending.take() else {
            return;
        };
        match choice {
            Some(on_clash) => self.run_import(plan, on_clash, cx),
            None => {
                let text = "Import cancelled; nothing was written".to_string();
                self.show_status(Status { text, error: false }, cx);
            }
        }
    }

    /// Write the import in the background, then load the new pages.
    fn run_import(&mut self, plan: Plan, on_clash: OnClash, cx: &mut Context<Self>) {
        let text = format!(
            "Importing {} pages from {}\u{2026}",
            plan.drafts.len(),
            plan.source.name()
        );
        self.show_status(Status { text, error: false }, cx);
        let existing = self.import_existing();
        let root = self.storage.root().to_path_buf();
        let stamp = chrono::Local::now().format("%Y-%m-%d %H.%M").to_string();
        self.import.task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(
                    async move { import::apply(plan, on_clash, &root, &existing, &stamp) },
                )
                .await;
            let _ = this.update(cx, |this, cx| {
                this.import.task = None;
                match result {
                    Ok(summary) => this.import_done(summary, cx),
                    Err(text) => this.show_status(Status { text, error: true }, cx),
                }
            });
        }));
    }

    /// Load what was written, open the log page, show the summary.
    fn import_done(&mut self, summary: Summary, cx: &mut Context<Self>) {
        let root = self.storage.root().to_path_buf();
        let log = (summary.log_title.clone(), false);
        for (title, is_journal) in summary.pages.iter().chain(std::iter::once(&log)) {
            let path = crate::storage::page_file(&root, title, *is_journal);
            if let Ok(text) = std::fs::read_to_string(path) {
                self.add_page(crate::model::Page::from_markdown(title, *is_journal, &text));
            }
        }
        self.storage.note_external_change();
        self.forget_history();
        self.open_page(&summary.log_title, cx);
        self.show_status(
            Status {
                text: summary.text(),
                error: false,
            },
            cx,
        );
    }

    /// "N pages already exist": Rename (the default), Skip, Cancel.
    pub(super) fn render_import_dialog(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let plan = self.import.pending.as_ref()?;
        let theme = self.theme;
        let n = plan.clashes.len();
        let mut names: Vec<String> = plan
            .clashes
            .iter()
            .take(5)
            .map(|t| format!("\u{201c}{t}\u{201d}"))
            .collect();
        if n > 5 {
            names.push(format!("and {} more", n - 5));
        }
        let title = if n == 1 {
            format!("A page named {} already exists", names[0])
        } else {
            format!("{n} pages already exist")
        };
        let body = format!(
            "{}Importing never overwrites a page. Rename imports them with \u{201c}(imported)\u{201d} added \
             (links among the imported pages follow); Skip leaves them out, and links to them go to your pages.",
            if n > 1 { format!("{}. ", names.join(", ")) } else { String::new() }
        );
        let button = |id: &'static str, label: &'static str, primary: bool| {
            div()
                .id(id)
                .debug_selector(move || id.to_string())
                .px_3()
                .py_1()
                .rounded_md()
                .border_1()
                .border_color(if primary { theme.accent } else { theme.border })
                .when(primary, |d| {
                    d.bg(theme.accent)
                        .text_color(theme.bg)
                        .font_weight(FontWeight::BOLD)
                })
                .cursor_pointer()
                .hover(|d| d.opacity(0.85))
                .child(label)
        };
        Some(
            div()
                .id("import-clash-backdrop")
                .debug_selector(|| "import-clash-backdrop".to_string())
                .absolute()
                .inset_0()
                .occlude()
                .bg(gpui::black().opacity(0.45))
                .flex()
                .flex_col()
                .items_center()
                .pt(px(160.0))
                .on_click(cx.listener(|this, _e, _w, cx| this.answer_import_clash(None, cx)))
                .child(
                    div()
                        .id("import-clash")
                        .debug_selector(|| "import-clash".to_string())
                        .occlude()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .p_4()
                        .w(px(460.0))
                        .rounded_lg()
                        .bg(theme.sidebar_bg)
                        .border_1()
                        .border_color(theme.border)
                        .shadow_lg()
                        .child(div().font_weight(FontWeight::BOLD).child(title))
                        .child(div().text_color(theme.muted).child(body))
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .justify_end()
                                .gap_2()
                                .child(button("import-clash-cancel", "Cancel", false).on_click(
                                    cx.listener(|this, _e, _w, cx| {
                                        this.answer_import_clash(None, cx)
                                    }),
                                ))
                                .child(button("import-clash-skip", "Skip them", false).on_click(
                                    cx.listener(|this, _e, _w, cx| {
                                        this.answer_import_clash(Some(OnClash::Skip), cx)
                                    }),
                                ))
                                .child(
                                    button("import-clash-rename", "Rename with suffix", true)
                                        .on_click(cx.listener(|this, _e, _w, cx| {
                                            this.answer_import_clash(Some(OnClash::Rename), cx)
                                        })),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }
}
