# Plan 7 — End the SQLite pool contention: measure, resize, and (if proven) split

## Problem
Under the benchmark's mixed 50-VU load, Ferrofin's latency is dominated by
**connection-acquisition queueing**, not query work, and C# Jellyfin beats Rust
Ferrofin on throughput. Direct evidence: `/Items/Counts` is 61 ms isolated (a single
query) but was 2,342 ms under load — ~2.28 s of that was waiting for one of the
pool's 4 permits. Every request makes N queries and re-enters the FIFO queue N
times; cheap 2 ms queries wait behind 61 ms ones (head-of-line blocking); ~46 of 50
VUs are parked on the semaphore at any instant.

Current implementation (`crates/ferrofin-db/src/database.rs`):
- One shared read+write `SqlitePool`, WAL + `synchronous=NORMAL`,
  `busy_timeout=30s` (lines 36-44).
- Pool size = `min(available_parallelism, cgroup CFS quota)`, fallback 4
  (`default_pool_size`, lines 120-129); `FERROFIN_DB_POOL` env overrides (line 82).
- Every call site acquires per query via `db.pool()` (~41 files).

The size-equals-cores rationale (comment at lines 66-80) came from a **Phase-B
single-endpoint saturation test** — a different regime. It optimizes CPU-bound
*throughput* on one hot query; the mixed-load pain is *latency* from queueing. At
fixed CPU, more connections don't add compute, but they let the OS scheduler
time-slice fairly across in-flight queries instead of FIFO-convoying them behind
the app-level semaphore. Jellyfin's effectively-unbounded ADO.NET pool is exactly
why it doesn't exhibit this cliff.

**Prior-evidence trap (why "once and for all"):** an earlier conclusion that
"pool=4 is optimal, 32 is worse" was drawn from the single-endpoint regime and
does not transfer — but it keeps getting re-cited. This plan's job is to settle
the question with the *right* experiment, script it so it's repeatable, and encode
the regime distinction in the code comments so no future agent re-litigates it
from the wrong data.

## Important context
- Plans 1–2 + the counts/filters fix (commits `cefe2f8`, `99de26f`, `cf17096`)
  already removed the multi-second queries that made the convoy catastrophic. The
  contention floor is now much lower — **all measurements must be re-baselined on
  current HEAD before concluding anything.**
- Benchmark harness: `benchmark/run-phase-b.sh` (`PHASE_B_ENDPOINTS` env selects
  endpoints), containers pinned to 4 CPUs, ports 18096/18097, servers measured
  one-at-a-time. Gotchas: no legacy auth headers; never reuse a DeviceId for
  probes during a run.
