#!/usr/bin/env bats
# Fast checks for the merged suite entry points — no docker/k6, so these run in CI in milliseconds.
#   bats suite/run.bats

setup() { cd "$BATS_TEST_DIRNAME"; }

@test "registry self-test passes" {
  run python3 registry_selftest.py
  [ "$status" -eq 0 ]
  [[ "$output" == *"registry self-test OK"* ]]
}

@test "generated registry matches the committed one (regen is a no-op)" {
  cp registry.json /tmp/registry.committed.json
  run python3 gen-registry.py
  [ "$status" -eq 0 ]
  run diff -q registry.json /tmp/registry.committed.json
  [ "$status" -eq 0 ]
}

@test "every contract operation is either benched or skipped with a reason" {
  run python3 coverage.py
  [ "$status" -eq 0 ]
  [[ "$output" == *"benchmark coverage OK"* ]]
}

# The parity ledger's headline ("N/412 deep-verified") means ONE thing: the response
# was diffed clean. Every row carrying a verdict must declare which method earned it,
# from the closed set in parity/verification.py. That rule was enforced only when a
# human typed `--check`: sweep.sh ran bare `gen-ledger.py`, no workflow and no test
# ran the check, and an unstamped verdict was written into ledger.json AND LEDGER.md
# — rendering the body-diff tick — before the process happened to die on an unrelated
# sort. These three tests make the rule hold by CI instead of by habit.
@test "every parity ledger verdict declares how it was verified" {
  run python3 parity/gen-ledger.py --check
  [ "$status" -eq 0 ]
  [[ "$output" == *"deep-verified (the headline)"* ]]
}

@test "verification method set self-test passes (nested empty envelopes, bare signatures)" {
  run python3 parity/verification.py
  [ "$status" -eq 0 ]
  [[ "$output" == *"5 verification methods"* ]]
}

# The guard must REFUSE, and refuse before writing: the failure mode being tested is
# a complete ledger.json + LEDGER.md on disk with an unstamped row rendered ✅.
@test "gen-ledger refuses to write a ledger when a verdict has no verification_method" {
  scratch="$BATS_TEST_TMPDIR/repo"
  mkdir -p "$scratch/suite"
  ln -s "$BATS_TEST_DIRNAME/../contracts" "$scratch/contracts"
  ln -s "$BATS_TEST_DIRNAME/../crates" "$scratch/crates"
  cp -r parity "$scratch/suite/parity"
  python3 - "$scratch/suite/parity" <<'STRIP'
import glob, json, os, sys
d = sys.argv[1]
for f in glob.glob(os.path.join(d, "*-results.json")):
    doc = json.load(open(f))
    for row in doc["rows"].values():
        row.pop("verification_method", None)
    json.dump(doc, open(f, "w"))
STRIP
  before=$(md5sum "$scratch/suite/parity/ledger.json" | cut -d' ' -f1)
  before_md=$(md5sum "$scratch/suite/parity/LEDGER.md" | cut -d' ' -f1)
  run python3 "$scratch/suite/parity/gen-ledger.py"
  [ "$status" -ne 0 ]
  [[ "$output" == *"verification_method"* ]]
  # nothing was written: the old code emitted both files complete, then crashed
  [ "$before" = "$(md5sum "$scratch/suite/parity/ledger.json" | cut -d' ' -f1)" ]
  [ "$before_md" = "$(md5sum "$scratch/suite/parity/LEDGER.md" | cut -d' ' -f1)" ]
}

# ledger.json and LEDGER.md are GENERATED — "do not hand-edit" is printed at the top
# of the file and was enforced by nothing. Regeneration is offline (it re-folds the
# committed parity/*-results.json against the contract and REAL_ROUTES), so the fix
# for a failure here is one command: `python3 suite/parity/gen-ledger.py`.
@test "regenerating the parity ledger is a no-op (it is generated, not hand-edited)" {
  cp parity/ledger.json "$BATS_TEST_TMPDIR/ledger.committed.json"
  cp parity/LEDGER.md "$BATS_TEST_TMPDIR/LEDGER.committed.md"
  run python3 parity/gen-ledger.py
  [ "$status" -eq 0 ]
  run diff -q parity/ledger.json "$BATS_TEST_TMPDIR/ledger.committed.json"
  [ "$status" -eq 0 ]
  run diff -q parity/LEDGER.md "$BATS_TEST_TMPDIR/LEDGER.committed.md"
  [ "$status" -eq 0 ]
}

@test "fingerprint shape hashing self-test passes" {
  run python3 fingerprint.py --selftest
  [ "$status" -eq 0 ]
}

@test "merge verdict/manifest self-test passes (noise-floor ties + ratio floor)" {
  run python3 merge_selftest.py
  [ "$status" -eq 0 ]
  [[ "$output" == *"all assertions passed"* ]]
}

@test "benchlib fixture self-test passes (playlist reuse + by-name quoting)" {
  run python3 perf/benchlib_selftest.py
  [ "$status" -eq 0 ]
  [[ "$output" == *"all assertions passed"* ]]
}

