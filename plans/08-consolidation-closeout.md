# Plan 8 — Close out the deviations from Plans 3/4/6 (single agent, sequential)

## Context
Plans 1–7 landed (commits `cefe2f8`…`1ccbe30`). A review of the Plan 3/4/6
implementations found a set of deviations, sanctioned-but-now-due deferrals,
and duplication. This plan resolves all of them. **One agent implements this
sequentially** — no concurrent edits are assumed, so no worktree coordination
is needed, but the measurement steps still require an **idle host** (a
concurrent cargo build silently corrupts k6 latency windows; this happened
during Plan 7 and cost a full sweep re-run).

Read first: `plans/04-perf-regression-gate.md`, `plans/06-unified-parity-perf-suite.md`,
`suite/README.md`, `benchmark/README.md`.

## Step 1 — One gate implementation (fixes the Plan 6 step-6 deviation)
Today there are TWO threshold implementations: `benchmark/perf-gate.mjs`
(+`perf-gate.sh`/`perf-gate.js`/`perf-gate.test.mjs`, Plan 4) and
`suite/gate.py` (Plan 6). Plan 6's design intent was that the Plan 4 gate gets
re-pointed at the merged record — instead it was reimplemented. Consolidate:

- **End state (hard requirement):** exactly one place computes
  "regressed = ANY of p50/p95/p99 > factor × baseline", and exactly one
  baseline file. The p50/p95/p99-all-three rule is a repo-owner hard
  requirement — do not weaken it while consolidating.
- Recommended shape (adjust if the code argues otherwise): keep
  `benchmark/perf-gate.sh` as the *capture* runner (docker + k6 — it works and
  is documented in CLAUDE.md), and make `suite/gate.py` the single
  *comparator*, reading either a raw capture or a merged suite record.
  `perf-gate.mjs`'s pure `classify()` logic and its unit tests move/port into
  `gate.py`'s tests (don't lose the tail-only-p99 and 200-rate test cases);
  then delete `perf-gate.mjs` + `perf-gate.test.mjs`.
- Keep the env knobs (`PERF_GATE_FACTOR/VUS/SECONDS/ENDPOINTS`) working from
  both entry points (`perf-gate.sh` and `suite/run.sh gate`).
- Update `benchmark/README.md` + root `CLAUDE.md` so the documented gate story
  names one comparator and one baseline path.

## Step 2 — Create the baseline + prove the detector (Plan 4's due deferrals)
The tree is settled (Plan 7 landed); the gate has been inert without a
baseline. On an **idle host**:

1. `cd benchmark && ./perf-gate.sh --rebaseline` — creates the baseline in the
   step-1 consolidated location/format. Commit it (it is deliberately not
   gitignored — it is the trend anchor).
2. Stability proof: run the gate twice on clean HEAD; both must pass. If p99
   flaps, lengthen that endpoint's window (never drop p99 — hard requirement).
3. Detector proof: inject a temporary slowdown (e.g. a
   `std::thread::sleep(50ms)` in the studios handler path), rebuild, run the
   gate, **confirm it fails naming the endpoint and percentile(s)**, then
   remove the injection (verify `git diff` is clean afterwards). Do not commit
   the injection.

## Step 3 — CI job (resolve the Plan 4 deviation by decision, not silence)
`.github/workflows/ci.yml` exists; Plan 4 said to add a path-gated job
(best-effort if runners lack docker/k6). The implementing agent for Plan 4
skipped it, arguing near-zero signal. Resolve it explicitly:

- Add a **non-blocking** (`continue-on-error: true`) job to `ci.yml`, gated on
  paths `crates/hermit-core/**`, `crates/hermit-db/**`, `crates/hermit-api/**`,
  `benchmark/**`, `suite/**`, that runs the gate if `docker` and `k6` are
  available and exits 0 with a clear "skipped: no docker/k6 on runner" notice
  otherwise. Cheap, honest, and the mandatory-local rule stays the real gate.
- If this proves impossible to express sanely, the fallback is to record the
  accepted deviation in `CLAUDE.md`'s quality-gates section ("perf gate is
  local-mandatory, CI-exempt because …") — but attempt the job first.

