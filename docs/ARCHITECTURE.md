# notesec — Architecture Notes

*How the app is designed, why it's built this way, and what Rust ideas it teaches.
Written from the actual codebase (~3300 lines).*

---

## 1. The big picture: layers

```
main.rs        — startup: open storage, load config, open the window
app.rs         — the GPUI root view: sidebar, page, editor, search overlay
model.rs       — data: Block, Page, link/tag parsing (NO ui code here)
storage.rs     — files on disk: load/save markdown (NO ui code here)
editor.rs      — text-editing state machine (NO ui code here)
display.rs     — reading view of a block: hidden markup + offset map (NO ui code)
search.rs      — fuzzy matcher, pure functions (NO ui code here)
tabs.rs        — open tabs: open/focus/close/cycle rules (NO ui code here)
export.rs      — a page as one self-contained HTML string (NO ui code here)
backup.rs      — git auto-backup: runs the `git` program (NO ui code here)
hotkeys.rs     — custom keys: overrides on top of the default keymap (NO ui code)
embed.rs       — live embeds: parse ![[page]] / block embeds, resolve (NO ui code)
app/embed_ui.rs — the reading view's embed boxes (a submodule of app.rs)
vim.rs         — vim mode's pure state machine: modes, motions, operators, counts, register, `:` line (NO ui code)
app/vim_ui.rs  — vim keys into the editor, block-level effects, mode pill, Settings row (a submodule of app.rs)
publish.rs     — publish a page as a static site folder: render, privacy, slugs, bundle files (no GPUI)
app/publish_ui.rs — the Publish commands and the status buttons (a submodule of app.rs)
import/        — import Obsidian, Logseq and Notion exports: walk, convert, plan, apply; CSV reader (no GPUI)
app/import_ui.rs — the Import commands, folder picker, name-clash dialog (a submodule of app.rs)
clipper/       — web clipper: localhost HTTP listener, request checks, HTML → outline, token (no GPUI)
app/clipper_ui.rs — the clipper's Settings section, saving clips, the Clipped status (a submodule of app.rs)
voice/         — voice notes: find and run a recorder, WAV check/repair, whisper.cpp transcription (no GPUI)
app/voice_ui.rs — recording pill, note and transcript blocks, Play/Transcribe, Settings > Voice notes (a submodule of app.rs)
whiteboard/    — whiteboards: cards/edges from a page's blocks, edits, view maths, hit testing (no GPUI)
app/whiteboard_ui.rs — the whiteboard canvas: drawing, mouse/zoom, card editing, commands (a submodule of app.rs)
vault/         — encrypted vault export/import: archive, Argon2id + chunked XChaCha20-Poly1305 (no GPUI)
app/vault_ui.rs — the vault commands, masked passphrase dialog, pickers (a submodule of app.rs)
ui.rs          — theme colours + tiny stateless view helpers
config.rs      — config.toml: theme, font size/family
```

**The key design rule:** `model`, `storage`, `editor`, `display`, `search` know *nothing* about
GPUI. They're plain Rust. That means:

- They can be **unit-tested** without opening a window (most of the 42 tests).
- You can reason about them without understanding GPU rendering.
- Only `app.rs` speaks GPUI. It's the "translator" between data and screen.

This is called **separation of concerns**, and it's the single most important
decision in the codebase. When something breaks, you know which layer to look in.

---

## 2. model.rs — the data model

### Blocks live in a flat Vec, not a tree

```rust
pub struct Block {
    pub id: Uuid,
    pub content: String,
    pub parent_id: Option<Uuid>,  // None = top-level block
    pub page_id: Uuid,
    pub order: usize,              // position among siblings
}

pub struct Page {
    pub id: Uuid,
    pub title: String,
    pub blocks: Vec<Block>,        // flat, in document order
    pub is_journal: bool,
}
```

**Why flat instead of a tree?** Three reasons:

1. **It serializes trivially.** Markdown on disk is already a flat list of lines
   with indentation. A flat `Vec` maps to it almost 1:1 — no recursive
   tree-walking to save/load.
2. **Rendering is a simple loop.** To draw the page you walk the Vec once;
   each block's indent depth = "count its ancestors" (`depth_of`). No recursion
   needed in the hot path.
3. **Cache-friendly.** A `Vec` is one contiguous chunk of memory. A tree of
   `Box<Node>` pointers jumps around RAM. For thousands of blocks this matters.

