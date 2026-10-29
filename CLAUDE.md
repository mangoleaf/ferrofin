# Ferrofin — contributor & agent guide

Ferrofin is a from-scratch **Rust** implementation of the [Jellyfin](https://github.com/jellyfin/jellyfin)
media server. It speaks the **same HTTP/REST API** as Jellyfin, so existing Jellyfin
clients (web, mobile, and TV apps) connect to it unchanged — a client only ever sees an
HTTP endpoint and can't tell the server is Rust.

**License:** GPL-3.0-only (Ferrofin is a derivative of Jellyfin's GPL-3.0-only server crates).

This document is the operating guide for anyone — human or AI agent — working in this
repository. Read it before making changes.

---

## The one idea that explains the whole codebase

**Clients depend on Jellyfin's API surface, not its code.** So the contract Ferrofin must
honor is the **HTTP API**, captured as a vendored OpenAPI spec
(`contracts/jellyfin-openapi-*.json`, the single copy — the api tests and the server
`include_str!` it). Everything else is an implementation detail we are free to design
idiomatically in Rust.

Consequences you must respect:

- **Every path in the vendored spec is registered as a route.** A route that isn't
  implemented yet returns **`501 Not Implemented`**, never `404`. A known route must never
  surprise a client with a 404. The test `crates/ferrofin-api/tests/contract_superset.rs` is a
  **hard gate**: the registered route table must be a superset of the vendored spec (checked
  both directions, plus a live-probe that no contract route 404s). If you add or rename a
  route, that test keeps you honest.
- **JSON is PascalCase.** DTOs use `#[serde(rename_all = "PascalCase")]` and
  `#[serde(skip_serializing_if = "Option::is_none")]`, matching the spec exactly. When in
  doubt about a field name/casing/nullability, check the vendored spec
  (`jq '.components.schemas.<Type>' contracts/jellyfin-openapi-*.json`).

---

## Workspace layout

Rust workspace, edition 2024, toolchain pinned to **1.99.1** (stable). Library crates in
`crates/`, the server binary in `apps/`.

```
util ─┐
model ┼─ common ─┬─ db ──────┐
naming┘          │            │
keyframes        networking   ├─ traits ─┬─ mediaencoding ─ hls
chromaprint      health       │          ├─ drawing
                 metrics      │          ├─ providers
                              │          ├─ livetv
                              │          ├─ extensions
                              │          └─ wasm (plugin host)
                              └──────────────► core ─► api ─► server (bin)
```

| Crate | Responsibility |
|---|---|
| `ferrofin-util` | leaf string/path/collection helpers |
| `ferrofin-model` | all DTOs + enums (serde + utoipa) — the shared data universe; also the DLNA profile/StreamBuilder logic |
| `ferrofin-naming` | filename → media parsing |
| `ferrofin-keyframes` | keyframe extraction: the ffprobe output parser and the Matroska cue reader |
| `ferrofin-chromaprint` | audio fingerprinting (chromaprint) for the intro-skipper extension |
| `ferrofin-common` | config, app-paths, **password hashing** (real PBKDF2-HMAC-SHA512/SHA1, byte-compatible with Jellyfin) |
| `ferrofin-networking` | bind / published-URL resolution |
| `ferrofin-health` | liveness/readiness router |
| `ferrofin-metrics` | Prometheus `/metrics` with Jellyfin-parity names (see `docs/conventions/METRICS.md`) |
| `ferrofin-db` | **sqlx + SQLite** — entity `FromRow` structs, the migration chain, `Database` handle (schema pinned byte-equal to Jellyfin 12.0; adopts 10.11.8–10.11.11, 12.0, 12.1, 12.2, 12.3 and 12.4 databases in place) |
| `ferrofin-traits` | the manager/service **traits** — the dependency-injection seam (see below) |
| `ferrofin-mediaencoding` | ffmpeg/ffprobe: probing, transcode arg-building, the live transcode runtime |
| `ferrofin-hls` | HLS playlist generation + the stream manager |
| `ferrofin-drawing` | image resize/crop/format (via the `image` crate) |
| `ferrofin-providers` | metadata providers (local NFO + images always on; remote TMDB/TVDB/MusicBrainz/AudioDb/fanart/Studio Images always compiled, gated per library; OMDb uses a built-in key) |
| `ferrofin-livetv` | Live TV — M3U/HDHomeRun tuners + XMLTV/Schedules Direct guides, DB-backed DVR timers/recordings |
| `ferrofin-extensions` | compiled-in extensions behind an `Extension` trait (Tier 1a — see `docs/EXTENSIONS.md`) |
| `ferrofin-wasm` | the Tier-1b WASM plugin host: sandboxed `ferrofin:plugin` components from `{data_dir}/plugins/` (wasmtime; WIT contract in `crates/ferrofin-wasm/wit/`) |
| `ferrofin-core` | the concrete manager implementations — the workhorse |
| `ferrofin-api` | axum router + handlers (the HTTP layer) |
| `apps/ferrofin-server` | the binary: config → DB → ffmpeg → wire everything → serve |

---

## Architecture rules (do not violate)

### Nothing is deferred — ever
**"Deferred" is not a state Ferrofin has.** Every Jellyfin behaviour reachable through the
contract is either ported faithfully or is an open work item to port now — never a stub, a
no-op, a "faithful-empty" shrug, or an "out-of-scope"/"subsystem decision" label. This is
a standing directive from the project owner (2026-08-22), and it is retroactive: any
`deferred`/`deferred-hollow`/`deferred-remote-or-feature-gated`/"documented out-of-scope"
wording you find in older notes, code comments, or trait-default `Err("… is deferred")`
bodies is **legacy debt to remove by porting the thing**, not a precedent to lean on.

- When a parity note says *deferred*, first verify the real code state (the notes go stale —
  several were already ported when re-checked), then port whatever is actually missing from
  the upstream C# (a checkout of github.com/jellyfin/jellyfin at `UPSTREAM_TAG`), test it,
  and run it live.
- The only accepted divergences are the ones listed under **Current scope** below (native .NET
  plugin loading, SSDP discovery). Adding to that list is the owner's call,
  never an agent's.
- Never write the word "deferred" into a classification, a doc, or a comment as a reason to
  skip work. If something cannot be finished in the current task, say so in the summary and
  leave it as an explicit TODO work item with the un-defer path — not as an accepted state.

### `ferrofin-traits` is the dependency-injection seam
The manager interfaces (`LibraryManager`, `UserManager`, `DtoService`, `ItemRepository`,
`TranscodeManager`, …) are `#[async_trait]` **traits** in `ferrofin-traits`. This is load-bearing:

- **API handlers depend only on `ferrofin-traits`** (via `AppState`, which holds each manager
  as `Arc<dyn Trait>`). Handlers must **never** import `ferrofin-core`.
- **`ferrofin-core` implements** those traits. **`apps/ferrofin-server` is the composition root**
  that constructs the concrete impls and injects them into `AppState`.
- This is why the dependency arrow points *away* from `ferrofin-core`: it breaks the C#
  implementation↔API reference cycle that Rust's crate graph forbids, and it lets the server
  run on stub implementations while a subsystem is unfinished.

Every trait must be **object-safe** (usable as `Arc<dyn Trait>`): no generic methods, no
`impl Trait` returns, no `Self`-by-value. Add a compile-time `fn _assert_object_safe(_: &dyn T) {}`.

### There is no domain-object hierarchy
Jellyfin's `BaseItem`/`Folder`/`Video` OOP tree is **not** ported — it's a service layer in
inheritance disguise. Trait signatures traffic in:

- **`uuid::Uuid`** for item identity / lookup keys,
- **`ferrofin-db` entities** for persistence in/out (repository layer),
- **`ferrofin-model` DTOs** for presentation (DTO service, API layer).

Behavior that was a `virtual` method on `BaseItem` becomes a **free function over
`BaseItemKind`** in `ferrofin-core` (see `ferrofin-core/src/kinds.rs`).

### Persistence: sqlx + SQLite, runtime queries only
`ferrofin-db` uses **runtime** sqlx — `#[derive(sqlx::FromRow)]` + `sqlx::query_as`, and
`sqlx::migrate!()` for the schema. **Do not use the compile-time `query!`/`query_as!`
macros** — they require a live `DATABASE_URL` at build/CI time, which we deliberately avoid.
The schema is an ordered migration chain (`crates/ferrofin-db/migrations/`) whose
Jellyfin-owned shape is **pinned byte-equal to a real Jellyfin 12.0 database** (`0032` converges the
10.11.8 shape the chain carried through `0029`; the gate adopts the exact
`__EFMigrationsHistory` sets for 10.11.8–10.11.11, 12.0.0, 12.1.0 and 12.2.0, including both
12.1 and both 12.2 upgrade routes in the [support matrix](gates/adoption/README.md#supported-and-tested-versions);
12.3.0 and 12.4.0 record 12.2.0's set and adopt as that generation) — that is
what makes drop-in adoption of an existing Jellyfin DB possible (point Ferrofin at it and it
migrates in place). Ferrofin-own tables/indexes live in a collision-proof
`Ferrofin*`/`FerrofinIX_*` namespace. The `schema_conformance` test guards the pin.

**Adoption is one-way, and every doc must say so.** The migration chain rebuilds several
Jellyfin-owned tables (0007, 0032) and normalises stored values (GUID casing, presentation keys),
so a database Ferrofin has migrated is not guaranteed to open under Jellyfin again. The
adoption path copies the original to `jellyfin.db.pre-ferrofin` before touching it and logs
the path; the table-rebuilding migrations (`0007`, `0032`) and the dangling-row cleanup
(`0044`) each snapshot the file to `jellyfin.db.pre-00NN` first, and every file-backed boot
runs `PRAGMA foreign_key_check`; returning to Jellyfin means restoring
`jellyfin.db.pre-ferrofin`, the only copy older than every change Ferrofin made. Any other
migrating boot also snapshots the file, to a `.pre-NNNN` it deletes once it has passed (as
Jellyfin does) and only warns about when it cannot write it. Every snapshot is written to
`.partial` and renamed whole, and an existing one is kept, never replaced. Never describe the
swap as two-way or round-trip in user-facing text. (Suite v3 exercises adoption — every server
boots a disposable copy of a Jellyfin-scanned database and Ferrofin migrates it in place —
but nothing checks that Jellyfin still opens the result, and nothing is expected to.)

### Errors
Libraries use per-crate `thiserror` enums; the binary uses `anyhow` at the top level.
Never `.unwrap()` on fallible paths; prefer `?` or explicit handling. `ferrofin-api` maps its
error enum to HTTP status via `IntoResponse` (`NotImplemented → 501`, `Unauthorized → 401`, …).

---

## Conventions

- **Dependencies live in the root `Cargo.toml`** `[workspace.dependencies]`; crates reference
  them as `dep.workspace = true`. **Never** inline a version in a crate's `Cargo.toml`. Check
  crates.io for the latest version before adding a new dependency.
- **Lints are workspace-wide** (`[workspace.lints]`): `missing_docs` and `clippy::pedantic`
  are warnings, and CI treats warnings as errors. So **every `pub` item needs a `///` doc**,
  and pedantic clippy must pass.
- **Match the surrounding code.** New HTTP crates mirror the existing `ferrofin-api` shape
  (`create_router`, `AppState`, `error.rs` with `IntoResponse`, a `utoipa` `ApiDoc`).
- **Port faithfully.** When implementing behavior that mirrors Jellyfin, port it faithfully
  from the upstream C# (github.com/jellyfin/jellyfin) at `UPSTREAM_TAG` — including regex tables
  copied verbatim. That tag is the **latest Jellyfin release when a release branch is cut**
  (owner decisions 2026-10-16 and 2026-10-25), never a release candidate. The release branch
  keeps it until its release, and the next release branch moves to the newest release. If
  `git ls-remote --tags https://github.com/jellyfin/jellyfin` shows a newer release, say so: the
  next release branch moves to it (see `UPSTREAM_TAG` in `handlers/mod.rs`).
  Where Jellyfin ships xUnit tests, transliterate the `[Theory]/[InlineData]` cases directly
  into `rstest` `#[case]` tests: the C# expected values are the oracle.
- **Metrics** (`/metrics`, `ferrofin-metrics`): all metric work follows
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

# Run the server. Config via flags, FERROFIN_* env, or {data_dir}/config.toml.
cargo run -p ferrofin-server -- --data-dir ./data --bind 127.0.0.1 --port 8096
#   On a fresh database it seeds a passwordless admin user (the /web wizard sets the
#   password; FERROFIN_ADMIN_PASSWORD seeds one).
#   ffmpeg/ffprobe are auto-discovered ($PATH or --ffmpeg); absent ffmpeg only disables transcode.
```

Note: Ferrofin serves the Jellyfin **web client** at `/web` when `FERROFIN_WEB_DIR` points
at a built jellyfin-web `dist/` (the release image bakes it in). Without it, Ferrofin is
API-only and a browser at `/` returns 404. Test with an API endpoint
(`curl http://localhost:8096/System/Info/Public`), load `/api-docs/openapi.json` into an API
tool, open `/web`, or point a native Jellyfin client at the server.

Clients authenticate with `POST /Users/AuthenticateByName`, then send the token on every
request via `Authorization: MediaBrowser Token="…", Client="…", Device="…", DeviceId="…", Version="…"`.

### Quality gates (all must pass — this is what CI enforces)

```bash
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo nextest run --workspace          # + `cargo test --workspace --doc` for doctests
cargo deny --locked check              # advisories, licenses, bans, sources (deny.toml)
cargo machete apps crates              # declared but unused dependencies
scripts/check-workspace-deps.py        # every dependency through [workspace.dependencies]
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
  logic: `ferrofin-traits` (trait definitions — bodies live in `ferrofin-core`) and
  `apps/ferrofin-server` (the composition root — wiring). Their gate is "compiles + integration
  test passes + fmt/clippy clean"; the config/planner logic they *do* contain is still tested.

### ffmpeg integration tests
The real `tokio::process` ffmpeg spawn (transcode) can't be unit-tested and is excluded from
the coverage gate — it lives behind a seam trait with a fake for unit tests. The real
integration tests are gated behind an env var and skip if ffmpeg is absent:

```bash
FERROFIN_FFMPEG_TESTS=1 cargo test -p ferrofin-mediaencoding --test segment_transcode_ffmpeg
FERROFIN_FFMPEG_TESTS=1 cargo test -p ferrofin-hls          --test hls_stream_manager_ffmpeg
FERROFIN_FFMPEG_TESTS=1 cargo test -p ferrofin-server       --test no_repeat_extraction_ffmpeg
FERROFIN_FFMPEG_TESTS=1 cargo test -p ferrofin-server       --test subtitle_streams_ffmpeg
```

`no_repeat_extraction_ffmpeg` checks that chapter images, trickplay and Intro Skipper
analysis are never redone for an unchanged file: library scans, the three scheduled tasks,
every choice of jellyfin-web's refresh dialog but "replace trickplay images" (a deliberate
redo) on a library, a series and an episode, and a restart, with extraction both during the
scan and by the tasks. Intro Skipper's automatic timer is off there (its scheduled task
drives the same engine).

`subtitle_streams_ffmpeg` writes a PGS subtitle (ffmpeg has no PGS encoder) and checks it
embedded from the movie and from a `.sup` sidecar and burned into a seeked HLS segment, at
the right time; and it burns an ASS track in with the fonts its file ships, which must be
extracted into the cache whatever their `filename` tags say.

Both of these need an ffmpeg with libx264, and `subtitle_streams_ffmpeg` also needs ffmpeg
5.1 or later. CI does not run them (CI never sets `FERROFIN_FFMPEG_TESTS`): run them by hand
for changes to those paths.

Similarly, the end-to-end WASM plugin test builds the reference guest from source (needs the
`wasm32-wasip2` target — the `examples/wasm-hello/` toolchain island provides it; the root
toolchain stays server-only). It skips with a message when the target is absent; CI always
runs it:

```bash
FERROFIN_WASM_GUEST_TESTS=1 cargo test -p ferrofin-wasm --test wasm_hello_guest
```

### Performance is measured per release, not per PR — the v3 benchmark is in `gates/bench/`
**Suite v3 exists and is the tool to use** (v2, formerly `suite/`, was scrapped by owner
decision on 2026-09-02 for policing itself instead of answering questions; do not
resurrect it from git history). v3 answers four questions and nothing else:
latency (screens and endpoints), time to first screen, memory, and response shape vs
Jellyfin. Read [`gates/bench/README.md`](gates/bench/README.md) first — it defines every number in one
sentence.

```bash
# needs docker, k6, jq, taskset, curl, python3, sha256sum, timeout, setsid; ffprobe for TTFS
# plus ffmpeg (libx264 + libx265) and Python Pillow to build the test data
export FERROFIN_GATES_DIR=/srv/gates           # output root: absolute, local disk, outside the repo (gates/lib.sh)
docker pull jellyfin/jellyfin:10.11.8
docker pull "$(cat gates/bench/testdata/jellyfin-image.txt)"
gates/bench/testdata/build.sh                          # source fixture, once: ~20 min, ~17 GB → $FERROFIN_GATES_DIR/fixtures/
gates/bench/testdata/build.sh --export-pools             # existing fixtures only; fresh builds already include pools
gates/bench/testdata/build.sh --prepare-jellyfin         # upgrade a copy, scan and verify, once
docker build -t ferrofin:bench .                 # the commit under test — REBUILD IT
gates/bench/run.sh                                     # ~40 min → $FERROFIN_GATES_DIR/runs/bench-<tag>/report.md
python3 gates/bench/report.py --serve                  # compare runs at 127.0.0.1:8097
```

`gates/run all` runs verify, adoption, parity and bench against one image as one session, after one
preflight, and ends with one report, the session's `index.html` ([`gates/README.md`](gates/README.md));
each gate's own script still runs alone. `make run-gates` (or `make run-<gate>`) does the same, and
`make test-gates-offline` runs the gates' own tests (no docker), as CI does.

Things that will otherwise cost you a run: the harness only ever *inspects* the
`ferrofin:bench` image, so a stale image silently measures old code under a run name that
claims the current commit. `run.sh` refuses to start unless the server/client cores are
90 % idle including their SMT siblings, and rejects shared physical cores. A rerun of the same code lands in `bench-<tag>-run2`, never on top of the first.
Publishable numbers (the Jellyfin comparison) come from **three** runs — `report.py` takes
the median and prints the range each cell spanned.

`gates/bench/` is NOT a CI gate and is not meant to become one; it is a measuring instrument you
run deliberately. **Measurement gates releases, not PRs** (owner decision 2026-10-16), which
keeps day-to-day work fast:
- No PR needs a `gates/bench/` or hyperfine before/after. That includes issue fixes, features,
  `perf:` changes and query-plan changes. A plan change still needs its `EXPLAIN QUERY PLAN`
  pin test (below).
- Every app release is measured before it is tagged, on the branch it is cut from:
  - feature releases come from the current `release/vX.Y.Z`. Only one exists at a time; it
    moves until it is frozen for the release;
  - patch releases of the released line come from `main`.

  `RELEASE.md` has the procedure: the interleaved A/B gate
  (`gates/bench/gate.py ab <previous release image> <candidate>`), Ferrofin only, three pairs.
  The three-server runs against Jellyfin produce the published tables and never decide a
  release.
- A gated cell that regresses past both of its `gates/bench/gate.toml` thresholds in at least 2 of
  the 3 pairs blocks the release until it is fixed or the owner accepts it. So does a gated
  cell that cannot be judged because the candidate's responses differ from the previous
  release's.
- When chasing a regression, `gates/bench/run.sh --servers ferrofin --only loaded` on two builds is
  the cheap form, skipping Jellyfin.

The fast stdlib reporter correctness tests (`gates/bench/test_report.py`) do run in CI and gate
releases; they start no servers and impose no performance thresholds. The benchmark
reference is the Jellyfin release named by `UPSTREAM_TAG` (below), pinned by digest in
`gates/bench/testdata/jellyfin-image.txt`. A release is measured against the Jellyfin release its
branch pins: when a newer one is out, `gates/bench/oracle.py` notes it and every run records the
note with the oracle; no run refuses it, a release gate's included. Only a pin it cannot read is refused.

Parity in the project's sense, implementation compared against the upstream C# and recorded
in `handlers::VERIFIED`, is still done per change. Only measurement moved.

The reason for the release gate is unchanged: body-diff correctness is not a latency signal,
and a 100× slowdown can land "green." So a PR touching `ferrofin-core`, `ferrofin-db`,
`ferrofin-api`, or the query/repository/DTO paths (`translate_query`, `item_repository`,
`dto_service`) notes that in its description, and the release run is read against those
notes.

**Never accept an index or a plan change from an isolated statement measurement** — a
statement that gets faster alone routinely drags a hotter query onto the wrong index. Prefer
`+col` / `CROSS JOIN` plan pins over new indexes (`item_repository.rs:1355,2622`;
`item_count_service.rs:549,841,886`), and guard the plan with an `EXPLAIN QUERY PLAN` test —
`item_count_service.rs:2386,2428,2835` and `crates/ferrofin-core/tests/item_repository.rs:941`
pin a chosen plan; `crates/ferrofin-core/tests/next_up_query_plan.rs` pins the opposite, that
a query stays *off* a `CROSS JOIN`. `ANALYZE` is deliberately unused and `sqlite_stat1`
never exists.

### Adoption has its own live check — `gates/adoption/`
`gates/adoption/run.sh --image ferrofin:bench` (fixtures and work directory from `FERROFIN_GATES_DIR`, or
`--fixtures DIR`) covers 10.11.8, 10.11.9,
10.11.10, 10.11.11, 12.0.0, 12.1.0, 12.2.0, 12.3.0 and 12.4.0 databases. The 12.1 and 12.2
fixtures are upgraded from 10.11.8 and from 12.0.0; the 12.3 and 12.4 fixtures from 10.11.8 and from
the previous 12.x release; and 12.2, 12.3 and 12.4 each have a fresh install.  Every fixture is **synthetic**: build them
once with `gates/adoption/build-fixtures.sh` (invented media; the official Jellyfin images seed and upgrade
each database themselves); they are never committed.  Never point the suite at a real
library or a network share — the scripts refuse a DIR on a network filesystem, because every
fixture is booted and scanned repeatedly.  A build that fails part-way is never retried
(seeding is not idempotent): build into a new DIR (`--fixtures`), or move the half-built
one aside.  On fresh copies it checks
generation, migrations, Jellyfin 12.4's normalised smoke summaries, file integrity and a
repair-free second boot.  The [test record](gates/adoption/README.md#supported-and-tested-versions)
identifies which versions passed and the image used.  Run it for any change to the migration
chain, the adoption gate or the boot repairs, because those risk user data. This per-change
requirement is unaffected by the release-branch rule above. The full adoption validation
(`gates/adoption/run.sh` and the synthetic preservation matrix) also runs before every release,
and a failure there blocks the release (see `RELEASE.md`).
The shell tests in CI exercise the harness without these live fixtures.

### Green tests are necessary, not sufficient
Several real bugs in this codebase passed their unit/integration tests and were caught only
by **running the server and exercising it over real HTTP** (e.g. login once returned no
access token despite a "passing" test that checked the token via a direct DB query). When you
touch a data path, an auth path, or anything stateful, **run the binary and hit it**, and read
the substance of what a handler actually does — don't rely on a green checkmark alone.

---

## Current scope

**All 412 operations in the vendored contract are wired to real handlers — 0 stubs, 0 `501`s.**
(Parity is tracked by `handlers::VERIFIED` beside `REAL_ROUTES`, validated and printed by
`crates/ferrofin-api/tests/contract_superset.rs` against the vendored **10.11.8** contract —
which is a different pin from the C# release named below, deliberately. The contract is the
API real clients speak, and Jellyfin 12's own surface is smaller (12.0-rc7 and 12.1.0: 364 operations to 10.11.8's 412 — it drops the
DynamicHls family, `/MusicGenres`, `/CriticReviews` and more), so satisfying it would break
clients. See `contracts/README.md`. The Jellyfin C# release each `VERIFIED` row was compared
*against* is `UPSTREAM_TAG`/`UPSTREAM_COMMIT` in `handlers/mod.rs`, currently `v12.4`, the release
this branch pins; rows compared against an earlier pin are listed in
`VERIFIED_AGAINST_EARLIER_PIN` until they are re-checked.
Owner's standing definition: **"deep verified" means the
Ferrofin implementation was compared against the upstream Jellyfin C# for behavioral
equivalence** — runtime probes are supporting evidence, never a substitute, and a metric
redefinition must never make completed verification work disappear. The benchmark's
response-shape pass is one of those supporting probes, not the parity number.)
Working end-to-end: authentication/users/QuickConnect,
library scan + live filesystem watch, browse/query/DTO, images, sessions/playstate/remote
control, WebSocket push, playlists/collections, direct play + live HLS transcode (subtitle
burn-in, fMP4 HEVC/AV1), Live TV (M3U/XMLTV + DVR timers), SyncPlay, 20 registered
scheduled tasks (18 shown in a stock install — the two guide/channel refreshers hide
themselves until a tuner exists),
metrics/tracing, trickplay/chapters/lyrics/media segments, photo and book libraries
(EXIF / `ComicInfo` / OPF), and backup/restore. See `docs/FEATURES.md` for the tiered
status matrix.

The real remaining gaps are **by design**, not un-ported routes:
- **.NET-style native plugin loading** — never (no stable Rust ABI, and full-trust loading is
  the security model Ferrofin rejects). In-process plugins come in two tiers instead
  (`docs/EXTENSIONS.md`): **compiled-in extensions** (Tier 1a, `ferrofin-extensions`) and
  **sandboxed WASM plugins** (Tier 1b, `ferrofin-wasm` — drop a `ferrofin:plugin` component
  into `{data_dir}/plugins/` and restart; no fs/network, capability-gated host API, enforced
  memory/time limits). WASM plugins also install from a **configured plugin repository
  over HTTP** (`POST /Packages/Installed/{name}`, admin-only): the server downloads
  (HTTPS-required, size-capped), verifies the checksum, validates the component + identity,
  stages the file, and flags restart-required — Jellyfin's exact flow; uninstall of a
  staged WASM plugin is real too. When someone asks for a plugin, answer by tier:
  deep-hook ports go in `ferrofin-extensions` (PR), self-contained sources can be WASM;
  never invent a third mechanism. `.wasm` artifacts are NEVER committed (repo-wide
  gitignore); test fixtures are inline WAT, and the conformance guest
  `examples/wasm-hello/` (a workspace-excluded toolchain island that binds the server's
  WIT and implements every export) is built from source by CI
  (`FERROFIN_WASM_GUEST_TESTS=1`). Plugin authors start from
  `github.com/mangoleaf/ferrofin-plugin-template`, never from the example.
- **DLNA server discovery (SSDP)** — Ferrofin has the profile/StreamBuilder logic but no
  SSDP broadcast/discovery.
- **No artwork is written into a media library without the library's opt-in** — an owner
  decision (2026-10-17). Upstream's `IsSaveLocalMetadataEnabled()` overrides (Genre,
  MusicGenre, Playlist, Person, Studio, Year, BoxSet, …) save artwork "beside the item"
  whatever the library says. Ferrofin ports them (Genre, MusicGenre, Playlist, Person,
  Studio, Year, BoxSet, a by-name MusicArtist and a library's CollectionFolder; not yet
  UserView) only while the item's own path is in its data or metadata directory or the
  libraries' root folder (a genre's `{metadata}/Genre/<name>`, a playlist in
  `{data}/playlists`, a library's `root/default/<name>`). An item inside a media library
  (an `.m3u` playlist file, a playlist or box-set folder) follows the library's "Save artwork
  into media folders" (`SaveLocalMetadata`), so with it off the media folders stay
  untouched and need no write permission.
- **Hardware transcoding is NVENC, VAAPI and QSV only.** AMF (AMD on Windows),
  VideoToolbox (macOS), RKMPP (Rockchip) and V4L2M2M are **not supported** — an owner
  decision (2026-08-26), not an un-ported route: their filter chains cannot be verified
  without the hardware to run them on, and an unverified hardware pipeline is how you
  ship silent green frames or a wedged GPU. Selecting one of them is safe — the job
  falls back to a full software transcode — and the server logs a warning naming the
  supported set, so an idle GPU is never a silent mystery. Supporting one means porting
  its chain **and** its `GetEncoderParam`/`GetVideoBitrateParam` arms, verifying on real
  hardware, and adding it to `hardware_path_is_ported`.
- **OMDb** uses Jellyfin's shared key by default; `FERROFIN_OMDB_KEY` / config
  `omdb_api_key` optionally overrides it. Remote providers use built-in keys where
  needed and are gated per library by "Metadata downloaders" / "Image fetchers".
  OpenSubtitles also has a shared application key; downloads still require the
  user's account credentials, and its `ApiKey` setting is an optional override.

The design invariant still holds for any **future** route added to the contract: every path
is registered, an un-ported one returns `501` (never `404`), and the pattern is — write the
handler in `ferrofin-api/src/handlers/<controller>.rs` calling the `AppState` managers, add it
to `handlers::REAL_ROUTES` and `handlers::register`, implement the backing method in
`ferrofin-traits` + `ferrofin-core`, keep the contract-superset test green. Never fake a
deferred subsystem to make a route "pass."
