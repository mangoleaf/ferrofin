# Hermit — contributor & agent guide

Hermit is a from-scratch **Rust** implementation of the [Jellyfin](https://github.com/jellyfin/jellyfin)
media server. It speaks the **same HTTP/REST API** as Jellyfin, so existing Jellyfin
clients (web, mobile, and TV apps) connect to it unchanged — a client only ever sees an
HTTP endpoint and can't tell the server is Rust.

**License:** GPL-3.0-only (Hermit is a derivative of Jellyfin's GPL-3.0-only server crates).

This document is the operating guide for anyone — human or AI agent — working in this
repository. Read it before making changes.

---

## The one idea that explains the whole codebase

**Clients depend on Jellyfin's API surface, not its code.** So the contract Hermit must
honor is the **HTTP API**, captured as a vendored OpenAPI spec
(`contracts/jellyfin-openapi-*.json`, also embedded at
`crates/hermit-api/tests/data/`). Everything else is an implementation detail we are free
to design idiomatically in Rust.

Consequences you must respect:

- **Every path in the vendored spec is registered as a route.** A route that isn't
  implemented yet returns **`501 Not Implemented`**, never `404`. A known route must never
  surprise a client with a 404. The test `crates/hermit-api/tests/contract_superset.rs` is a
  **hard gate**: the registered route table must be a superset of the vendored spec (checked
  both directions, plus a live-probe that no contract route 404s). If you add or rename a
  route, that test keeps you honest.
- **JSON is PascalCase.** DTOs use `#[serde(rename_all = "PascalCase")]` and
  `#[serde(skip_serializing_if = "Option::is_none")]`, matching the spec exactly. When in
  doubt about a field name/casing/nullability, check the vendored spec
  (`jq '.components.schemas.<Type>' contracts/jellyfin-openapi-*.json`).

---

## Workspace layout

Rust workspace, edition 2024, toolchain pinned to **1.97.1** (stable). Library crates in
`crates/`, the server binary in `apps/`.

```
util ─┐
model ┼─ common ─┬─ db ──────┐
naming┘          │            │
keyframes        networking   ├─ traits ─┬─ mediaencoding ─ hls
                 health       │          ├─ drawing
                              │          ├─ providers
                              │          └─ livetv (stub)
                              └──────────────► core ─► api ─► server (bin)
```

| Crate | Responsibility |
|---|---|
| `hermit-util` | leaf string/path/collection helpers |
| `hermit-model` | all DTOs + enums (serde + utoipa) — the shared data universe; also the DLNA profile/StreamBuilder logic |
| `hermit-naming` | filename → media parsing |
| `hermit-keyframes` | keyframe extraction structures |
| `hermit-common` | config, app-paths, **password hashing** (real PBKDF2-HMAC-SHA512/SHA1, byte-compatible with Jellyfin) |
| `hermit-networking` | bind / published-URL resolution |
| `hermit-health` | liveness/readiness router |
| `hermit-db` | **sqlx + SQLite** — entity `FromRow` structs, the schema migration, `Database` handle |
| `hermit-traits` | the manager/service **traits** — the dependency-injection seam (see below) |
| `hermit-mediaencoding` | ffmpeg/ffprobe: probing, transcode arg-building, the live transcode runtime |
| `hermit-hls` | HLS playlist generation + the stream manager |
| `hermit-drawing` | image resize/crop/format (via the `image` crate) |
| `hermit-providers` | metadata providers (local NFO; remote TMDB/MusicBrainz are feature-gated) |
| `hermit-livetv` | Live TV (currently a disabled stub) |
| `hermit-core` | the concrete manager implementations — the workhorse |
| `hermit-api` | axum router + handlers (the HTTP layer) |
| `apps/hermit-server` | the binary: config → DB → ffmpeg → wire everything → serve |

---

## Architecture rules (do not violate)

### `hermit-traits` is the dependency-injection seam
The manager interfaces (`LibraryManager`, `UserManager`, `DtoService`, `ItemRepository`,
`TranscodeManager`, …) are `#[async_trait]` **traits** in `hermit-traits`. This is load-bearing:

- **API handlers depend only on `hermit-traits`** (via `AppState`, which holds each manager
  as `Arc<dyn Trait>`). Handlers must **never** import `hermit-core`.
- **`hermit-core` implements** those traits. **`apps/hermit-server` is the composition root**
  that constructs the concrete impls and injects them into `AppState`.
- This is why the dependency arrow points *away* from `hermit-core`: it breaks the C#
  implementation↔API reference cycle that Rust's crate graph forbids, and it lets the server
  run on stub implementations while a subsystem is unfinished.

Every trait must be **object-safe** (usable as `Arc<dyn Trait>`): no generic methods, no
`impl Trait` returns, no `Self`-by-value. Add a compile-time `fn _assert_object_safe(_: &dyn T) {}`.

### There is no domain-object hierarchy
Jellyfin's `BaseItem`/`Folder`/`Video` OOP tree is **not** ported — it's a service layer in
inheritance disguise. Trait signatures traffic in:

- **`uuid::Uuid`** for item identity / lookup keys,
- **`hermit-db` entities** for persistence in/out (repository layer),
- **`hermit-model` DTOs** for presentation (DTO service, API layer).

Behavior that was a `virtual` method on `BaseItem` becomes a **free function over
`BaseItemKind`** in `hermit-core` (see `hermit-core/src/kinds.rs`).

### Persistence: sqlx + SQLite, runtime queries only
`hermit-db` uses **runtime** sqlx — `#[derive(sqlx::FromRow)]` + `sqlx::query_as`, and
`sqlx::migrate!()` for the schema. **Do not use the compile-time `query!`/`query_as!`
macros** — they require a live `DATABASE_URL` at build/CI time, which we deliberately avoid.
The schema is a single migration reflecting the current head (`crates/hermit-db/migrations/`).

### Errors
Libraries use per-crate `thiserror` enums; the binary uses `anyhow` at the top level.
Never `.unwrap()` on fallible paths; prefer `?` or explicit handling. `hermit-api` maps its
error enum to HTTP status via `IntoResponse` (`NotImplemented → 501`, `Unauthorized → 401`, …).

---

## Conventions

- **Dependencies live in the root `Cargo.toml`** `[workspace.dependencies]`; crates reference
  them as `dep.workspace = true`. **Never** inline a version in a crate's `Cargo.toml`. Check
  crates.io for the latest version before adding a new dependency.
- **Lints are workspace-wide** (`[workspace.lints]`): `missing_docs` and `clippy::pedantic`
  are warnings, and CI treats warnings as errors. So **every `pub` item needs a `///` doc**,
  and pedantic clippy must pass.
- **Match the surrounding code.** New HTTP crates mirror the existing `hermit-api` shape
  (`create_router`, `AppState`, `error.rs` with `IntoResponse`, a `utoipa` `ApiDoc`).
- **Port faithfully.** When implementing behavior that mirrors Jellyfin, port it faithfully
  from the upstream C# (github.com/jellyfin/jellyfin) — including regex tables copied verbatim.
  Where Jellyfin ships xUnit tests, transliterate the `[Theory]/[InlineData]` cases directly
  into `rstest` `#[case]` tests: the C# expected values are the oracle.
- **Metrics** (`/metrics`, `hermit-metrics`): all metric work follows
  `docs/conventions/METRICS.md` — parity-first names (Jellyfin's `http_*`/`process_*`),
  bounded labels, noop-when-disabled, sync observable callbacks. Docs live in
  `contrib/metrics/`.
- **Traces** (OTLP → Tempo, opt-in via `OTEL_EXPORTER_OTLP_ENDPOINT`): all span work
  follows `docs/conventions/TRACING.md` — off by default, `skip_all` + typed fields,
  sampling is the storage knob, flush on shutdown, no secrets in spans.
- **Logging**: all log statements follow `docs/conventions/LOGGING.md` — levels mean
  things, errors logged once at the outermost layer with context, spans for units of
  work with the standard field vocabulary (`item_id`/`user_id`/`task`/…), no level whose
  volume scales with library size above `debug`, panics stay visible, no secrets.

---

## Building, running, testing

```bash
cargo build --workspace

# Run the server. Config via flags, HERMIT_* env, or {data_dir}/config.toml.
cargo run -p hermit-server -- --data-dir ./data --bind 127.0.0.1 --port 8096
#   On a fresh database it seeds an admin user and logs a generated password — record it.
#   ffmpeg/ffprobe are auto-discovered ($PATH or --ffmpeg); absent ffmpeg only disables transcode.
```

Note: Hermit serves the **API**, not a web UI — a browser at `/` returns 404 by design.
Test with an API endpoint (`curl http://localhost:8096/System/Info/Public`), load
`/api-docs/openapi.json` into an API tool, or point a native Jellyfin client at the server.

Clients authenticate with `POST /Users/AuthenticateByName`, then send the token on every
request via `Authorization: MediaBrowser Token="…", Client="…", Device="…", DeviceId="…", Version="…"`.

### Quality gates (all must pass — this is what CI enforces)

```bash
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo nextest run --workspace          # + `cargo test --workspace --doc` for doctests
```

**Coverage gate — ≥80% line coverage, enforced per-crate:**

```bash
cargo llvm-cov nextest -p <crate> --fail-under-lines 80 --summary-only
```

- **Gate each crate on its own line.** Do **not** pass multiple `-p` flags to one
  `--fail-under-lines` run — that checks the *merged* total and lets a weak crate hide behind
  a strong one. Loop one crate at a time (the CI job does this).
- **Line coverage only.** `--branch` needs a nightly toolchain; we pin stable, so don't use it.
- Two crates are **exempt** from the coverage gate because they contain almost no unit-testable
  logic: `hermit-traits` (trait definitions — bodies live in `hermit-core`) and
  `apps/hermit-server` (the composition root — wiring). Their gate is "compiles + integration
  test passes + fmt/clippy clean"; the config/planner logic they *do* contain is still tested.

### ffmpeg integration tests
The real `tokio::process` ffmpeg spawn (transcode) can't be unit-tested and is excluded from
the coverage gate — it lives behind a seam trait with a fake for unit tests. The real
integration tests are gated behind an env var and skip if ffmpeg is absent:

```bash
HERMIT_FFMPEG_TESTS=1 cargo test -p hermit-mediaencoding --test segment_transcode_ffmpeg
HERMIT_FFMPEG_TESTS=1 cargo test -p hermit-hls          --test hls_stream_manager_ffmpeg
```

### Perf regression gate (mandatory for perf-touching changes)
Body-diff correctness is not a latency signal — a 100× slowdown can land "green." Any
change touching `hermit-core`, `hermit-db`, `hermit-api`, or the query/repository/DTO
paths (`translate_query`, `item_repository`, `dto_service`) **must pass the perf gate**:

```bash
cd suite/perf && ./perf-gate.sh          # Hermit-only; fails if any sentinel endpoint
                                        # exceeds 1.5× baseline on p50, p95, or p99
```

~5 min; compares Hermit to the `raw` section of `suite/perf-baseline.json` — the ONE
baseline file, with `suite/gate.py` as the ONE comparator (also driven by
`suite/run.sh gate` over the merged parity+perf record). Re-`./perf-gate.sh --rebaseline`
at each release and after any *intended* perf change so only unintended slowdowns trip it.
All three percentiles gate (tail regressions are what users feel). See `suite/README.md` + `suite/perf/README.md`.

### Green tests are necessary, not sufficient
Several real bugs in this codebase passed their unit/integration tests and were caught only
by **running the server and exercising it over real HTTP** (e.g. login once returned no
access token despite a "passing" test that checked the token via a direct DB query). When you
touch a data path, an auth path, or anything stateful, **run the binary and hit it**, and read
the substance of what a handler actually does — don't rely on a green checkmark alone.

---

## Current scope

Hermit boots and serves the core: authentication, library browse/query, item read+write,
images, sessions/playstate, playlists/collections, direct-play, and live HLS transcode.

Not everything is implemented. Un-ported routes return `501`. The largest gaps are the
subsystems that need real subsystem work (not just a handler): **Live TV**, **SyncPlay**, a
**dynamic plugin host** (no Rust equivalent to .NET runtime assembly loading), and the **web
UI** (a separate frontend — nothing is served at `/` or `/web`). Remote metadata providers
(TMDB/MusicBrainz) are feature-gated off and return empty results until enabled.

When you implement a `501` route, the pattern is: write the handler in
`hermit-api/src/handlers/<controller>.rs` calling the `AppState` managers, add it to
`handlers::REAL_ROUTES` and `handlers::register`, and — if the backing manager method is
missing or stubbed — implement it for real in `hermit-traits` + `hermit-core`. Keep the
contract-superset test green. Never fake a deferred subsystem to make a route "pass."
