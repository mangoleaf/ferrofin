# Plan 6 — Merge parity + benchmark into one suite, cross-referenced and fair

## Current state (three stacks, grown separately)
1. **`benchmark/`** — k6 load harness. `run.sh` (+ `run-phase-{a,b,c}.sh`) runs the
   Ferrofin and Jellyfin containers **one at a time** (ports 18096/18097), endpoints
   defined ad-hoc in `bench-lib.js` as `{name: 'items_filters2', path: (c) => ...}`.
   Per-version results in `bench-data.json`; viewer `index.html` served at :8124.
2. **`benchmark/parity.sh` + `parity.js`** — an older k6 body-diff harness (both
   servers up at once → `results/PARITY.md`). Superseded by (3), still around.
3. **`parity/`** — the real parity suite (Python): `sweep.py` (Layer 1:
   status+schema over all 412 contract ops), `reads.py` (Layer 2: deep body diff),
   `journeys.py` (write journeys), `assets.py` (Layer 3: binary/asset diff),
   merged by `gen-ledger.py` into `ledger.json` + `classifications.json`, keyed by
   contract operation strings (`"GET /Albums/{itemId}/InstantMix"`). Viewer at :8123.
   Its `sweep.sh` reuses benchmark's docker-compose.

## The early mistakes to fix (each is a design requirement below)
- **M1 — No join key.** Ledger keys by contract operation; benchmark keys by ad-hoc
  name. Cross-referencing parity ↔ perf is manual. (`items_filters2` vs
  `GET /Items/Filters2`.)
- **M2 — Perf was never conditioned on parity.** Early runs showed 800–2100× median
  "speedups" while Ferrofin served hollow/incomplete bodies; as parity work made
  responses real (e.g. the NFO scan populating genres/studios/people), Ferrofin
  "got slower". The headline trend conflates *doing the work correctly now* with
  *actual regressions* — unfair in both directions and it hid real regressions.
- **M3 — No stable endpoint identity across runs.** The bench set grew 7 → 31 → 83
  endpoints with renames; the intersection of endpoints across all 14 recorded runs
  is ~1. Version-over-version trend analysis is nearly impossible from
  `bench-data.json` alone.
- **M4 — Run comparability isn't enforced.** `bench-data.json` records library size
  and load as display strings, not as structured identity (fixture content hash,
  CPU/mem limits, Jellyfin image digest, load params). Runs with different fixtures
  are silently comparable in the viewer.
- **M5 — Two parity implementations** (k6 `parity.js` vs the Python suite) that can
  disagree; double maintenance.
- **M6 — Copy-pasted orchestration.** The env/LIBS/fixture/bring-up block is
  duplicated across `benchmark/run.sh`, `benchmark/parity.sh`, `parity/sweep.sh`.
- **M7 — Tribal-knowledge gotchas** (no legacy auth headers; never reuse a DeviceId
  for mid-run probes; ports) live in memory/notes, not in the harness.

## Target design

### One registry (fixes M1, M3)
A single machine-readable registry, e.g. `suite/registry.json` (or a JS module both
stacks import), where every entry is keyed by **contract operation**, and bench
variants are parameterizations of an operation:

```json
{ "op": "GET /Items/Filters2", "tag": "Filter",
  "variants": [ { "id": "items_filters2",
                  "params": "userId={userId}&includeItemTypes=Movie",
                  "load": "default" } ] }
```

- The parity sweep continues to enumerate ops **from the vendored OpenAPI spec**
  (`contracts/jellyfin-openapi-*.json`) — the registry adds bench variants on top;
  parity coverage never shrinks to "what we benchmark".