@test "aggregate self-test passes (paired ratio floor + distributions)" {
  run python3 aggregate_selftest.py
  [ "$status" -eq 0 ]
  [[ "$output" == *"all assertions passed"* ]]
}

@test "run.sh with no stage prints usage and fails" {
  run ./run.sh
  [ "$status" -ne 0 ]
  [[ "$output" == *"suite/run.sh parity"* ]]
}

@test "run.sh rejects an unknown stage" {
  run ./run.sh bogus
  [ "$status" -ne 0 ]
}

@test "bench.conf resolution: env wins, file supplies, defaults fill" {
  run env BENCH_RATE=99 python3 perf/config.py
  [ "$status" -eq 0 ]
  [[ "$output" == *"BENCH_RATE=99  (env)"* ]]
  [[ "$output" == *"BENCH_RUNS=5  (bench.conf)"* ]]
}

# A1: without raw summaries the manifest is unmeasurable — merge must FAIL, not
# write a green record with holes. (Skipped on a host that has fresh summaries.)
@test "merge fails loud when the manifest is unmeasured" {
  [ -f perf/results/raw/ferrofin-summary.json ] && skip "raw summaries present on this host"
  run python3 merge.py
  [ "$status" -eq 2 ]
  [[ "$output" == *"manifest incomplete"* ]]
}

@test "merge with MERGE_ALLOW_INCOMPLETE writes an incomplete record, out of the trend" {
  [ -f perf/results/raw/ferrofin-summary.json ] && skip "raw summaries present on this host"
  export OUTDIR
  # MERGE_OUT_DIR keeps the record (and the trend it must NOT join) inside the
  # test tmpdir — the old version wrote into the committed results/ and then
  # `rm -f results/run-*-incomplete*.json`'d, which would also delete an
  # operator's real incomplete record.
  OUTDIR="$BATS_TEST_TMPDIR/results"; mkdir -p "$OUTDIR"
  cp results/runs.json "$OUTDIR/runs.json"
  before=$(python3 -c "import json,os;print(len(json.load(open(os.environ['OUTDIR']+'/runs.json'))['runs']))")
  run_out=$(MERGE_ALLOW_INCOMPLETE=1 MERGE_OUT_DIR="$OUTDIR" MERGE_ALLOW_STALE_BUILD=1 python3 merge.py)
  [[ "$run_out" == *"INCOMPLETE, excluded from the trend"* ]]
  after=$(python3 -c "import json,os;print(len(json.load(open(os.environ['OUTDIR']+'/runs.json'))['runs']))")
  [ "$before" -eq "$after" ]
  python3 -c "import json,glob,os; \
r=json.load(open(sorted(glob.glob(os.environ['OUTDIR']+'/run-*-incomplete*.json'))[-1])); \
assert r['meta']['incomplete'], 'incomplete stamp missing'"
}

# Skips rather than degrade: on a host without raw summaries the old version
# took the ALLOW_INCOMPLETE early-return, globbed up its own incomplete record
# (operations: []) and passed every assertion vacuously — green while checking
# zero rows, the exact pattern this branch exists to kill (review, round 3).
# The non-empty assert makes vacuous passes impossible regardless of host.
@test "merge produces a valid run record with the fairness fields" {
  [ -f perf/results/raw/ferrofin-summary.json ] || skip "no raw summaries on this host"
  # Optional legs pinned OFF: TTFS and the cold restarts are env-driven and
  # their raw files may legitimately be absent; the mandatory manifest (every
  # registry variant, both servers) is still asserted strictly.
  #
  # MERGE_OUT_DIR: this test merges FOR REAL, and merge.py appends to the
  # committed trend (results/runs.json + run-<sha>.json). Running the test
  # suite must not mint a benchmark data point — it did, once, re-stamping
  # week-old raw artifacts onto the checked-out SHA. MERGE_ALLOW_STALE_BUILD
  # goes with it: whatever raw artifacts this host has are almost certainly
  # from another build, and record SHAPE is what is under test here.
  OUTDIR="$BATS_TEST_TMPDIR/results"
  run env RUN_TRANSCODE=0 BENCH_COLD_ENDPOINTS="" MERGE_OUT_DIR="$OUTDIR" \
      MERGE_ALLOW_STALE_BUILD=1 python3 merge.py
  [ "$status" -eq 0 ]
  run env OUTDIR="$OUTDIR" python3 -c "import json,glob,os; \
r=json.load(open(sorted(glob.glob(os.environ['OUTDIR']+'/run-*.json'),key=os.path.getmtime)[-1])); \
h=r['headline']; assert 'median_speedup' in h; \
assert 'median_speedup_p95' in h and 'median_speedup_p99' in h; \
assert 'excluded_rows' in h and 'excluded_by_reason' in h; \
assert 'measured_rows' in h and 'comparable_rows' not in h; \
assert r['operations'], 'record has no operations'; \
assert all('comparable' in o['perf'] for o in r['operations'])"
  [ "$status" -eq 0 ]
}