The tree structure isn't lost — it's *encoded* in `parent_id`. Whenever the code
needs tree behaviour (find a block's children, its subtree), it derives it with
a scan, e.g. `subtree_end(index)`: "keep walking while depth keeps increasing."

**Trade-off:** structural edits (indent/outdent) must fix up `order` values, so
there's a `renumber()` that recomputes them after any change. Cheap and simple.

### `Option<Uuid>` instead of null

```rust
pub parent_id: Option<Uuid>,
```

Rust has **no null**. "This block might not have a parent" is expressed as
`Option<Uuid>` — either `Some(uuid)` or `None`. The compiler *forces* you to
handle both cases (via `match`, `if let`, `.unwrap_or(...)`). An entire class of
null-pointer crashes becomes impossible. You'll see `Option` everywhere in Rust;
it's the idiomatic replacement for nullable fields.

### IDs regenerate on every load

Block `id`s are fresh `Uuid::new_v4()` each time a file is loaded — they're
**not** stored in the markdown. Why? Because nothing in the MVP references a
block *by ID* across sessions (no block embeds yet). Persisting IDs would add
noise to the files for zero benefit. Simpler files = more portable files =
open them in real Logseq without junk.

### One parser for links AND tags

```rust
pub struct Reference {
    pub range: Range<usize>,  // byte range in the block text
    pub target: String,       // page name, without brackets or #
    pub is_tag: bool,         // true for #tag, false for [[wikilink]]
}
```

In Logseq's model, a `#tag` **is** a link to a page named `tag`. So instead of
two systems, there's one `Reference` type with a flag. That one decision lets
tags reuse *everything*: click-to-navigate, lazy page creation, the backlinks
panel (which doubles as each tag's index page), and case-insensitive matching.
Less code, fewer bugs.

The parsers are hand-written scanners (not regex): `parse_wikilinks` walks the
text looking for `[[...]]`, `parse_tags` looks for `#` at word boundaries
(so `page.html#top`, `C#`, and `# Title` headings are correctly ignored).
Hand-written is verbose but precise — and fast.

---

## 3. storage.rs — files on disk

### Markdown files are the source of truth

```
~/notesec/
  pages/My Idea.md
  pages/Projects___Alpha.md     # "/" in titles becomes "___" (Logseq convention)
  journals/2026_10_07.md        # shown in-app as 2026-10-07
  config.toml
```

**There is no database.** The `.md` files *are* the data. This means:

- Your notes survive even if notesec disappears — any text editor opens them.
- A notesec graph opens in real Logseq and vice versa.
- Backups = copying a folder. Sync = Dropbox/Git.

`Page::from_markdown` / `to_markdown` convert between the flat `Vec<Block>`
and indented `- ` bullet lines. Indentation = nesting.

### Atomic saves (crash safety)

```rust
pub fn write_atomic(path: &Path, contents: &str) -> io::Result<()>
```

Saving works in three steps: (1) write to a hidden temp file *in the same
folder*, (2) `sync_all()` to force it to disk, (3) `rename()` the temp file over
the real one. On every OS, `rename` within one folder is **atomic** — a crash or
kill mid-save leaves either the complete old file or the complete new one,
never a half-written corrupt file. This is the standard trick databases use,
and it's ~15 lines.

### Save on commit, not on keystroke

A page is written to disk when you press Enter/Tab/Escape, delete a block, or
switch pages — **not** on every keystroke. Writing a file 10x/second while
typing would be wasteful and would spam file-watchers (Dropbox, git). The
in-memory model is always current; the disk just lags by a moment.

### `io::Result` and the `?` operator

```rust
pub fn open(root: PathBuf) -> io::Result<Self> {
    fs::create_dir_all(root.join("pages"))?;
    ...
}
```

Rust has **no exceptions**. Fallible functions return `Result<T, E>` (`Ok(value)`
or `Err(error)`), and `?` means "if this failed, return the error immediately."
It's error handling you can see — no hidden `try/catch` five layers up.

---

## 4. editor.rs — the editing state machine

### Deliberately NOT a GPUI thing

`EditorState` is pure data + methods:

```rust
pub struct EditorState {
    pub text: String,
    pub cursor: usize,                // byte offset (see below)
    pub anchor: Option<usize>,        // other end of the selection, if any
    pub marked: Option<Range<usize>>, // in-progress IME composition
}
```

`app.rs` owns **one shared** `EditorState`. Clicking a block loads its text into
it; every other block renders as plain text. Why one? Because Logseq lets you
edit only one block at a time anyway — and one editor means one place to wire
up GPUI's input handling, instead of N.

Keeping it UI-free means it's **unit-testable**: tests cover backspace,
splitting, multibyte characters, etc., with zero windows involved.

### Grapheme clusters: the cursor never splits an emoji

```rust
pub fn backspace(&mut self) -> bool {
    let prev = self.prev_boundary(self.cursor);
    ...
}
```

Rust strings are UTF-8: one visible character can be multiple bytes. If the
cursor moved by *bytes*, backspace could delete half an emoji and corrupt the
string. So the cursor moves by **grapheme clusters** (what a human calls "a
character"), using the `unicode-segmentation` crate. `prev_boundary` /
`next_boundary` find the nearest legal stop.

### UTF-16 conversion: speaking the OS's language

The OS input-method API (for CJK input, dead keys, etc.) speaks **UTF-16**
offsets, but Rust strings are UTF-8. Hence `offset_from_utf16` /
`offset_to_utf16`: walk the chars, counting both ways. It's the kind of
unglamorous correctness work that separates "works on my machine" from "works."

### Single-line blocks

`replace_range` strips newlines from inserted text. Blocks are single-line by
design (MVP decision) — pasting multi-line text flattens it instead of breaking
the outliner model.

---

## 5. search.rs — the fuzzy matcher

Ctrl-K search uses **subsequence matching**, like every command palette you've
used: each query character must appear *in order*, but not necessarily adjacent
(`fbr` matches `Foo Bar`).

Scoring (higher = better):
- each matched char: +10
- match at text/word start: +8/+6 bonus
- consecutive matches: +8 each
- skipped characters between matches: -1 each

Implementation: **dynamic programming with rolling arrays** — `prev`/`cur` rows
only, so memory is O(text length) not O(query x text). It also picks the *best*
placement, not just the first one. Two pragmatic guards: only the first 200
chars of a block are searched (one keystroke stays fast even with pasted walls
of text), and exotic Unicode that changes length when lowercased falls back to
plain subsequence matching instead of risking an index panic.

**Rust lesson:** `fuzzy_score` returns `Option<i32>` — `None` means "no match."
Callers use `?` or `if let Some(score)` to handle it. No sentinel values like
`-1`, no exceptions.

---

## 6. app.rs — where GPUI lives

The biggest file (~1600 lines) because it's the *only* file that talks to GPUI.
Everything else is plain Rust it orchestrates.

### Views and the `Render` trait

```rust
impl Render for NoteSec {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>)
        -> impl IntoElement
    { ... div().child(...).child(...) ... }
}
```

A GPUI "view" is a Rust struct holding state. `render()` describes the UI as a
tree of elements (`div()`, text, ...) — think React, but compiled. When state
changes you call `cx.notify()` and GPUI re-renders. **GPUI owns the view**
(inside an `Entity`); you mutate through the context. That's why you see
`cx.listener(...)` closures everywhere instead of direct method calls.

### Actions: named commands for keys

```rust
actions!(notesec, [Enter, Tab, ShiftTab, Backspace, ...]);

// shortcuts(): one line per binding, with its cheatsheet group and text.
s("enter", Enter, Some("BlockEditor"), Editing, "New block at the cursor"),
...
cx.bind_keys(shortcuts().into_iter().map(|s| s.binding));
```

Keyboard input goes: **key press -> action -> handler**. The `"BlockEditor"`
context scopes bindings so Enter only splits blocks *while editing* — the same
key can mean different things in different contexts. Cleaner than one giant
`match` on key codes. The Ctrl-K palette's commands dispatch the same actions
(decision 33), and the Ctrl+/ dialog lists the same table.

### Events, not callbacks

The block editor *emits* events; the root view *subscribes* via GPUI's event
system. The editor never reaches up and pokes the app directly. One-way data
flow — the same idea as React's props-down/events-up, and it keeps the big
file from becoming spaghetti.

### Custom Element for the text cursor

GPUI gives you `StyledText`, but a blinking cursor needs **paint-phase hooks**
(draw *after* layout, at the exact pixel of the cursor). So `app.rs` defines a
custom `Element` wrapping `StyledText`, delegating layout/prepaint/paint while
adding cursor rendering and click handling. This is the deepest GPUI water in
the project — everything else is composed of built-in elements.

### Focus

Nothing had keyboard focus when the window opened, so Ctrl-K silently did
nothing until you clicked something. Fix: `NoteSec::new` takes the window and
focuses the root view immediately. Lesson: in GUI frameworks, *focus* is state
too, and "nothing is focused" is a real bug class.

---

## 7. ui.rs, config.rs, main.rs — the supporting cast

- **`ui.rs` — the theme lives in ONE struct.**
  ```rust
  pub struct Theme { pub bg: Rgba, pub text: Rgba, pub accent: Rgba, ... }
  ```
  Every colour in the app comes from here. Dark/light switching = constructing
  a different `Theme`. If colours were scattered as literals across `app.rs`,
  theming would be a rewrite; here it's a struct swap.

- **`config.rs` — settings as data.**
  ```rust
  #[derive(Serialize, Deserialize)]
  pub struct Config { pub theme: ThemeKind, pub font_size: f32, ... }
  ```
  `serde`'s derive macros auto-generate TOML conversion — no hand-written
  parsing. `#[serde(rename_all = "lowercase")]` maps `Dark` to `"dark"`.
  Missing keys fall back to defaults, so old config files never break.

- **`state.rs` — UI state, not settings.** Favorite and recent page titles
  and the sidebar's custom page order in `state.toml`, with the same
  load/save rules as `config.rs` (see decisions 21 and 30).

- **`main.rs` — boring on purpose.** Declare modules, open storage, load config,
  open an 1100x700 window, hand control to GPUI's event loop. If `main` is long,
  something's wrong.

---

## 8. Rust ideas this codebase teaches (Python-dev lens)

| Python habit | Rust equivalent here |
|---|---|
| `None` checks | `Option<T>` — compiler-enforced (`parent_id: Option<Uuid>`) |
| `try/except` | `Result<T, E>` + `?` — errors are values (`storage.rs`) |
| dict/object | `struct` with named fields + `impl` blocks |
| duck typing | **traits** — `Render`, `Serialize`/`Deserialize` (via derive macros) |
| `if x is None` | `if let Some(x) = ...`, `match`, `let ... else` |
| string slicing | byte offsets + grapheme boundaries (`editor.rs`) — slicing a `String` by bytes can panic; the code never does it blindly |
| list comprehensions | iterators: `.iter().find()`, `.map_or()`, `flat_map` (`search.rs`) |
| `deepcopy` | `.clone()` is explicit and visible — you always know when data is copied |
| monkey-patching | impossible — the type system is the contract |

**The borrow checker in one sentence:** you can have many readers *or* one
writer of data at a time, and the compiler proves it. That's why `&self`
methods only read, `&mut self` methods (like `renumber()`) are the only ones
that mutate, and data races are essentially impossible.

---

## 9. Decisions log (why, not just what)

1. **Flat `Vec<Block>` + `parent_id`** — serializes to markdown trivially, renders
   in one pass, cache-friendly. Tree derived on demand via DFS.
2. **IDs regenerated on load** — no cross-block references in MVP, so persisted
   IDs would be file noise.
3. **Markdown files as source of truth** — portable, Logseq-compatible,
   backup-friendly. No database to corrupt or migrate.
4. **Atomic saves, on commit not keystroke** — crash-safe without write floods.
5. **One shared `EditorState`** — matches Logseq's one-block-at-a-time model;
   simplifies GPUI input wiring enormously.
6. **UI-free model/storage/editor/search** — unit-testable, reason-about-able.
   42 tests, most never opening a window.
7. **`Reference` unifies links + tags** — tags *are* page links; one parser
   powers navigation, creation, backlinks, and the tag index.
8. **Hand-rolled fuzzy matcher** — no dependency, tuned scoring, bounded cost.
9. **Theme as a struct** — theme switch = struct swap, not a rewrite.
10. **TOML + serde for config** — human-editable, standard, zero hand-parsing.
11. **Block types are markdown prefixes** — `BlockKind` (Text, Heading 1-3,
    Quote) is read from a `# ` / `## ` / `### ` / `> ` prefix on `content`
    rather than stored in a field, so files stay plain Logseq markdown. The
    editor shows the raw prefix; display mode hides it and styles the row.
    Typing "/" in an empty block opens a menu to switch the type.
12. **Selection = anchor + cursor, within one block** — `EditorState.anchor`
    is the fixed end and `cursor` the moving end (byte offsets on grapheme
    boundaries, so mouse positions are snapped back to a whole character).
    Pressing on any block puts the cursor under the mouse (a block that is
    not being edited starts editing first; its hidden type prefix is
    accounted for) and dragging from there selects; a click without a drag
    clears the selection. A press on a `[[link]]` or `#tag` in a block that
    is not being edited never edits or selects: its click navigates.
    Shift+Left/Right/Home/End extend or shrink the selection.
    Plain Left/Right collapse it to its start/end. Typing, pasting,
    Backspace, Delete, and Enter replace or remove the selected text first.
    Replacing a selection is always its own undo step, and undo snapshots
    include the anchor, so undo brings back the text *and* the selection.
    Esc clears a selection before it leaves the block (the slash menu's Esc
    comes first). The OS input handler is told the real selection. The
    highlight is a translucent theme colour (`Theme::selection`) painted
    under the text, so it never alters the text's own colours.
13. **"/" over a selection opens the slash menu without replacing it** —
    the "/" and the filter text are inserted right after the selection, and
    the menu keeps a snapshot from before the "/". Choosing a type rebuilds
    the block from that snapshot (full text kept, nothing deleted); Esc (or
    deleting the "/") restores it exactly, selection included. If the menu
    closes any other way (no match, arrows, click), the "/query" is applied
    as ordinary typing, i.e. it replaces the selection, as one undo step.
14. **Ctrl+B / Ctrl+I write Logseq's own markers: `**bold**`, `*italic*`** —
    Logseq's `frontend/config.cljs` (`get-bold` / `get-italic`, checked on
    master and tag 0.10.9) uses `**` and `*` for markdown, so italic is `*`,
    not `_`. The shortcut wraps the selection, else the word at the cursor
    (Unicode word boundaries; between two words the left one wins), else
    inserts empty markers with the cursor between them. It toggles: the `*`
    runs at both edges of the target (stars just inside the selection plus
    stars just outside it) decide whether the style is already there. A run
    of 1 is italic, 2 bold, 3 both, so `**` is never taken for italic and
    Ctrl+I inside `**x**` gives `***x***`. Removing takes the stars next to
    the text; the selection (or the cursor's place in the word) still covers
    the same text afterwards. Each press is one undo step and the text is
    saved when editing ends, like typing. Display mode renders the result
    (see 15).
15. **Reading view hides paired emphasis markers; one offset map for all
    hidden text** — a block that isn't being edited is shown through
    `DisplayBlock` (`display.rs`): the type prefix and paired `*` markers
    are removed from the shown text, which keeps a list of visible source
    chunks. That one map turns a click or drag-start position into an
    editor offset and moves link/tag ranges into shown offsets, so
    hit-testing and highlighting keep working. The edited block shows the
    raw markdown; `content` and the files never change.
    *Pairing rules* (simplified CommonMark): a maximal run of `*` is a
    delimiter; it can open if followed by a non-space and close if preceded
    by a non-space (so `2 * 3`, a lone `**`, `** x**` and a leading
    `* item` stay literal). A closer pairs with the nearest earlier opener
    that has stars left, using two stars (bold) if both have two or more,
    else one (italic), innermost stars first, so `***x***` is bold+italic;
    openers in between are dropped, unpaired stars stay visible. Stars
    inside `[[links]]`/`#tags` are plain text. Not handled: CommonMark's
    punctuation-flanking and "multiple of 3" rules, `_` emphasis, code spans.
    *Boundary rule:* a shown offset sticks to the visible character before
    it. Clicking just after a bold word's last letter puts the cursor
    before the closing `**` (inside the bold); clicking before the first
    letter of a bold word that follows other text puts it before the
    opening `**` (after the previous character). Offset 0 is the first
    visible character (after the prefix and any opening markers); past the
    end is after the last visible character. Byte positions inside a chunk
    map one to one, so they stay on character boundaries, and the editor
    snaps to graphemes as usual.
    Bold/italic are applied as highlights on top of link/tag styles and
    under the row's heading/quote style.
16. **Task states are Logseq keywords, not `[ ]`/`[x]`** — Logseq's
    markdown writes a task as a keyword at the start of the block, after any
    heading prefix: `TODO x`, `DOING x`, `DONE x` (and `LATER`/`NOW` for
    its other workflow), e.g. `## TODO Plan`. Checked in Logseq 0.10.9
    `frontend/util/marker.cljs`: `marker-pattern` is `^(#+\s+)?(NOW|LATER|
    TODO|DOING|DONE|...)?\s?` and `cycle-marker-state` goes TODO -> DOING ->
    DONE -> none -> TODO and LATER -> NOW -> DONE; `[ ]`/`[x]` are plain
    markdown checklists that Logseq doesn't treat as task state. So
    `TaskState` (`model.rs`) parses the keyword after the `BlockKind` prefix
    (it must be the whole text or be followed by a space; `TODOS` and
    lowercase `todo` are text) and `cycle_task` follows Logseq's order.
    Quotes take the keyword after `> ` too, which Logseq would read as
    quoted text rather than a task. `WAITING`/`CANCELED` etc. stay text.
    Ctrl+Enter (edit mode) or a click on the checkbox (reading view) cycles;
    both are one undo step and save right away. The reading view hides the
    keyword through `DisplayBlock` and draws a checkbox (empty for
    TODO/LATER, half-filled for DOING/NOW, ticked for DONE, with DONE text
    dimmed and struck through); its mouse-down stops propagation, so it
    never starts editing. The edited block shows the raw keyword. The "/"
    menu doesn't offer TODO: it lists block kinds only.
17. **Folding is UI-only and keyed by block id** — `NoteSec.collapsed` is a
    `HashSet<Uuid>` of folded blocks; nothing is written to the file (ids
    are regenerated on load, so folds reset on restart, as decided for v1).
    `Page::visible_blocks` hides every block that has a folded ancestor;
    hidden rows aren't rendered. A block with children gets a `▾`/`▸`
    arrow left of its bullet (in the indent, so text doesn't move); a press
    on it stops propagation, so it never starts editing. A folded block
    shows a badge with the number of **all** hidden descendants
    (`Page::descendant_count`), not just direct children. Click only; no
    keyboard folding yet. Interaction rules: the edited block is always
    made visible (`load_editor` and undo/redo unfold its ancestors, which
    covers search hits, arrows, Enter and Backspace); folding an ancestor of
    the edited block leaves edit mode (saving it); Up/Down and the
    Backspace-delete target skip hidden blocks; Enter on a folded block adds
    a sibling after its subtree (as Logseq does) instead of a hidden first
    child; Tab under a folded sibling unfolds it; clicking a backlink
    unfolds the referencing block on the opened page. Folding is not an
    undo step.

---

18. **"Today" button / Ctrl-J opens today's journal, creating it if missing**:
    startup already creates the day's journal, but the app can stay open past
    midnight or the file can be deleted, so the button re-checks every time.
    It only matches journal pages (`is_journal`), never a regular page that
    happens to be named like a date, and it saves the block being edited
    first. Startup and the button share `create_journal`, so the file is
    always Logseq's `journals/YYYY_MM_DD.md`.
19. **Templates are plain markdown files in `<graph>/templates/`, inserted at
    the cursor**: the file name (without `.md`) is the template's name and
    the body uses the same bullet format as pages, so templates are edited in
    any text editor and Logseq simply ignores the folder. A new graph gets one
    example (`Daily review`, shipped from `assets/templates/`); it is written
    only when the folder doesn't exist yet, so deleting it sticks. The folder
    is re-read each time the picker opens, so new files show up without a
    restart. Ctrl-K, then "Insert template", turns the palette into a template
    picker. Of the two options in the spec (insert at the cursor, or create a
    new page) we chose inserting at the cursor because it needs no page name
    and works in journals, where templates are most used: the blocks go right
    after the block you were editing when you opened the palette (after its
    children too, as its siblings), or at the end of the page if you weren't
    editing. An empty leaf block, or a page that is just one blank bullet, is
    replaced instead of leaving a stray empty bullet. The template's own
    nesting is kept, every copy gets fresh IDs, and the whole insert is one
    undo step.
20. **Tabs: a list of page titles (or the graph), like browser tabs** —
    `Tabs` (`tabs.rs`) holds `TabTarget::Page(title)` / `TabTarget::Graph`
    plus the active index. Titles, not indices, because `pages` is re-sorted
    when pages are added. The active tab drives what's on screen
    (`apply_tab` sets `mode` and `selected`); no tabs means `Mode::Empty`, an
    empty state with an "Open today's journal" button and the Ctrl-J / Ctrl-K
    / Ctrl-N / Ctrl-G hints. Startup opens one tab on today's journal.
    Tabs are not saved across restarts.
    *Navigation rules* (`Nav::Tab` vs `Nav::Replace`):
    - Sidebar page or tag, Today button / Ctrl-J, Ctrl-K search hits
      (page or block), new page (Ctrl-N / "+ New page"), the empty state's
      button, and inserting a template: focus the tab already showing that
      page (the active one first), else open a new tab at the end.
    - `[[link]]` / `#tag` clicks in a block, backlink clicks: replace the
      active tab's page (like following a link in a browser), so two tabs
      may show the same page.
    - Graph node clicks: the graph tab stays; the page opens in (or
      focuses) a page tab. In general `Replace` from the graph tab, or with
      no tabs, behaves like `Tab`.
    - Ctrl-G / "Graph view" / the palette command: focus the graph tab or
      open one; from the graph tab itself it closes that tab (the old
      toggle back to notes). The `GraphView` entity is kept, so node
      positions survive closing and reopening.
    *Tab keys*: Ctrl+W closes the active tab, × closes any tab (on press, so
    the tab isn't focused first), Ctrl+Tab / Ctrl+Shift+Tab cycle with
    wrap-around. They are global bindings; nothing else used them, and plain
    Tab / Shift-Tab (indent) in the editor context don't match them because
    modifiers must match exactly. GPUI's Linux backend maps both `Tab` and
    `ISO_Left_Tab` (what Shift+Tab sends on X11) to "tab", and Zed's own Linux
    keymap uses ctrl-tab, ctrl-shift-tab and ctrl-w, so they arrive. They do
    nothing while the Ctrl-K palette or the settings panel (decision 22) is
    open, since both are modal overlays. Closing the
    active tab focuses the tab that takes its place, else the one to its
    left; closing another tab keeps the active one.
    *Per-tab state* is just the page. Every tab switch, close or cycle saves
    the block being edited and ends editing (Ctrl+W never deletes text).
    Folds stay global per block, scroll position isn't kept per tab.
    *Missing pages*: there is no rename or delete, but undo/redo can restore
    a page list without a page created later; tabs on pages that no longer
    exist are closed, and the restored page is brought into a tab when a page
    was on screen or a block is being edited.
21. **Favorites and recent pages live in `state.toml`, and every navigation
    goes through `show_page`**: hovering a sidebar page row shows a `☆`;
    clicking it stars the page (its click handler calls
    `cx.stop_propagation()`, so the row's own click doesn't also open the
    page). Starred pages show a filled `★` and are listed under FAVORITES
    (right after Today / Graph view, only when non-empty; hovering a row
    shows the `★` that unstars it). RECENT (between FAVORITES and PAGES)
    lists the last 10 opened pages, most recent first, deduped ignoring case.
    Both are stored by page title in `<graph>/state.toml` (`state.rs`), not in
    `config.toml`: RECENT changes on every page you open, and rewriting the
    hand-edited settings file that often would be rude. `state.rs` copies
    `config.rs`'s rules (serde defaults, atomic write, a missing file means
    empty lists, an invalid one is copied to `state.toml.bak`), cleans
    hand-edits on load (blank titles, duplicates, more than 10 recent), and
    is only written when something actually changed. Titles whose page no
    longer exists are skipped in the sidebar but kept in the file (the page
    may come back, e.g. from a sync or `git checkout`). Navigation is
    centralized: `NoteSec::show_page(ix)` sets `selected`, switches to the
    notes view and records the page in RECENT. The sidebar page rows,
    `open_page` (links, tags, backlinks, search, graph nodes, favorites and
    recent rows), `open_today` and `new_page` all end there, and startup
    records its page too. Undo/redo and `add_page` move `selected` without
    going through it, since they aren't the user opening a page. Anything
    new that opens a page should call `show_page` or `open_page`.
    *With tabs* (decision 20), `show_page(ix)` opens or focuses the page's
    tab, and `show_page_in(ix, nav)` is the general form that `open_page`
    (`Nav::Replace`) uses. Both end in `enter_page`, which sets `selected`
    and the mode and records RECENT. Focusing an existing page tab (a tab
    click, Ctrl+Tab / Ctrl+Shift+Tab, or the neighbour taking over after a
    close) also counts as opening that page, so `apply_tab` calls
    `enter_page` too. The graph tab and the empty state are never recorded.
    The sidebar row click re-finds its page by title after `stop_edit`
    (saving can re-sort the pages) before calling `show_page`.
22. **Settings are an overlay in the main window, not a second window**:
    the panel reuses the Ctrl-K palette's pattern (a dimming backdrop that
    closes on click, an `occlude`d panel on top) and opens from the
    "Settings" row pinned under the sidebar's scrolling list, from Ctrl-,
    (Zed's binding), or from "Open settings" in the palette (also found by
    "preferences", "theme" and "font": `Command::keywords` match at a
    penalty, so a label match like "Toggle dark/light theme" still ranks
    first). While it is open the root's key context is `Settings` instead of
    `BlockEditor`, which is how Esc closes it; opening it saves the edited
    block and closes the palette, and anything that starts editing (e.g.
    Ctrl-N) closes it. Every control applies at once and writes
    `config.toml` through one small method each (`set_theme`,
    `change_font_size` / `reset_font_size`, `set_font_family`), which also
    re-render and push the style into the graph view, exactly like the
    keyboard shortcuts. The font list comes from GPUI's
    `TextSystem::all_font_names()` (sorted and deduped by GPUI), read once
    when the panel opens and shown in a fixed-height scrolling list after
    "System default". Choosing "System default" removes `font_family` from
    the file; a configured family that isn't installed is kept in the file
    and named in the panel, but the system font is used, as at startup.
23. **Blocks move by dragging their bullet, as one subtree, through one
    model method**: `Page::move_subtree(from, before)` cuts out the block
    and its descendants (they are contiguous in the flat list) and splices
    them in just before `before`, taking `before`'s parent, or at the end of
    the top level for `None`. It refuses a target inside the moved subtree.
    Drag-and-drop and Alt+Up / Alt+Down (`move_up` / `move_down`, which only
    swap with a sibling, so a child never leaves its parent) all go through
    it, then save the page like any edit (so the new order is in the file
    and survives a restart) and push one undo step. The drag uses GPUI's
    own drag and drop: an invisible 14px handle over the 6px bullet has
    `on_drag` with a `DraggedBlock { id }` value (`BlockDragPreview` draws
    the first line under the mouse) and stops the mouse-down, so grabbing a
    bullet never starts editing. Every row listens with `on_drag_move`: the
    upper half of a row means "before this row", the lower half "before the
    next visible row" (or the end of the page), stored by block id as
    `block_drop` so it can't go stale if indices shift. A 2px accent line is
    drawn there at the target's indent, only while `cx.has_active_drag()`.
    Dropping anywhere on the page moves the block to the line; leaving the
    page hides the line and makes the drop a no-op.

24. **Block references use the block's UUID, written to the file only
    when something references it**: every block already gets a `Uuid` when
    it is created or loaded, but unreferenced ones get a fresh one on each
    load, which keeps the markdown free of ids. Once a block is referenced
    its id goes under its first line as Logseq's `id:: <uuid>` property
    (`Page::saved_ids`; `save_page` adds the targets of every `((id))` on
    the page and saves their pages too), and loading gives the block that
    id again, so the reference survives a restart. A duplicate id line is
    dropped rather than giving two blocks one id. Typing `((` in a block
    opens a picker under it that fuzzy-matches every other non-empty block
    (`search_blocks`); it is derived from the text (`block_ref_query`: a
    `((` before the cursor with no `)` or line break after it), so typing,
    deleting and moving the cursor need no extra state, only the highlight
    and an Esc flag. Enter or a click replaces `((query` with `((<uuid>))`
    as one undo step. In reading view `DisplayBlock::with_refs` shows the
    referenced block's own reading text in place of the reference, one level
    deep (references inside it stay as written, so cycles can't recurse),
    on a faint accent wash with a wavy underline; the shown text maps back
    to the start or end of the reference in `content`, so the editor and the
    file only ever hold `((uuid))`. Clicking it opens the source page and
    edits that block. A reference to a missing block shows as written.
25. **Tag queries are computed while rendering, from the pages in
    memory**: a block whose text holds `{{query #tag}}` or
    `{{query #[[multi word]]}}` (`parse_query`: the inside must be exactly
    one tag) shows, under its text in reading view, a list of every block
    tagged that way on any page (`tag_query`, case-insensitive like page
    names; `[[links]]` don't count, and the tag inside a block's own query
    macro doesn't either, so a query never lists itself). There is no
    index or cache: the list is rebuilt every frame from `self.pages`,
    which every save and navigation already keeps current, so results are
    always live. Each result shows its page and the block's reading text;
    a press on it stops there (it never starts editing the query block) and
    a click opens that page. The macro itself stays in the file and in the
    editor exactly as typed.
26. **Pipe tables are drawn only in reading view, and blocks keep their
    text byte for byte**: `table::parse_table` finds a header row, a
    `---` separator row (`:` sets left, center or right alignment) and the
    rows after it, splitting cells on unescaped `|`. Short or long rows are
    padded to the widest, so ragged tables never panic. The table is laid
    out column by column (each column as wide as its widest cell, every
    cell one line high), so cells line up without a grid engine, and a wide
    table scrolls sideways. Cells use `DisplayBlock::inline` (links, tags,
    emphasis and block refs, but no `# `/`TODO` prefix). Nothing is
    rewritten: continuation lines now drop exactly the indent `to_markdown`
    writes (two spaces per level plus two) and keep the rest, padding and
    trailing spaces included, so tables survive a load and save unchanged.
    Because tables are multi-line blocks, the editor (`BlockText`) is now
    multi-line too: it shapes each `\n`-separated line on its own
    (`TextLines`), since GPUI's `shape_line` panics on newlines, which
    clicking any multi-line Logseq block used to trigger. Up and Down move
    between lines before moving between blocks, Shift+Enter inserts a line
    break, and pasted newlines still become spaces.
27. **Fenced code blocks render as their own box, with a copy button
    kept in app state**: `code::split_code` cuts a block into prose and
    ```` ``` ```` fenced code (a longer fence holds shorter ones; an
    unclosed fence runs to the end of the block). Code is drawn monospace
    (the first installed of `MONO_FONTS`, else the UI font) on the theme's
    sidebar colour with whitespace kept, and scrolls sideways when wide.
    Links and tags inside fences aren't references anywhere
    (`parse_wikilinks`/`parse_references` drop them), so `#include` makes
    no tag. Hover is tracked in `hovered_code` with `on_hover`, rather than a
    group-hover style, so the button really isn't there until the mouse is,
    which tests can see. A click writes the exact code (the lines between
    the fences, as stored) to the clipboard, sets `copied_code` and starts a
    `COPIED_FOR` timer task. Keeping the task in a field means a newer copy
    drops, and so cancels, the old timer. All colours come from `Theme`, so
    both themes work. A press on the button never starts editing.
28. **Images live in `assets/` and blocks only point at them.** Pasting an
    image while editing a block (Ctrl+V; an image beats any text on the
    clipboard) or dropping image files onto the page saves each one as
    `assets/image-<milliseconds>.png` (the folder is made when first needed;
    `create_new` plus a `-1`, `-2` suffix means nothing is ever
    overwritten) and puts `![image](../assets/<file>)` in the block, the way
    Logseq does, so the markdown stays readable by other tools. PNGs are
    written byte for byte; other formats go through the `image` crate, which
    GPUI already builds, so the new dependency compiles nothing extra.
    Dropped while editing, the reference goes in at the cursor; otherwise
    each image gets a new top-level block at the end of the page. Either way
    it is one undo step; the files themselves are kept. Reading view draws
    images like tables and code: as their own boxes between the prose, at
    most `IMAGE_MAX_HEIGHT` (320 px) tall and the page width wide, keeping
    their shape. A missing file, or one GPUI can't decode, shows a dashed
    "Image not found" or "Image could not be loaded" box, never a crash.
    Images inside code fences stay text. Drops don't use `on_drop`, which
    needs a hovered element: after a keypress GPUI treats nothing as hovered
    until the mouse moves in the window, and files dragged in from a file
    manager don't move it, so a drop right after typing was lost. Instead
    `on_drag_move::<ExternalPaths>` remembers the files and whether they're
    over the page, and a window-level mouse-up listener (painted by a
    zero-size `canvas`) takes the drag on release.
29. **Page context menu: rename, delete, copy title** (23-28 are the blocks
    track's): right-clicking a page row in PAGES, FAVORITES or RECENT
    (`on_mouse_down(MouseButton::Right)`) opens a small menu at the click
    position, placed with `anchored()` like Zed's right-click menus, over a
    transparent full-window backdrop that closes it on any click (left or
    right) outside; the menu panel `occlude`s so its own clicks don't reach
    the backdrop. Esc closes it via a `PageMenu` key context. Opening it
    saves the block being edited. The page is held by title, never index.
    *Rename* turns the menu into a text field in place, reusing the
    palette's `EditorState` + `BlockText` input (`active_editor` returns
    it), with the old name selected. Enter renames, Esc cancels; a refused
    name keeps the field open with the reason under it: empty, another
    page's name (ignoring case, as links do; a case-only rename is fine), a
    literal `___` (it reads back as `/`), control characters, or a file
    name over 255 bytes counting `write_atomic`'s `.<name>.md.tmp`
    (`storage::validate_title`). The file moves with one `fs::rename`
    (`Storage::rename`, which uses the same title-to-file mapping as saving
    and refuses to overwrite another file). The title, page id and blocks'
    `page_id` change (`Page::rename`), pages re-sort, and selection, tabs
    (`Tabs::rename`) and favorites/recent (`UiState::rename`) follow.
    `[[links]]` to the old name are not rewritten (v1). Journals can't be
    renamed: their name is their date and their file
    `journals/YYYY_MM_DD.md`. *Delete* asks first in a modal (danger-styled
    Delete, Cancel; Esc or a backdrop click cancels, Enter does nothing),
    then removes the file (since decision 37: moves it to the trash), the
    page, its tabs (with the same neighbour rule as closing a tab) and its
    favorites/recent entries.
    Journals can be deleted (today's comes back on Ctrl-J or at startup),
    but the last remaining page can't: the app always has a page to show.
    *Undo:* neither can be undone, and both clear the undo/redo history,
    because a snapshot holds whole pages under their titles and restoring
    one saves them all, so it would write the old file back. *Copy page
    title* writes the title with `cx.write_to_clipboard`.
30. **Drag to reorder pages; the order lives in `state.toml`.** Only
    regular pages can be dragged: journals stay above them, newest first,
    since a date order is the useful one for a diary and mixing the two
    would make "where is today" depend on dragging. A row is a GPUI drag
    source (`on_drag` with a `DraggedPage { title }` value and a small
    preview view that follows the pointer); every page row and a short
    end-of-list zone (`page-drop-end`) listen with
    `on_drag_move::<DraggedPage>`. The upper half of a row means "before
    this page", the lower half "before the next one" (after the last row,
    or in the end zone: at the very end); each zone also covers half the
    gap next to it. GPUI calls every zone's drag-move handler on every
    move, so a zone only clears the drop position it set itself (Zed's
    project panel does the same). The position shows as an accent line in
    the gap above the target (`page-drop-indicator`); none is shown where
    dropping would change nothing. The drop itself is one `on_drop` on the
    whole sidebar list, which moves the page to the position shown; a drag
    released anywhere else just ends (GPUI drops it on mouse-up), and
    `render` forgets a stale position once no drag is active. Clicking
    still opens a page: GPUI only starts a drag past a 2px move, and a
    drag never becomes a click. The order is `page_order` (titles) in
    `state.toml`, as UI state like favorites, not a setting: a drop saves
    the whole order of regular pages. Absent or empty means alphabetical,
    so nothing changes until you drag. `sort_pages(pages, order)` is the
    one place that orders pages (listed pages by position, matched ignoring
    case, then unlisted ones alphabetically; stale entries are ignored),
    and `NoteSec::sort_pages` applies it keeping `selected` on the same
    page. Startup, adding a page, rename, a drop, "Sort pages A-Z" and
    undo/redo (a snapshot may hold the old order) all go through it; tabs,
    favorites and recent hold titles, so indices moving doesn't touch them.
    *New pages* (Ctrl-N, a followed `[[link]]`, the Today journal aside)
    are appended to `page_order` while a custom order exists, so they show
    up at the end of the list where you just made them, rather than at an
    alphabetical spot inside an order you chose by hand. Pages that appear
    otherwise (a file added outside the app) also come after the listed
    ones, alphabetically among themselves. Rename keeps the page's place
    (`UiState::rename` edits the entry in place); delete drops it
    (`UiState::forget`). *Sort pages A-Z* (a palette command and an item
    in the page menu, disabled while already alphabetical) clears
    `page_order`. Reordering isn't an undo step (like favorites).
31. **Local graph: a filtered `Graph`, same physics and drawing.** The
    graph view's toolbar has a `Global | Local` switch
    (`graph-mode-global` / `graph-mode-local`). Local shows the current page
    and every page one link away, in either direction: pages it links to,
    pages linking to it, and its tags. A "2 hops" chip (local only,
    `graph-local-depth`) reaches one ring further, which is the same
    breadth-first search one step longer, so it costs nothing. Tags became
    graph edges for this, in both modes: `Graph` now reads references with
    `parse_references` (`[[links]]` and `#tags`, a tag pointing at the page
    named like it, which the app creates anyway) instead of wikilinks only,
    so local and global agree on what a neighbour is. The filtering is pure
    and in `graph.rs`: `Graph::local_subgraph(center, hops)` keeps the
    nodes within `hops` edges and only the edges between kept nodes, with
    positions, pins and backlink counts unchanged (node size still means
    "linked from many pages" in the whole graph); `Graph::build_local`
    builds the graph with the journal filter (a journal centre is kept even
    with journals hidden, but no other journal, so none bridges two hops)
    and cuts it down. The result is an ordinary `Graph`, so `GraphView`
    runs the same simulation, camera and painting on it; `rebuild` picks
    global or local and `Graph::preserve_layout` (split out of the old
    `build_preserving`) keeps the positions of nodes that stay, so
    switching doesn't scramble the layout. The *current page* is the one
    the graph already highlights: the page last shown in a page tab
    (`selected`), passed in by `refresh` whenever the graph tab is focused.
    After clicking a node (which opens the page in a page tab, the graph
    tab stays, decision 20), coming back to the graph shows the local graph
    of that page. Switching scope or depth, and a new centre in local mode,
    refit the camera. The mode lives in the `GraphView`, which the app
    keeps for the session, so it survives leaving and reopening the graph
    but not a restart (not saved; it is a way of looking, not a setting).
    The palette's "Toggle local graph" shows the graph and flips the mode.
32. **Agenda: open tasks by date, as a tab.** `SCHEDULED:` /
    `DEADLINE:` markers are Logseq's: a line of the block (a continuation
    line under the task) that starts with either keyword and a date in
    angle brackets (`SCHEDULED: <2026-10-09 Fri>`, weekday and time
    optional, Logseq repeaters ignored). Both markers may share a line;
    the first valid one of each kind wins. The format is documented in
    `agenda.rs`. Open tasks are `TODO`/`DOING`/`LATER`/`NOW` (`DONE` is
    finished). A task's date is the earlier of its scheduled and deadline
    dates; a task on a journal page gets no date from the journal (only a
    marker counts, as in Logseq). Grouping relative to today is pure
    (`Agenda::build` in `agenda.rs`): Overdue, Today, Upcoming (one day
    header each, soonest first) and Unscheduled (open tasks with no
    marker, last). Within a group (or day), started tasks come first, then
    by page title. The agenda is a tab (`TabTarget::Agenda`, mode
    `Mode::Agenda`), like the graph: the sidebar's "Agenda" entry
    (`sidebar-agenda`) and the palette's "Open agenda" open or focus it;
    no keyboard shortcut (Ctrl keys are already taken). Built from the
    pages on every render, so finishing or dating a task shows up when
    you come back (or when you focus the tab again). Clicking an item
    opens its page in a new (or existing) page tab — the agenda tab
    stays, like the graph's — and unfolds the task's block; it is not
    put in edit mode, because the date lives on a second line and the
    editor is still single-line on this branch. Reading view leaves the
    marker lines as plain text for now: stripping them in `DisplayBlock`
    and drawing a chip would be nicer, but it is not free and the agenda
    itself already shows the date.
33. **The palette runs every command; one key table feeds hints and the
    cheatsheet.** *Commands*: `commands.rs` declares `Command` with one
    `commands!` row each: `Name => "Label", [keywords], Needs;`. `Name`
    is both the variant and the GPUI action in `app.rs` it dispatches
    (`window.dispatch_action`, deferred until the palette has closed),
    so a command and its key always do the same thing, through the same
    handler. Adding one is that row plus the action and its handler
    (needed for a key anyway); it then also appears in Settings >
    Shortcuts, rebindable (decision 41). *Keys*: `app.rs`'s `shortcuts()`
    is the DEFAULT keymap, one line per binding with its key context, its
    group (Navigation, Editing, View, Tabs, App) and a description. What
    is registered is that table with the user's overrides from
    `state.toml` on top (`hotkeys::effective_shortcuts`, decision 41):
    `bind_keys` registers the defaults at startup, and the window
    (`NoteSec::new`, and every change in Settings) replaces the keymap
    with the effective table (`register_keys`: `App::clear_key_bindings`,
    then `App::bind_keys`). Hints and the cheatsheet read that same
    effective keymap, so they follow every rebinding. *Hints*: a command's hint is read back from GPUI's keymap
    (`App::key_bindings` -> `Keymap::bindings_for_action`): the first
    binding without a key context, else the first binding (an editor
    command's "BlockEditor" key), written as `Ctrl+Shift+T`
    (`format_keystrokes`). So hints can't drift; the sidebar's Today and
    Settings hints and the empty state use the same lookup. *Cheatsheet*:
    "Keyboard shortcuts" or Ctrl+/ (free; Zed's comment key, which we
    don't need) opens a modal like Settings (dimmed backdrop, `occlude`,
    own key context "Shortcuts", Esc / backdrop / Ctrl+/ close it) listing
    `cheatsheet(&key_table())` (the effective table, with a link to
    Settings > Shortcuts at the top): per group, one row per description with
    all its keys ("Ctrl+= / Ctrl++"). *Palette*: an empty query lists the
    pages (as before, up to 12) and then every offered command under a
    "Commands" header; a query keeps the best 12 hits by the same fuzzy
    score as before (label, or keyword at a penalty) and shows them as two
    groups, pages and blocks / commands, the group with the best hit
    first so Enter still runs the top match. A "Pages" header appears
    when both groups do. Each command row shows its hint right-aligned
    and muted; the list scrolls and arrows keep the highlighted row in
    view (`ScrollHandle::scroll_to_item`). Enter or a click runs the
    command and closes the palette; Insert template keeps it open as the
    template picker, and Rename / Delete / Keyboard shortcuts open their
    own dialog. *Availability*: commands that make no sense are *hidden*,
    not greyed out (Enter always runs something real): `Needs::Page`
    commands (Rename, Delete, Copy title, Toggle favorite, Insert
    template, Collapse all, Expand all) need a page tab on screen (not the
    graph, the agenda, the trash or no tab), Rename also a non-journal and
    Delete more than one page, as in the page menu; `Needs::Editing` commands
    (Cycle task, and Coder 1's Move block / image paste at merge) need
    the palette to have been opened while editing a block — running one
    resumes that block with its cursor and selection, then dispatches.
    *New commands*: Collapse all / Expand all fold or unfold every block
    with children on the current page through the same `collapsed` set
    as the arrows, so like them they are not undo steps; Fit graph and
    Toggle journals in graph open the graph tab first, then press its
    Fit / Journals chips; Rename and Delete open the page menu's rename
    field (over the page title) or confirm dialog for the current page.
    *At merge* (both tracks): Move block up / down (Alt+Up / Alt+Down) and
    Insert image from clipboard (Ctrl+V) joined the `shortcuts()` table
    and the palette as `Needs::Editing` commands. The rename field shares
    the `BlockEditor` key context, so the block editor's multi-line keys
    are guarded by `text_input_open()` (palette or rename field open):
    Shift+Enter adds no line break, Up / Down don't move between lines,
    and Ctrl+V pastes text only and never saves an image. The sidebar's
    reorder line is `page_drop_line`, so it doesn't shadow `ui::drop_line`.
34. **Global search is the palette in a second mode, with exact matches.**
    Ctrl+Shift+F (or the palette's *Search all pages*) opens the Ctrl-K
    overlay with `SearchState.global` set, so it reuses the query box,
    arrow keys, Enter, Esc and mouse handling as they are; only where the
    results come from and how a row is drawn differ. `search::search_text`
    is a substring search ignoring case (`find_ignore_case` compares
    character by character, so offsets stay on the text's own char
    boundaries), not the fuzzy match: across hundreds of pages, scattered
    letters match nearly everything. It lists page titles that contain
    the query first (whole title, then title prefix, then anywhere), then
    one result per matching *line* of a block, in page and document
    order, journals included, at most 100. A line result is a
    `Target::Match` holding the byte range in the block's `content`, and
    the row shows the page title over the matching line (`search::snippet`:
    indentation dropped, cut to about 90 characters around the match with
    "…", the match in bold accent). Enter or a click opens the page in a
    tab and edits the block with the match *selected*: that is the
    highlight, and it uses the editor's own selection, so there is no
    extra state to clear. Searching scans every block on each keystroke;
    like backlinks, an index can come later if it is ever slow.
35. **"Linked from" covers block references too, and is always shown.**
    `model::backlinks` takes the page (an index) instead of a title, so
    besides `[[Title]]` and `#Title` it can also match `((id))` references
    to any of that page's blocks; a block that does both is listed once,
    and the page's own blocks never count. Every page ends with the
    "Linked from" section: a page row per linking page (click opens it)
    and a row per linking block (click opens its page and unfolds down to
    it), with the page and reference counts, or "No other page links here
    yet." The block rows show reading text (`DisplayBlock::with_refs`), so
    a block reference reads as the text it points at, not `((uuid))`. It
    is still a full scan per render, as before.

36. **Page aliases are read from the page, not stored anywhere else.**
    A page declares other names with an `alias::` line in its first block
    (`alias:: JS, [[ECMAScript]]`, comma-separated; the key ignores case).
    `model::page_aliases` reads them on demand and `model::resolve_page`
    turns a link name into a page: a real title first, then the first
    page (in order) claiming the alias, so an alias never steals a link
    from a page that really has that name. Everything that follows links
    goes through it: `navigate` (so clicking `[[JS]]` opens JavaScript
    and committing the block creates no "JS" page), "Linked from", the
    graph's edges and global search (an alias match ranks like a title
    match, one hit per page). Typing `[[` now opens a page picker in the
    same place as the `((` block picker (the same code, `RefKind`/
    `RefItem`; whichever was typed last wins): it fuzzy-matches titles
    and aliases, shows "alias: X" when an alias matched, and inserts the
    real title, taking in a `]]` that's already after the cursor. Links
    keep the alias text you typed by hand; nothing rewrites them. The
    Ctrl-K palette still matches titles only. Reading aliases per lookup
    is a scan over pages, like backlinks; fine at this size.

37. **Trash: deleting moves the file to `.trash/`; restore, delete
    forever, empty** (34-36 are the pages track's). *Layout*: one folder
    per deleted page, `<graph>/.trash/<millis>/<relative path>`, e.g.
    `.trash/1791545112345/pages/Area___Sub.md` or
    `.../journals/2026_10_08.md`. The folder name is the deletion time in
    milliseconds since the Unix epoch (sorts, needs no time zone, unique:
    `create_dir` bumps it by one if two deletions share a millisecond, so
    the same title can be in the trash several times). Keeping the path
    relative to the graph is what lets Restore put the file back exactly
    where it was. A hidden folder outside `pages/` and `journals/` is
    never read as pages (`load_all` only reads those two), so the page
    list, graph, agenda, palette search and tags (all built from the
    loaded pages) ignore it without a filter; a test drops a fake
    `.trash/pages/*.md` to prove it. Anything that later scans the graph
    folder itself (git backup, a full-text index on disk) must skip
    `.trash/` too. *Storage* (`storage.rs`, pure `std::fs`): `trash`
    (move with `fs::rename`, atomic on one filesystem; a page whose file
    is missing gets its content written instead), `list_trash` (newest
    first; folders that aren't a number with exactly one page file are
    skipped and left alone), `restore`, `delete_forever`, `empty_trash`
    (only the entries `list_trash` recognises, so stray files a user put
    there survive). The old `Storage::delete` is gone. *Delete*: the page
    menu and the palette's "Delete current page" keep decision 29's
    confirm dialog, now "Move “X” to the trash?" with a danger "Move to
    trash" button. Everything else is as before: tabs close, undo history
    is cleared, and favorites, recent and the custom order *forget* the
    page (`UiState::forget`). A restored page comes back like a new one:
    unstarred, at the end of a custom order (`add_page`), in RECENT once
    opened. Remembering them would mean stale `state.toml` entries that a
    new page with the same name would inherit. *Last page*: still can't
    be deleted. The reason is unchanged: the app always has a loaded page
    to show (`pages[selected]`); that the file could be restored doesn't
    change what is loaded. *Trash view*: a tab like the agenda
    (`TabTarget::Trash`, `Mode::Trash`), opened by the sidebar's "Trash"
    row under Agenda (`sidebar-trash`, with a count, `sidebar-trash-count`,
    while non-empty) or the palette's "Open trash" (keywords deleted,
    restore, bin, recycle, undelete; no key). It lists entries newest
    first (`trash-item-{i}`): title, "Journal" for journals, "Deleted
    today 14:05" / "yesterday" / a date (local time, `deleted_label`), and
    Restore (`trash-restore-{i}`) and Delete forever (`trash-delete-{i}`)
    buttons; Empty trash (`trash-empty`) is at the top, muted and inert
    while empty (`trash-empty-state`). The list is cached in `NoteSec::trash`
    and re-read at startup, when the tab is focused and after each change.
    *Restore* moves the file back and adds the page, so it is in the
    sidebar, graph, agenda and search right away; the trash tab stays
    open. *Name clash*: Restore is refused while a page with that name
    exists (ignoring case for regular pages, as links and rename do; the
    same date for journals, e.g. today's journal re-created by Ctrl-J),
    with the reason in the view (`trash-error`) and the entry kept.
    Nothing is restored under another name, because `[[links]]` find
    pages by name and a "Notes (restored)" would silently lose them; the
    user renames or deletes the other page, then restores. Restore also
    clears the undo history (a snapshot from before it would drop the
    page). *Delete forever / Empty trash* ask first in the same modal as
    the page delete, now shared as `render_confirm` (dimmed backdrop,
    Cancel, danger button; backdrop click or Esc cancels, Enter does
    nothing): `trash-confirm`, `-ok`, `-cancel`. Esc works through a
    "TrashDialog" key context (one more `escape` row in `shortcuts()`).
    The question closes when the palette, settings, shortcuts list or
    another tab or page takes over. *Later features*: git auto-backup
    should put `.trash/` in the graph's `.gitignore` (the history already
    keeps deleted content, and the trash would double it); split panes can
    show the trash in either pane like any tab target.
38. **Export: the current page to one self-contained HTML file.** The
    palette's "Export page to HTML" (`ExportHtml`; keywords export, html,
    save, share, web page; no key; `Needs::Page`, so it is hidden on the
    graph, agenda and trash tabs, and the action does nothing there)
    saves the block being edited, then writes
    `<graph>/exports/<page file name>.html` (`Area___Sub.html`,
    `2026_10_08.html` for a journal: `Storage::export_path` reuses
    `path_for`, so one page always lands on one file and re-exporting
    replaces it, atomically via `write_atomic`). `exports/` is outside
    `pages/` and `journals/`, so exports are never loaded as pages.
    *Pure builder*: `export::page_html(page, resolve_ref, load_image)`
    returns the whole document as a `String`; it touches no files and no
    GPUI. The two closures are the only outside facts: `resolve_ref` maps
    a block id to the referenced block's text (`find_block` over the
    loaded pages) and `load_image` reads image bytes (`assets::resolve` +
    `is_image_path`), so unit tests pass fakes. *Same meaning as the app*:
    it reuses the reading view's parsers (`split_code`, `parse_table`,
    `parse_images`, `DisplayBlock::with_refs` and its `segments`), so a
    block can't export differently from how it reads. Blocks become nested
    `<ul class="outline"><li>` by depth (a jump of more than one level is
    clamped, like the outline). Folded blocks are exported unfolded: a
    file has no fold button, and hiding content in an export would lose
    it. Headings become `heading1-3` blocks, `>` quotes `quote`, paired
    `*`/`**` `<em>`/`<strong>`; task keywords are badges
    (`<span class="task task-todo">TODO</span>`) and a DONE block is
    struck through, as in the app. `[[links]]` and `#tags` are styled
    spans with the page name in `data-page`, not `<a>`: the pages they
    point at aren't exported, and a dead link is worse than none. Block
    references show the referenced text (`<span class="ref">`); an
    unknown id shows what the reading view shows. `SCHEDULED:` /
    `DEADLINE:` lines (those `agenda::parse_dates` accepts, never a
    block's first line) get their own muted line with the keyword in bold.
    Fenced code becomes `<pre><code>` with its language label; tables keep
    their column alignment. Inline `` `code` `` is not special, because
    the reading view doesn't support it either; when it does, the export
    should follow. *Escaping*: every piece of user text goes through
    `export::escape` (`& < > " '`), including attributes and code.
    *Self-contained*: one inline `<style>` (light, dark through
    `prefers-color-scheme`, and `@media print` without backgrounds), no
    scripts, no external links, and a Content-Security-Policy meta
    (`default-src 'none'; img-src data:; style-src 'unsafe-inline'`), so
    a browser refuses anything else even if a bug let it in. Images are
    embedded as `data:` URIs (`export::base64`, a small RFC 4648 encoder
    tested against the RFC's vectors; no new crate). PNG, JPEG, GIF, WebP,
    BMP and TIFF are embedded; web images (`https://...`) are not fetched,
    and missing or unknown files show a note ("Image not found: ...").
    *Status message*: `NoteSec::status` shows "Exported to <path>" (or
    "Export failed: <error>" in the danger colour) at the bottom right
    (`status-toast`) for `STATUS_FOR` (5 s); a newer message replaces the
    timer, like the copy button's "Copied". Other features can reuse
    `show_status`. *PDF: not done.* GPUI has no print or PDF API; the
    platform calls it offers here are `open_url`, `open_with_system` and
    `reveal_path`. Those could open the HTML in a browser but not reach a
    print dialog, and rendering PDF ourselves would need a new crate and a
    second layout engine. Instead the export prints well: the browser's
    Print -> Save as PDF uses the `@media print` styles. *Git backup*:
    `exports/` should go in the graph's `.gitignore`, like `.trash/`. It
    is derived output that one command re-creates, and embedded images
    would bloat the history.
39. **Git auto-backup: debounced local commits of the graph folder.**
    *Consent*: `git_backup` in `config.toml`, **off by default**, so
    nothing touches a folder's git state until the user asks. It is
    switched in Settings ("Git auto-backup", Off / On buttons
    `git-backup-off` / `git-backup-on`, with a muted note
    `git-backup-note`) or by the palette's "Toggle git auto-backup"
    (`ToggleGitBackup`; keywords git, backup, version, history,
    autosave; no key; always offered). *How*: `backup.rs` runs the `git`
    program with `std::process::Command` (no libgit2 / `git2` crate), so
    it behaves exactly like the user's git, config and hooks included.
    Each call blocks, so `app.rs` runs them with `cx.background_spawn`
    and never on the UI thread (except at quit, below). Every command is
    `git -C <graph> ...`, with stdin closed, `GIT_TERMINAL_PROMPT=0` and
    `GIT_EDITOR=true` (never waits for input), and `GIT_DIR`,
    `GIT_WORK_TREE`, `GIT_INDEX_FILE` and friends removed (a stray one
    would point git at another repository). *Local only*: the commands
    used are `rev-parse`, `check-ignore`, `init`, `config --get`,
    `status`, `add` and `commit`. Never push, fetch, remote, `--force`,
    reset or amend; a test checks no remote ever appears. *Identity*: if
    git has no `user.name` / `user.email` (`git config --get`), the
    missing one is passed as `-c user.name=notesec` /
    `-c user.email=notesec@localhost` for that commit only, so commits
    never fail for lack of identity and a configured identity is used as
    is. *Turning it on* (`start_backup`, also at startup when on, which
    picks up edits made while the app was closed): `backup::prepare`
    finds the repository (`rev-parse --show-toplevel`) or runs `git init`
    in the graph folder, then appends to the graph's `.gitignore` the
    lines it lacks among `.trash/`, `exports/`, `published/` (decision
    49) and `.*.tmp` (the temp
    files of atomic saves) under a `# notesec: not backed up` comment.
    The user's own lines are never changed or reordered, and `/.trash`
    or `.trash` counts as present. Then everything is committed at once.
    The status says where: "Git backup on: new repository in <graph>",
    "Git backup on: <graph>", or the repository-above form below.
    *Repository above the graph* (e.g. the graph is `notes/` inside a
    dotfiles or project repository): no nested repository is made (the
    outer one would then see an embedded repository). Instead every
    status / add / commit is limited to the pathspec `.` run from the
    graph folder, so only files inside the graph are staged and
    committed: `git commit -- .` takes just those paths, and anything the
    user staged elsewhere stays staged and out of our commits (tested).
    The status names both folders ("committing <graph> in the repository
    at <root>"), so the user knows their repository gets the commits. If
    that repository ignores the graph (`check-ignore` on `pages`, e.g. a
    home-folder repository with `*` in its `.gitignore`), nothing would
    ever be committed, so it is refused instead. *Debounce*: `Storage`
    counts page file changes (`changes()`: save, rename, trash, restore;
    not exports, delete forever or empty trash, which only touch ignored
    folders). `NoteSec` watches its own notifications (`cx.observe_self`)
    and, when the count moved, restarts a `BACKUP_AFTER` (5 s) timer
    (`schedule_backup`; replacing the task cancels the old wait). So one
    commit follows a burst of edits, 5 s after the last save. That one
    hook catches every save path (block edits, structure changes, pages
    created, renamed, deleted or restored, pasted images with their
    block) without a call at each save site. `config.toml` and
    `state.toml` (favorites, order, recent) are committed along but don't
    start a commit themselves: opening a page changes RECENT, and that
    alone shouldn't make a commit. *No empty commits*: `backup::commit`
    first runs `git status --porcelain --untracked-files=all -- .` and
    stops if it is empty. The message is
    `notesec autosave: N file(s) changed` (N from that status). A global
    lock keeps two commits (the timer's and quit's) from racing for
    `index.lock`. *Quit*: `on_app_quit` (Ctrl-Q) and the view's
    `on_release` (closing the window, which on Linux also quits) both
    call `flush_backup`. If changes are waiting for the timer, it commits
    right away on the UI thread, which is closing anyway. GPUI gives quit
    handlers 200 ms (`SHUTDOWN_TIMEOUT`), and a blocking call inside the
    handler isn't cut off by it. Otherwise it waits for a commit that is
    running. Both paths are tested. A block being edited when the app
    quits isn't saved by the app (as before), so it isn't committed
    either. *Errors*: never fatal. Without git (or with a repository
    above that ignores the graph), turning it on fails, the setting goes
    back to Off and the status says "Git backup is off: git is not
    installed" in the danger colour. The same happens at startup
    (saved Off too, so Settings shows the truth and a later config save
    can't disagree); the user turns it on again once git is there. A
    failed commit (say a
    pre-commit hook refused) shows "Git backup failed: <git's first
    error line>" and stays waiting, so the next change or quit tries
    again. Successful autosaves are silent. *Not handled*: GPG signing
    the user turned on (`commit.gpgsign`) applies to these commits too,
    and if it needs a passphrase prompt, commits fail and say why. We
    don't override the user's signing choice. *Tests*: real git in temp
    folders (skipped, not failed, without git), with an isolated config
    (`GIT_CONFIG_GLOBAL=/dev/null`, `GIT_CONFIG_NOSYSTEM`,
    `GIT_CEILING_DIRECTORIES`). The UI tests drive the timer with
    `advance_clock`.
40. **Split panes: two pages side by side, one shared tab bar.**
    *Commands* (palette group Tabs): "Split right" (`SplitRight`,
    Ctrl+\\, keywords split view, side by side, second pane, two pages),
    "Close pane" (`ClosePane`, Ctrl+Shift+W, unsplit, single pane) and
    "Focus other pane" (`FocusOtherPane`, Ctrl+|, switch pane, other
    side). Close pane and Focus other pane are offered only while split
    and do nothing otherwise. None of the three acts under an overlay
    (palette, settings, page menu, shortcuts). *Keys*: the bindings are
    `ctrl-\` and `ctrl-|`, not `ctrl-shift-\`. On Linux GPUI reports
    the shifted character with shift dropped (`keystroke_from_xkb` in
    gpui_linux: Shift+\\ arrives as key `|` without shift), and a binding
    matches only with equal modifiers and key, so `ctrl-shift-\` would
    never fire. Same rule as `ctrl-+` for Increase font. So Focus other
    pane is Ctrl+Shift+\\ on a US layout, shown as "Ctrl+|". Ctrl+Shift+W
    arrives as `ctrl-shift-w` (a letter keeps its shift) and doesn't
    collide with Ctrl+W (Close tab): modifiers must match. A test checks
    both bindings. *At most two panes*: `split: Option<Split>`, where
    `Split { right: String, right_focused: bool }`. Splitting again only
    focuses the right pane. **Tab bar: one, shared, owned by the left
    pane.** The left pane is the existing main view: the tab bar and its
    active tab (page, graph, agenda or trash). The right pane shows one
    page with a small header: its title (accent while focused,
    `pane-right-title`) and a × (`pane-close-right`). The left header is
    the tab bar plus a × (`pane-close-left`). Per-pane tab bars were
    rejected: a second `Tabs`, with its own RECENT, history and close
    rules, would double the state every tab action has to keep straight,
    and everything that already works through the tab bar (graph,
    agenda, trash, the neighbour after a close) would need a "which bar"
    argument. The right pane holds pages only; graph, agenda, trash and
    settings stay where they were, and opening one (or any tab action:
    clicking a tab, Ctrl+Tab, Ctrl+W on a tab) focuses the left pane.
    *One focused pane*: `selected`, `mode`, `editing` and the current
    page always describe the focused pane, so every existing command
    (rename, delete, favorite, copy title, export, collapse / expand all,
    templates, the local graph, undo) acts on the focused pane's page
    without knowing about panes. A 2px accent line tops the focused pane
    (`pane-focus-left` / `pane-focus-right`); the other pane's line is
    the border colour. *Navigation*: `show_page` / `navigate` /
    `open_page` keep their signatures; their funnel `show_page_in` sends
    the page to the focused pane: with the right pane focused it replaces
    the right pane's page and leaves the tabs alone; otherwise tabs work
    as before. So the sidebar, links, block references, backlinks,
    search and the agenda all open into the focused pane. *One editor*:
    only the focused pane edits. Switching focus (key, palette, or a
    press anywhere in the other pane) saves and ends the edit. The press
    is caught in the capture phase on the pane (`capture_any_mouse_down`)
    and focuses it before the children's handlers run, so the same click
    then acts there (starts editing the block, follows the link, clicks
    the ×). The unfocused pane draws the page read-only (no drag-move
    targets, no image or file drop). Its debug selectors start with
    `other-` (`other-block-0`): selectors are a per-frame map, so the two
    panes can't share them. *Same page in both*: both draw from the same
    `pages`. The other pane mirrors the block being edited live (it reads
    the editor's text for that row), and structure changes show at once.
    Folds are per block, so they show on both sides. *Close pane* closes
    the focused pane. Closing the right pane leaves the tabs as they
    were. Closing the left pane moves the right page into the tab bar
    (its tab, or a new one, focused). Ctrl+W in the focused right pane
    closes that pane rather than a tab. *Stable identity*: the right
    pane stores its page's title, not an index, so sorting and reordering
    are harmless. Rename updates it. When the page goes away (deleted
    from either pane, or an undo removing it), `prune_split` in
    `apply_tab` / `restore_history` closes the right pane, so no stale
    index is ever drawn. Restoring the page from the trash doesn't reopen
    the pane. *Undo* stays app-wide; with the right pane focused, the
    restored page is shown there. *Not persisted*: the split isn't saved
    in `state.toml` (a fresh start has one pane). Nice to have later:
    save `Split` with the tabs, and a draggable divider (the panes are
    equal halves now).
41. **Custom hotkeys: rebind any command in Settings, saved in
    state.toml.** *What can be rebound*: every `commands!` command (the
    palette's list), including palette-only ones, which start
    "Unbound" and can get a key. The other rows of `shortcuts()` are
    fixed: Ctrl+K (the palette is how you reach everything, so it can't
    be lost), the editing core (arrows, Home / End, Backspace, Delete,
    Enter, Tab / Shift+Tab, Shift+Enter, the selection keys, Bold /
    Italic) and Esc in dialogs. *Model*: `UiState.shortcuts`, a
    `[shortcuts]` table in state.toml from the command's name to one
    keystroke in GPUI's binding syntax, or `""` for unbound
    (`SplitRight = "ctrl-alt-s"`). The name is the `commands!` name,
    which is also the GPUI action's name without `notesec::` (a test
    checks they agree); it is stable unless someone renames the action.
    The context isn't stored: each command binds in one context
    (`Command::key_context`: "BlockEditor" for `Needs::Editing`
    commands, else global; a test checks the default table agrees). The
    table is written only when non-empty. *Applying*
    (`hotkeys::effective_shortcuts`): start from `shortcuts()`; for each
    command with a valid override, drop ALL its default rows and add one
    row with the new key (none if unbound), in its context, keeping the
    group and description of its first default row (App and its label if
    it had none). So a command with two default keys (Redo: Ctrl+Shift+Z
    and Ctrl+Y; Increase font: Ctrl+= and Ctrl++) has exactly the one
    new key after a rebind; its ↺ gives both back. Choosing a command's
    own single default key again removes the override instead of storing
    it. *Lenient file*: unknown names (an older or newer build's
    command, e.g. a not-yet-merged one) and invalid values (not a key,
    several keys, a bare key) are ignored, so the command keeps its
    default keys. They stay in the file until Reset to defaults. A
    non-string value is dropped, and a `shortcuts` that isn't a table
    reads as none, so a slip there never costs favorites or recents
    (`lenient_shortcuts`). *Registering*: `register_keys` clears GPUI's
    keymap (`App::clear_key_bindings`) and binds the effective table
    (`App::bind_keys`). Both are on `App` and refresh the windows. The
    app-wide Quit handler from `bind_keys` is an action handler, not a
    binding, so it survives. Palette hints and the cheatsheet read the
    live keymap / effective table, so they update at once (tested).
    *UI*: Settings has two sections, General (as before) and Shortcuts
    (`settings-tab-general` / `settings-tab-shortcuts`). The panel is
    capped at the window's height and its body scrolls (`settings-body`),
    so large fonts don't push it off screen (tested at the largest font
    in a 1100×700 window). Shortcuts lists every command (`hotkey-row-
    <Name>`) in palette order in a 320 px scrolling list: the label, a
    "changed" marker (`hotkey-modified-<Name>`) when a valid override
    exists, the key chip (`hotkey-key-<Name>`: "Ctrl+Shift+Z / Ctrl+Y",
    or "Unbound" muted), and a ↺ per changed row (`hotkey-reset-<Name>`).
    Below are a message line (`hotkey-message`) and "Reset to defaults"
    (`hotkeys-reset-all`). No filter box: the palette's text fields are
    built on the block editor and its "BlockEditor" context, and a third
    one wasn't worth it for one short list. Opening it: the tab, the palette's
    "Change keyboard shortcuts" (`CustomizeShortcuts`; customize, rebind,
    hotkeys, key bindings, remap; no default key), or the cheatsheet's
    link (`shortcuts-customize`). *Capture*: clicking a chip shows "Press
    keys…" (clicking it again stops). GPUI dispatches key bindings
    BEFORE key-down listeners (`Window::dispatch_key_event`: actions
    first, `on_key_down` only if no action stopped propagation), so a
    key-down listener couldn't stop the old binding. Instead `NoteSec`
    registers a keystroke interceptor (`App::intercept_keystrokes`).
    Interceptors run before binding matching, and `stop_propagation`
    there skips the bindings. While a capture waits, the interceptor
    takes every key: a lone modifier keeps waiting; Esc (plain) cancels
    (a second Esc closes Settings as before); Backspace / Delete (plain)
    unbind; any other key goes through `choice_for`, then the conflict
    check. The key is stored as `Keystroke::unparse` of what GPUI
    reported, checked to parse back to the same key and modifiers, so
    it matches when pressed again. On Linux, Ctrl+Shift+\ arrives (and
    is stored) as `ctrl-|`, as decision 40 binds it (tested round trip,
    and through a fresh window). Tests prove the captured key didn't
    run its old command (Ctrl+G, Ctrl+K, Ctrl+Shift+T). *Bare keys*: a
    command's key needs Ctrl, Alt or Super, or is a function key
    (F1–F24, alone or with modifiers). A plain or Shift+ key would type
    text, since commands work while a block or the palette takes typing.
    So "A", "Shift+A", "Enter" and "Space" are refused with "X would type
    text: add Ctrl, Alt or Super (F1-F24 work alone)", and the capture
    keeps waiting. The same rule applies to values read from the file.
    *Conflicts: refused, never moved.* If the key already belongs to
    another binding where contexts overlap (either is global, or both
    are "BlockEditor"; the dialog contexts never overlap the editor's),
    the message names the owner: another command ("Ctrl+G is already the
    key of “Toggle graph view”. Change or unbind that one first.") or a
    fixed key ("…is used for “Search pages, blocks and commands”, which
    can't be changed."). Nothing changes and the capture keeps waiting.
    Moving silently would unbind something the user may not notice. Two
    steps (Backspace on the other row, then capture) do a swap. Conflicts
    in a hand-edited file aren't checked; both bindings are registered
    and GPUI picks one. *Reset to defaults*: one click, no confirmation.
    It only touches key overrides (never notes or settings), the defaults
    are listed on screen, and a ↺ per row exists for finer undo. It
    clears every override (unknown names too), saves (the `[shortcuts]`
    table disappears), re-registers, and says "Every shortcut is back to
    its default." *Adding a command later* (e.g. Coder 1's global
    search) needs nothing here: its `commands!` row gives it a Settings
    row, a name for state.toml, a context from its `Needs`, and a
    binding (`Command::binding`, generated by the macro). Its default
    key is one `shortcuts()` row, in the same context (global for
    `Needs::Nothing`).

47. **Live embeds: `![[Page]]` and `![[((id))]]` show the source inline.**
    *Syntax* (`embed::parse_embeds`): `![[Page name]]` embeds a whole
    page, `![[((block-uuid))]]` a block with its children, and Logseq's
    own `{{embed [[Page]]}}` / `{{embed ((uuid))}}` mean the same (an
    imported Logseq graph uses them; "embed" in any case, spaces free).
    Embeds inside fenced code are code. A malformed one (empty name, bad
    uuid) is just text. A page name resolves exactly like a link
    (`resolve_page`: title in any case, then aliases), so `![[JS]]`
    embeds JavaScript. *What it is underneath:* the `[[Page]]` of a page
    embed is an ordinary wikilink and the `((id))` of a block embed an
    ordinary block reference, so with no extra code "Linked from" counts
    embeds (as Logseq does), saving creates a missing embedded page like
    any link target, the embedded block's `id::` is saved, and `[[`
    after `!` opens the page picker and inserts the canonical title.
    `parse_wikilinks` skips a `[[((uuid))]]` inner (it names a block, not
    a page "((uuid))"). The graph adds an edge for a block embed to the
    block's page (`graph::build`); page embeds were edges already.
    *Live:* `embed::Resolver` reads the pages in memory each frame (and
    the editor's unsaved text for the block being edited), so a change to
    the source shows in every embed at once, also in the other split pane
    while typing. Nothing is copied into the embedding page's file.
    *Read-only:* the box shows the source's title (click: open the page)
    and its blocks as an indented outline (click a block: open its page
    and edit it there, `open_block_ref`). Editing in place would need a
    second editor per embed and make "which block is being edited"
    ambiguous across panes; one click to the real block is cheap. The
    embedding block keeps its own text visible above the box (like
    `{{query}}`), so a click on the text edits the embed itself. All
    blocks of the source are shown: folding is a per-page view setting
    and isn't carried into embeds. *Guards* (one resolver for the app and
    the export): an embed of something already being shown further out
    (a page embedding itself, A → B → A, a block inside itself) is drawn
    once and then "Circular embed of “A”"; nesting stops after
    `MAX_EMBED_DEPTH` = 4 levels with "Embeds nested more than 4 deep are
    not shown"; a missing target says "Page “X” not found" / "Block not
    found". A trashed page is not in `pages`, so it is missing. *Export*
    (`export::page_html_with_embeds`) draws the same tree inline in a
    bordered `.embed` box, notes as `.embed-note`. *Search and agenda*
    read only the pages' own text, so embedded content is found (and a
    task listed) once, on its source page. Block references to blocks
    shown inside an embed resolve as always (by id).

48. **Vim mode: normal/insert/visual editing behind a Settings toggle.**
    Off by default (`vim_mode` in `config.toml`); Settings > General >
    Editor > "Vim keybindings" (Off / On), or the palette's "Toggle vim
    mode" (`ToggleVimMode`, no default key, rebindable in Shortcuts like
    every command). With it off nothing below runs. *Split:* `vim.rs` is
    a pure state machine over the editor's `EditorState` (text, cursor,
    selection): it moves and edits within the block itself and returns an
    `Effect` for what needs the outline or the app (other blocks, undo,
    save, stop). `app/vim_ui.rs` feeds it keys and applies the effects;
    `app.rs` has one-line hooks. *How keys arrive:* through the keystroke
    interceptor, like hotkey capture (decision 41), because it runs before
    the keymap AND before text input; a handled key calls
    `stop_propagation`, so it neither fires a binding nor types. In Normal
    and Visual mode the text-input handler also drops text (an IME commit
    can arrive without a key-down), and the `[[` picker doesn't open.
    *Which keys vim takes:* only while a block is being edited, and never
    while the rename field, search, the palette, Settings (incl. key
    capture), the Shortcuts sheet, a page menu or a trash confirmation is
    open (and any later text input that sets `text_input_open`). In Insert
    mode only Esc, and not while the slash menu, the `[[` picker or IME
    composition is open (Esc closes those first, as without vim). Ctrl /
    Alt / Super keys pass through to the app's shortcuts (Ctrl+K opens
    the palette from Normal mode), except Ctrl-R (redo); arrows and Tab
    pass too (Tab indents). Vim's own keys are fixed, not rebindable: a
    modal grammar of operators, counts and motions doesn't fit a
    one-key-per-command table (`d2w`, `3dd`). *Modes:* clicking a block (or any start of
    editing) begins in Normal mode, as vim opens a file; a mouse
    selection becomes Visual. `i a I A o O`, `c…`, `s` enter Insert; Esc
    goes back (cursor one left, as vim); Esc in Normal clears a pending
    command, else stops editing. j/k keep the current mode; Enter and
    Up/Down in Insert stay in Insert (the app's own keys).
    *The block is the line* for linewise commands: `dd`/`yy` take the
    block with its children (a count takes that many sibling subtrees,
    never climbing to a parent); `p` pastes them after the block's
    subtree (replacing an empty childless block, like paste), `P` before
    it; `o`/`O` open a block below/above; `>>`/`<<` indent/outdent; `cc`
    clears the text. Deleting the last block leaves one empty block. j/k
    move over the block's own lines (not wrapped rows), then into the
    previous/next visible block keeping the column; gg/G go to the
    first/last visible block. Within a block: h l w b e 0 ^ $, x X dw d$
    D cw C s S, yw y$, r, ~, counts, v (characterwise, inclusive) and V
    (the block). Graphemes are never split. *Register:* one internal
    register, text or blocks; not the system clipboard (Ctrl+C/V still
    are). *Undo:* each Normal-mode change is one step of the app's undo;
    `u`/Ctrl-R are its undo/redo. *`:` line:* `:w` saves now and says
    "Saved “Title”", `:q` stops editing, `:wq`/`:x` both, anything else
    "Not an editor command: …" as an error status. *Indicator:* a pill at
    the bottom left shows the mode and pending keys (`NORMAL  2d`) or the
    `:` line being typed. Split panes share the one editor, so one vim.

49. **Publish: a page as a self-contained static site in `published/`.**
    The app doesn't host anything: "Publish page" (`PublishPage`) writes
    a folder the user puts on any static host (GitHub Pages, Netlify
    Drop, `python3 -m http.server`), and that folder is the "link". Like
    Export, both commands need a page on screen (hidden on the graph,
    trash and agenda tabs) and have no default key. *Layout*
    (`publish.rs`): `<graph>/published/<slug>/` holds `index.html` (the
    page), `<slug>.html` per linked page, `style.css` (the export's
    stylesheet), `assets/<name>` (each image the pages show, once, cleaned
    of metadata; real files rather than the export's base64: lighter and
    cacheable on a host), `README.txt` (how to host it) and `.notesec-bundle` (the
    manifest: the page's title and the files the app wrote). *Rendering*
    is the HTML exporter (`export::document`, which `page_html_with_embeds`
    now calls too): the same outline, embeds inline (decision 47), block
    references resolved, no scripts, nothing from elsewhere. The CSP is
    `default-src 'none'; img-src 'self'; style-src 'self'` (checked in
    Chrome: the stylesheet and images load from disk and over http), so
    table alignment became classes instead of inline `style=` (in exports
    too). *Linked pages:* "Publish page with linked pages"
    (`PublishPageWithLinks`) adds the pages the page links to one hop away
    (`[[links]]` and `#tags` in its own blocks): existing, not journals
    (a diary is published only on purpose, as the page itself), not
    private. Links between published pages become relative
    `<a href="beta.html">` (and back to `index.html`); links to anything
    else stay styled text, as in an export. A linked page named "Index"
    gets `index-2.html`. *Private pages:* `public:: false` or
    `private:: true` among a page's properties (its first block, keys and
    values in any case) means never published: publishing it is refused
    with an error status (nothing written); it is left out as a linked
    page; an embed of it (or of a block on it) shows "Private page, not
    published"; a block reference to one of its blocks stays `((uuid))`.
    *Properties:* every `key:: value` line (outside code) is left out of
    the published pages: `alias::`, `tags::`, `public::` are metadata,
    and a block holding only properties (the page properties) disappears
    unless it has children. `id::` lines never reach a block's content.
    *Privacy:* the bundle contains only the files above. `state.toml`
    (with the AI key), `config.toml`, `.trash` and the pages' markdown
    are never read for it; images are copied only from inside the graph,
    not from hidden folders (`.trash`), `exports/` or `published/` (else
    "Image not published"), and an image's note shows only its file name,
    never a local path. *Image metadata* (`publish::clean_image`): every
    image is decoded and re-encoded from its pixels, so no EXIF (camera,
    time, GPS position), XMP, IPTC, ICC profile or text chunk survives;
    the `image` crate's encoders write metadata only when given it
    (`set_exif_metadata` / `set_icc_profile`, never called), checked in
    its 0.25 source. The format is read from the bytes. An EXIF
    orientation (`ImageDecoder::orientation`) is applied to the pixels
    first (`DynamicImage::apply_orientation`), so photos stand the right
    way up without the tag. Per format: JPEG stays JPEG at quality 90;
    PNG stays PNG, lossless; WebP stays WebP, re-encoded lossless (the
    crate has no lossy WebP encoder, so files can grow; an animated WebP
    keeps its first frame); an animated GIF stays a GIF (frames and
    delays kept, looping forever), a one-frame GIF becomes PNG; BMP and
    TIFF become PNG (TIFF tags carry EXIF and browsers mostly can't show
    TIFF). A renamed file (`scan.bmp` → `assets/scan.png`) is referenced
    by its new name. A file that doesn't decode is not published (note
    "Image not published"): copying it could carry the very metadata
    this removes. SVG never gets here (the exporter shows it as "Not an
    image": it could carry scripts). *Off the UI thread:* publishing
    (rendering, re-encoding photos, writing) runs on a background thread
    over a copy of the pages, with "Publishing “Page”…" meanwhile; a
    second publish while one runs is refused with a status. *Slug:*
    lowercase ASCII from the title (accents folded, `ß` → `ss`, other
    runs → one hyphen, at most 60 bytes; "page" when nothing is left,
    e.g. a title in another script), made unique among the published
    folders with `-2`, `-3`. A page finds its own folder again through
    the manifest's title, so republishing reuses it; a folder without our
    manifest (or another page's) is never written into. Renaming a page
    publishes to a new folder; the old one stays until deleted.
    *Republishing* rewrites the bundle's files and deletes those the
    previous manifest listed that aren't needed any more (a removed
    image, a page no longer linked), and an emptied `assets/`. Files the
    user added (a `CNAME`) stay. Paths are checked: a manifest entry with
    `..` or an absolute path is ignored, a folder inside the bundle that
    is a symbolic link is never written through or deleted from, and a
    file that is a link is replaced, not written through. *Afterwards*
    the status says "Published “Page” (and N linked pages) to <folder>"
    with "Open folder" (`App::open_with_system`) and "Copy path" buttons,
    shown while that status is. `published/` is not backed up (decision
    39's `IGNORED`) and never loaded as pages (only `pages/` and
    `journals/` are).

50. **Import: Obsidian vaults, Logseq graphs and Notion exports.**
    "Import from Obsidian…", "Import from Logseq…" and "Import from
    Notion…" (`ImportObsidian`, `ImportLogseq`, `ImportNotion`; no key,
    any tab) ask for a folder (`App::prompt_for_paths`, directories
    only) and bring its notes into this graph as pages, links kept.
    *Split:* `import/` is pure (no GPUI) and tested on fixture trees in
    temp folders: `walk.rs` (which files), `markdown.rs` (paragraphs →
    blocks, frontmatter → properties, link rewriting), `csv.rs`, one
    reader per source, and `mod.rs` with the two steps; `app/import_ui.rs`
    is the commands and the dialog. *Two steps:* `import::plan` reads and
    converts everything and finds the names already taken, writing
    nothing; `import::apply` writes. Both run on a background thread
    ("Reading <folder>…", "Importing N pages from X…"); a second import
    while one runs is refused with a status. *Never overwrites:* when
    imported titles are taken (pages or aliases, any case) a dialog asks:
    **Rename with suffix** (the default button: "T (imported)", then
    "T (imported 2)"; links among the imported pages follow the new
    name; a taken journal day comes in as the page "2026-10-09
    (imported)"), **Skip them** (links to them then reach the existing
    page), or **Cancel** (nothing written). No dialog when nothing
    clashes. The dialog is modal like the others: vim keys, typing and
    the tab keys don't reach the page behind it (`vim_applies`,
    `overlay_open`, the text input handler), Escape cancels, and a block
    opened while the export was being read is closed (saved) when it
    appears. A file that exists on disk anyway is skipped, never
    replaced; pages are written atomically (`write_atomic`). *Titles:*
    a note's file name, flat; a folder only shows when two notes share
    a name ("Work/Plan" next to "Plan"; the nearest the root keeps the
    plain name, as Obsidian resolves `[[Plan]]`). Titles are cleaned for
    our files (no `___` or control characters, at most 150 bytes,
    "Untitled" when empty) and made unique among the imported ones.
    *Tags and aliases* go in the first block in the same form as the AI tag
    suggestions (decision 44): `tags:: #x, #[[y z]]` and `alias:: A, B`,
    so chips and the tag index see them; each page also gets
    `imported-from:: [[Import from X <date time>]]` there. *Rollback
    aid:* that log page lists every page imported, renamed and skipped
    file with its reason, the number of assets copied and the links to
    pages that don't exist; its "Linked from" lists the same pages. There
    is no bulk undo: delete pages from the list (they go to the trash).
    *Assets* are copied into `<graph>/assets/` (decision 28) under sanitised unique
    names (`photo-2.png` beside an existing `photo.png`) and links are
    rewritten to `../assets/<name>` (images as `![…]`, other files as
    links); only attachments a note uses are copied, except Logseq, whose
    whole `assets/` folder comes. *Safety:* hidden folders (`.obsidian`,
    `.trash`, `.git`…), `logseq/`, `bak/`, `node_modules`,
    `version-files`, `__MACOSX` are skipped; a symbolic link is followed
    only when it points inside the chosen folder (each folder once);
    notes over 5 MB, attachments over 50 MB and files that aren't UTF-8
    are skipped with a reason; at most 50,000 files; the graph itself,
    a folder inside it or one containing it are refused, as is a file
    (a `.zip`: "unzip the export first": there is no zip reader among
    our crates, and we add none). After loading, history is cleared
    (as after a restore, decision 37) and git backup sees the change.
    *Block ids are never duplicated:* `apply` collects the ids already
    in the graph (the loaded pages' saved ids, plus every `id::` in the
    files of `pages/`, `journals/` and `.trash/`, whose pages can be
    restored). An imported block whose id is among them gets a fresh
    uuid, and every `((old))` in the imported pages (also inside
    `![[((old))]]`) is rewritten to it through a remap table kept for
    this import only, so the import's own references reach its copy. The
    graph's pages are not touched: their `((old))` still reach the
    original. An id that appears twice within the import is kept by the
    first block (references reach it) and the others get fresh ids. The
    log page says how many blocks got new ids and how many ids were
    remapped (importing the same Logseq graph twice with Rename: every
    `id::` block of the second copy).

    Obsidian (`import/obsidian.rs`):

    | Obsidian | notesec |
    |---|---|
    | `Folder/Note.md` | page "Note" ("Folder/Note" on a duplicate name) |
    | YAML frontmatter `tags`, `aliases`/`alias` | first block `tags:: #a, #[[b c]]`, `alias:: …` |
    | other frontmatter keys | `key:: value` (lists joined with commas; `title` dropped) |
    | `# Heading` + paragraphs | a heading block with the paragraphs as children (levels > 3 → `###`) |
    | lists, `- [ ]` / `- [x]` | nested blocks, `TODO` / `DONE` |
    | fenced code | one block |
    | `[[Note]]`, `[[Note\|alias]]`, `[[Note#Heading]]` | `[[Note]]` (label and anchor dropped) |
    | `![[Note]]` | `![[Note]]` (live embed, decision 47) |
    | `![[pic.png]]`, `![[pic.png\|300]]`, `[x](file.pdf)` | copied to assets, `![pic.png](../assets/pic.png)` |
    | `[x](Other%20Note.md)` | `[[Other Note]]` |
    | `[[#Heading]]` | plain text |
    | daily note `2026-10-09.md` | journal |

    Logseq (`import/logseq.rs`): needs `pages/` or `journals/`.

    | Logseq | notesec |
    |---|---|
    | `pages/a___b.md`, `a%2Fb.md` | page "a/b" |
    | `journals/2026_10_09.md` (also `-`, `.`, none) | journal 2026-10-09 |
    | `[[Oct 9th, 2026]]` (the `:journal/page-title-format` of `logseq/config.edn`, or common formats) | `[[2026-10-09]]` |
    | `((uuid))`, `id:: uuid` | kept: the ids are saved (decision 24), references resolve |
    | first block of properties | first block; `title::` renames, `tags::` → `#x, #[[y z]]` |
    | `assets/` | copied (renamed on a clash, links follow) |
    | whiteboards, `.org` pages | skipped, listed with the reason |

    Notion (`import/notion.rs`): the unzipped "Markdown & CSV" export.

    | Notion | notesec |
    |---|---|
    | `Page 0123…cdef.md` (` ` or `-` + 32 hex) | page "Page" (duplicates "Parent/Page") |
    | `[x](Other%20Page%200123….md)`, `notion.so/…-<id>` | `[[Other Page]]` |
    | `Page 0123…/image.png` | copied to assets |
    | database `DB 0123….csv` (`_all.csv` preferred) | page "DB": a table, first column `[[row page]]` |
    | database row page `Key: value` lines | properties (`Tags` → `tags:: #a, #b`), `# Title` dropped |

    *Not done:* links that don't resolve become `[[name]]` (counted in
    the summary and listed in the log); labelled links and heading
    anchors are lost (`[[Page|alias]]` → `[[Page]]`: the app has no
    labelled links); Obsidian `^block` references, callouts and Dataview
    stay text; Notion relations and rollups stay text in the table;
    journals whose
    title format isn't in `config.edn` or the common ones are linked as
    plain pages; a code line starting with `- ` inside a block can read
    back as a bullet (the outline format's known limit).

51. **Web clipper: a localhost endpoint that saves pages from the browser.**
    Off by default (`web_clipper` in `config.toml`); Settings > Web
    clipper turns it on and off (the listener starts and stops at once),
    shows the port (`clipper_port`, default 27183; −, +, Default, or any
    1024-65535 in the file), the token masked with Copy and Regenerate,
    the state ("Listening on 127.0.0.1:27183", or why not) and "Copy
    bookmarklet". The request format, the bookmarklet's source, an
    extension and the threat model are in `docs/WEB_CLIPPER.md`. *Split:*
    `clipper/` has no GPUI: `http.rs` (an HTTP/1.1 reader with limits,
    form and JSON bodies, responses), `server.rs` (listener and checks),
    `html.rs` (HTML → outline rows) and `mod.rs` (token, titles, the
    page, the bookmarklet); `app/clipper_ui.rs` is the app side. No new
    crate: `std::net`, a hand-written HTTP parser and JSON reader, and the
    `uuid` crate's v4 ids (the OS random source) for the token.
    *Listener:* `TcpListener` on `127.0.0.1:<port>` only, non-blocking,
    polled every 50 ms so `stop` (also on drop) closes the port promptly
    and cleanly; each connection gets its own thread (at most 4 at once,
    more get 503), one request, then `Connection: close`. A port in use
    is an error status and a red line in Settings, never a panic.
    *Checks, in order:* head at most 16 KB / 64 headers (431), HTTP/1.0
    or 1.1 (505); `Host` exactly `127.0.0.1:<port>` or `localhost:<port>`
    (403: DNS rebinding); path `/clip` (404); POST or OPTIONS (405);
    `Content-Type` form or JSON (415); a token header, if sent, must
    match before the body is read (403); no `Transfer-Encoding` (501:
    clients send `Content-Length`; chunked parsing is surface we don't
    need), `Content-Length` required (411) and at most 5 MB (413); the
    whole request within 15 s (408); the token (header or `token` field)
    compared in constant time (403); 30 clips a minute (429). Only then
    is the HTML converted (on the connection's thread). *Hand-off:* the
    clip goes to the app through a `std::sync::mpsc` channel with a reply
    channel; GPUI has no channel a foreground task can await (its
    `PriorityQueueReceiver` blocks), so while the clipper is on the UI
    thread drains it every 200 ms (`background_executor().timer`). The
    page is written there with `Storage::save` like any new page, so git
    backup and the indexes see it; the connection waits up to 10 s for
    the answer. *Page:* the request's title cleaned (`clipper::clip_title`:
    one line, `[]` → `()`, `/` → `-`, no leading dot, then the import's
    `clean_title`) and made unique with " (2)"; first block
    `source:: <url>` (http/https only), `clipped:: [[<today>]]`,
    `tags:: #clipped` (decision 44's form); then the content. *Inbox:*
    "Clipped [[Title]]" is added at the end of today's journal (created
    if needed; an empty first block is used). That puts new clips where
    the day's notes are and lists them by date, with no special page to
    maintain; `#clipped` lists them all. Saving a clip stops editing and,
    like an import, clears undo history (a snapshot from before would
    drop the page). The status says "Clipped “Title”" with Open.
    *Conversion* (`clipper/html.rs`): a tokenizer that never looks inside
    raw-text elements, then one pass that keeps headings (nesting what
    follows, as the import does), paragraphs, nested lists, quotes, code
    blocks, table rows, links (http/https/mailto after removing control
    characters, relative ones resolved against the page's address,
    `[ ] ( )` and spaces percent-encoded so they can't leave the link),
    bold, italics and inline code; scripts, styles, frames, forms,
    objects, SVG, media, templates and `<nav>` go with their content,
    and no attribute but `href`/`src` is read. Images stay links (never
    downloaded). Our syntax in the text is defanged with a zero-width
    space: `[[`, `((`, `{{`, and line starts that would read as a
    property (`id::` would take over block references), bullet,
    heading, quote, fence or task; code lines get the line-start part
    only. Output cap 1 MB / 5,000 blocks, with a note. *Responses:* JSON
    for JSON requests; for the bookmarklet's form a self-contained page
    "Saved to NoteSec ✓" (title escaped) whose one script, closing the
    window, runs under a per-response CSP nonce (`default-src 'none'`).
    CORS allows any origin without credentials (the token is the key);
    a preflight asking for Private Network Access gets it. *Bookmarklet:*
    a top-level form POST into a small new window, not `fetch` (blocked
    more and more for public pages reaching localhost, and by pages'
    `connect-src`); it sends the selection's HTML when there is one,
    else `<article>`, `<main>` or the body. *Secrets:* the token lives in
    `state.toml` (`clipper_token`) next to `ai_api_key`; publish never
    reads that file, and sync/export must strip both. Git backup commits
    `state.toml` locally (never pushes). *Limits:* sites with a strict
    `form-action` CSP block the bookmarklet (use an extension); iframes
    and shadow DOM aren't clipped; tables become rows; a "didn't answer
    in time" 503 may follow a save that did happen; the port can only be
    typed in `config.toml` beyond the − / + buttons (Settings has no text
    field for it).

52. **Voice notes: the microphone into the page, transcribed locally.**
    "Record voice note" (palette, sidebar row; a toggle) starts a
    recorder; a pill at the top right shows "● REC 0:12 / 30:00" with
    Stop and Cancel ("Stop recording" / "Cancel recording" are in the
    palette only while recording). Stopping adds
    `![voice note](../assets/voice-2026-10-09-141503.wav)` (the image
    syntax and relative path of pasted images; `-2`… if the name is
    taken) below the block that was being edited (after its subtree),
    else at the end of the page on screen, else today's journal; the
    status says "Voice note saved (0:12) on “Page”". Setup, privacy and
    sizes for users: `docs/VOICE_NOTES.md`. *Split:* `voice/` has no
    GPUI: `mod.rs` (recorder detection and argument lists, WAV
    check/repair, names), `run.rs` (spawning, SIGINT, timeouts),
    `whisper.rs` (transcription, the Settings test);
    `app/voice_ui.rs` is the app side. *No new crate:* audio capture is
    a recorder program, found on `PATH` in this order: `pw-record`
    (PipeWire), `parecord` (PulseAudio), `arecord` (ALSA), `ffmpeg`
    (`-f pulse -i default`), all asked for 16 kHz mono 16-bit WAV (what
    whisper.cpp wants, ~1.9 MB a minute); `voice_recorder` in
    `config.toml` (program then arguments, `{file}` for the output)
    replaces the search. None found: an error status naming the packages.
    Every program runs with `Command` and an argument list, never a
    shell, so no file name can become a command. *Recording:* outside
    the vault, so a git backup that runs meanwhile can't commit half a
    file, and in a private folder (`voice/private.rs`): a fresh
    `notesec-voice-<random>` made with mode 0700 (`DirBuilderExt`; mkdir
    fails on an existing name or planted symlink) under
    `$XDG_RUNTIME_DIR/notesec-voice` (per user, tmpfs; checked to be a
    real folder, ours, closed to others) or else the system temp folder.
    The shared temp folder alone wouldn't do: the recorder's umask
    (often 0644) would let other local users read the recording.
    whisper's output and program logs use such folders too (a transcript
    is as private). The folder goes, contents and all, when the
    recording is done or cancelled. The finished file is moved into
    `assets/` (`voice::store`): a rename on one file system, else (tmpfs
    to home is EXDEV) a copy into `.voice-….wav.tmp` (a name backup
    ignores), fsync, rename, then the source removed. Either way it gets
    the mode a new file in `assets/` gets, like a pasted image (found by
    creating a probe file: no umask call without libc), not the private
    one.
    *Stopping:* SIGINT through the `kill` program (`kill -INT <pid>`,
    as Ctrl+C would: recorders then write their header's sizes), no
    `libc`/`unsafe`; after 5 s `Child::kill`. That and checking the file
    run off the UI thread. A 1 s timer moves the clock, stops at 30
    minutes (`MAX_LENGTH`) and notices a recorder that quit by itself
    (its output's last lines are shown). *Repair:* `normalize_wav`, a
    pure function, rewrites the file as RIFF/WAVE with just `fmt ` and
    `data`: sizes left at 0 or 0xFFFFFFFF (a recorder killed early) or
    past the end become "to the end of the file", a half sample frame is
    cut, metadata chunks (ffmpeg's `LIST`) are dropped; not RIFF, not
    PCM, or no audio is an error and nothing is saved. *Cancel* kills
    the recorder and deletes the file. Closing the app while recording
    finishes the file into `assets/` without a block (documented).
    *Transcription:* only with a whisper.cpp program set (Settings >
    Voice notes, Choose… through the system file picker, or
    `whisper_binary` / `whisper_model` / `whisper_language` in
    `config.toml`; Settings shows each path as missing if it is, and
    Test runs `--help` and checks the model's ggml/GGUF magic). Run as
    `-m <model> -f <wav> -l <auto|xx> -nt -otxt -of <tmp>` off the UI
    thread, one at a time (a queue), with a timeout of 2 minutes plus
    6× the recording; `<tmp>.txt` is read (stdout if absent),
    timestamps and `[BLANK_AUDIO]`-style markers removed, whitespace
    collapsed, and the text defanged like clipped text (decision 51)
    so speech can't make links, properties or bullets. It goes in as a
    child of the note, `**Transcript:** …`; a note with one isn't done
    twice. "Transcribe new voice notes automatically" (off by default)
    does it after each recording; the chip's Transcribe button and
    "Transcribe voice notes on page" do it on request. Errors (no
    program, missing model, exit code with the end of stderr, timeout)
    are statuses. Paths are config, not secrets: they're in
    `config.toml`, not `state.toml`. NoteSec itself never touches the
    network; what a program the user chose does is up to it. *Reading
    view:* an audio target is a chip (🎙 name, ▶ Play, Transcribe), not
    an image; Play opens the file with the system's player
    (`open_with_system`, recorded in tests). *Export and publish* leave
    audio out ("Voice note not included: name"): a page shouldn't grow
    by megabytes, and publishing someone's voice should take a
    deliberate step; the file is never copied. *Backup:* WAVs in
    `assets/` are committed like images (they're notes). *Keys:* the
    pill isn't a dialog, so typing, vim keys and shortcuts keep working
    while recording, and Escape doesn't cancel it. *Limits:* Linux
    recorders only (macOS/Windows need a custom command); the default
    input device; no pause; WAV only (no compression); a program that
    ignores SIGINT loses what it hadn't written when killed; editing the
    page the note goes to is closed when it's inserted.

53. **Whiteboards: a canvas page stored as ordinary blocks.** A page
    whose first block is `type:: whiteboard` shows a canvas instead of
    the outline. *Storage, no JSON:* each other top-level block is a
    card, with `x::`, `y::`, `w::`, `h::` and an optional `color::`
    (yellow, green, blue, red, purple, gray) after its text. A card is
    text, `[[Page]]` (a page card: its title, which opens it, and the
    page's first lines) or `((uuid))` (a live block card). Arrows are
    children of a top-level `edges::` block,
    `edge:: ((from)) -> ((to))` with an optional `label::`. The ends use
    the normal `id::` mechanism, so cards keep their ids, and backlinks
    and the graph count card links with no extra code. *Hand edits:*
    unreadable or out-of-range coordinates get the card auto-placed
    (rows of four under the placed cards, not saved until the card is
    moved). Sizes are clamped, and arrows to missing cards or to
    themselves are ignored. Nothing panics. *Canvas:* `canvas()` +
    `PathBuilder` paint the arrows (a line and a filled head, anchored
    on card borders); cards are absolutely placed divs, only those in
    view (culling; 500 cards tested). Every press is hit-tested in
    canvas coordinates (`geom::hit`: handles, cards top-down, the
    page-card title strip, then arrows within a few px). Moves and
    resizes are drawn from the drag state and written on release: one
    undo step and one save each. Drags are tracked by root listeners, so
    they continue past the canvas edge. Pan: drag empty space, middle
    button, or Space+drag, or the wheel. Zoom: Ctrl+wheel or pinch
    around the pointer, 0.1–4×, with Fit and 100% buttons and palette
    commands. A board's view is per title and in memory. It is fitted
    on first show, and refitted on the next frame if the first frame
    had no size. *Editing:* a double-click on empty space makes a card
    there; a double-click on a card edits it with the outline's block
    editor (`BlockText`), which loads and saves the text without the
    card's property lines (`editor_source`/`editor_content`). Enter
    finishes. Tab, Alt+arrows, block hops and the empty-Backspace delete
    do nothing in a card, and vim is off there (`vim_applies`). The
    canvas has the "Whiteboard" key context only when nothing is edited
    or open: then Delete/Backspace remove the selected card (with its
    arrows) or arrow, and Esc deselects. Drag the dot on a selected card
    onto another card to connect them. "Add page" uses the palette: the
    pick becomes a page or block card. "Open as outline" shows the
    blocks as usual (per page, in memory). *Export/publish:* an inline
    SVG (lines, polygons, and each card's block HTML in a
    `<foreignObject>`), all positioned by attributes, so it passes both
    CSPs (publish forbids `style=""`), runs nothing and loads nothing.
    *Limits:* the other split pane shows the outline; no multi-select
    or copy/paste; the edited card's text isn't scaled with zoom; a
    card's own text line like `x:: 5` would be read as its coordinate;
    no curved arrows; no page-menu item (palette command "New
    whiteboard").

54. **Encrypted vault export/import (not sync).** Live end-to-end
    encrypted sync is future work. "Export encrypted vault…" writes the
    graph to one `.notesec-vault` file; "Import encrypted vault…"
    unpacks one into a new, empty folder, never the open graph and never
    merged. Threat model, contents and format: `docs/ENCRYPTION.md`.
    *Crypto:* Argon2id (64 MiB, 3 passes, 1 lane; stored in the header
    and bounded on import as a DoS guard) derives the key.
    XChaCha20-Poly1305 encrypts 1 MiB chunks, STREAM-style: nonce = base
    ‖ counter ‖ last flag, and AAD = header ‖ last flag. That
    authenticates the header and catches truncation, reordering and
    appended chunks. Salt and nonce come from `OsRng` (the `aead`
    re-export). *New crates:* `chacha20poly1305 0.10` and `argon2 0.5`
    (10 packages added to Cargo.lock); no tar/zip crate. The archive is
    hand-rolled (path, length, bytes), and paths are checked when packing
    and when unpacking. No `zeroize`: secrets are overwritten by hand
    (`vault::wipe`, best effort without unsafe). *Contents:* pages,
    journals, assets, `config.toml`, and `state.toml` parsed with `toml`
    with `ai_api_key`/`clipper_token` removed at any depth (dropped whole
    if it can't be parsed). Not included: `.git/`, `.trash/`,
    `.notesec/`, `exports/`, `published/`, hidden and `*.tmp` files,
    symlinks. *Import:* decrypt, authenticate and check everything in
    memory first, then write new files into a staging folder in the
    destination and move them out; a failure writes nothing. There's no
    graph switching, so the status line says to start NoteSec with
    `NOTESEC_DIR`. *UI:* a modal dialog. The passphrase fields are
    `EditorState`s on the app's input path (`active_editor`), drawn as
    bullets, with a `canvas()` registering `handle_input`. Export asks
    twice and needs 12+ characters; Esc closes and wipes. It's in
    `overlay_open`, `text_input_open` and `vim_applies`. Crypto runs on
    the background executor. *Backup (agreed with track 1):* `IGNORED`
    gains `/state.toml`, and `prepare` runs `git rm --cached state.toml`
    once when it's tracked, then commits just that removal from a
    temporary index (the pathspec commit would re-add the file from
    disk; anything the user staged stays out). Older commits keep it; ENCRYPTION.md explains
    purging with git filter-repo and rotating the secrets. *Limits:*
    whole vault in memory (2 GiB cap); no in-app graph switching; a
    failure partway through moving files out of staging can leave some
    files in the (new) folder; wiping can't reach copies made by
    reallocation.

*Next to learn, in order:* ownership/borrowing -> `Option`/`Result` -> traits ->
iterators -> lifetimes (you'll meet them in GPUI signatures). Each one maps to
code you've already seen above — that's the advantage of learning from your own
project.
