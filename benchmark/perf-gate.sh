#!/usr/bin/env bash
# Perf regression gate — Hermit-only, sentinel endpoints, diff vs perf-baseline.json.
#
# Builds the current working tree into the benchmark image, brings up Hermit
# alone (skips Jellyfin for speed — the gate compares Hermit to its OWN baseline,
# not to Jellyfin), drives each sentinel endpoint at a light fixed load, and
# fails (exit 1) if any endpoint exceeds PERF_GATE_FACTOR× its baseline on p50,
# p95, OR p99, or if its 200-rate < 100%. Median-only gating hides tail
# regressions, so all three percentiles gate (plan 04).
#
#   ./perf-gate.sh                 gate the current working tree
#   ./perf-gate.sh --rebaseline    rerun and overwrite perf-baseline.json (release / first run)
#
# Knobs (env-overridable; ask the repo owner before changing defaults — they're
# tuned "loose enough to ignore noise, tight enough to catch a 2× regression"):
#   PERF_GATE_FACTOR   regression threshold (default 1.5)
#   PERF_GATE_VUS      closed-model VUs per endpoint (default 10)
#   PERF_GATE_SECONDS  measured window per endpoint (default 10)
#   PERF_GATE_ENDPOINTS  sentinel endpoint names (default: the 11 below)
set -euo pipefail
cd "$(dirname "$0")"
# shellcheck source=/dev/null
set -a; [ -f .env ] || cp .env.example .env; . ./.env; set +a
# shellcheck source=_phase-common.sh
. ./_phase-common.sh
mkdir -p results/raw

FACTOR=${PERF_GATE_FACTOR:-1.5}
VUS=${PERF_GATE_VUS:-10}
SECS=${PERF_GATE_SECONDS:-10}
ENDPOINTS_LIST=${PERF_GATE_ENDPOINTS:-"info_public user_me items_sortname items_mixed item_detail persons studios suggestions movie_recommendations items_filters2 image_primary"}
# The ONE baseline file (suite/perf-baseline.json, section `raw`) — the
# comparator is suite/gate.py, the single implementation of the
# any-of-p50/p95/p99 rule for both this runner and `suite/run.sh gate`.
BASELINE=../suite/perf-baseline.json
export PERF_GATE_VUS="$VUS" PERF_GATE_SECONDS="$SECS"

REBASELINE=0
[ "${1:-}" = "--rebaseline" ] && REBASELINE=1

# Library list — identical construction to run-phase-*.sh (parsed by k6, no bash splitting).
LIBS="["; sep=""
[ -n "${REAL_MEDIA_DIR:-}" ] && { LIBS="$LIBS${sep}{\"name\":\"Movies\",\"type\":\"movies\",\"path\":\"/media/movies-real\"}"; sep=","; }
[ -n "${REAL_TV_DIR:-}" ]    && { LIBS="$LIBS${sep}{\"name\":\"Shows\",\"type\":\"tvshows\",\"path\":\"/media/tv-real\"}"; sep=","; }
[ "${FIXTURE_MOVIES:-0}" -gt 0 ] && { LIBS="$LIBS${sep}{\"name\":\"Movies (synth)\",\"type\":\"movies\",\"path\":\"/media/synth/movies\"}"; sep=","; }
[ "${FIXTURE_SERIES:-0}" -gt 0 ] && { LIBS="$LIBS${sep}{\"name\":\"Shows (synth)\",\"type\":\"tvshows\",\"path\":\"/media/synth/tv\"}"; sep=","; }
LIBS="$LIBS]"
[ "$LIBS" = "[]" ] && { echo "No media: set REAL_MEDIA_DIR or FIXTURE_MOVIES>0 in .env"; exit 1; }
if [ -n "${REAL_MEDIA_DIR:-}" ] || [ -n "${REAL_TV_DIR:-}" ]; then EXPECTED_ITEMS=0
else EXPECTED_ITEMS=$(( ${FIXTURE_MOVIES:-0} + ${FIXTURE_SERIES:-0} * ${FIXTURE_EPISODES_PER_SERIES:-0} )); fi
export LIBRARIES="$LIBS" REAL_MEDIA_DIR REAL_TV_DIR BENCH_ADMIN_USER BENCH_ADMIN_PASSWORD EXPECTED_ITEMS JELLYFIN_IMAGE

BASE="http://localhost:$HERMIT_HOST_PORT"

run_endpoints() {   # $1 = space-separated endpoint names → results/raw/perfgate-hermit-<name>.json
  for name in $1; do
    k6 run -e ENDPOINT="$name" -e TARGET=hermit -e BASE_URL="$BASE" \
      perf-gate.js </dev/null >/dev/null 2>&1 || true
  done
}

echo ">> perf-gate: ${VUS} VUs × ${SECS}s/endpoint, factor ${FACTOR}× (Hermit only)"
# bringup_scan takes the base URL, not the bare port — passing the port made
# the readiness loop poll host "18196" and hang (caught the first time this
# script was actually run end-to-end; plan 08 step 2).
bringup_scan hermit "$BASE" hermit || { echo "perf-gate: Hermit failed to come up"; exit 2; }
run_endpoints "$ENDPOINTS_LIST"

if [ "$REBASELINE" = 1 ]; then
  # shellcheck disable=SC2086  # word-splitting is intentional: names → separate args
  python3 ../suite/gate.py rebaseline-raw "$BASELINE" "$VUS" "$SECS" $ENDPOINTS_LIST
  docker compose down -v >/dev/null 2>&1 || true
  exit 0
fi

# Round 1. `if ! x=$(…)` checks the exit status explicitly — a `set -e` on a bare
# `x=$(cmd)` does NOT abort on cmd failure in bash, which would silently false-PASS
# on a missing baseline. A non-zero exit means a hard error (missing baseline); a
# zero exit with names on stdout means those endpoints regressed.
# shellcheck disable=SC2086  # word-splitting is intentional: names → separate args
if ! fails=$(python3 ../suite/gate.py compare-raw "$BASELINE" "$FACTOR" $ENDPOINTS_LIST); then
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
if ! fails2=$(python3 ../suite/gate.py compare-raw "$BASELINE" "$FACTOR" $fails); then
  docker compose down -v >/dev/null 2>&1 || true
  echo ">> perf-gate ERROR: comparator failed on retry"; exit 2
fi
docker compose down -v >/dev/null 2>&1 || true

if [ -z "$fails2" ]; then
  echo ">> perf-gate PASS (round-1 failures did not reproduce)"; exit 0
fi
echo ">> perf-gate FAIL: [$fails2] exceeded ${FACTOR}× baseline on retry"
exit 1