- Variant `id`s are permanent (they're the trend key). Renaming requires an explicit
  alias entry (`"was": "old_name"`) so history joins survive.
- A suite self-test (mirroring the spirit of `contract_superset.rs`) asserts: every
  variant's `op` exists in the vendored spec; no duplicate variant ids; every alias
  points at a real id.

### One orchestrator (fixes M5, M6)
`suite/run.sh` with stages, replacing the three entry points:

```
suite/run.sh parity          # both servers up → sweep + reads + journeys + assets → ledger
suite/run.sh perf            # servers one-at-a-time → k6 phases → perf results
suite/run.sh all             # parity, then perf, same build + same fixture
suite/run.sh gate            # Plan 4's fast regression gate, reading merged results
```

- Extract the shared env/LIBS/fixture/wait200 bring-up into `suite/lib.sh`, sourced
  by every stage (single copy).
- **Delete `benchmark/parity.sh` + `parity.js` + `parity-diff.js`** (and their test)
  after confirming the Python suite covers everything they check; port anything
  unique (the `.env.loop` fast-iteration dataset flow already exists in `sweep.sh`).
- Keep the measurement disciplines exactly as they are — they're correct:
  **parity with both servers up** (diffing needs simultaneous state),
  **perf one-at-a-time** (no resource sharing during measurement).
- Encode the gotchas (M7) as code + comments in `suite/lib.sh`: auth header
  construction in one function (no legacy `X-Emby-*`), DeviceId minted per stage,
  a guard that refuses to run probes against a server while a k6 phase is active.

### One merged result record per run (fixes M2, M4)
Each `suite/run.sh all` emits one JSON:

```json
{ "meta": { "ferrofin": "<git describe>", "ferrofin_sha": "…",
            "jellyfin_image": "jellyfin/jellyfin:10.11.8@sha256:…",
            "fixture_hash": "<sha256 of gen-fixtures output manifest>",
            "cpus": 4, "mem": "…", "load": {"vus": 50, "seconds": 30},
            "when": "…" },
  "operations": [ { "op": "GET /Studios", "parity": { "depth": "REAL",
                    "deep_verified": true, "classification": null },
                    "perf": { "variant": "studios", "h_p50": 191.2, "h_p95": …,
                              "j_p50": 208.8, "speedup": 1.09, "h_ok": 100 } } ] }
```

- `gen-ledger.py` stays the parity accumulator; the merge step joins
  ledger-at-this-sha into the run record. Perf rows for ops the ledger hasn't
  deep-verified are **kept but flagged** `"comparable": false`.
- **Headline metrics (the fairness rule):** median speedup / win-rate are computed
  **only over `comparable: true` rows**. Non-comparable rows render greyed with the
  reason ("not deep-verified — speed not meaningful"). Each run's summary also
  records `parity_coverage` (% of benched ops deep-verified) so the historical
  trend decomposes into "coverage went up" vs "speed went down".
- **Percentile rule (repo owner, 2026-08-03):** every perf record carries p50,
  p95, and p99 for both servers, and an endpoint counts as a Ferrofin "win" only
  when it wins on **all three**. A p50 win with a p95/p99 loss is surfaced as a
  tail loss, never folded into a single "faster" verdict — median-only speedup
  scoreboards hid 2× p99 regressions in the past.
- The viewer refuses (or loudly warns) to diff two runs whose
  `fixture_hash`/`load`/`cpus`/`jellyfin_image` differ — comparability enforced by
  data, not by the reader's memory.
- **Mid-run honesty check:** during perf phases, hash 1-in-N response bodies per
  variant and compare against the body fingerprint the parity stage recorded for
  that op. A mismatch marks the row `comparable: false` for that run — catches
  "fast because it drifted wrong since the last parity pass" at near-zero cost.

### One viewer, one port
Merge the two dashboards into `suite/viewer/` (single `serve.sh`): the ledger table
gains perf columns (p50/speedup, greyed when non-comparable); the benchmark trend
view gains parity-coverage annotations and filters to comparable rows by default.
Reuse the existing `index.html` code — both viewers are already plain static
JS + JSON; this is mostly a join, not a rewrite.

### History migration (bounded, don't over-do)
Write a one-shot `suite/migrate-history.py` that wraps the existing
`bench-data.json` versions into the new record shape with
`"legacy": true, "comparable": false` (no fixture hash exists for them). They stay
visible in the trend as a greyed pre-merge era. Do **not** attempt to retro-compute
parity status for old runs.

## Execution order for the implementing agent
1. Registry + self-test (pure data, no behavior change).
2. `suite/lib.sh` extraction; make the three existing entry points source it
   (behavior identical; verify by running `parity/sweep.sh` and one phase-b bench).
3. Merged result record + the join step + fairness flags.
4. Unified `suite/run.sh` entry points; retire `benchmark/parity.sh`/`parity.js`.
5. Viewer merge + history migration.
6. Point Plan 4's perf-gate at the merged record (gate additionally fails when a
   previously `deep_verified` op regresses to unverified — parity and perf now
   gate each other, which is the cross-referencing the suite exists for).

Each step lands independently and keeps both old paths working until step 4.

## Verification
- After step 2: byte-compare a `parity/sweep.sh` ledger run and a phase-b
  `bench-data.json` run before/after the extraction (identical output = safe).
- After step 4: `suite/run.sh all` end-to-end on the fixture library; confirm the
  merged record's parity fields match `ledger.json` and perf fields match the last
  standalone bench for the same build.
- Registry self-test green; `run.bats` updated for the new entry points;
  `shellcheck` clean on new shell.
- Viewer: load the migrated history, confirm legacy runs are greyed and the
  comparable-only median reproduces (for the latest run) the number computed by
  hand from the raw JSON.

## Constraints
- Never create/switch branches; no AI-attribution trailers in commits.
- The vendored OpenAPI spec remains the single source of truth for the operation
  universe; the registry must never let the parity sweep's coverage shrink.
- Keep runtime costs where they are: the merged `all` run should take no longer
  than today's parity + bench runs back-to-back.
- Fairness disciplines are non-negotiable: one-at-a-time perf measurement,
  identical container resources both sides, identical auth flow, pinned Jellyfin
  image by digest.

## Conflicts
Touches `benchmark/` and `parity/` scripts — **coordinate with Plan 4** (the
perf-gate): either land Plan 4 first against `run-phase-b.sh` and re-point it in
step 6 here, or fold Plan 4's gate directly into `suite/run.sh gate` if this plan
runs first. Plans 1–3, 5 (Rust) are unaffected and can run in parallel.
