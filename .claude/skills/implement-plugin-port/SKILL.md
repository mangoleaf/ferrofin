---
name: implement-plugin-port
description: >-
  Execute a brain/plans/PLAN_*.md that ports a third-party Jellyfin plugin into
  a compiled-in Ferrofin extension: write the extension + trait + handlers +
  tasks, vendor the settings page, wire the composition root, and drive it
  through every quality gate plus live-HTTP verification. Use when asked to
  "implement the plugin port plan", "execute PLAN_<PLUGIN>", "build the
  <plugin> extension", "finish porting <plugin>", or "/implement-plugin-port
  <plan>". Expects a plan (from plan-plugin-port) to already exist.
---

# Implement a Jellyfin plugin port

Execute an approved `brain/plans/PLAN_<PLUGIN>.md`, turning a Jellyfin plugin
into a compiled-in Ferrofin extension. The argument names the plan (or plugin).
Follow the plan phase by phase; this skill is the how-to for the mechanics the
plan references. Obey `CLAUDE.md` throughout: handlers depend only on
`ferrofin-traits`, traits stay object-safe, DTOs are PascalCase, sqlx is runtime
only, every `pub` item is documented, pedantic clippy is clean.

If no plan exists, stop and run `plan-plugin-port` first — do not improvise the
design.

## The one non-negotiable

**Never fake a subsystem to make a route "pass"** (`no-deferring-full-parity`
memory). A ported feature is fully implemented and tested or it isn't in the
build. Green unit tests are necessary but not sufficient — the last step is
always exercising the real server over HTTP (`no-deferring-full-parity` and the
CLAUDE.md "green tests are necessary, not sufficient" rule).

## Grouping — the whole plugin in one place

Put the entire extension in `crates/ferrofin-extensions/src/<plugin>.rs`: the
`Extension` impl, the config struct, the service, the scheduled tasks, the pure
helpers, and their unit tests. The API handlers are the only piece that lives
elsewhere (`ferrofin-api`), and they stay thin. This mirrors
`merge_versions.rs`/`file_transformation.rs` — read the closest one the plan
names and match its shape.

## Build order (typical)

1. **Trait seam** (if the plugin adds API routes): a new object-safe manager
   trait in `ferrofin-traits/src/<plugin>.rs`, added to `ferrofin-traits/src/lib.rs`,
   with the `_assert_object_safe_*` guard. Any new persistence write path the
   logic needs (grep first — it may exist) goes on the right repository trait in
   `ferrofin-traits/src/persistence.rs` with a default impl, then the real impl in
   the matching `ferrofin-core/src/*_service.rs`.
2. **Extension module** `crates/ferrofin-extensions/src/<plugin>.rs`:
   - `pub const EXTENSION_ID` = the upstream GUID (`Uuid::from_u128(0x…)`).
   - `Extension` impl: `descriptor()` (name/version/description), a PascalCase
     `#[serde(rename_all = "PascalCase", default)]` config struct behind
     `default_config()`, `config_pages()` pointing at the vendored asset via
     `include_bytes!(concat!(env!("OUT_DIR"), "/<name>/<file>"))`, and `tasks()`.
   - The service impl of the manager trait; tasks that self-gate on the plugin's
     `enabled` flag (read via `PluginManager::get_plugin`) so toggling is live.
   - Port C# constants/regex verbatim; keep pure helpers testable.
3. **Register**: add the extension to `builtin_extensions()` in
   `ferrofin-extensions/src/lib.rs`; extend `ExtensionContext` if a new
   collaborator is needed.
4. **Handlers** (`ferrofin-api/src/handlers/<plugin>.rs`): thin `RequireAuth`
   seams over the trait, resolved from `AppState` (add an `Option<Arc<dyn …>>`
   field + `with_<plugin>` builder in `ferrofin-api/src/state.rs`; absent → the
   route reports the plugin unavailable, matching a disabled Jellyfin
   controller). Add each route to `handlers::REAL_ROUTES` and `register`, and
   keep the `contract_superset` test green.
5. **Settings page vendoring** (`ferrofin-extensions/build.rs`): add `<PLUGIN>_REPO`
   + `<PLUGIN>_REV` (the pinned upstream rev) + an assets list, a
   `vendor_plugin_assets(...)` call, and a `build_<plugin>_assets` fn (fetch by
   sha → copy, or `npm install && npm run build` for a web-built page). Copy the
   committed page into `crates/ferrofin-extensions/assets/<name>/` so normal builds
   are hermetic; a refresh is `FERROFIN_REFRESH_PLUGIN_ASSETS=1 cargo build -p
   ferrofin-extensions`.