- On the 50-VU question: 50 zero-think-time VUs is not a realistic home-server
  load (that's ~5–10 clients with think time), but it is a legitimate stress
  signal — and the goal stands regardless: **Ferrofin must beat Jellyfin under the
  same conditions.** Fix the contention, then also add a realistic-load scenario
  (step 5) so future numbers aren't read through an unrealistic lens.

## Step 1 — The decisive experiment: a scripted pool sweep
Write `benchmark/pool-sweep.sh` (reuses `run-phase-b.sh` internals):

1. Build the current HEAD image once.
2. For `FERROFIN_DB_POOL` in `4 8 16 32 64` (the env override already plumbs
   through — verify it reaches the container via docker-compose env, add it if
   not): run the **mixed** endpoint set (the one that exposed the convoy:
   `items_counts items_filters2 user_me sessions studios items_mixed
   item_detail suggestions`) at 50 VUs, Ferrofin only.
3. Emit one table: pool size × {p50, p95, p99, rps} per endpoint + aggregate.
   Persist to `benchmark/results/pool-sweep-<sha>.json`.
4. Run the winner's config against Jellyfin side-by-side (full phase-b) for the
   headline comparison.

Also capture once at pool=4 and once at the winner: `PRAGMA busy_timeout` hits /
`SQLITE_BUSY` counts if observable, and (cheap, high-value) sqlx's pool metrics
(`pool.size()`, `pool.num_idle()`) sampled during the run via a debug endpoint or
log line — this confirms the queue-wait diagnosis directly rather than inferring
it.

## Step 2 — Fix the default (code change, driven by step 1's curve)
In `default_pool_size` (`database.rs:120`):
- Decouple the mixed-load pool size from core count: the expected outcome is a
  default like `clamp(cores × K, cores, CAP)` with K≈4–8 chosen from the sweep
  knee. Keep `FERROFIN_DB_POOL` as the override.
- Rewrite the lines 66-80 comment block to document **both regimes** (single-hot-
  query saturation vs mixed-load queueing), cite the sweep script as the way to
  re-derive the number, and state why size≠cores is correct for latency.
- **Decided (repo owner, 2026-08-03): the pool size becomes a `config.toml`
  setting.** Add `[database] pool_size` to the config (`ferrofin-common` config
  structs + the server's config plumbing), defaulting to the string/variant
  `auto`, where `auto` = the formula chosen from the sweep. An explicit integer
  overrides. Precedence: `FERROFIN_DB_POOL` env > config value > `auto` — the
  same env-over-file order as every other Ferrofin knob (an earlier draft of this
  plan inverted it; consistency won). Document the setting where
  the other config keys are documented, and cover parse + precedence with a
  test. The multiplier/cap behind `auto` stay named consts with the sweep
  evidence in their doc comments.

## Step 3 — Writer/reader split (do it — it's cheap insurance and the standard
SQLite pattern)
Writes currently share the pool: a write holds a permit for its duration and,
under WAL, a blocked writer sits on its connection while readers churn. Split:
- `Database` gains a dedicated **single-connection write pool**
  (`max_connections(1)`) and the existing pool becomes read-oriented. SQLite
  allows exactly one writer at a time anyway — a size-1 write pool turns
  `SQLITE_BUSY`/`busy_timeout` collisions into orderly async queueing at the app
  layer.
- API: add `Database::writer()` returning the write pool; `pool()` keeps working
  (reads). Migrate **write call sites only** (`execute`-shaped: INSERT/UPDATE/
  DELETE and the transaction begins in write paths) to `writer()` — grep for
  `.execute(` and `begin()` in ferrofin-core/ferrofin-db/ferrofin-livetv; the SQL
  boundary ratchet's file list (`crates/ferrofin-db/tests/sql_boundary.rs`) is a
  ready worklist. Transactions that read-then-write use the writer connection.
- In-memory test DB (`connect_in_memory`, line 55) must keep a single shared
  connection semantics — give it writer==reader (same 1-connection pool) so
  tests see one database. Watch for test flakiness from writes on one connection
  and reads on another with WAL: file-backed test DBs are fine; pure in-memory
  needs the shared handle.
- Set `busy_timeout` lower on the reader pool (readers under WAL shouldn't ever
  wait 30 s) — keep 30 s only on the writer.

## Step 4 — Per-request connection reuse: **deferred, decision gated on data**
Holding one connection per request (unit-of-work) would cut the N-queue-entries-
per-request multiplier, but it means threading an executor through every trait
method — a large, invasive change. Only consider it if, **after steps 2–3**, the
mixed-load p50 still trails Jellyfin and pool metrics still show meaningful
acquire-wait. Record the decision either way in the plan's closing report.

## Step 5 — Realistic-load scenario (small, additive)
Add a `phase-d` (or a `LOAD_PROFILE=realistic` mode): 8 VUs with 1–3 s think
time, a session mix that resembles a real client (home screen → library page →
item detail → image fetches → playback start), run for both servers. Headline
stays 50-VU stress; this becomes the "what users feel" number. Feed both into
the perf-gate baseline (Plan 4) once merged.

## Verification
- Standard gates: fmt, clippy `-D warnings`, `cargo nextest run --workspace`,
  doctests, coverage ≥80% on ferrofin-db (the split adds logic — test writer
  serialization: two concurrent writes both land; a write during a long read
  doesn't error).
- The sweep table committed with the change (in the commit message or
  `benchmark/results/`), showing the chosen default's column beating pool=4 on
  mixed-load p50/rps and not regressing single-endpoint saturation throughput
  (re-run one Phase-B hot endpoint at the new default to prove the old regime
  didn't regress).
- Headline: full phase-b vs Jellyfin at the new default — the win/loss table.
  Target: Ferrofin ≥1× Jellyfin p50 on every previously-losing mixed-load
  endpoint, and the 48/83-slower-than-Jellyfin count from v0.8.2 substantially
  reduced. Report the exact numbers.
- No write-path regressions: livetv guide sync test, playstate tests, and the
  scan tests all green (they exercise the writer path).

## Constraints
- Never create/switch branches; no AI-attribution trailers; tests in
  domain-named files; `///` docs on all new pub items; runtime sqlx only.
- Respect the SQL boundary ratchet: new SQL only in exempt files, or lower
  ceilings in-commit.
- Jellyfin DB drop-in is a release blocker: the split must not change schema,
  journal mode, or any on-disk property of the database file.

## Conflicts
Touches `ferrofin-db` (all plans read it, none still in flight modify it) and
write call sites across ferrofin-core/ferrofin-livetv — coordinate with Plan 3 (DTO
pass) if run concurrently; prefer landing Plan 3 first or accepting rebase pain
in dto_service.rs only. `pool-sweep.sh` is additive alongside Plan 4/6 harness
work; if Plan 6's `suite/` restructure lands first, put the sweep there instead.
