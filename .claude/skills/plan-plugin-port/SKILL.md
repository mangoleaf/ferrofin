---
name: plan-plugin-port
description: >-
  Plan the port of a third-party Jellyfin (.NET) plugin into a compiled-in
  Ferrofin extension: clone the upstream repo, map its surface (config, tasks,
  API routes, settings page) onto Ferrofin's extension seams, decide accepted
  divergences, and write a self-contained brain/plans/PLAN_*.md an executor can
  follow. Use when asked to "plan a plugin port", "port <jellyfin plugin> to
  ferrofin", "plan porting <plugin>", "add <jellyfin plugin> as an extension", or
  "/plan-plugin-port <repo-url-or-name>". Produces a plan only — the
  implement-plugin-port skill executes it.
---

# Plan a Jellyfin plugin port

Turn a third-party Jellyfin plugin into a concrete, self-contained plan for a
compiled-in Ferrofin **extension** (`crates/ferrofin-extensions/`). This skill only
plans; `implement-plugin-port` executes. Read `CLAUDE.md` first — the
architecture rules (trait DI seam, PascalCase DTOs, runtime sqlx, per-`pub`
docs, pedantic clippy) constrain every decision here.

The argument is the upstream repo URL or a plugin name.

## The one idea

A Jellyfin plugin is a `.NET` service bundle. Ferrofin can't load .NET at runtime,
so a "port" re-expresses the plugin's **behavior** as Rust that surfaces on the
frozen `/Plugins` API. Faithfulness is to the plugin's observable behavior and
its config/route/settings-page contract — not its C# class structure. Port the
regex tables and constants verbatim; redesign the plumbing idiomatically.

## Step 1 — get the source

```bash
git clone --depth 50 <repo-url> ~/dev/3rdparty/<repo-name>   # skip if already there
git -C ~/dev/3rdparty/<repo-name> log -1 --format='%h %ad %s' --date=short
git -C ~/dev/3rdparty/<repo-name> describe --tags
```

Read every `.cs`, the `Configuration/*.html` settings page(s), and the
`.csproj` (for the version). The files that matter, by convention:
- `Plugin.cs` — the **GUID** (Ferrofin must reuse it — the settings page and
  clients hardcode it) and `GetPages()` (which settings pages, main-menu flag).
- `Configuration/PluginConfiguration.cs` — the config schema (→ a PascalCase
  Rust struct).
- `Configuration/*.html` — the settings page (vendored as-is).
- `Api/*Controller.cs` — the HTTP routes it adds.
- `ScheduledTasks/*.cs` — background tasks (key, name, category, default
  triggers).
- The manager/service class — the actual logic.

## Step 2 — study Ferrofin's extension seams

The three existing extensions are the templates — pick the closest:
- `crates/ferrofin-extensions/src/file_transformation.rs` — simplest: static
  settings page, a service trait, no tasks.
- `crates/ferrofin-extensions/src/merge_versions.rs` — a service behind a
  dedicated manager trait, config, two scheduled tasks, eligibility filters,
  API routes as thin handlers. **The canonical full port.**
- `crates/ferrofin-extensions/src/intro_skipper.rs` — heavy config, a scheduled
  task with a fingerprint pipeline, a vendored web-built (vite) settings page.

Key mechanisms to map onto (all in `crates/ferrofin-extensions/src/lib.rs`):
- `Extension` trait: `id()` (the upstream GUID), `descriptor()` (name/version/
  description for `/Plugins`), `default_config()`, `config_pages()`, `tasks()`.
- `builtin_extensions()` — the registry the new extension is added to.
- `ExtensionContext` — the trait objects an extension's tasks may touch; extend
  it if the new plugin needs a collaborator not already there.
- `PluginConfigPage` + `build.rs` — how a settings page is vendored and served.
- `ScheduledTask` (`ferrofin-traits/src/tasks.rs`) — the task trait; tasks
  self-gate on the plugin's `enabled` flag for live toggling.
