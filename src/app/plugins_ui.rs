//! WASM plugins in the app (decision 55): loading, the Settings > Plugins
//! section, plugin commands in the palette, applying their actions, and
//! render hooks under blocks. The sandbox and formats are in
//! `crate::plugins`.
//!
//! Commands run on the background executor (50M fuel); their actions are
//! checked and applied here, each mutating one an undo step saved like
//! any edit. Render hooks run while drawing with a small budget (5M fuel)
//! and are cached per (binary, args, block text). Three failures in a row
//! turn a plugin off, with a message.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use gpui::{div, prelude::*, px, AnyElement, Context, FontWeight, Task};
use uuid::Uuid;

use super::{Mode, Nav, NoteSec, Status};
use crate::editor::EditorState;
use crate::plugins::marketplace::{self, RegistryEntry};
use crate::plugins::protocol::{self, Action, Line};
use crate::plugins::sandbox::{self, Compiled, COMMAND_FUEL, RENDER_FUEL};
use crate::plugins::{self, Plugin, MAX_FAILURES};

const RENDER_CACHE: usize = 256;

type RenderResult = Result<Vec<Line>, String>;

#[derive(Default)]
pub(super) struct PluginsState {
    pub(super) found: Vec<Plugin>,
    pub(super) errors: Vec<(String, String)>,
    /// Compiled binaries by hash.
    compiled: RefCell<HashMap<String, Arc<Compiled>>>,
    /// Failures in a row, by plugin id.
    failures: RefCell<HashMap<String, u32>>,
    render_cache: RefCell<HashMap<(String, String, String), RenderResult>>,
    /// Plugins to turn off (found failing while drawing).
    pending_disable: RefCell<Vec<(String, String)>>,
    task: Option<Task<()>>,
    /// Marketplace (Browse tab): true shows the registry, false the
    /// installed list.
    browse: bool,
    registry: Vec<RegistryEntry>,
    registry_error: Option<String>,
    fetching: bool,
    installing: Option<String>,
    browse_task: Option<Task<()>>,
}

/// What a command ran against, to check before applying its actions.
struct CommandContext {
    page: Option<String>,
    block: Option<Uuid>,
}

impl NoteSec {
    /// (Re)read `plugins/`. Called at startup and from Settings. Keeps the
    /// Browse tab (registry listing) across reloads.
    pub(super) fn reload_plugins(&mut self) {
        let (found, errors) = plugins::discover(self.storage.root());
        let keep = std::mem::take(&mut self.plugins);
        self.plugins = PluginsState {
            found,
            errors,
            browse: keep.browse,
            registry: keep.registry,
            registry_error: keep.registry_error,
            fetching: keep.fetching,
            installing: keep.installing,
            browse_task: keep.browse_task,
            ..Default::default()
        };
    }

    /// Enabled, and with the binary that was enabled (a changed
    /// `plugin.wasm` must be enabled again).
    pub(super) fn plugin_enabled(&self, p: &Plugin) -> bool {
        self.config.plugins.get(&p.id) == Some(&p.hash)
    }

    pub(super) fn set_plugin_enabled(&mut self, id: &str, on: bool, cx: &mut Context<Self>) {
        let Some(p) = self.plugins.found.iter().find(|p| p.id == id) else {
            return;
        };
        if on {
            self.config.plugins.insert(p.id.clone(), p.hash.clone());
        } else {
            self.config.plugins.remove(id);
        }
        self.plugins.failures.borrow_mut().remove(id);
        self.plugins.render_cache.borrow_mut().clear();
        self.save_config();
        cx.notify();
    }

