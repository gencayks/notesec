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
search.rs      — fuzzy matcher, pure functions (NO ui code here)
ui.rs          — theme colours + tiny stateless view helpers
config.rs      — config.toml: theme, font size/family
```

**The key design rule:** `model`, `storage`, `editor`, `search` know *nothing* about
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

cx.bind_keys([
    KeyBinding::new("enter", Enter, Some("BlockEditor")),
    ...
]);
```

Keyboard input goes: **key press -> action -> handler**. The `"BlockEditor"`
context scopes bindings so Enter only splits blocks *while editing* — the same
key can mean different things in different contexts. Cleaner than one giant
`match` on key codes.

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

---

*Next to learn, in order:* ownership/borrowing -> `Option`/`Result` -> traits ->
iterators -> lifetimes (you'll meet them in GPUI signatures). Each one maps to
code you've already seen above — that's the advantage of learning from your own
project.
