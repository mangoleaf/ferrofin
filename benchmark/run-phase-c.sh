#!/usr/bin/env bash
# Phase C — mixed contention run + whole-run memory footprint.
#
# Drives all endpoints concurrently (closed VU loop) to surface cross-endpoint
# contention that the isolated Phase A hides, and reads the container's cgroup
# memory.peak (kernel-accounted high-water mark) + anon working set afterwards —
# the fair Rust-vs-.NET footprint number (same external yardstick for both).
#
#   ./run-phase-c.sh                    both servers
#   BENCH_ONLY=hermit ./run-phase-c.sh
set -euo pipefail
cd "$(dirname "$0")"
set -a; [ -f .env ] || cp .env.example .env; . ./.env; set +a
mkdir -p results/raw
export BENCH_VUS="${BENCH_VUS:-50}" BENCH_DURATION="${BENCH_DURATION:-30s}"

# Library list — same construction as run.sh (parsed by k6).
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

cg() { docker compose exec -T "$1" cat "/sys/fs/cgroup/$2" </dev/null 2>/dev/null; }

mixed() {   # $1=service $2=port $3=target
  local svc="$1" base="http://localhost:$2" target="$3"
  echo ">> [$target] up + scan"
  docker compose down -v >/dev/null 2>&1 || true
  if [ "${BENCH_SKIP_BUILD:-0}" = "1" ]; then docker compose up -d "$svc"; else docker compose up -d --build "$svc"; fi
  until curl -sf "$base/System/Info/Public" >/dev/null 2>&1; do sleep 1; done
  k6 run -e TARGET="$target" -e BASE_URL="$base" bootstrap.js

  echo "   mixed ${BENCH_VUS}-VU load for ${BENCH_DURATION}"
  k6 run -e TARGET="$target" -e BASE_URL="$base" phase-c.js </dev/null || true

  # Whole-run peak (incl. scan) — the same yardstick for both runtimes. anon is
  # the working set (excludes reclaimable page cache).
  local peak anon; peak=$(cg "$svc" memory.peak || echo null); anon=$(cg "$svc" memory.stat | awk '/^anon /{print $2}')
  echo "   memory.peak=$((${peak:-0}/1048576)) MiB  anon=$((${anon:-0}/1048576)) MiB"
  echo "{\"target\":\"$target\",\"mem_peak\":${peak:-null},\"mem_anon\":${anon:-null}}" > "results/raw/phaseCmem-$target.json"
  docker compose stop "$svc" >/dev/null 2>&1 || true
}

[ "${BENCH_ONLY:-}" != "jellyfin" ] && mixed hermit   18096 hermit
[ "${BENCH_ONLY:-}" != "hermit" ]   && mixed jellyfin 18097 jellyfin
docker compose down -v >/dev/null 2>&1 || true

echo ">> rendering Phase C report"
VERSION=$(git -C .. describe --tags --always 2>/dev/null || echo dev)
node render-phase-c.mjs "$VERSION" "$BENCH_VUS" "$BENCH_DURATION"
echo ">> wrote results/phaseC-${VERSION}.md"
