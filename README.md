# notesec

A fast, GPU-rendered outliner notes app — Logseq-style, written in Rust.

No Electron, no webviews. Your notes live as plain Markdown files on your disk, so they keep working with Logseq and Obsidian too.

## Features

- **Outliner editing** — blocks, indentation, wikilinks `[[like this]]`, `#tags`
- **Backlinks panel** — everything that links to the current page
- **Fuzzy search** (`Ctrl+K`) and **global search** (`Ctrl+Shift+F`)
- **Graph view** — your notes as a network
- **Whiteboards** — cards and arrows on a canvas
- **Journals** — daily notes; `Ctrl+J` opens today
- **Vim mode** — if that's your thing
- **Split panes, tabs, page aliases, trash with restore**
- **Smart folders** — saved searches, plain and semantic
- **AI, your way** — three modes: local server (default), API key, or fully off. Never phones home silently.
- **WASM plugins** — sandboxed, hash-pinned; ships with a word-count example
- **Voice notes** — record and transcribe locally with whisper.cpp
- **Web clipper** — save browser pages as notes
- **Import** — bring your Obsidian, Logseq, or Notion exports
- **Encrypted vault export/import** — Argon2id + XChaCha20-Poly1305
- **Publish** — export a page as a static site
- **HTML export** with print styles
- **Git auto-backup** — your vault versioned automatically (off by default)
- **Custom hotkeys** and themes

## Install

### From source

You need Rust and the usual Linux build tools.

```bash
git clone https://github.com/gencayks/notesec
cd notesec
cargo build --release
./target/release/notesec
```

### AppImage / tarball

Grab the latest from the [releases page](https://github.com/gencayks/notesec/releases).

## Where are my notes?

`~/notesec` — plain Markdown under `pages/`, `journals/`, `templates/`. Point it elsewhere with `NOTESEC_DIR`.

## Quick capture

Jot a note from anywhere into today's journal:

```bash
notesec --capture "call mom tomorrow"
```

This appends the text as a new block to today's journal and exits without opening a window. Inside the app, `Ctrl+Shift+C` opens a small capture box (`Enter` captures, `Esc` cancels).

Note on global hotkeys: on Wayland an app cannot grab a system-wide hotkey by itself. To capture from anywhere, bind `notesec --capture "..."` (or a small script that prompts for text and passes it) to a system-wide shortcut via e.g. KDE Settings > Shortcuts.

## License

MIT
