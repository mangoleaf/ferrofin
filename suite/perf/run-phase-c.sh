#!/usr/bin/env bash
# Phase C — mixed contention run + whole-run memory footprint.
#
# Drives all endpoints concurrently (closed VU loop) to surface cross-endpoint
# contention that the isolated Phase A hides, and reads the container's cgroup
# memory.peak (kernel-accounted high-water mark) + anon working set afterwards —
# the fair Rust-vs-.NET footprint number (same external yardstick for both).
#
#   ./run-phase-c.sh                    both servers
#   BENCH_ONLY=ferrofin ./run-phase-c.sh
set -euo pipefail
cd "$(dirname "$0")"
# shellcheck source=../lib.sh
source ../lib.sh
suite_load_env
. ./_phase-common.sh
mkdir -p results/raw
export BENCH_VUS="${BENCH_VUS:-50}" BENCH_DURATION="${BENCH_DURATION:-30s}"

# Shared bring-up: library list + passthrough env (suite/lib.sh).
suite_build_libraries

cg() { docker compose exec -T "$1" cat "/sys/fs/cgroup/$2" </dev/null 2>/dev/null; }

mixed() {   # $1=service $2=port $3=target
  local svc="$1" base="http://localhost:$2" target="$3"
  bringup_scan "$svc" "$base" "$target" || return 0

  echo "   mixed ${BENCH_VUS}-VU load for ${BENCH_DURATION}"
  k6 run -e TARGET="$target" -e BASE_URL="$base" phase-c.js </dev/null || true

  # Whole-run peak (incl. scan) — the same yardstick for both runtimes. anon is
  # the working set (excludes reclaimable page cache).
  local peak anon; peak=$(cg "$svc" memory.peak || echo null); anon=$(cg "$svc" memory.stat | awk '/^anon /{print $2}')
  echo "   memory.peak=$((${peak:-0}/1048576)) MiB  anon=$((${anon:-0}/1048576)) MiB"
  echo "{\"target\":\"$target\",\"mem_peak\":${peak:-null},\"mem_anon\":${anon:-null}}" > "results/raw/phaseCmem-$target.json"
  docker compose stop "$svc" >/dev/null 2>&1 || true
}

[ "${BENCH_ONLY:-}" != "jellyfin" ] && mixed ferrofin "$FERROFIN_HOST_PORT" ferrofin
[ "${BENCH_ONLY:-}" != "ferrofin" ]   && mixed jellyfin "$JELLYFIN_HOST_PORT" jellyfin
docker compose down -v >/dev/null 2>&1 || true

echo ">> rendering Phase C report"
VERSION=$(git -C .. describe --tags --always 2>/dev/null || echo dev)
node render-phase-c.mjs "$VERSION" "$BENCH_VUS" "$BENCH_DURATION"
echo ">> wrote results/phaseC-${VERSION}.md"
