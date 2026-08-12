# Ferrofin vs Jellyfin benchmark

Ferrofin is a Rust port of the Jellyfin server that speaks the **identical HTTP API**. That's
the whole reason this benchmark is simple: point one request driver at each server, feed both
the **same media library** and the **same request sequence**, and compare. No per-server client
code — the only thing that differs is the layer we reimplemented (routing, DB access, DTO
serialization), which is exactly what we want to measure.

Reports render into `results/` (gitignored); keep the ones worth publishing in
`suite/results/` (the committed run records are the trend history).

## What it measures

**Per-endpoint latency + throughput** (the read-heavy, serialization-heavy surface where Rust
vs C# actually differs):

| Endpoint | Why it's here |
|---|---|
| `GET /System/Info/Public` | baseline floor — near-zero work, isolates framework overhead |
| `GET /UserViews` | home-screen view assembly |
| `GET /Items` (Movie, SortName) | the library query + DTO hot path |
| `GET /Items` (Movie, DateCreated desc) | the query planner under a different sort |
| `GET /Items` (Episode) | episode-shaped DTOs + the Series/Season/Episode resolver (needs `REAL_TV_DIR`) |
| `GET /Items/{id}` | single-item DTO build |
| `GET /Items/{id}/Images/Primary` | image serve + resize (`ferrofin-drawing`) — see caveat |
| `POST /Items/{id}/PlaybackInfo` | the hottest real-client POST: play-decision + MediaSource build |
| `POST /Sessions/Playing/Progress` | the media-server write path — clients report every ~10 s |
| `POST /UserItems/{id}/UserData` | idempotent user-data upsert |
| `POST /Users/AuthenticateByName` | login storm: PBKDF2 + token mint + the SQLite single-writer |

### Write rows (the POST entries above)

Writes in a 30 s constant load would normally mutate the fixture mid-measurement and corrupt
every other row. The write rows dodge that by construction (rules enforced in `bench-lib.js`):

- **State writes target the LAST movie by SortName** (`ctx.writeItemId`), never the first
  (`ctx.itemId`) that all the read rows key on — write traffic can't drift a read row's body.
- **Bodies are fixed and state-preserving** (position 0, unplayed defaults): every request
  re-asserts the same state, so the row measures the steady-state upsert, and the library
  looks the same after the run as before it.
- **`auth_login` runs in its own scenario window after the mixed loop drains** (PBKDF2
  saturates CPU and each login invalidates the server-side auth cache — in-loop it would
  poison every other row). Knobs: `BENCH_LOGIN_VUS` (default 10 — 50 concurrent PBKDF2s on
  4 cores would measure pure CPU queueing) × `BENCH_LOGIN_DURATION` (default 15s). It uses a
  per-VU DeviceId: reusing the main `bench` DeviceId would revoke the measurement token.
- **Success for a write row is its contract status** (204 for playstate progress), not 200.
- In the merged suite record they are **fingerprint-exempt**: `suite/merge.py` gates them on
  the parity **write journey** (`deep_verified`) + 100% expected-status instead of the
  body-shape fingerprint (which a probe would itself mutate state to capture).

Plus **footprint**, which is a bigger Rust-vs-.NET story than percentiles:

- **Cold start** — container launch → first `200 /System/Info/Public`
- **Peak RSS** under load

And, optional/experimental (`RUN_TRANSCODE=1`): **time-to-first-HLS-segment**. Low signal —
both servers call the *same ffmpeg*, so this measures only the pipeline/playlist overhead
before ffmpeg output, not transcode throughput.

## What it deliberately does *not* measure

- **Sustained transcode throughput** — that's ffmpeg, identical on both. Benchmarking it would
  compare ffmpeg to itself.
- **Web UI** — Ferrofin ships no UI; there's nothing to compare.

## Fairness (this is where benchmarks lie, so it's most of the harness)

1. **Identical media, mounted read-only into both.** Point `REAL_MEDIA_DIR` at your own movies
   (recommended — real files mean real probing and a meaningful transcode test). Both servers
   scan the exact same directory. For query-scaling headroom you can also generate synthetic
   padding (`FIXTURE_MOVIES`/`FIXTURE_SERIES`): `gen-fixtures.sh` hardlinks one tiny real A/V
   clip to every synthetic path so ffprobe behaves identically on both.
2. **Equal item count is asserted.** Scan completion is detected by polling `GET /Items` until
   the total settles — a signal defined purely in terms of the shared API, so it's identical on
   both servers. The report records each server's actual scanned count and **flags a mismatch**:
   real-world filenames can parse differently between Ferrofin's and Jellyfin's naming resolvers,
   and if the counts diverge you're comparing different workloads. (That divergence is itself a
   useful parity signal worth chasing down.)
3. **Equal resource caps.** Both run in Docker with the same `cpus` / `mem_limit` (`.env`).
4. **Sequential, never concurrent.** `run.sh` benches one server at a time on the same host —
   contention would corrupt both.
5. **Warm, then measure.** Full scan + one warmup pass fills caches before the measured load.
6. **Pinned versions.** The Jellyfin image (pin by digest) and Ferrofin git SHA are recorded in
   every report.

### Caveats worth knowing

- **Image endpoint** relies on each server discovering a local `poster.jpg`. If Ferrofin's local
  image discovery differs from Jellyfin's, that row may show a not-found path on one side rather
  than a real resize. It's flagged, not hidden.