    /// Fetch the marketplace index on the background executor (docs/MARKETPLACE.md).
    pub(super) fn fetch_registry(&mut self, cx: &mut Context<Self>) {
        if self.plugins.fetching {
            return;
        }
        self.plugins.fetching = true;
        self.plugins.registry_error = None;
        cx.notify();
        let job = cx.background_spawn(async move {
            marketplace::fetch_index(marketplace::DEFAULT_REGISTRY_URL)
        });
        self.plugins.browse_task = Some(cx.spawn(async move |this, cx| {
            let result = job.await;
            let _ = this.update(cx, |this, cx| {
                this.plugins.fetching = false;
                this.plugins.browse_task = None;
                match result {
                    Ok(entries) => this.plugins.registry = entries,
                    Err(err) => this.plugins.registry_error = Some(err),
                }
                cx.notify();
            });
        }));
    }

    /// One-click install: download the WASM, verify its sha256 against the
    /// index, write it, then reload and enable. A mismatch is refused loudly
    /// and installs nothing.
    pub(super) fn install_registry_plugin(&mut self, id: String, cx: &mut Context<Self>) {
        if self.plugins.fetching || self.plugins.installing.is_some() {
            return;
        }
        let Some(entry) = self.plugins.registry.iter().find(|e| e.id == id).cloned() else {
            return;
        };
        self.plugins.installing = Some(id);
        self.plugins.registry_error = None;
        cx.notify();
        let root = self.storage.root().to_path_buf();
        let job = cx.background_spawn(async move {
            let bytes = marketplace::download_wasm(&entry.download_url)?;
            marketplace::install(&root, &entry, &bytes)?;
            Ok::<_, String>(entry.id.clone())
        });
        self.plugins.browse_task = Some(cx.spawn(async move |this, cx| {
            let result = job.await;
            let _ = this.update(cx, |this, cx| {
                this.plugins.installing = None;
                this.plugins.browse_task = None;
                match result {
                    Ok(installed) => {
                        this.reload_plugins();
                        this.set_plugin_enabled(&installed, true, cx);
                        let text = format!("Plugin {installed} installed, verified and enabled");
                        this.show_status(Status { text, error: false }, cx);
                    }
                    Err(err) => {
                        this.plugins.registry_error = Some(err.clone());
                        this.show_status(
                            Status {
                                text: err,
                                error: true,
                            },
                            cx,
                        );
                    }
                }
                cx.notify();
            });
        }));
    }

    /// The palette's plugin entries: (plugin, command) indices and labels.
    pub(super) fn plugin_commands(&self) -> Vec<(usize, usize, String)> {
        self.plugins
            .found
            .iter()
            .enumerate()
            .filter(|(_, p)| self.plugin_enabled(p))
            .flat_map(|(pi, p)| {
                p.commands
                    .iter()
                    .enumerate()
                    .map(move |(ci, c)| (pi, ci, format!("Plugin: {}", c.label)))
            })
            .collect()
    }

    fn compiled_plugin(&self, p: &Plugin) -> Result<Arc<Compiled>, String> {
        if let Some(c) = self.plugins.compiled.borrow().get(&p.hash) {
            return Ok(c.clone());
        }
        let bytes =
            std::fs::read(&p.wasm).map_err(|err| format!("can't read plugin.wasm: {err}"))?;
        if plugins::wasm_hash(&bytes) != p.hash {
            return Err(
                "plugin.wasm changed since it was loaded: reload and enable it again".into(),
            );
        }
        let c = Arc::new(sandbox::compile(&bytes)?);
        self.plugins
            .compiled
            .borrow_mut()
            .insert(p.hash.clone(), c.clone());
        Ok(c)
    }

    /// A failure: counted; the third in a row turns the plugin off.
    /// Returns the message to show.
    fn plugin_failure(&self, p: &Plugin, err: &str) -> String {
        let mut failures = self.plugins.failures.borrow_mut();
        let n = failures.entry(p.id.clone()).or_default();
        *n += 1;
        if *n >= MAX_FAILURES {
            self.plugins
                .pending_disable
                .borrow_mut()
                .push((p.id.clone(), err.to_string()));
            format!(
                "Plugin {} failed {n} times and was turned off: {err}",
                p.name
            )
        } else {
            format!("Plugin {} failed: {err}", p.name)
        }
    }

