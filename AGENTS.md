# notesec — agent instructions

## Project
GPU-rendered outliner notes app (Logseq-style) in Rust + GPUI. Public open-source, MIT.

## Iron rules
1. Never invent GPUI APIs. Before using any GPUI type/method, check
   ~/gpui-playground/crates/gpui (especially examples/) or https://gpui.rs.
   If unsure, stop and ask the human.
2. One MVP feature at a time (list below). `cargo build` must pass with
   zero warnings before moving on.
3. No `unsafe` to make things compile. No disabling features to dodge errors.
4. Linux first (Wayland + X11). No Electron, no webviews.

## MVP order
1. Window + sidebar (pages) + block list  <-- current
2. Block editing (Enter / Tab / Shift-Tab / Backspace)
3. [[wikilinks]]
4. Backlinks panel
5. Fuzzy search
6. #tags
7. Themes + fonts

## Non-goals (v1)
sync, mobile, collaboration, plugins, graph view, PDFs, whiteboards, encryption.

## Conventions (already decided — don't re-litigate)
- Blocks stored flat per page: { id, content, parent_id, page_id, order }.
  Derive visible order via DFS.
- Disk format is Logseq-compatible markdown: `- ` bullets, 2-space indents,
  `journals/` + `pages/` folders, `___` for namespaced pages.
- Notes dir: ~/.notesec (env var / CLI flag can override).
- Saves are atomic (tmp file + rename), on edit-exit or structural change —
  never on every keystroke.
- Block IDs regenerate on load; don't persist them.
