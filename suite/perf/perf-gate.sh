#!/usr/bin/env bash
# Perf regression gate — Ferrofin-only, sentinel endpoints, diff vs perf-baseline.json.
#
# Builds the current working tree into the benchmark image, brings up Ferrofin
# alone (skips Jellyfin for speed — the gate compares Ferrofin to its OWN baseline,
# not to Jellyfin), drives each sentinel endpoint at a light fixed OPEN-LOOP
# arrival rate (workstream G: a closed VU loop absorbs regressions by slowing the
# generator down), and fails (exit 1) if any endpoint exceeds PERF_GATE_FACTOR×
# its baseline on p50, p95, OR p99, or if its 200-rate < 100%. Median-only gating
# hides tail regressions, so all three percentiles gate (plan 04).
#
#   ./perf-gate.sh                 gate the current working tree
#   ./perf-gate.sh --rebaseline    rerun and overwrite perf-baseline.json (release / first run)
#
# Knobs (suite/bench.conf or env; ask the repo owner before changing defaults —
# they're tuned "loose enough to ignore noise, tight enough to catch a 2× regression"):
#   PERF_GATE_FACTOR   regression threshold (default 1.5)
#   PERF_GATE_RATE     open-loop arrival rate per endpoint, req/s (default 25)
#   PERF_GATE_SECONDS  measured window per endpoint (default 10)
#   PERF_GATE_ENDPOINTS  sentinel endpoint names (default: the 11 below)
set -euo pipefail
cd "$(dirname "$0")"
# shellcheck source=../lib.sh
source ../lib.sh
suite_load_env .env
# shellcheck source=_phase-common.sh
. ./_phase-common.sh
mkdir -p results/raw

FACTOR=${PERF_GATE_FACTOR:-1.5}
RATE=${PERF_GATE_RATE:-25}
SECS=${PERF_GATE_SECONDS:-10}
ENDPOINTS_LIST=${PERF_GATE_ENDPOINTS:-"info_public user_me items_sortname items_mixed item_detail persons studios suggestions movie_recommendations items_filters2 image_primary"}
# The ONE baseline file (suite/perf-baseline.json, section `raw`) — the
# comparator is suite/gate.py, the single implementation of the
# any-of-p50/p95/p99 rule for both this runner and `suite/run.sh gate`.
BASELINE=../perf-baseline.json

REBASELINE=0
[ "${1:-}" = "--rebaseline" ] && REBASELINE=1

# Containers must come down on EVERY exit path — including a set -e abort
# mid-script (a review-caught crash here once left docker up after measuring
# all 11 endpoints). The explicit downs below remain (idempotent).
trap 'docker compose down -v >/dev/null 2>&1 || true' EXIT

# Library list — the ONE construction in suite/lib.sh (this script used to
# duplicate it inline; the copies drifted being the risk, not a bug yet).
suite_build_libraries
# The synthetic tree is MOUNTED and scanned here (docker-compose mounts
# ./fixtures/media at /media/synth, and suite_build_libraries registers it
# whenever FIXTURE_*>0), so the gate must generate it and refuse to measure
# without it — gating a 20-item library against a 520-item baseline is the
# silently-wrong-number case, and --rebaseline would enshrine it.
suite_gen_fixtures
suite_require_fixtures
# …and refuse to start if that library does not exist on disk. Docker would
# otherwise CREATE the missing mount point (root-owned, which then needs sudo to
# clear), scan nothing, and report it as a memory problem.
suite_require_media
suite_assert_media_readonly   # real media is never writable from the suite

BASE="http://localhost:$FERROFIN_HOST_PORT"

run_endpoints() {   # $1 = space-separated endpoint names → results/raw/perfgate-ferrofin-<name>.json
  for name in $1; do
    # Drop the previous capture FIRST. perf_gate.py failing (bad ctx, vegeta
    # blowing up) is swallowed below, and a surviving stale file would be read
    # by gate.py as this run's data — a PASS on yesterday's numbers, or a
    # --rebaseline that baselines them. Absent file ⇒ "NO DATA" ⇒ failure.
    rm -f "results/raw/perfgate-ferrofin-$name.json"
    python3 perf_gate.py --base "$BASE" --endpoint "$name" --rate "$RATE" --secs "$SECS" \
      </dev/null >/dev/null 2>&1 || true
  done
}

echo ">> perf-gate: ${RATE}/s × ${SECS}s/endpoint (open-loop), factor ${FACTOR}× (Ferrofin only)"
# bringup_scan takes the base URL, not the bare port — passing the port made
# the readiness loop poll host "18196" and hang (caught the first time this
# script was actually run end-to-end; plan 08 step 2).
bringup_scan ferrofin "$BASE" ferrofin || { echo "perf-gate: Ferrofin failed to come up"; exit 2; }
run_endpoints "$ENDPOINTS_LIST"

if [ "$REBASELINE" = 1 ]; then
  # shellcheck disable=SC2086  # word-splitting is intentional: names → separate args
  python3 ../gate.py rebaseline-raw "$BASELINE" "$RATE" "$SECS" $ENDPOINTS_LIST
  docker compose down -v >/dev/null 2>&1 || true
  exit 0
fi

# Round 1. `if ! x=$(…)` checks the exit status explicitly — a `set -e` on a bare
# `x=$(cmd)` does NOT abort on cmd failure in bash, which would silently false-PASS
# on a missing baseline. A non-zero exit means a hard error (missing baseline); a
# zero exit with names on stdout means those endpoints regressed.
# shellcheck disable=SC2086  # word-splitting is intentional: names → separate args
if ! fails=$(python3 ../gate.py compare-raw "$BASELINE" "$FACTOR" $ENDPOINTS_LIST); then
  docker compose down -v >/dev/null 2>&1 || true
  echo ">> perf-gate ERROR: comparator failed (missing baseline? run --rebaseline)"; exit 2
fi
if [ -z "$fails" ]; then
  docker compose down -v >/dev/null 2>&1 || true
  echo ">> perf-gate PASS"; exit 0
fi

# Percentiles under short runs are noisy — retry the failures ONCE and require the
# regression to reproduce before failing the gate.
echo ">> perf-gate: [$fails] regressed on round 1 — retrying once to rule out noise"
run_endpoints "$fails"
# shellcheck disable=SC2086  # word-splitting is intentional: names → separate args
if ! fails2=$(python3 ../gate.py compare-raw "$BASELINE" "$FACTOR" $fails); then
  docker compose down -v >/dev/null 2>&1 || true
  echo ">> perf-gate ERROR: comparator failed on retry"; exit 2
fi
docker compose down -v >/dev/null 2>&1 || true

if [ -z "$fails2" ]; then
  echo ">> perf-gate PASS (round-1 failures did not reproduce)"; exit 0
fi
echo ">> perf-gate FAIL: [$fails2] exceeded ${FACTOR}× baseline on retry"
exit 1