    /// Turn off the plugins found failing (called at the start of render).
    pub(super) fn apply_plugin_disables(&mut self, cx: &mut Context<Self>) {
        let pending = std::mem::take(&mut *self.plugins.pending_disable.borrow_mut());
        for (id, err) in pending {
            if self.config.plugins.remove(&id).is_some() {
                self.save_config();
                let text =
                    format!("Plugin {id} was turned off after {MAX_FAILURES} failures: {err}");
                self.show_status(Status { text, error: true }, cx);
            }
        }
    }

    /// Run palette entry `entry` (see `plugin_commands`).
    pub(super) fn run_plugin_command(&mut self, entry: usize, cx: &mut Context<Self>) {
        let Some(&(pi, ci, _)) = self.plugin_commands().get(entry) else {
            return;
        };
        if self.plugins.task.is_some() {
            let text = "A plugin command is still running".to_string();
            return self.show_status(Status { text, error: true }, cx);
        }
        let p = self.plugins.found[pi].clone();
        let command = p.commands[ci].id.clone();
        let compiled = match self.compiled_plugin(&p) {
            Ok(c) => c,
            Err(err) => {
                let text = self.plugin_failure(&p, &err);
                return self.show_status(Status { text, error: true }, cx);
            }
        };
        self.commit();
        let page = (self.mode == Mode::Notes).then(|| self.pages[self.selected].title.clone());
        let (block, text, selection) = match self.editing {
            Some(ix) => (
                Some(self.pages[self.selected].blocks[ix].id),
                self.editor.text.clone(),
                self.editor.text[self.editor.selected_range()].to_string(),
            ),
            None => (None, String::new(), String::new()),
        };
        let input =
            protocol::command_input(&command, page.as_deref().unwrap_or(""), &text, &selection);
        let context = CommandContext { page, block };
        let job = cx.background_spawn(async move {
            sandbox::call(&compiled, "run_command", input.as_bytes(), COMMAND_FUEL)
                .and_then(|out| protocol::parse_actions(&out.bytes).map(|a| (a, out.log)))
        });
        self.plugins.task = Some(cx.spawn(async move |this, cx| {
            let result = job.await;
            let _ = this.update(cx, |this, cx| {
                this.plugins.task = None;
                this.plugin_command_done(p, context, result, cx);
            });
        }));
    }

    fn plugin_command_done(
        &mut self,
        p: Plugin,
        context: CommandContext,
        result: Result<(Vec<Action>, Vec<String>), String>,
        cx: &mut Context<Self>,
    ) {
        let actions = match result {
            Ok((actions, log)) => {
                for line in log {
                    eprintln!("notesec: plugin {}: {line}", p.id);
                }
                actions
            }
            Err(err) => {
                let text = self.plugin_failure(&p, &err);
                self.apply_plugin_disables(cx);
                return self.show_status(Status { text, error: true }, cx);
            }
        };
        self.plugins.failures.borrow_mut().remove(&p.id);
        let same_place = context.page.as_ref().is_some_and(|t| {
            self.mode == Mode::Notes
                && self.pages[self.selected].title == *t
                && self
                    .editing
                    .map(|ix| self.pages[self.selected].blocks[ix].id)
                    == context.block
        });
        for action in actions {
            let refuse = |this: &mut Self, why: &str, cx: &mut Context<Self>| {
                let text = format!("Plugin {}: {why}", p.name);
                this.show_status(Status { text, error: true }, cx);
            };
            match action {
                Action::SetStatus(text) => {
                    let text = format!("{}: {text}", p.name);
                    self.show_status(Status { text, error: false }, cx);
                }
                Action::OpenPage(title) => match self.find_page(&title) {
                    Some(ix) => {
                        let title = self.pages[ix].title.clone();
                        self.navigate(&title, Nav::Tab, cx);
                    }
                    None => refuse(self, &format!("no page called \u{201c}{title}\u{201d}"), cx),
                },
                Action::InsertBlock(_) | Action::ReplaceBlock(_) if !same_place => refuse(
                    self,
                    "the page changed while it ran, so nothing was changed",
                    cx,
                ),
                Action::InsertBlock(text) => {
                    self.record_edit();
                    let page = &mut self.pages[self.selected];
                    match self.editing {
                        Some(ix) => {
                            page.insert_after(ix, text);
                        }
                        None => {
                            let last = (0..page.blocks.len())
                                .rev()
                                .find(|&i| page.blocks[i].parent_id.is_none())
                                .unwrap_or(0);
                            page.insert_after_subtree(last, text);
                        }
                    }
                    self.save_page();
                }
                Action::ReplaceBlock(text) => {
                    if self.editing.is_none() {
                        refuse(self, "replace_block needs a block being edited", cx);
                        continue;
                    }
                    self.record_edit();
                    self.editor = EditorState::new(&text);
                    self.editor.cursor = self.editor.text.len();
                    self.sync_content();
                    self.save_page();
                }
            }
        }
        cx.notify();
    }