- **Cold start** includes container init (same for both), so read it as relative, not absolute.
- **Peak RSS** is sampled at 1s granularity via `docker stats` — good enough for an order-of-
  magnitude comparison, not a precise high-water mark.

## Running it

Prereqs on the host: `docker` + `docker compose`, `k6`, `jq`, and `ffmpeg` (only for fixture
generation). Then:

```bash
cd suite/perf
cp .env.example .env
#   set REAL_MEDIA_DIR to your movies dir (absolute path)
#   set JELLYFIN_IMAGE to match your vendored spec version
./run.sh
```

With a small real library (e.g. 20 movies) the query endpoints are valid but won't show
row-count *scaling*. If you want that too, set `FIXTURE_MOVIES=500` / `FIXTURE_SERIES=50` in
`.env` to pad with synthetic items — your real files still drive the transcode test.

`run.sh` generates fixtures (first run only), builds the Ferrofin image, benches both servers, and
writes `results/<ferrofin-version>.md` + `results/latest.md`.

Every knob is in `.env`: fixture size, VU count, load duration, resource caps, Jellyfin pin.

> **Match the Jellyfin version to your vendored OpenAPI spec** (`contracts/jellyfin-openapi-*.json`).
> Comparing against a different Jellyfin version compares against a different API contract.

## Perf regression gate (`perf-gate.sh`)

`run.sh` is the full release comparison (both servers, every endpoint) and runs per
release. That's too slow to catch a regression the moment an agent introduces one —
and its only in-loop signal is body-diff *correctness*, so a 100× latency regression
can land "green" (this happened: studios p50 191 ms → 19,152 ms, invisibly). The gate
closes that hole.

`perf-gate.sh` builds the **current working tree**, brings up **Ferrofin only** (it
compares Ferrofin to its own past self, not to Jellyfin — half the containers, half the
time), drives the sentinel endpoints at a light fixed load, and **fails (exit 1)** if
any endpoint exceeds `1.5×` its baseline on **p50, p95, *or* p99**, or if its 200-rate
drops below 100%. All three percentiles gate deliberately: median-only gating has hidden
2× p99 tail regressions before, and tail latency is what users feel as stutter.

```bash
cd suite/perf
./perf-gate.sh --rebaseline   # once: capture perf-baseline.json from current HEAD
./perf-gate.sh                # per change: gate the working tree against the baseline
```

Short runs make percentiles (p99 especially) noisy, so a first-round failure is
**re-run once** and must reproduce before the gate fails — a one-off blip passes.

Knobs (env; defaults tuned "loose enough to ignore noise, tight enough to catch a 2×
regression" — **ask the repo owner before changing**):

| Env | Default | Meaning |
|---|---|---|
| `PERF_GATE_FACTOR` | `1.5` | fail if any percentile exceeds this × baseline |
| `PERF_GATE_VUS` | `10` | closed-model VUs per endpoint |
| `PERF_GATE_SECONDS` | `10` | measured window per endpoint |
| `PERF_GATE_ENDPOINTS` | 11 sentinels | endpoint ids (from `bench-lib.js`) to gate |

`perf-baseline.json` stores all three percentiles per endpoint plus the params it was
captured under. **Re-`--rebaseline` at each release** (and after any intended perf change
— e.g. a DB pool-size change), so the baseline tracks intended improvements and only
*unintended* slowdowns trip the gate. It reads `.env` like the other scripts, so capture
the baseline and gate on the same host/fixture.

> Runs in ~5 min, so the loop agent runs it per iteration on any change touching
> `ferrofin-core`, `ferrofin-db`, `ferrofin-api`, or the query/repository/DTO paths. See the
> quality-gates section of the root `CLAUDE.md`.

## Regenerating on every release

`./run.sh` *is* the regeneration command — run it at each Ferrofin release and commit the new
`results/<version>.md`. As the AI-generated handlers get optimized, diffing successive reports
shows the port getting faster over time.

To wire it into CI on a tag, a minimal GitHub Actions job:

```yaml
# .github/workflows/benchmark.yml
on: { push: { tags: ['v*'] } }
jobs:
  bench:
    runs-on: ubuntu-latest      # note: shared runners are noisy; a dedicated runner gives stable numbers
    steps:
      - uses: actions/checkout@v4
      - run: |
          sudo apt-get update && sudo apt-get install -y ffmpeg jq
          curl -sL https://github.com/grafana/k6/releases/latest/download/k6-... -o /usr/local/bin/k6 && chmod +x /usr/local/bin/k6
      - run: cd suite/perf && ./run.sh
      - uses: actions/upload-artifact@v4
        with: { name: benchmark, path: suite/perf/results/latest.md }
```

## Files

| File | Role |
|---|---|
| `docker-compose.yml` | both servers, equal caps, shared fixture volume |
| `Dockerfile.ferrofin` | release build of the Ferrofin server + ffmpeg |
| `gen-fixtures.sh` | build the identical media library |
| `scenario.js` | k6: provision → scan-wait → warm → per-endpoint load |
| `transcode.js` | k6: experimental time-to-first-segment (opt-in) |
| `run.sh` | orchestrate both, capture footprint, render the report |
| `perf-gate.sh` | fast Ferrofin-only regression gate vs `../perf-baseline.json` (p50/p95/p99) |
| `perf-gate.js` | k6: closed-model per-endpoint load for the gate (emits the percentiles) |
| `.env.example` | every tunable, with defaults |
