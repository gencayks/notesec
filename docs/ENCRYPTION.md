# Encrypted vaults

"Export encrypted vault…" writes your whole graph into one encrypted
`.notesec-vault` file. "Import encrypted vault…" unpacks one into a new,
empty folder. Use it to carry or keep notes on storage you don't trust:
a USB stick, a cloud drive, an email to yourself.

**This is export/import, not sync.** Nothing is kept in step
automatically. Live end-to-end encrypted sync is future work.

## What it protects, and what it doesn't

Protects:
- **The file at rest and in transit.** Without the passphrase, the file
  can't be read. Any change to it is detected: a flipped bit, a cut-off
  end, chunks reordered or added, or an edited header.

Does not protect:
- **A compromised device.** Malware or someone using your unlocked computer
  can read the notes in your graph folder, which isn't encrypted, or
  capture the passphrase as you type it.
- **A weak passphrase.** Argon2id makes each guess slow and costly, but a
  short or common passphrase can still be guessed. NoteSec asks for at
  least 12 characters; several random words are better.
- **Metadata.** The file's size (roughly the size of your notes), its name,
  and when it was written are visible.
- **A lost passphrase.** The passphrase is never stored anywhere. If you
  lose it, the vault can't be opened, by you or by anyone else. There is
  no recovery.

Where keys live: the key is derived from the passphrase in memory, used,
then overwritten. The passphrase fields show bullets and are wiped when the
dialog closes; the passphrase is never logged or saved. This wiping is best
effort: memory the operating system or allocator copied earlier can't be
reached without unsafe code.

## What goes in

- `pages/` (whiteboards are pages too), `journals/` and `assets/`;
- `config.toml`;
- `state.toml` **without `ai_api_key` and `clipper_token`**, wherever they
  appear in it. A `state.toml` that can't be parsed is left out entirely.

Left out:
- `.git/` (backup history; it may hold old secrets, see below);
- `.trash/` (deleted pages: deleting should mean they don't travel);
- `.notesec/` (caches that can be rebuilt);
- `exports/` and `published/` (HTML you can make again);
- hidden files and `*.tmp` files (half-finished saves);
- symlinks (never followed);
- anything else at the top of the graph folder.

## Import

You pick the vault file, then a folder, then type the passphrase. The
folder must be empty, and can't be the open graph or contain it. A vault
is never merged into notes.

Everything is decrypted, authenticated and checked before anything is
written. A wrong passphrase or a damaged file writes nothing. Every path
in the archive is checked:
- relative, with no `..`, `.` or empty parts, no backslashes, colons or
  NULs;
- under `pages/`, `journals/` or `assets/`, or exactly `config.toml` or
  `state.toml`;
- no duplicates (case-insensitive), and no file where a folder is
  expected.

Files are created new (never overwriting) in a hidden staging folder
inside the destination, then moved into place. NoteSec opens one graph
per run, so to use the imported notes, start it with
`NOTESEC_DIR=/that/folder`.

## Git backup and state.toml

Git auto-backup no longer tracks `state.toml`. It is listed in the graph's
`.gitignore`, and if an earlier backup committed it, NoteSec runs
`git rm --cached state.toml` once and records that in a commit of its own
("notesec autosave: stop tracking state.toml"). The file stays on disk.

**Old commits still contain it.** If an earlier backup committed
`state.toml` with a clipper token or an AI key, that secret is still in
your local git history (and in any copy of the repository you pushed).
Removing it means rewriting history, for example with
[git filter-repo](https://github.com/newren/git-filter-repo):

    cd ~/notesec        # your graph folder (the repository root)
    git filter-repo --invert-paths --path state.toml

Then force-push any remote copies. The safest fix is to rotate the
secret: make a new AI key, and change the clipper token (and update the
browser extension to match).

## File format (version 1)

All integers are big-endian.

| Bytes | Field |
|-------|-------|
| 8 | magic `NSVAULT\0` |
| 2 | version (1) |
| 4 | Argon2id memory, KiB (default 65536 = 64 MiB) |
| 4 | Argon2id passes (default 3) |
| 4 | Argon2id lanes (default 1) |
| 4 | chunk size (1 MiB) |
| 16 | salt (from the OS random generator) |
| 19 | nonce base (from the OS random generator) |

That is a 61-byte header. Then come the chunks: each holds up to `chunk
size` bytes of plaintext, encrypted with XChaCha20-Poly1305 using a 32-byte
key from `Argon2id(passphrase, salt)`, with a 16-byte tag at the end.

- **Nonce** for chunk `i`: nonce base, then `i` as a u32, then one byte
  that is 1 for the last chunk and 0 otherwise.
- **Associated data:** the whole header, then the same last-chunk byte.
  So the header is authenticated, and a file cut at a chunk boundary fails
  (its new final chunk wasn't sealed as final).

On import, settings outside these bounds are refused before any work is
done, so a crafted file can't make NoteSec use gigabytes of memory:
- memory 8 KiB to 1 GiB;
- passes 1 to 10;
- lanes 1 to 4;
- chunk size 4 KiB to 16 MiB.

The plaintext is a small archive:
- the magic `NSARCH1\n`;
- per file: a u32 path length, the UTF-8 path, a u64 size, then the
  bytes;
- a path length of 0 to end it.

**Limit:** the archive is built and checked in memory, so a vault holds at
most 2 GiB of notes.