    /// The boxes the render hooks draw under a block (reading view).
    pub(super) fn render_plugin_boxes(&self, content: &str) -> Option<AnyElement> {
        if !content.contains("{{") {
            return None;
        }
        let theme = self.theme;
        let mut boxes = Vec::new();
        for p in self.plugins.found.iter().filter(|p| self.plugin_enabled(p)) {
            let Some(name) = &p.render else { continue };
            for args in plugins::macro_calls(content, name) {
                let key = (p.hash.clone(), args.clone(), content.to_string());
                let cached = self.plugins.render_cache.borrow().get(&key).cloned();
                let result = cached.unwrap_or_else(|| {
                    let result = self.compiled_plugin(p).and_then(|c| {
                        let input = protocol::render_input(&args, content);
                        sandbox::call(&c, "render", input.as_bytes(), RENDER_FUEL)
                            .and_then(|out| protocol::parse_render(&out.bytes))
                    });
                    let result = match result {
                        Ok(lines) => {
                            self.plugins.failures.borrow_mut().remove(&p.id);
                            Ok(lines)
                        }
                        Err(err) => Err(self.plugin_failure(p, &err)),
                    };
                    let mut cache = self.plugins.render_cache.borrow_mut();
                    if cache.len() >= RENDER_CACHE {
                        cache.clear();
                    }
                    cache.insert(key, result.clone());
                    result
                });
                let id = p.id.clone();
                let body: Vec<AnyElement> = match result {
                    Ok(lines) => lines
                        .into_iter()
                        .map(|l| {
                            div()
                                .when(l.bold, |d| d.font_weight(FontWeight::BOLD))
                                .when(l.italic, |d| d.italic())
                                .child(l.text)
                                .into_any_element()
                        })
                        .collect(),
                    Err(err) => vec![div().text_color(theme.danger).child(err).into_any_element()],
                };
                boxes.push(
                    div()
                        .debug_selector(move || format!("plugin-render-{id}"))
                        .mt_1()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .border_1()
                        .border_color(theme.border)
                        .flex()
                        .flex_col()
                        .children(body),
                );
            }
        }
        (!boxes.is_empty()).then(|| div().flex().flex_col().children(boxes).into_any_element())
    }

