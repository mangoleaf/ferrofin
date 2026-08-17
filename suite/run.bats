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

@test "fingerprint shape hashing self-test passes" {
  run python3 fingerprint.py --selftest
  [ "$status" -eq 0 ]
}

@test "merge verdict/manifest self-test passes (noise-floor ties + ratio floor)" {
  run python3 merge_selftest.py
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
  before=$(python3 -c "import json;print(len(json.load(open('results/runs.json'))['runs']))")
  MERGE_ALLOW_INCOMPLETE=1 run_out=$(MERGE_ALLOW_INCOMPLETE=1 python3 merge.py)
  [[ "$run_out" == *"INCOMPLETE, excluded from the trend"* ]]
  after=$(python3 -c "import json;print(len(json.load(open('results/runs.json'))['runs']))")
  [ "$before" -eq "$after" ]
  python3 -c "import json,glob; \
r=json.load(open(sorted(glob.glob('results/run-*-incomplete*.json'))[-1])); \
assert r['meta']['incomplete'], 'incomplete stamp missing'"
  rm -f results/run-*-incomplete*.json
}

# Skips rather than degrade: on a host without raw summaries the old version
# took the ALLOW_INCOMPLETE early-return, globbed up its own incomplete record
# (operations: []) and passed every assertion vacuously — green while checking
# zero rows, the exact pattern this branch exists to kill (review, round 3).
# The non-empty assert makes vacuous passes impossible regardless of host.
@test "merge produces a valid run record with the fairness fields" {
  [ -f perf/results/raw/ferrofin-summary.json ] || skip "no raw summaries on this host"
  # Optional legs pinned OFF: TTFS and the cold restarts are env-driven and
  # their raw files may legitimately be absent; the mandatory manifest (all
  # 118 variants, both servers) is still asserted strictly.
  run env RUN_TRANSCODE=0 BENCH_COLD_ENDPOINTS="" python3 merge.py
  [ "$status" -eq 0 ]
  run python3 -c "import json,glob,os; \
r=json.load(open(sorted(glob.glob('results/run-*.json'),key=os.path.getmtime)[-1])); \
h=r['headline']; assert 'parity_coverage' in h and 'median_speedup' in h; \
assert 'dropped_rows' in h and 'dropped_by_reason' in h; \
assert r['operations'], 'record has no operations'; \
assert all('comparable' in o['perf'] for o in r['operations'])"
  [ "$status" -eq 0 ]
}
