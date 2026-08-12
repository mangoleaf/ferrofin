---
name: sync-plugin-upstream
description: >-
  Check the third-party Jellyfin plugins ported into hermit-extensions against
  their upstream repos and port any behavioral changes, keeping
  brain/PLUGINS_UPSTREAM.md pins current. Use when asked to "sync plugins",
  "update <plugin> from upstream", "check plugin upstream", "is intro
  skipper/merge versions up to date", or "/sync-plugin-upstream". Optional
  argument: a plugin name to sync just one.
---

# Sync ported plugins with upstream

Keep Hermit's compiled-in plugin ports faithful to their upstream repos. The
manifest `brain/PLUGINS_UPSTREAM.md` is the source of truth: one table row per
plugin with upstream repo, local clone path, and **Ported rev** (the commit the
Hermit code is faithful to). Run from the repo root.

## Step 1 — read the manifest, fetch upstream

Parse the table in `brain/PLUGINS_UPSTREAM.md`. For each plugin (or just the
one named in the argument):

```bash
git -C <clone> fetch origin            # clone it from the manifest URL if missing
git -C <clone> log --oneline <ported-rev>..origin/HEAD -- .
git -C <clone> describe --tags origin/HEAD
```

No commits since the ported rev → report "up to date" and stop for that plugin.

A plugin whose Status is `partial` has a pending plan (linked in the manifest's
per-plugin notes) — report that the plan should be executed first; don't try to
sync a half-ported plugin.

## Step 2 — classify the delta

Diff only the plugin's source (skip CI/packaging noise):

```bash
git -C <clone> diff <ported-rev>..origin/HEAD --stat
git -C <clone> diff <ported-rev>..origin/HEAD -- '*.cs' '*.html' '*.js'
```

Sort each change into:
- **Behavioral** — logic, config schema, task, API, or config-page changes →
  must be ported.
- **Not applicable** — .NET packaging, DI plumbing, Jellyfin-internal
  representation Hermit doesn't model. Check the manifest's **accepted
  divergences** for that plugin first — never "fix" a listed divergence.
- **Upstream bug** — if upstream is buggy and Hermit is correct, keep Hermit
  correct and record it as a new accepted divergence in the manifest
  (project rule: don't port Jellyfin bugs).

## Step 3 — port behavioral changes

The manifest's per-plugin notes list the Hermit files. Port faithfully from the
C# (regex tables and constants verbatim; upstream xUnit `[InlineData]` cases
become `rstest` `#[case]`s). Respect the architecture rules in `CLAUDE.md` —
handlers only touch `hermit-traits`, object-safe traits, PascalCase DTOs.

If dashboard assets changed (`Configuration/*.html|js|css`), bump the plugin's
`*_REV` const in `crates/hermit-extensions/build.rs` to the new rev, rebuild
with `HERMIT_REFRESH_PLUGIN_ASSETS=1 cargo build -p hermit-extensions`, and
commit the refreshed `crates/hermit-extensions/assets/`.

## Step 4 — gates

```bash
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo nextest run --workspace
cargo llvm-cov nextest -p <each-touched-crate> --fail-under-lines 80 --summary-only
```

If `hermit-core`, `hermit-db`, or `hermit-api` were touched:
`./suite/run.sh gate --measure`. For stateful/data-path changes, also verify
over live HTTP against a running server (green tests are necessary, not
sufficient).

## Step 5 — update the manifest

In `brain/PLUGINS_UPSTREAM.md`, for each synced plugin set **Ported rev** to
the new upstream commit (short hash) and refresh **Upstream version** from
`git describe --tags`. Record any new accepted divergences in the per-plugin
notes. Update the matching `*_REV` in `build.rs` even when only logic changed,
so the two pins never drift apart.

Commit per repo conventions: no branching, no AI attribution trailers.
Summarize per plugin: up to date / synced (what changed) / divergence recorded.
