# Plugins

NoteSec can run small WebAssembly plugins: palette commands and render hooks
for `{{macros}}` in blocks. They run in a sandbox (wasmi, an interpreter)
and can only see what NoteSec hands them. See decision 55 in
ARCHITECTURE.md for the reasoning.

## Enabling a plugin is a trust decision

New plugins are **disabled**. Turn them on in **Settings > Plugins**. A plugin
can't read your files, other pages, the network or `state.toml`, but it does
see the block you run it on (and the blocks that contain its macro), and it can
change that block, add a block, show a status or open a page. Only enable
plugins you trust.

NoteSec remembers *which binary* you enabled: `config.toml` stores the plugin
id with a hash of its `plugin.wasm`:

```toml
[plugins]
word-count = "5dfb9d78..."
```

If `plugin.wasm` changes, the plugin is off until you enable it again.
After three failures in a row (a crash, running out of fuel or memory, or bad
output), NoteSec turns it off and says why.

## Layout

```
<notes folder>/plugins/<id>/plugin.toml
<notes folder>/plugins/<id>/plugin.wasm
```

The folder name must equal the manifest `id`. Symlinks and other non-regular
files are refused. Use **Reload plugins** in Settings after adding one.
`examples/plugins/word-count/` is a complete example, hand-written in WAT
(`plugin.wat`, with the compiled `plugin.wasm` next to it).

## Manifest (`plugin.toml`)

```toml
id = "word-count"            # a-z, 0-9 and '-', at most 48, not starting with '-'
name = "Word count"
version = "1.0.0"            # the plugin's own version, shown in Settings
api_version = 1              # must be 1 (this document)
description = "Counts words." # optional

[[commands]]                 # 0 to 16; shown in the palette as "Plugin: <label>"
id = "count"
label = "Count words in this block"   # at most 80 characters

[render]                     # optional: claims {{word-count ...}} in blocks
macro = "word-count"
```

Unknown keys are errors. Limits: manifest 64 KiB, `plugin.wasm` 4 MiB.

## ABI

A plugin is a core wasm module (no WASI, no component model) that exports:

| export | signature | |
|---|---|---|
| `memory` | memory | |
| `alloc` | `(len: i32) -> i32` | returns a pointer to `len` free bytes |
| `dealloc` | `(ptr: i32, len: i32)` | optional |
| `run_command` | `(ptr: i32, len: i32) -> i64` | needed if it has commands |
| `render` | `(ptr: i32, len: i32) -> i64` | needed if it has `[render]` |

NoteSec calls `alloc`, writes the input there and calls the function. The
result packs the output as `(ptr << 32) | len`. The output must lie inside
`memory` and be at most 64 KiB. Each call gets a **fresh instance**, so nothing
survives from one call to the next.

The only import allowed is

```wat
(import "env" "host_log" (func (param i32 i32)))   ;; ptr, len: UTF-8 text
```

which writes a line to NoteSec's stderr (at most 20 lines of 500 bytes per
call). A module importing anything else is refused.

### Input

Input is UTF-8 TOML: `key = "value"` lines with basic strings, in this order
(the block comes first, so a simple plugin can find it easily):

```toml
block = "the block's text"     # the block being edited ("" if none)
command = "count"              # the manifest command id
page = "Page title"
selection = "selected text"
api_version = "1"
```

Render hooks get `block` (the block's text), `args` (the text after the
macro name, trimmed) and `api_version`.

### Command output

```toml
[[actions]]
type = "set_status"      # also: insert_block, replace_block, open_page
text = "Words: 3"

[[actions]]
type = "open_page"
title = "Some page"
```

| action | field | effect |
|---|---|---|
| `insert_block` | `text` | a new block after the current one (or at the end of the page) |
| `replace_block` | `text` | the current block's new text (needs a block being edited) |
| `set_status` | `text` | a status message, prefixed with the plugin name |
| `open_page` | `title` | opens an existing page in a new tab |

At most 16 actions. Texts are at most 64 KiB with no control characters but
newline and tab. Status texts and titles are one line of at most 200 bytes.
Anything else (unknown keys or types, bad TOML, JSON) fails the call. Each
edit is one undo step and is saved like a normal edit. If you moved to another
page or block while the command ran, its edits are refused.

### Render output

```toml
text = "3 words\n**bold line**\n*italic line*"
```

At most 8 KiB and 50 lines. A line wrapped in `**...**` is bold, and one in
`*...*` or `_..._` is italic. Everything else is plain text, drawn as text and
never as HTML or markup. Results are cached per (binary, args, block text).

## Limits

- Commands: 50M fuel (roughly instructions) on a background thread.
- Render hooks: 5M fuel. They run while drawing, so keep them small; results
  are cached.
- Memory: 32 MiB. Growing beyond it fails the call.
- One command at a time.

## Export and publish

Plugins never run during HTML export, publishing or vault export. A macro such
as `{{word-count}}` stays as its text there.

## Community marketplace

Reviewed plugins are listed in **Settings > Plugins > Browse** and install
with one click (download → sha256 check against the index → enable; a
mismatch is refused). See `docs/MARKETPLACE.md` for the registry, how to
submit a plugin, and what review checks.

## Versioning

`api_version` is this ABI's version (1). NoteSec refuses plugins with another
version. A future version would be added alongside, not by changing version 1.
The manifest `version` is yours.

## Writing a plugin in Rust

Not built by CI; here as a starting point. `Cargo.toml`: `[lib] crate-type =
["cdylib"]` and `toml = "0.8"`. Build with `cargo build --release --target
wasm32-unknown-unknown`, then copy `target/wasm32-unknown-unknown/release/<name>.wasm`
to `plugins/<id>/plugin.wasm`.

```rust
use std::alloc::{alloc as raw_alloc, dealloc as raw_dealloc, Layout};

#[no_mangle]
pub extern "C" fn alloc(len: usize) -> *mut u8 {
    unsafe { raw_alloc(Layout::from_size_align(len.max(1), 1).unwrap()) }
}

#[no_mangle]
pub extern "C" fn dealloc(ptr: *mut u8, len: usize) {
    unsafe { raw_dealloc(ptr, Layout::from_size_align(len.max(1), 1).unwrap()) }
}

fn answer(out: String) -> u64 {
    let out = out.into_bytes().leak(); // freed with the instance
    ((out.as_ptr() as u64) << 32) | out.len() as u64
}

#[no_mangle]
pub extern "C" fn run_command(ptr: *const u8, len: usize) -> u64 {
    let input = unsafe { std::slice::from_raw_parts(ptr, len) };
    let input: toml::Table = toml::from_str(std::str::from_utf8(input).unwrap()).unwrap();
    let block = input["block"].as_str().unwrap_or("");
    let mut out = toml::Table::new();
    let mut action = toml::Table::new();
    action.insert("type".into(), "set_status".into());
    action.insert("text".into(), format!("Words: {}", block.split_whitespace().count()).into());
    out.insert("actions".into(), toml::Value::Array(vec![action.into()]));
    answer(toml::to_string(&out).unwrap())
}
```

(The `unsafe` here is in your plugin, which runs inside the sandbox. NoteSec
itself has none.)
