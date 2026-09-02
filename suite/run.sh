#!/usr/bin/env bash
# suite/run.sh — the one entry point for the merged parity + perf suite (Plan 6).
#
#   suite/run.sh calibrate  measure per-endpoint capacity on Jellyfin → suite/perf/rates.json
#   suite/run.sh parity   both servers up   → sweep + reads + journeys + assets → ledger
#   suite/run.sh perf     one-at-a-time     → open-loop vegeta bench → per-endpoint latencies (+fingerprints)
#   suite/run.sh all      parity, then perf, same build + same fixture → merged run record
#   suite/run.sh publish  parity once, then BENCH_RUNS × (perf + merge) → agg-<sha> distributions
#   suite/run.sh merge    join the latest parity ledger + perf summaries into the run record
#   suite/run.sh gate [--measure|--rebaseline]   regression gate over the merged record
#   suite/run.sh push     against a RUNNING Ferrofin → WebSocket push checks (cast + SyncPlay)
#
# Fairness disciplines (non-negotiable, enforced by the sub-scripts they call):
#   parity runs BOTH servers up (diffing needs simultaneous state); perf runs them ONE AT A TIME
#   (no resource sharing during measurement). Both legs use identical container caps + auth flow.
set -euo pipefail
cd "$(dirname "$0")"
ROOT="$(cd .. && pwd)"

