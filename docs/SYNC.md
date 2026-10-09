# Git sync

Sync keeps one vault identical on several machines through a git remote
you control. It reuses the local git backup repository: local changes
are committed first, then the remote is fetched and merged, then pushed
(pull-then-push). The sidebar shows the status: **clean**, **ahead N**
(local commits to push), **behind N** (remote commits to pull),
**diverged**, or **conflict**. The **Sync now** palette command and the
optional auto-sync (every N minutes, 0 = off) run a sync. Conflicts are
never auto-resolved: the sync stops, both sides are backed up under
`.sync-conflicts/`, and you pick a version per file in the sync dialog.

## Recommended setup

1. Make a **private** repository (GitHub, Gitea, your own server —
   anything git speaks over SSH). Private, because the vault holds your
   notes; `state.toml` (API keys, tokens) is never committed, but page
   contents are.
2. Use **SSH**: make a key if you have none (`ssh-keygen -t ed25519`),
   add the public half as a deploy key (write access) or to your
   account, and make the host known once
   (`ssh-keyscan example.org >> ~/.ssh/known_hosts`). Notesec never
   prompts for passwords or host confirmation: SSH runs in batch mode,
   so a missing key or host fails loudly instead of hanging.
3. Paste the remote (e.g. `git@example.org:notes.vault.git`) into the
   sync dialog's Remote row (sidebar: Sync). The vault folder must be
   its own git repository — sync refuses to run inside a bigger one
   rather than syncing a stranger's files.
4. Optionally set auto-sync minutes. The first sync from a fresh vault
   joins the remote's history (or pushes to an empty remote).

HTTPS remotes work, but the password/token prompt is disabled, so they
need a credential helper configured outside the app.

## Conflicts

Edit the same line on two machines and sync both: the second sync stops
with the conflicting files listed. Each file keeps working-tree markers
(`<<<<<<<`) and both full versions under
`.sync-conflicts/<timestamp>/<path>.ours` / `.theirs` (that folder is
never committed). **Keep mine** / **Keep theirs** stages your pick per
file; when none remain, the merge is committed (`notesec sync: resolved
conflicts`) and pushed. Undo covers the retag-equivalent local edits,
but re-pulling after a bad pick needs the `.sync-conflicts/` backup —
don't delete it until you're sure.

## What syncs, and what doesn't

Synced: `pages/`, `journals/`, `assets/`, `.gitignore`.
Never synced: `state.toml` (secrets and UI state), `.trash/`,
`exports/`, `published/`, `.sync-conflicts/`, temp files.
