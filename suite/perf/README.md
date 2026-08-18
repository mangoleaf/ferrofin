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
every other row. The write rows dodge that by construction (rules enforced in `endpoints.py`):

- **State writes target the LAST movie by SortName** (`ctx.writeItemId`), never the first
  (`ctx.itemId`) that all the read rows key on — write traffic can't drift a read row's body.
- **Bodies are fixed and state-preserving** (position 0, unplayed defaults): every request
  re-asserts the same state, so the row measures the steady-state upsert, and the library
  looks the same after the run as before it.
- **`auth_login` runs in its own window after the main legs drain** (PBKDF2
  saturates CPU and each login invalidates the server-side auth cache — in-loop it would
  poison every other row). Knobs: `BENCH_LOGIN_RATE` (default 10/s — a wide storm on
  4 cores would measure pure CPU queueing) × `BENCH_LOGIN_DURATION_SECS` (default 15).
  Every request carries a fresh DeviceId (as target data): reusing the main `bench`
  DeviceId would revoke the measurement token.
- **Success for a write row is its contract status** (204 for playstate progress), not 200.
- In the merged suite record they are **fingerprint-exempt**: `suite/merge.py` gates them on
  the parity **write journey** (`deep_verified`) + 100% expected-status instead of the
  body-shape fingerprint (which a probe would itself mutate state to capture).

Plus **footprint**, which is a bigger Rust-vs-.NET story than percentiles:

- **Cold start** — container launch → first `200 /System/Info/Public`
- **Peak RSS** under load

### Warm and cold, side by side (never blended)

The headline latency is **steady state**, via two-stage warmup identical on both
servers: one global pass cycling every endpoint after bring-up
(`BENCH_GLOBAL_WARMUP_SECS` — .NET tier-1 promotion is per-method and mostly shared
code, so promoting it once beats paying a long warmup 117 times), then a short
same-endpoint top-up at the measured rate before each window (`BENCH_WARMUP_SECS`).
The comparison is never Rust-vs-quick-JIT. **Cold** is a real user experience too (server
restart, first browse) and a legitimate Rust advantage, so it's published as its own
labeled metric: after the warm legs, the server is **restarted before each sentinel
endpoint** (hitting one endpoint warms shared state for the next) and the first
`BENCH_COLD_REQUESTS` requests are timed individually (`cold_probe.py`) — the first
request is the number, the rest show the warm-up curve. The record carries `warm`
percentiles and a `cold` block per sentinel; the regression gate runs on warm, and
cold gates only cold-vs-cold on the same gross factor.

And, optional/experimental (`RUN_TRANSCODE=1`): **time-to-first-HLS-segment**. Low signal —
both servers call the *same ffmpeg*, so this measures only the pipeline/playlist overhead
before ffmpeg output, not transcode throughput.

## What it deliberately does *not* measure

- **Sustained transcode throughput** — that's ffmpeg, identical on both. Benchmarking it would
  compare ffmpeg to itself.
- **Web UI** — Ferrofin ships no UI; there's nothing to compare.

## Load model: open loop (this is the part most benchmarks get wrong)

The per-endpoint comparison windows are driven by **vegeta** at a **constant arrival
rate** (open loop), not by a fixed pool of virtual users (closed loop). The difference
decides whether the numbers mean anything:

- A closed loop **coordinates with the server under test** ("coordinated omission"):
  when the server stalls, the generator politely stops sending, so the stall is
  recorded once instead of hitting every request that *would* have arrived. Tail
  percentiles come out flattering and unstable.
- A closed loop also makes throughput **self-regulating** — the faster server
  automatically receives more load, so the two servers are never measured under the
  same workload. An open loop fixes the workload and lets latency be the signal.

Mechanics (all enforced, not advisory):

- **Rates are per endpoint and recorded.** `compare.py --calibrate-rates` (run against
  Jellyfin — the weaker side) measures each endpoint's max throughput and writes
  `rates.json` = `BENCH_RATE_FRACTION` (default 0.5) of it. Both servers are then
  driven at the *same* recorded rate. Without a calibrated entry the flat `BENCH_RATE`
  applies and the record says so (`rate_source: flat-default`). Re-calibrate on
  rebaseline (host/fixture-local, like the baseline itself).
