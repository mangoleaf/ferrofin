# Plan 4 — Perf regression gate for the agent loop / CI

## Problem
The parity loop's only reward signal is body-diff correctness. Commit `8451f23` landed
a **100× latency regression** (studios p50 191 ms → 19,152 ms) invisibly because
nothing in the loop measures latency; the full benchmark only runs per release. The
median speedup vs Jellyfin fell 4.4× → 0.8× across releases while every individual
commit was "green." This is the highest-leverage fix in the whole perf effort: it
converts slowdowns from a trend someone notices into a gate agents can't slip past.

## What exists already (reuse it, don't rebuild)
- `benchmark/run-phase-b.sh` — runs a configurable endpoint subset via
  `PHASE_B_ENDPOINTS` env var (default already includes the right sentinels:
  `info_public user_me items_sortname items_mixed item_detail persons studios
  suggestions movie_recommendations items_filters2 image_primary`).
- `benchmark/docker-compose.yml`, `Dockerfile.hermit`, `gen-fixtures.sh` — the
  containerized Hermit+Jellyfin pair with a deterministic 2,637-item fixture library.
- `benchmark/bench-data.json` — historical per-version results (p50/p95/p99/speedup
  per endpoint) served at :8124; use it to seed the baseline.
- Harness gotchas (hard-won, do not rediscover): ports 18096/18097; no legacy auth
  headers; don't send mid-run probes reusing a DeviceId (it perturbs sessions).

## Deliverable
A `benchmark/perf-gate.sh` that:

1. Builds the current working tree into the benchmark image and runs phase-b on the
   sentinel endpoints, **Hermit only** (skip the Jellyfin side for speed — the gate
   compares Hermit to its own baseline, not to Jellyfin). Reduced load is fine
   (e.g. 10 VUs × 10 s/endpoint) as long as the baseline is captured under the
   *same* parameters.
2. Compares each endpoint's **p50, p95, AND p99** to
   `benchmark/perf-baseline.json` and **fails (exit 1)** if any endpoint exceeds
   `1.5×` its baseline on **any of the three percentiles**, printing a
   before/after table with all three columns. Also fail if any endpoint's
   200-rate < 100%.

   **This is a hard requirement from the repo owner (2026-08-03), not a
   default to simplify away:** past benchmarks called an endpoint "faster"
   on p50 while its p99 was 2× worse than Jellyfin's — median-only gating
   hides tail regressions, and tail latency is what users feel as stutter.
   The baseline file stores all three percentiles per endpoint, the failure
   report names which percentile(s) tripped, and any "Hermit vs Jellyfin"
   summary the gate prints must show per-percentile comparisons (an endpoint
   only counts as a "win" if it wins on p50, p95, and p99; a p50 win with a
   tail loss is reported as a tail loss).
3. Has a `--rebaseline` flag that reruns and rewrites `perf-baseline.json`
   (used at release time, and once now to create the initial baseline from the
   current HEAD — *after* Plans 1–3 land, so the baseline isn't the regressed state).
4. Total runtime target: under ~5 minutes, so it can run per loop iteration.

Then wire it in:
- Document in `benchmark/README.md` and in `CLAUDE.md` under quality gates: perf-gate
  must pass for any change touching `hermit-core`, `hermit-db`, `hermit-api`, or
  `translate_query`/repository/dto code.
- If a CI workflow exists (`.github/workflows/`), add it as a job gated on those
  paths; if CI has no Docker/ffmpeg, make the job best-effort but keep the local gate
  documented as mandatory for the loop agent.

## Design notes
- The threshold (1.5×) and load (10 VUs × 10 s) are knobs — put them at the top of the
  script as env-overridable vars (`PERF_GATE_FACTOR`, `PERF_GATE_VUS`,
  `PERF_GATE_SECONDS`), defaults as above. Ask the repo owner before changing
  defaults; the numbers were chosen as "loose enough to ignore noise, tight enough to
  catch a 2× regression".
- Percentiles under short runs are noisy — p99 especially (at 10 VUs × 10 s an
  endpoint sees ~hundreds of samples, so p99 rides on a handful of requests).
  Mitigations, in order: retry-once-on-fail (re-run and require the failure to
  reproduce before declaring a regression); if p99 alone still flaps, bump that
  endpoint's sample count (longer window) rather than loosening the p99 factor.
  Do NOT drop p99 from the gate to fix flakiness — see the hard requirement in
  step 2.
- Keep it plain bash + node (the harness already uses both); no new dependencies.

## Verification
- Run the gate twice on clean HEAD: must pass both times (no flaky failures).
- Sanity-check the detector: `git stash` a deliberate slowdown (e.g. add
  `std::thread::sleep(50ms)` into the studios path), run the gate, confirm it fails,
  unstash. Do not commit the slowdown.
- `shellcheck benchmark/perf-gate.sh` clean; `run.bats` still passes if it covers the
  scripts.

## Constraints
- Never create/switch branches; no AI-attribution trailers.
- Don't modify `run.sh`/phase scripts' existing behavior — add alongside.

## Conflicts
None with Plans 1–3/5 (different files). Create the initial baseline only after
Plans 1–3 are merged.