    pub(super) fn render_plugin_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let browse = self.plugins.browse;
        let tab = |id: &'static str, label: &str, active: bool| {
            div()
                .id(id)
                .debug_selector(move || id.to_string())
                .px_3()
                .py_1()
                .rounded_md()
                .border_1()
                .border_color(if active { theme.accent } else { theme.border })
                .when(active, |d| d.bg(theme.selected_bg))
                .cursor_pointer()
                .child(label.to_string())
        };
        let tabs = div().flex().flex_row().gap_2().child(
            tab("plugins-tab-installed", "Installed", !browse).on_click(cx.listener(
                |this, _, _, cx| {
                    this.plugins.browse = false;
                    cx.notify();
                },
            )),
        );
        let tabs = tabs.child(
            tab("plugins-tab-browse", "Browse", browse).on_click(cx.listener(|this, _, _, cx| {
                this.plugins.browse = true;
                if this.plugins.registry.is_empty() && this.plugins.registry_error.is_none() {
                    this.fetch_registry(cx);
                }
                cx.notify();
            })),
        );
        if browse {
            return div()
                .flex()
                .flex_col()
                .gap_2()
                .child(tabs)
                .child(self.render_registry_browse(cx))
                .into_any_element();
        }
        let rows = self.plugins.found.iter().map(|p| {
            let on = self.plugin_enabled(p);
            let changed = !on && self.config.plugins.contains_key(&p.id);
            let id = p.id.clone();
            let toggle_id = format!("plugin-toggle-{}", p.id);
            let selector = toggle_id.clone();
            let mut what: Vec<String> = p.commands.iter().map(|c| format!("command \u{201c}{}\u{201d}", c.label)).collect();
            if let Some(m) = &p.render {
                what.push(format!("{{{{{m}}}}} in blocks"));
            }
            div()
                .flex()
                .flex_row()
                .justify_between()
                .gap_3()
                .py_1()
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .flex()
                        .flex_col()
                        .child(div().font_weight(FontWeight::BOLD).child(format!("{} {}", p.name, p.version)))
                        .when(!p.description.is_empty(), |d| d.child(div().text_color(theme.muted).child(p.description.clone())))
                        .child(div().text_color(theme.muted).child(what.join(" \u{00b7} ")))
                        .when(changed, |d| {
                            d.child(div().text_color(theme.danger).child("plugin.wasm changed since you enabled it: check it, then enable it again"))
                        }),
                )
                .child(
                    div()
                        .id(gpui::SharedString::from(toggle_id))
                        .debug_selector(move || selector.clone())
                        .flex_shrink_0()
                        .px_3()
                        .py_1()
                        .rounded_md()
                        .border_1()
                        .border_color(if on { theme.accent } else { theme.border })
                        .when(on, |d| d.bg(theme.selected_bg))
                        .cursor_pointer()
                        .child(if on { "Enabled" } else { "Disabled" })
                        .on_click(cx.listener(move |this, _, _, cx| this.set_plugin_enabled(&id, !on, cx))),
                )
        });
        let errors = self.plugins.errors.iter().map(|(name, err)| {
            div()
                .text_color(theme.danger)
                .child(format!("plugins/{name}: {err}"))
        });
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(tabs)
            .child(div().text_color(theme.muted).child(
                "Plugins live in plugins/<id>/ in your notes folder and run in a sandbox: no files, \
                 network or other pages, only the block you run them on. Enabling one is still a \
                 trust decision. New and changed plugins start disabled.",
            ))
            .when(self.plugins.found.is_empty(), |d| {
                d.child(div().text_color(theme.muted).child("No plugins found."))
            })
            .children(rows)
            .children(errors)
            .child(
                div()
                    .id("plugins-reload")
                    .debug_selector(|| "plugins-reload".to_string())
                    .px_3()
                    .py_1()
                    .w(px(140.0))
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .cursor_pointer()
                    .child("Reload plugins")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.reload_plugins();
                        cx.notify();
                    })),
            )
            .into_any_element()
    }

    /// Settings > Plugins > Browse: the community registry (docs/MARKETPLACE.md).
    fn render_registry_browse(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let mut items: Vec<AnyElement> = Vec::new();
        items.push(
            div()
                .text_color(theme.muted)
                .child(
                    "Community plugins from the public notesec-plugins registry. Every listing \
                     passed review (\u{2713} Verified). Installing downloads the plugin, checks its \
                     sha256 against the index, and enables it \u{2014} a mismatch is refused loudly \
                     and installs nothing.",
                )
                .into_any_element(),
        );
        items.push(
            div()
                .text_color(theme.muted)
                .child(marketplace::DEFAULT_REGISTRY_URL.to_string())
                .into_any_element(),
        );
        let fetching = self.plugins.fetching;
        items.push(
            div()
                .id("plugins-browse-refresh")
                .debug_selector(|| "plugins-browse-refresh".to_string())
                .px_3()
                .py_1()
                .w(px(140.0))
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .cursor_pointer()
                .child(if fetching {
                    "Fetching\u{2026}"
                } else {
                    "Refresh"
                })
                .on_click(cx.listener(|this, _, _, cx| this.fetch_registry(cx)))
                .into_any_element(),
        );
        if let Some(err) = &self.plugins.registry_error {
            items.push(
                div()
                    .text_color(theme.danger)
                    .child(err.clone())
                    .into_any_element(),
            );
        }
        if self.plugins.registry.is_empty() && !fetching {
            items.push(
                div()
                    .text_color(theme.muted)
                    .child("No plugins listed yet.")
                    .into_any_element(),
            );
        }
        for e in &self.plugins.registry {
            let installing = self.plugins.installing.as_deref() == Some(e.id.as_str());
            let installed = self.plugins.found.iter().find(|p| p.id == e.id);
            let action: AnyElement = if installing {
                div()
                    .text_color(theme.muted)
                    .child("Installing\u{2026}")
                    .into_any_element()
            } else {
                match installed {
                    Some(p) if p.version == e.version => div()
                        .text_color(theme.muted)
                        .child("Installed")
                        .into_any_element(),
                    _ => {
                        let label = if installed.is_some() {
                            "Update"
                        } else {
                            "Install"
                        };
                        let id = e.id.clone();
                        let selector = format!("plugin-install-{}", e.id);
                        div()
                            .id(gpui::SharedString::from(format!("plugin-install-{}", e.id)))
                            .debug_selector(move || selector.clone())
                            .flex_shrink_0()
                            .px_3()
                            .py_1()
                            .rounded_md()
                            .border_1()
                            .border_color(theme.accent)
                            .bg(theme.selected_bg)
                            .cursor_pointer()
                            .child(label)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.install_registry_plugin(id.clone(), cx);
                            }))
                            .into_any_element()
                    }
                }
            };
            items.push(
                div()
                    .flex()
                    .flex_row()
                    .justify_between()
                    .gap_3()
                    .py_1()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .gap_2()
                                    .child(
                                        div()
                                            .font_weight(FontWeight::BOLD)
                                            .child(format!("{} {}", e.name, e.version)),
                                    )
                                    .child(
                                        div().text_color(theme.accent).child("\u{2713} Verified"),
                                    ),
                            )
                            .when(!e.description.is_empty(), |d| {
                                d.child(div().text_color(theme.muted).child(e.description.clone()))
                            })
                            .child(
                                div()
                                    .text_color(theme.muted)
                                    .child(format!("by {}", e.author)),
                            ),
                    )
                    .child(action)
                    .into_any_element(),
            );
        }
        div()
            .flex()
            .flex_col()
            .gap_2()
            .children(items)
            .into_any_element()
    }

    /// Failures in a row (tests).
    #[cfg(test)]
    pub(super) fn plugin_failures(&self, id: &str) -> u32 {
        self.plugins.failures.borrow().get(id).copied().unwrap_or(0)
    }

    /// Text of the cached render-hook lines (tests).
    #[cfg(test)]
    pub(super) fn plugin_render_texts(&self) -> Vec<String> {
        let cache = self.plugins.render_cache.borrow();
        let mut texts: Vec<String> = cache
            .values()
            .flat_map(|r| match r {
                Ok(lines) => lines.iter().map(|l| l.text.clone()).collect(),
                Err(err) => vec![err.clone()],
            })
            .collect();
        texts.sort();
        texts
    }
}
