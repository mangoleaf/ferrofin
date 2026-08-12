---
name: run-benchmark
description: >-
  Run the Ferrofin benchmark: the fast per-change perf gate, the full
  Ferrofin-vs-Jellyfin two-leg comparison, or the merged parity+perf suite —
  and read the results honestly. Use when asked to "run the benchmark",
  "bench it", "run the perf gate", "check for a perf regression",
  "compare against Jellyfin", "rebaseline", or "/run-benchmark".
---

# Running the Ferrofin benchmark

Three entry points, escalating in cost. Pick by intent — don't run the full
comparison to answer a regression question.

| Intent | Command | Time | Servers |
|---|---|---|---|
| "Did my change regress perf?" | `cd suite/perf && ./perf-gate.sh` | ~5 min | Ferrofin only (working tree) |
| Full Ferrofin-vs-Jellyfin numbers | `cd suite/perf && ./run.sh` (= `suite/run.sh perf`) | ~30+ min | both, one at a time |
| Release record (parity + perf, merged) | `suite/run.sh all` | ~45+ min | both |
| Re-join existing artifacts, no measurement | `suite/run.sh merge` | seconds | none |

Prereqs on the host: `docker` + `docker compose`, `k6`, `jq` (`ffmpeg` only for
first-time fixture generation). `suite/perf/.env` must exist (`cp .env.example .env`;
`REAL_MEDIA_DIR` set, `JELLYFIN_IMAGE` matching the vendored OpenAPI spec version).

## Hard rules during a run

- **Never compile, run tests, or start anything heavy while a k6 window is
  measuring.** The numbers are host-sensitive; a `cargo build` mid-window
  poisons the percentiles. Start the run, then wait — don't multitask on CPU.
- **Never probe the servers mid-run** (`curl` against ports 18096/18097 while a
  leg is measuring). `suite_guard_no_probe` refuses some of this, but the real
  rule is: hands off until the leg finishes. Reusing a bench DeviceId in a probe
  can revoke the measurement token and zero a row.
- Each perf leg starts from a **fresh DB** (`docker compose down -v`) — don't
  "save time" by reusing state. `BENCH_ONLY=ferrofin|jellyfin ./run.sh` re-runs
  one leg while keeping the other's raw results, and skips the wipe of the
  other leg's summary only.
- The full run **overwrites `suite/perf/results/raw/*.json`** — if the current
  raw summaries matter (e.g. they back an unmerged run record), run
  `suite/run.sh merge` first.

## The perf gate (the one you'll run most)

```bash
cd suite/perf
./perf-gate.sh                # gate the working tree against suite/perf-baseline.json
./perf-gate.sh --rebaseline   # capture a new baseline (see rules below)
```

- Mandatory for any change touching `ferrofin-core`, `ferrofin-db`, `ferrofin-api`,
  or the query/repository/DTO paths (per root `CLAUDE.md`).
- Fails if any sentinel exceeds **1.5× baseline on p50, p95, or p99** or its
  200-rate drops. A first-round failure re-runs once and must reproduce —
  a one-off blip passes. Jitter floor is ~3ms; sub-3ms deltas are noise.
- **Rebaseline only after an *intended* perf change or at a release** — never
  to make a red gate green. Capture the baseline on the same host/fixture you
  gate on (an idle host; the numbers are host-local, CI rebaselines from its
  own merge-base instead).

## Reading the results

- Full run: `suite/perf/results/latest.md` (+ raw per-leg
  `results/raw/{ferrofin,jellyfin}-summary.json`). Merged suite record:
  `suite/results/run-<sha>.json`. Dashboard: `suite/viewer/serve.sh` →
  http://127.0.0.1:8125/suite/viewer/.
- Headline stats (median speedup, win rate) count **comparable rows only** —
  deep-verified parity + both-200 + no body drift. A "loss" can be p50-only:
  check p95/p99 before concluding Ferrofin is slower (a win requires all three
  percentiles; the board has had rows where Ferrofin "lost" p50 by 4ms and won
  p99 by 20×).
- Cheap endpoints' p50 under the mixed 50-VU loop includes **DB-pool queueing
  behind the heavy rows** — it is not that endpoint's intrinsic cost. Verify
  intrinsic cost with a single curl against an idle server before "fixing" it.
- Write rows (`auth_login`, `playstate_progress`, `item_playbackinfo_post`,
  `item_userdata_post`) are fingerprint-exempt: comparable = parity
  deep-verified + 100% expected-status (204 for progress, not 200).
  `auth_login` measures its own post-drain window (`BENCH_LOGIN_VUS` ×
  `BENCH_LOGIN_DURATION`), so its rps isn't comparable to the mixed-loop rows.
- New/renamed variants have **no baseline until the next `--rebaseline`**; the
  gate skips unknown variants silently — don't read that as "passed".

## Publishing

Reports worth keeping go to `suite/results/` next to the run records
(`2026-08-04-COMPARISON.md`); `results/` is gitignored except committed run
records under `suite/results/`.
