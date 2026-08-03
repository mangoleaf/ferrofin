#!/usr/bin/env bash
# suite/run.sh — the one entry point for the merged parity + perf suite (Plan 6).
#
#   suite/run.sh parity   both servers up   → sweep + reads + journeys + assets → ledger (+fingerprints)
#   suite/run.sh perf     one-at-a-time     → k6 load bench → per-endpoint latencies (+fingerprints)
#   suite/run.sh all      parity, then perf, same build + same fixture → merged run record
#   suite/run.sh merge    join the latest parity ledger + perf summaries into the run record
#   suite/run.sh gate [--measure|--rebaseline]   regression gate over the merged record
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
  parity)  exec "$ROOT/parity/sweep.sh" "$@" ;;
  perf)    exec "$ROOT/benchmark/run.sh" "$@" ;;
  all)
    "$ROOT/parity/sweep.sh"
    "$ROOT/benchmark/run.sh"
    python3 "$ROOT/suite/merge.py"
    ;;
  merge)   exec python3 "$ROOT/suite/merge.py" "$@" ;;
  gate)
    if [ "${1:-}" = "--measure" ]; then
      # Fresh measurement at reduced load (fast via VUs/duration), then merge, then check. Runs
      # both legs so run.sh's report step has both summaries; the gate itself only reads Hermit's.
      shift
      RUN_TRANSCODE=0 BENCH_VUS="${PERF_GATE_VUS:-10}" BENCH_DURATION="${PERF_GATE_SECONDS:-10}s" \
        "$ROOT/benchmark/run.sh"
      python3 "$ROOT/suite/merge.py"
    fi
    exec python3 "$ROOT/suite/gate.py" "$@"
    ;;
  *) usage ;;
esac
