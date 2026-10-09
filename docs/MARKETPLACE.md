# Community plugin marketplace

NoteSec lists community plugins in **Settings > Plugins > Browse**. The
listing comes from a public registry repo, `notesec-plugins`, so installing
is one click — and every download is checked before it touches your notes.

## How it works (users)

1. Open Settings > Plugins > **Browse** (the tab fetches the index on first
   open; **Refresh** fetches again).
2. Every listing carries a **✓ Verified** badge: it passed review (below)
   before it was merged into the registry.
3. Click **Install**: NoteSec downloads the plugin's WASM, checks its
   SHA-256 against the index, writes it to `plugins/<id>/`, and enables it.
4. If the hash doesn't match, the install is **refused loudly** — a status
   error plus a message in the Browse tab — and nothing is written. Tell us
   if you ever see this; it means the download was tampered with or the
   index is stale.

A new version of an installed plugin shows **Update** (same download +
verify + enable flow). Enabling is still a trust decision: plugins run in
the wasmi sandbox (see `docs/PLUGINS.md`), but they see the block you run
them on.

Registry: `https://github.com/gencayks/notesec-plugins`
Index: `https://raw.githubusercontent.com/gencayks/notesec-plugins/main/index.toml`

## Registry layout

```
notesec-plugins/
  index.toml          # the only file the app fetches
  plugins/
    <id>/
      plugin.toml     # the manifest, as installed
      plugin.wasm     # the reviewed binary the sha256 pins
```

`index.toml`:

```toml
[[plugins]]
id = "word-count"            # a-z, 0-9 and '-', at most 48 (must equal the folder name)
name = "Word count"
version = "1.0.0"            # shown in Settings; an Update when it changes
description = "Counts words." # one line, at most 500 characters
author = "Jane Doe"          # who wrote it
download_url = "https://raw.githubusercontent.com/gencayks/notesec-plugins/main/plugins/word-count/plugin.wasm"
sha256 = "ba7816bf…"          # 64 hex digits, of exactly the file above
api_version = 1              # must be 1 (this NoteSec's plugin API)

[[plugins.commands]]         # optional; becomes the installed manifest's commands
id = "count"
label = "Count words in this block"
```

Rules the app enforces on every entry: valid id, short single-line
name/version/author, `api_version = 1`, `https://` download URL, 64-hex
sha256, at most 16 commands with valid ids. A bad entry fails the whole
fetch with a message naming it — fix the index, don't ship around it.

## How to submit a plugin

1. Fork the `notesec-plugins` repo.
2. Add `plugins/<id>/plugin.toml` (see `docs/PLUGINS.md` for the manifest
   format) and `plugins/<id>/plugin.wasm` built from source. Include the
   source: either the `.wat`/Rust sources in the PR or a link to the
   plugin's own public repo + the exact build command.
3. Add a `[[plugins]]` entry to `index.toml` (fields above). Compute the
   hash with `sha256sum plugins/<id>/plugin.wasm`.
4. Open a PR. One plugin per PR; bump `version` in a follow-up PR when it
   changes (the hash must change with the binary).

## What review checks

- **Builds from the submitted source.** The `plugin.wasm` in the PR must
  match a clean build of the submitted source (`sha256sum` agrees).
- **Manifest matches the index entry**: same id/name/version, the folder
  name equals the id, commands and labels are sane (no look-alike or
  misleading labels such as "Sync now").
- **Runs in the sandbox**: imports nothing but `env.host_log`, stays in
  fuel and memory on the word-count-sized smoke test, fails gracefully
  (no 3-strikes disable) on the reviewer's vault copy.
- **No surprises**: the description and author are accurate, the plugin
  does what the listing says and nothing else (commands and render
  output are small TOML the reviewer can read).
- **Version hygiene**: new plugin, or a version bump with a changelog
  note in the PR. Never rewrite a published binary in place — publish a
  new version so installed copies don't silently drift.

Only after all of these does the PR get merged — merging is what puts
the ✓ Verified badge on it.
