# suite/ — merged parity + perf suite

One harness, one join key, one fair scoreboard (Plan 6). Replaces the three stacks that grew
separately: the k6 load bench (now `suite/perf/`), the retired k6 parity diff (deleted), and
the Python parity suite (now `suite/parity/`). Everything lives under this one folder:
the hub scripts here, the perf leg in `perf/`, the parity leg in `parity/`.

## Entry points

```
suite/run.sh parity   # both servers up  → sweep+reads+journeys+assets → suite/parity/ledger.json (+fingerprints)
suite/run.sh perf     # one at a time    → k6 load bench → suite/perf/results/raw/*-summary.json (+fingerprints)
suite/run.sh all      # parity, then perf, same build + fixture → suite/results/run-<sha>.json
suite/run.sh merge    # join the latest ledger + perf into the run record (no measurement)
suite/run.sh gate [--measure|--rebaseline]   # regression gate over the merged record
suite/viewer/serve.sh # → http://127.0.0.1:8125/suite/viewer/   (THE dashboard, one page)
```

## Where the old dashboards went (bookmark update)

The parity ledger viewer (`:8123`) and the benchmark viewer (`:8124`) are **retired**; both
lived as separate pages over separate data and could disagree. Everything they showed is on
the one page above: the full contract ledger (all ops, with depth/status/schema/deep filters),
the per-endpoint version-vs-version compare (pick a base run — Δ vs base column,
comparability-guarded), the footprint line (cold start / peak RSS / items), and the trend.
Serve it and go to **http://127.0.0.1:8125/suite/viewer/**.

## Why the numbers are fair (the whole point)

- **Speed is shown only for deep-verified ops.** A row is `comparable` only if the parity ledger
  deep-verified that op, both servers answered 200, and the body didn't drift since the parity pass
  (`suite/fingerprint.py`). Median-speedup / win-rate are computed over comparable rows **only** —
  so "Hermit got slower" can't secretly mean "Hermit started doing the work correctly."
- **Write (non-GET) rows are fingerprint-exempt by design** — a fingerprint probe would itself
  mutate state, and write bodies mint per-run tokens/timestamps. Their honesty gate instead:
  `deep_verified` must come from the parity **write journey**, and both servers must hit 100%
  expected-status (204 for playstate) during the bench. See `suite/perf/README.md` "Write rows".
- **A win means p50 AND p95 AND p99.** A p50 win with a tail regression is surfaced as `tail_loss`,
  never folded into "faster" (median-only boards hid 2× p99 regressions before).
- **`suite/run.sh gate`** fails on a >1.5× latency regression *or* when a previously deep-verified
  op regresses to unverified — parity and perf gate each other.

## The join key

`suite/registry.json` keys every bench variant by its contract operation (`items_filters2` →
`GET /Items/Filters2`). Variant ids are permanent trend keys; rename only via a `was` alias.
Regenerate with `gen-registry.py`; `registry_selftest.py` is the hard gate (every op in the
vendored spec, no dup ids, aliases resolve). The parity sweep still enumerates the full spec —
the registry only adds bench variants on top, never shrinks parity coverage.

## Gotchas encoded (not tribal)

`suite/lib.sh` is the single copy of the bring-up: modern `MediaBrowser` auth grammar only (no
legacy `X-Emby-*`), DeviceId minted per stage, and `suite_guard_no_probe` refuses probes while a
measured k6 phase is running. Parity keeps both servers up (diffing needs simultaneous state);
perf runs them one at a time (no resource sharing during measurement).

## History

`migrate-history.py` (one-shot) folds the old `bench-data.json` runs into `results/runs.json` as
greyed `legacy: true` / `comparable: false` entries — visible in the trend, never comparable.

## Accepted deviations (decided — do not re-litigate)

- **`MediaSourceManager::get_alternate_versions_batch` trait default returns "no alternates".**
  Kept: the sole concrete impl overrides it with the repository query, the test fakes rely on
  the default, and the doc comment warns any future third implementor. Not a stub in production.
- **Commit `f108b19` contains Plan 6's four `benchmark/parity.*` deletions.** A shared-index
  sweep during the multi-agent wave attributed them to the Plan 4 commit. Content is correct;
  history stays as-is (rewriting shared `main` is worse than a muddled attribution).
- **The CI perf-gate job is best-effort/non-blocking by design** (`.github/workflows/ci.yml`):
  shared runners are too noisy for a latency threshold to block on, and it never reads the
  repo's committed baseline (dev-host numbers aren't comparable to runner numbers — it
  rebaselines from the PR's merge-base on the same runner instead). The mandatory gate is the
  local `suite/perf/perf-gate.sh` per `CLAUDE.md`.