6. **Composition root** (`apps/ferrofin-server/src/state.rs`): construct the
   service over its concrete collaborators, pass it into `ExtensionContext`
   (for the tasks) and into the app state via `.with_<plugin>(…)`.

## Tests

Write the tests in the extension module. Transliterate any upstream xUnit
`[InlineData]` to `rstest` `#[case]` (the C# expected values are the oracle).
Cover the config PascalCase round-trip, the descriptor GUID, each task's
key/category/trigger + the disabled-plugin no-op, the settings page bytes, and
the logic's branches over a real in-memory `ferrofin-db`. Seed through the real
persistence/repository seams — **not raw `sqlx` in the extension crate**, which
the `sql_boundary` ratchet forbids outside repository modules. Name tests by
domain, never `batchN` (`test-organization-by-domain`).

## Gates — all must pass

```bash
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo nextest run --workspace
cargo test --workspace --doc
cargo llvm-cov nextest -p <each-touched-crate> --fail-under-lines 80 --summary-only
```

- Gate each crate on its **own** line (never merge `-p` flags). The local
  per-crate gate can exit 1 even on clean main — compare per-file rows against a
  stashed baseline (`llvm-cov-gate-local-quirk` memory).
- If you moved SQL out of a file, lower its ceiling in
  `crates/ferrofin-db/tests/sql_boundary.rs` in the same commit (it only ratchets
  down; never raise one). A pre-existing violation elsewhere isn't yours to
  raise — flag it.
- Perf-path crate touched (`ferrofin-core`/`ferrofin-db`/`ferrofin-api` or the
  query/repo/DTO paths)? Run `./suite/run.sh gate --measure` on a **quiet** host
  — this box's background load (compiles, other agents) contaminates k6 windows;
  confirm any "regression" with an isolate-curl control before believing it
  (`perf-record-losses-are-queueing`).

## Live-HTTP verification (mandatory)

Restart the dev server on the fresh binary — the full command **must** include
the env prefix (`dev-server-restart-command` memory):

```bash
cargo build --release -p ferrofin-server
FERROFIN_WEB_DIR=/home/mango/dev/3rdparty/jellyfin-web/dist \
  target/release/ferrofin-server --bind 127.0.0.1 --port 8096
```

Reuse a live token (`playback-debug-via-live-token`):
`sqlite3 ~/.local/share/ferrofin/hermit.db "SELECT AccessToken FROM Devices WHERE AppName='Jellyfin Web' LIMIT 1;"`,
then send it as `Authorization: MediaBrowser Token="…", Client="curl",
Device="cli", DeviceId="cli-verify", Version="1.0"`. Confirm:
- the plugin appears in `GET /Plugins` (right name/version/GUID, Active);
- the settings page loads: `GET /web/ConfigurationPage?name=<Name>` (200);
- each new route works with a real token;
- `POST /Plugins/{id}/{ver}/Disable` → routes 404 and tasks no-op; Enable
  restores — no restart;
- config round-trips via `POST`/`GET /Plugins/{id}/Configuration` (PascalCase);
- any task shows in `GET /ScheduledTasks` with the right category/trigger;
- if the feature writes to the DB, a round-trip leaves it Jellyfin-readable
  (drop-in constraint, `jellyfin-db-dropin-requirement`).

## Close out

- Update `brain/PLUGINS_UPSTREAM.md`: set the plugin's row to the ported rev +
  version, status `ported`, and note the accepted divergences. Keep the row's
  rev equal to the `build.rs` `<PLUGIN>_REV`.
- Mark the plan executed (a short status banner at its top).
- Update the extensions memory (`extensions-and-intro-skipper.md`) with a short
  paragraph on the new extension.
- Commit only the port's files (no unrelated tree noise), no branching, and **no
  AI attribution trailers** (`no-coauthored-by-in-commits`,
  `never-branch-without-explicit-instruction`) — commit only when the user asks.
- Report: what's on `/Plugins`, the gate results honestly (including any
  pre-existing red gate that isn't yours), and the live-HTTP outcomes.