- **Windows are sample-count-based.** Percentile precision scales with samples, not
  wall time: each endpoint's measured window is
  `clamp(BENCH_MIN_SAMPLES/rate, BENCH_MIN_WINDOW_SECS, BENCH_DURATION_SECS)` —
  identical on both servers (it derives only from the shared rate) and recorded per
  row. A flat 30 s window at calibrated rates collected 10-100× more samples than the
  tails need, ×118 endpoints ×2 servers ×N runs; this is what keeps a full run in
  hours, not days. Publish runs ≥2 also reuse the scanned volume (`BENCH_KEEP_DATA` —
  DB state is identical by construction; only measurement noise needs independence).
- **A window that can't hold its rate fails.** If the achieved rate falls below
  `BENCH_RATE_TOLERANCE` × target, the generator has silently degraded into a closed
  loop — the leg exits non-zero and `merge.py` marks the row incomparable.
- **The generator proves it isn't the bottleneck.** Every run measures the max
  `/System/Ping` throughput on the server under test — `meta.ping_ceiling_rps`, i.e.
  min(generator capacity, that server's ping capacity), a conservative lower bound on
  what the generator can dispatch — and any target rate at/above it fails loud.
- **Two legs stay closed-loop on purpose** and say so: phase C (mixed contention — the
  interference is the point) and phase D (think-time user journeys — a home media
  server has a fixed small user population, which is exactly the case where the
  closed model matches reality).
- **Isolated vs loaded is structural now** (F3): the comparison legs measure each
  endpoint in isolation, so cheap-endpoint tails can no longer be inflated by another
  endpoint's pool queueing and mistaken for intrinsic cost; phase C exists precisely
  to measure that interference, separately and labeled.
- **Leg order alternates across the publish runs** (F1): F-then-J on odd runs,
  J-then-F on even, so slow host drift cancels in the aggregate instead of always
  taxing the second leg.

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

Prereqs on the host: `docker` + `docker compose`, `jq`, `python3`, `ffmpeg` (only for
fixture generation), and the pinned tools from `mise.toml` (`cd suite/perf && mise install`
— vegeta, bats). Then:

```bash
cd suite/perf
cp .env.example .env
#   set REAL_MEDIA_DIR to your movies dir (absolute path)
#   set JELLYFIN_IMAGE to match your vendored spec version
./run.sh
```

Methodology knobs (rates, durations, warmup, noise floor, gate thresholds) live in the
committed `suite/bench.conf` — edit there, or override per-run via env. First-time setup
worth doing once per host/fixture: calibrate the per-endpoint arrival rates against the
weaker server (see "Load model" above).

With a small real library (e.g. 20 movies) the query endpoints are valid but won't show
row-count *scaling*. If you want that too, set `FIXTURE_MOVIES=500` / `FIXTURE_SERIES=50` in
`.env` to pad with synthetic items — your real files still drive the transcode test.

`run.sh` generates fixtures (first run only), builds the Ferrofin image, benches both servers, and
writes `results/<ferrofin-version>.md` + `results/latest.md`.

Every knob is in `.env`: fixture size, VU count, load duration, resource caps, Jellyfin pin.

> **Match the Jellyfin version to your vendored OpenAPI spec** (`contracts/jellyfin-openapi-*.json`).
> Comparing against a different Jellyfin version compares against a different API contract.

## Measurement integrity (fail-loud rules)

Two classes of silently-wrong runs happened once each and are now structurally impossible:

- **Stale binary (B1).** The harness passes the host's `git describe` into the image build
  (`GIT_DESCRIBE` build arg → baked into the binary by `ferrofin-health`'s `build.rs`) and,
  after cold start, reads it back from `GET /health/live` — a mismatch aborts the run before
  any measurement. Note: `docker build --no-cache` does **not** clear `RUN --mount=type=cache`
  mounts (the compile cache lives there); `suite/run.sh all` prunes them for shareable
  records (`BENCH_KEEP_CACHE=1` to opt out on a slow host).
- **Missing legs (A1).** `suite/merge.py` treats `suite/registry.json` as a manifest: every
  bench variant must have produced latencies on both servers (and the TTFS legs their
  footprint blocks when `RUN_TRANSCODE=1`), or the merge exits non-zero and writes no
  record. Deliberate omissions are declared (`SKIP_VARIANTS=name1,name2`) and stamped into
  the record; `MERGE_ALLOW_INCOMPLETE=1` writes a `run-<sha>-incomplete.json` for
  inspection that never enters the trend file.

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

| Setting (bench.conf / env) | Default | Meaning |
|---|---|---|
| `PERF_GATE_FACTOR` | `1.5` | fail if any percentile exceeds this × baseline |
| `PERF_GATE_RATE` | `25` | open-loop arrival rate per endpoint (req/s) |
| `PERF_GATE_SECONDS` | `10` | measured window per endpoint |
| `PERF_GATE_ENDPOINTS` | 11 sentinels | endpoint ids (from `endpoints.py`) to gate |

`perf-baseline.json` stores all three percentiles per endpoint plus the params it was
captured under. **Re-`--rebaseline` at each release** (and after any intended perf change
— e.g. a DB pool-size change), so the baseline tracks intended improvements and only
*unintended* slowdowns trip the gate. It reads `.env` like the other scripts, so capture
the baseline and gate on the same host/fixture.

> Runs in ~5 min, so the loop agent runs it per iteration on any change touching
> `ferrofin-core`, `ferrofin-db`, `ferrofin-api`, or the query/repository/DTO paths. See the
> quality-gates section of the root `CLAUDE.md`.

## Profiling the server (chasing a benchmark row down to a cause)

When a row here says an endpoint is slow, the profiling loop is: reproduce the
row's load against a **native** build (never inside Docker), then walk down —
spans → SQL → flamegraph. The suite's own drivers are the load harness.

**One-time host setup:**

```bash
# The profiling build profile is in the workspace Cargo.toml already:
cargo build --profile profiling -p ferrofin-server   # release speed + debug symbols

cargo install samply          # sampling profiler (Firefox Profiler UI)

# Sampling profilers use the kernel's perf facility; the default
# kernel.perf_event_paranoid=2 blocks samply. Level 1 = "profile your OWN
# processes, incl. kernel-side stacks" — the standard dev-box setting. It is
# machine-wide, so set it on dev/bench hosts only, never shared/prod machines.
echo 1 | sudo tee /proc/sys/kernel/perf_event_paranoid                  # until reboot
# samply maps a perf ring buffer PER THREAD; a tokio server runs dozens of
# threads (~70 observed), which blows the default 516 KiB unprivileged mlock
# budget ("Failed to start profiling: mmap failed"). 8 MiB gives headroom.
echo 8192 | sudo tee /proc/sys/kernel/perf_event_mlock_kb
# persistent for both:
printf 'kernel.perf_event_paranoid = 1\nkernel.perf_event_mlock_kb = 8192\n' \
  | sudo tee /etc/sysctl.d/99-perf.conf

sudo pacman -S heaptrack      # heap profiling (memory workstreams only)
```

**Per-investigation loop:**

```bash
# 1. A scanned DB: copy the bench volume once (or scan fresh locally).
docker run --rm -v ferrofin-benchv2_ferrofin-data:/data \
  -v /tmp/ferrofin-profile:/out alpine \
  sh -c "cp -r /data /out/ && chown -R $(id -u):$(id -g) /out/data"

# 2. Native server on it, profiling build.
FERROFIN_ADMIN_USER=bench FERROFIN_ADMIN_PASSWORD=benchpass123 \
  ./target/profiling/ferrofin-server --data-dir /tmp/ferrofin-profile/data \
  --bind 127.0.0.1 --port 18496

# 3. Mint a run context once (from suite/perf/):
python3 - <<'EOF'
import json, pathlib, sys; sys.path.insert(0, ".")
import benchlib
base = "http://127.0.0.1:18496"
ctx = benchlib.authenticate(base, "ferrofin")
ctx.update({"username": "bench", "password": "benchpass123"})
benchlib.pick_items(base, ctx); benchlib.enrich_context(base, ctx)
pathlib.Path("results/raw/ferrofin-ctx.json").write_text(json.dumps(ctx))
EOF

# 4. Reproduce the row: same endpoint, same rate the record shows.
python3 phase_a.py --target ferrofin --base http://127.0.0.1:18496 \
  -e items_resume --rate 464 --dur 20 --warmup 5 --out /tmp/repro.json

# 5. Profile during the load (writes a profile you open in profiler.firefox.com):
samply record -p "$(pgrep -f 'profiling/ferrofin-server')" --duration 15 \
  --save-only -o /tmp/profile.json.gz
```

**Reading it:** a big flamegraph tower = CPU-bound, optimize the frames. CPU
flat but latency huge = the request is *waiting* (async off-CPU) — check the
tracing spans (`OTEL_EXPORTER_OTLP_ENDPOINT` → Tempo), then
`RUST_LOG=sqlx::query=debug` to see each SQL statement with its duration, then
`EXPLAIN QUERY PLAN` in `sqlite3` against the copied DB (full scans and temp
b-trees on hot paths are the usual culprits). Verify every fix at the top:
`./perf-gate.sh`, then the endpoint's row in a fresh leg.

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
          curl -sL https://github.com/tsenart/vegeta/releases/download/v12.12.0/vegeta_12.12.0_linux_amd64.tar.gz | tar -xz -C /usr/local/bin vegeta
      - run: cd suite/perf && ./run.sh
      - uses: actions/upload-artifact@v4
        with: { name: benchmark, path: suite/perf/results/latest.md }
```

## Files

| File | Role |
|---|---|
| `docker-compose.yml` | both servers, equal caps, shared fixture volume |
| `Dockerfile.ferrofin` | release build of the Ferrofin server + ffmpeg (+ baked build identity) |
| `gen-fixtures.sh` | build the identical media library |
| `../bench.conf` | every methodology knob, committed, at its default |
| `config.py` | knob resolution: code default < bench.conf < env; recorded into meta |
| `endpoints.py` | THE endpoint table (name/path/method/ok/body) — registry + all legs import it |
| `benchlib.py` | bring-up: wizard → auth → provision → scan-wait → item pick → enrichment |
| `vegeta.py` | the load-engine seam: targets, open-loop attack, decode, summarize |
| `compare.py` | the comparison leg: per-endpoint open-loop windows (+ `--calibrate-rates`) |
| `rates.json` | calibrated per-endpoint arrival rates (committed; re-derive on rebaseline) |
| `bootstrap.py` | one-shot bring-up for the phase legs; persists `results/raw/<t>-ctx.json` |
| `ttfs.py` | experimental time-to-first-segment journey (opt-in, `RUN_TRANSCODE=1`) |
| `phase_a.py` / `run-phase-a.sh` | isolated per-endpoint open-loop profiling (+ CPU/req) |
| `run-phase-b.sh` | saturation ladder over `phase_a.py` (max sustained rate per endpoint) |
| `phase_c.py` / `run-phase-c.sh` | mixed contention (deliberately closed-loop) + memory |
| `phase_d.py` / `run-phase-d.sh` | think-time user journeys (deliberately closed-loop) |
| `pool_sweep.py` / `pool-sweep.sh` | DB pool-size sweep over the mixed load |
| `render_phases.py` / `render_closed.py` | report renderers (markdown/JSON) |
| `run.sh` | orchestrate both servers, verify build identity, capture footprint, render |
| `perf-gate.sh` / `perf_gate.py` | fast Ferrofin-only regression gate vs `../perf-baseline.json` |
| `mise.toml` | pinned tools: vegeta (the engine), bats (script tests) |
| `.env.example` | host identity (media dirs, image pin, caps, creds) |