usage() { sed -n '2,15p' "$ROOT/suite/run.sh"; exit 1; }
[ $# -ge 1 ] || usage
stage="$1"; shift || true

case "$stage" in
  calibrate)
    # One-time per host/fixture (and on rebaseline): measure each endpoint's
    # capacity on the WEAKER server (Jellyfin) and write suite/perf/rates.json
    # at BENCH_RATE_FRACTION of it. Commit the result next to the baseline —
    # without it the bench runs at the flat BENCH_RATE and every window hits
    # the duration cap (slower run, rate_source=flat-default in the record).
    cd "$ROOT/suite/perf"
    # shellcheck source=lib.sh
    source ../lib.sh
    suite_load_env .env
    suite_mint_device_id calibrate
    suite_build_libraries
    suite_require_media
    suite_assert_media_readonly
    suite_gen_fixtures
    suite_require_fixtures
    docker compose down -v >/dev/null 2>&1 || true
    rm -f results/raw/jellyfin-ctx.json   # fresh DB ⇒ saved tokens/ids are dead
    docker compose up -d jellyfin
    suite_assert_running_mounts_readonly
    JPORT="${JELLYFIN_HOST_PORT:-18097}"
    suite_wait200 "http://localhost:$JPORT" jellyfin
    python3 compare.py --target jellyfin --base "http://localhost:$JPORT" --calibrate-rates
    docker compose down -v >/dev/null 2>&1 || true
    echo ">> wrote suite/perf/rates.json — commit it (host/fixture-local, like the baseline)"
    ;;
  parity)  exec "$ROOT/suite/parity/sweep.sh" "$@" ;;
  perf)    exec "$ROOT/suite/perf/run.sh" "$@" ;;
  push)
    # The Ferrofin-only SMOKE TEST for server→client pushes (remote control / cast
    # + SyncPlay): it drives many more verbs than any parity layer, but it asserts
    # only Ferrofin's own message shape, compares nothing against Jellyfin, and
    # writes no results file — so it can never earn a ledger row.
    #
    # The DIFFERENTIAL is suite/parity/push.py: same sockets, but opened on BOTH
    # servers, with the pushed messages diffed against each other (method
    # `push-diff`) and folded into the ledger. It runs inside `suite/run.sh parity`.
    #
    # Needs a Ferrofin already running (it does not manage containers):
    #   FERROFIN_BASE=http://127.0.0.1:8096 FERROFIN_USER=admin FERROFIN_PASS=… \
    #     suite/run.sh push
    : "${FERROFIN_BASE:?set FERROFIN_BASE to a running Ferrofin}"
    "$ROOT/suite/ws/seed_library.sh"
    exec python3 "$ROOT/suite/ws/probe_remote_control.py"
    ;;
  all)
    # B2 (hermetic-enough build): a shareable release record must not trust the
    # incremental BuildKit cache mounts — a poisoned mount once served a stale
    # binary while the tree compiled fine locally. (`docker build --no-cache`
    # does NOT clear RUN --mount=type=cache mounts; pruning them does.) The
    # fast per-change gate keeps the cache; BENCH_KEEP_CACHE=1 opts out here.
    # B1's /health/live build check still verifies whatever binary comes out.
    if [ "${BENCH_KEEP_CACHE:-0}" != "1" ]; then
      echo ">> pruning BuildKit cache mounts (hermetic release build; BENCH_KEEP_CACHE=1 to skip)"
      docker builder prune -f --filter type=exec.cachemount >/dev/null 2>&1 || true
    fi
    "$ROOT/suite/parity/sweep.sh"
    "$ROOT/suite/perf/run.sh"
    python3 "$ROOT/suite/merge.py"
    ;;
  publish)
    # C1: the publishable record is a DISTRIBUTION over BENCH_RUNS independent
    # perf runs (single-run point estimates carry ~30% noise on the ratio
    # metrics — measured on identical code). Parity runs once (deterministic);
    # each perf run merges into its own run-<sha>[-N].json; aggregate.py then
    # reduces them to agg-<sha>.{json,md} with per-endpoint median ± IQR.
    if [ "${BENCH_KEEP_CACHE:-0}" != "1" ]; then
      echo ">> pruning BuildKit cache mounts (hermetic release build; BENCH_KEEP_CACHE=1 to skip)"
      docker builder prune -f --filter type=exec.cachemount >/dev/null 2>&1 || true
    fi
    RUNS_N="${BENCH_RUNS:-$(cd "$ROOT/suite/perf" && python3 -c 'from config import CONFIG; print(CONFIG["BENCH_RUNS"])')}"
    "$ROOT/suite/parity/sweep.sh"
    # One wipe up front, one cleanup at the end: runs 2..N reuse the scanned
    # volumes (BENCH_KEEP_DATA — rescanning identical media N times bought
    # nothing but wall-clock; only measurement noise needs independence).
    (cd "$ROOT/suite/perf" && docker compose down -v >/dev/null 2>&1) || true
    export BENCH_KEEP_DATA=1
    for i in $(seq 1 "$RUNS_N"); do
      echo ">> publish run $i/$RUNS_N"
      # Rebuild only on the first pass; identical tree → identical image after,
      # and B1's /health/live check verifies the binary every pass regardless.
      if [ "$i" -gt 1 ]; then export BENCH_SKIP_BUILD=1; fi
      # F1: alternate which server measures first so slow host drift cancels
      # across the aggregate instead of always biasing the second leg.
      if [ $((i % 2)) -eq 0 ]; then export BENCH_LEG_ORDER=jf; else export BENCH_LEG_ORDER=fj; fi
      "$ROOT/suite/perf/run.sh"
      python3 "$ROOT/suite/merge.py"
    done
    (cd "$ROOT/suite/perf" && docker compose down -v >/dev/null 2>&1) || true
    exec python3 "$ROOT/suite/aggregate.py"
    ;;
  merge)   exec python3 "$ROOT/suite/merge.py" "$@" ;;
  gate)
    if [ "${1:-}" = "--measure" ]; then
      # Fresh measurement at reduced load (short windows, no warmup ramp, no
      # cold restarts), then merge, then check. Runs both legs so run.sh's
      # report step has both summaries; the gate itself only reads Ferrofin's.
      # EVERY leg-shaping override must reach the merge too: merge.py's
      # manifest check (A1) re-resolves RUN_TRANSCODE and BENCH_COLD_ENDPOINTS
      # to know which legs were part of this measurement — a mismatch makes it
      # demand legs that were deliberately skipped (review finding, round 1).
      shift
      GATE_SECS="${PERF_GATE_SECONDS:-$(cd "$ROOT/suite/perf" && python3 -c 'from config import CONFIG; print(CONFIG["PERF_GATE_SECONDS"])')}"
      export RUN_TRANSCODE=0 BENCH_COLD_ENDPOINTS=""
      BENCH_DURATION_SECS="$GATE_SECS" BENCH_GLOBAL_WARMUP_SECS=10 BENCH_WARMUP_SECS=5 \
        "$ROOT/suite/perf/run.sh"
      python3 "$ROOT/suite/merge.py"
    fi
    exec python3 "$ROOT/suite/gate.py" "$@"
    ;;
  *) usage ;;
esac