- Dedicated manager trait in `ferrofin-traits/` when the plugin adds API routes:
  handlers depend only on `ferrofin-traits`, the impl lives with the extension.

## Step 3 — decide the mapping, route by route

For each piece of the plugin, decide one of: **port faithfully**, **already
exists in Ferrofin** (reuse — check first, e.g. a DB column or manager method may
already back it), or **accepted divergence** (a .NET-ism or internal
representation with no Ferrofin equivalent — document, don't fake it). Consult
`brain/PLUGINS_UPSTREAM.md` and the `dont-port-jellyfin-bugs` memory: when
upstream is buggy and Ferrofin is correct, keep Ferrofin correct and record it as a
divergence. Surface any magic numbers (per the global instruction) as decisions
for the user.

Concretely resolve:
- **GUID** — from `Plugin.cs`, reused verbatim.
- **Config** — the PascalCase struct + defaults; which fields the logic
  actually consumes vs. store-for-parity.
- **Data model** — does Ferrofin already have the columns/entities/manager methods
  the logic needs? Grep `ferrofin-db/src/entities/`, `ferrofin-traits/`,
  `ferrofin-core/`. Missing write paths (e.g. a persistence method) are plan items.
- **API routes** — check the vendored OpenAPI + `handlers::REAL_ROUTES`: is the
  route already registered (possibly 501)? New routes need a trait + handler.
- **Tasks** — keys, categories, default triggers (convert C# `TimeSpan.Ticks`).
- **Settings page** — static HTML (copy) or a web build (npm/vite, like intro
  skipper); the `build.rs` vendoring entry + pinned rev.

## Step 4 — write the plan

Write `brain/plans/PLAN_<PLUGIN>.md`, self-contained (a lighter model executes
it with no extra context — see the `plans-go-to-brain-plans` memory). Model it
on `brain/plans/PLAN_MERGEVERSIONS.md`. It must contain:

1. **Context** — what the plugin does, upstream repo + pinned rev/tag, the clone
   path, and the closest existing extension to copy.
2. **Accepted divergences** — explicit list, each with why (so the executor and
   future `sync-plugin-upstream` runs don't "fix" them).
3. **Phased steps**, each naming the exact Ferrofin files to touch and the C#
   source (file + symbol) that is the oracle. Cover: the extension struct +
   `builtin_extensions()` registration; the config struct; the manager trait +
   impl (if routes); the handlers + `REAL_ROUTES`/`register`; the tasks; the
   settings-page vendoring (`build.rs` const + `assets/` + `FERROFIN_REFRESH_PLUGIN_ASSETS`);
   the `ExtensionContext`/composition-root wiring (`apps/ferrofin-server/src/state.rs`).
4. **Tests** — transliterate any upstream xUnit `[Theory]/[InlineData]` into
   `rstest` `#[case]` (C# expected values are the oracle); otherwise enumerate
   the unit tests to write (domain-named per `test-organization-by-domain`).
5. **Verification + gates** — the exact commands: `cargo fmt --all --check`;
   `cargo clippy --all-targets --all-features -- -D warnings`; `cargo nextest
   run --workspace`; doctests; per-crate `cargo llvm-cov nextest -p <crate>
   --fail-under-lines 80`; `./suite/run.sh gate --measure` if a perf-path crate
   is touched; and a **live-HTTP** checklist (green tests are necessary, not
   sufficient) — plugin in `/Plugins`, settings page loads, routes work,
   disable→routes 404/tasks no-op, config round-trips, drop-in DB stays
   Jellyfin-readable.
6. **Close the loop** — add a row to `brain/PLUGINS_UPSTREAM.md` (repo, clone,
   ported rev, version, status) so `sync-plugin-upstream` tracks it.

## Step 5 — hand off

Report the plan path and a 3–5 line summary: what surfaces on `/Plugins`, what's
faithfully ported, what's an accepted divergence, and any decisions the user
still owes (magic numbers, whether a settings page needs a node build). Do not
start implementing — that's `implement-plugin-port`.