## Step 4 — One viewer (fixes the Plan 6 "one viewer, one port" deviation)
Three dashboards exist: parity `:8123` (`parity/index.html`), benchmark
`:8124` (`benchmark/index.html`), merged suite `:8125` (`suite/viewer/`).
Converge on the suite viewer:

1. Verify the suite viewer covers what the two old ones show: the full parity
   ledger table (all 412 ops, not just benched ones) and the benchmark
   version-over-version trend (the migrated history is already folded in).
   Port any missing view before deleting anything — the repo owner actively
   uses both dashboards.
2. Then delete `parity/index.html` + `parity/serve.sh` and
   `benchmark/index.html` + `benchmark/serve.py`/`serve.sh`, and make
   `suite/viewer/serve.sh` the single entry point. Note the port change
   prominently in `suite/README.md` (owner bookmarks :8123/:8124 — say where
   things went).
3. Any generator that wrote data for the old viewers (`benchmark/gen-viewer.py`,
   `parity/gen-ledger.py` HTML bits) must keep producing whatever the suite
   viewer consumes; ledger generation itself is untouched.

## Step 5 — Finish the bring-up consolidation (both sides' disclosed leftovers)
Fold the remaining copies of the LIBS/fixture/bring-up block onto
`suite/lib.sh`, exactly as `run.sh`/`run-phase-b.sh`/`parity/sweep.sh` already
do: `benchmark/run-phase-a.sh`, `benchmark/run-phase-c.sh`,
`benchmark/pool-sweep.sh`, `benchmark/run-phase-d.sh`. Behavior-preserving
refactor only; `shellcheck` clean; `benchmark/run.bats` + `suite/run.bats`
still pass.

## Step 6 — Real suite seed data (replaces Plan 6's stale local seeds)
`suite/perf-baseline.json` and `suite/results/` are untracked and were
generated mid-flight (pre-Plan-7 — their 0.73× median is stale; current tree
measures 2.41×). On an idle host, run one real `suite/run.sh all`, sanity-check
the merged record (comparable count, headline computed over comparable rows
only, parity_coverage present), then commit the run record / `runs.json` /
suite baseline as the trend anchor. Delete the stale untracked seeds they
supersede.

## Step 7 — Record the explicit no-action items (so nobody re-litigates)
Add a short "accepted deviations" section to `suite/README.md` (or the plan
docs) recording, with one-line rationales:
- `MediaSourceManager::get_alternate_versions_batch` trait default returns "no
  alternates": kept — the sole concrete impl overrides it, test fakes rely on
  the default, and the doc comment warns future implementors. No code change.
- Commit `f108b19` contains Plan 6's four `benchmark/parity.*` deletions
  (shared-index sweep): history stays as-is; content is correct.
- Housekeeping while here: gitignore `__pycache__/` (benchmark/, .claude
  skills) and the stray `benchmark/results/*.log` files so `git status` stops
  drowning in them (results/*.md and results/raw/ are already ignored).

## Verification (all required)
- `cargo fmt --all --check`, `clippy -D warnings`, `cargo nextest run
  --workspace`, doctests — nothing in this plan should touch Rust except the
  step-2 injection (which must not survive).
- `shellcheck` (warning+) on every touched script; `run.bats` suites pass;
  `suite/registry_selftest.py` and `suite/fingerprint.py --selftest` pass.
- The consolidated gate: passes twice on clean HEAD, fails on the injected
  slowdown, and both `perf-gate.sh` and `suite/run.sh gate` drive it.
- The single viewer serves and shows: full ledger, perf columns with
  greyed non-comparable rows, legacy trend era.
- Final `git status` is clean apart from intentionally-ignored files.

## Constraints
- Never create/switch branches; no AI-attribution trailers in commits; tests
  in domain-named files; `///` docs on any new pub items; runtime sqlx only;
  respect the SQL boundary ratchet (this plan should not add SQL anywhere).
- **Idle host during every measurement step** (steps 2 and 6). Do not run
  cargo builds, tests, or other containers while a k6 window is open.
- Commit in logical pieces per step (gate consolidation / baseline / CI /
  viewer / lib.sh folding / seeds / housekeeping), so any step can be
  reverted alone.
