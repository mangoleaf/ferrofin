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

@test "run.sh with no stage prints usage and fails" {
  run ./run.sh
  [ "$status" -ne 0 ]
  [[ "$output" == *"suite/run.sh parity"* ]]
}

@test "run.sh rejects an unknown stage" {
  run ./run.sh bogus
  [ "$status" -ne 0 ]
}

@test "merge produces a valid run record with the fairness fields" {
  run python3 merge.py
  [ "$status" -eq 0 ]
  run python3 -c "import json,glob,os; \
r=json.load(open(sorted(glob.glob('results/run-*.json'),key=os.path.getmtime)[-1])); \
h=r['headline']; assert 'parity_coverage' in h and 'median_speedup' in h; \
assert all('comparable' in o['perf'] for o in r['operations'])"
  [ "$status" -eq 0 ]
}
