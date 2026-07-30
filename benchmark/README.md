# Hermit vs Jellyfin benchmark

Hermit is a Rust port of the Jellyfin server that speaks the **identical HTTP API**. That's
the whole reason this benchmark is simple: point one request driver at each server, feed both
the **same media library** and the **same request sequence**, and compare. No per-server client
code — the only thing that differs is the layer we reimplemented (routing, DB access, DTO
serialization), which is exactly what we want to measure.

Reports render into `results/` (gitignored); keep the ones worth publishing in
`brain/benchmarks/` with a date prefix (e.g. `2026-07-30-COMPARISON.md`).

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
| `GET /Items/{id}/Images/Primary` | image serve + resize (`hermit-drawing`) — see caveat |

Plus **footprint**, which is a bigger Rust-vs-.NET story than percentiles:

- **Cold start** — container launch → first `200 /System/Info/Public`
- **Peak RSS** under load

And, optional/experimental (`RUN_TRANSCODE=1`): **time-to-first-HLS-segment**. Low signal —
both servers call the *same ffmpeg*, so this measures only the pipeline/playlist overhead
before ffmpeg output, not transcode throughput.

## What it deliberately does *not* measure

- **Sustained transcode throughput** — that's ffmpeg, identical on both. Benchmarking it would
  compare ffmpeg to itself.
- **Web UI** — Hermit ships no UI; there's nothing to compare.

## Fairness (this is where benchmarks lie, so it's most of the harness)

1. **Identical media, mounted read-only into both.** Point `REAL_MEDIA_DIR` at your own movies
   (recommended — real files mean real probing and a meaningful transcode test). Both servers
   scan the exact same directory. For query-scaling headroom you can also generate synthetic
   padding (`FIXTURE_MOVIES`/`FIXTURE_SERIES`): `gen-fixtures.sh` hardlinks one tiny real A/V
   clip to every synthetic path so ffprobe behaves identically on both.
2. **Equal item count is asserted.** Scan completion is detected by polling `GET /Items` until
   the total settles — a signal defined purely in terms of the shared API, so it's identical on
   both servers. The report records each server's actual scanned count and **flags a mismatch**:
   real-world filenames can parse differently between Hermit's and Jellyfin's naming resolvers,
   and if the counts diverge you're comparing different workloads. (That divergence is itself a
   useful parity signal worth chasing down.)
3. **Equal resource caps.** Both run in Docker with the same `cpus` / `mem_limit` (`.env`).
4. **Sequential, never concurrent.** `run.sh` benches one server at a time on the same host —
   contention would corrupt both.
5. **Warm, then measure.** Full scan + one warmup pass fills caches before the measured load.
6. **Pinned versions.** The Jellyfin image (pin by digest) and Hermit git SHA are recorded in
   every report.

### Caveats worth knowing

- **Image endpoint** relies on each server discovering a local `poster.jpg`. If Hermit's local
  image discovery differs from Jellyfin's, that row may show a not-found path on one side rather
  than a real resize. It's flagged, not hidden.
- **Cold start** includes container init (same for both), so read it as relative, not absolute.
- **Peak RSS** is sampled at 1s granularity via `docker stats` — good enough for an order-of-
  magnitude comparison, not a precise high-water mark.

## Running it

Prereqs on the host: `docker` + `docker compose`, `k6`, `jq`, and `ffmpeg` (only for fixture
generation). Then:

```bash
cd benchmark
cp .env.example .env
#   set REAL_MEDIA_DIR to your movies dir (absolute path)
#   set JELLYFIN_IMAGE to match your vendored spec version
./run.sh
```

With a small real library (e.g. 20 movies) the query endpoints are valid but won't show
row-count *scaling*. If you want that too, set `FIXTURE_MOVIES=500` / `FIXTURE_SERIES=50` in
`.env` to pad with synthetic items — your real files still drive the transcode test.

`run.sh` generates fixtures (first run only), builds the Hermit image, benches both servers, and
writes `results/<hermit-version>.md` + `results/latest.md`.

Every knob is in `.env`: fixture size, VU count, load duration, resource caps, Jellyfin pin.

> **Match the Jellyfin version to your vendored OpenAPI spec** (`contracts/jellyfin-openapi-*.json`).
> Comparing against a different Jellyfin version compares against a different API contract.

## Regenerating on every release

`./run.sh` *is* the regeneration command — run it at each Hermit release and commit the new
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
      - run: cd benchmark && ./run.sh
      - uses: actions/upload-artifact@v4
        with: { name: benchmark, path: benchmark/results/latest.md }
```

## Files

| File | Role |
|---|---|
| `docker-compose.yml` | both servers, equal caps, shared fixture volume |
| `Dockerfile.hermit` | release build of the Hermit server + ffmpeg |
| `gen-fixtures.sh` | build the identical media library |
| `scenario.js` | k6: provision → scan-wait → warm → per-endpoint load |
| `transcode.js` | k6: experimental time-to-first-segment (opt-in) |
| `run.sh` | orchestrate both, capture footprint, render the report |
| `.env.example` | every tunable, with defaults |
