#!/usr/bin/env bash
# Orchestrator for the API parity comparison: bring BOTH servers up at once (the load bench runs
# them one at a time), then run parity.js which does the actual fetch+diff. Thin on purpose —
# all comparison logic is JS (parity.js), this only does docker + env, matching run.sh.
#   ./parity.sh                      build + run
#   BENCH_SKIP_BUILD=1 ./parity.sh   reuse the existing hermit-bench:local image
set -euo pipefail
cd "$(dirname "$0")"
# PARITY_ENV=.env.loop selects a small synthetic-only dataset for fast fix-loop iterations.
ENVF="${PARITY_ENV:-.env}"
set -a; [ -f "$ENVF" ] || cp .env.example "$ENVF"; . "./$ENVF"; set +a
mkdir -p results/raw

# Library list — same construction as run.sh (passed to parity.js via LIBRARIES; JS parses it,
# so no bash word-splitting on names like "Movies (synth)").
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

# Synthetic fixtures only when padding is requested and none exist yet (same as run.sh).
if { [ "${FIXTURE_MOVIES:-0}" -gt 0 ] || [ "${FIXTURE_SERIES:-0}" -gt 0 ]; } && \
   [ -z "$(find fixtures/media -type f 2>/dev/null | head -1)" ]; then ./gen-fixtures.sh; fi

echo ">> starting both servers"
docker compose down -v >/dev/null 2>&1 || true
if [ "${BENCH_SKIP_BUILD:-0}" = 1 ]; then docker compose up -d hermit jellyfin
else docker compose up -d --build hermit jellyfin; fi

wait200() { local i; for i in $(seq 1 120); do curl -sf "$1/System/Info/Public" >/dev/null 2>&1 && return; sleep 0.5; done; echo "$2 never came up"; exit 1; }
wait200 http://localhost:18096 hermit
wait200 http://localhost:18097 jellyfin

# parity.js emits the finished report as one base64 line (k6 file-writing can't see setup data).
HERMIT_URL=http://localhost:18096 JELLYFIN_URL=http://localhost:18097 k6 run parity.js 2>&1 | tee results/raw/parity-k6.log
docker compose down -v >/dev/null 2>&1 || true

payload=$(grep -oE '===PARITY_MD_BASE64===[A-Za-z0-9+/=]+===END===' results/raw/parity-k6.log | tail -1 \
  | sed -E 's/^===PARITY_MD_BASE64===//; s/===END===$//')
[ -n "$payload" ] || { echo "!! no parity payload in k6 output — see results/raw/parity-k6.log"; exit 1; }
echo "$payload" | base64 -d > results/PARITY.md
echo ">> wrote results/PARITY.md"
