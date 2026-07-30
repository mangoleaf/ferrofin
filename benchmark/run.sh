#!/usr/bin/env bash
# One command to (re)generate the Hermit-vs-Jellyfin benchmark. Run this every release.
#   ./run.sh
# Requires: docker, docker compose, k6, jq on the host. ffmpeg only for gen-fixtures.sh.
set -euo pipefail
cd "$(dirname "$0")"
set -a; [ -f .env ] || cp .env.example .env; . ./.env; set +a

mkdir -p results/raw fixtures/empty fixtures/media/movies fixtures/media/tv
# BENCH_ONLY=hermit|jellyfin re-runs one leg, keeping the other's raw results.
if [ -z "${BENCH_ONLY:-}" ]; then rm -f results/raw/*.json; fi

# Build the library list from your real media (REAL_MEDIA_DIR) and/or synthetic padding.
# Same JSON drives provisioning on both servers.
LIBS="["; sep=""
[ -n "${REAL_MEDIA_DIR:-}" ] && { LIBS="$LIBS${sep}{\"name\":\"Movies\",\"type\":\"movies\",\"path\":\"/media/movies-real\"}"; sep=","; }
[ -n "${REAL_TV_DIR:-}" ]    && { LIBS="$LIBS${sep}{\"name\":\"Shows\",\"type\":\"tvshows\",\"path\":\"/media/tv-real\"}"; sep=","; }
if [ -n "${REAL_MEDIA_DIR:-}" ] || [ -n "${REAL_TV_DIR:-}" ]; then
  EXPECTED_ITEMS=0   # real count is unknown up front; scenario waits for the count to settle
else
  EXPECTED_ITEMS=$(( FIXTURE_MOVIES + FIXTURE_SERIES * FIXTURE_EPISODES_PER_SERIES ))
fi
[ "${FIXTURE_MOVIES:-0}" -gt 0 ] && { LIBS="$LIBS${sep}{\"name\":\"Movies (synth)\",\"type\":\"movies\",\"path\":\"/media/synth/movies\"}"; sep=","; }
[ "${FIXTURE_SERIES:-0}" -gt 0 ] && { LIBS="$LIBS${sep}{\"name\":\"Shows (synth)\",\"type\":\"tvshows\",\"path\":\"/media/synth/tv\"}"; sep=","; }
LIBS="$LIBS]"
[ "$LIBS" = "[]" ] && { echo "No media: set REAL_MEDIA_DIR or FIXTURE_MOVIES>0 in .env"; exit 1; }

export LIBRARIES="$LIBS" REAL_MEDIA_DIR REAL_TV_DIR BENCH_VUS BENCH_DURATION BENCH_ADMIN_USER BENCH_ADMIN_PASSWORD EXPECTED_ITEMS BENCH_WARMUP_SECONDS
echo ">> libraries: $LIBRARIES"

# Synthetic fixtures only when padding is requested.
if { [ "${FIXTURE_MOVIES:-0}" -gt 0 ] || [ "${FIXTURE_SERIES:-0}" -gt 0 ]; } && \
   [ -z "$(find fixtures/media -type f 2>/dev/null | head -1)" ]; then
  echo ">> generating synthetic fixtures"; ./gen-fixtures.sh
fi

# Authoritative item count for the fairness check (naming resolvers can diverge on real files).
count_items() {  # $1=base url
  local resp tok uid
  resp=$(curl -sf -X POST "$1/Users/AuthenticateByName" -H 'Content-Type: application/json' \
    -H 'Authorization: MediaBrowser Client="bench", Device="bench", DeviceId="bench", Version="1.0"' \
    -d "{\"Username\":\"$BENCH_ADMIN_USER\",\"Pw\":\"$BENCH_ADMIN_PASSWORD\"}") || { echo "?"; return; }
  tok=$(echo "$resp" | jq -r .AccessToken); uid=$(echo "$resp" | jq -r .User.Id)
  curl -sf "$1/Items?userId=$uid&Recursive=true&IncludeItemTypes=Movie,Episode&Limit=0" \
    -H "Authorization: MediaBrowser Token=\"$tok\", Client=\"bench\", Device=\"bench\", DeviceId=\"bench\", Version=\"1.0\"" \
    | jq -r '.TotalRecordCount // "?"'
}

# Wait for container start -> first 200, return elapsed seconds (cold-start metric).
coldstart() {  # $1=base url
  local start now; start=$(date +%s.%N)
  for _ in $(seq 1 120); do
    curl -sf "$1/System/Info/Public" >/dev/null 2>&1 && { now=$(date +%s.%N); awk -v a="$start" -v b="$now" 'BEGIN{printf "%.1f", b-a}'; return; }
    sleep 0.5
  done
  echo "NaN"
}

# Sample container memory every 1s into a file until told to stop; report peak MiB.
sample_rss() {  # $1=service $2=outfile
  while :; do
    docker compose stats --no-stream --format '{{.Name}} {{.MemUsage}}' 2>/dev/null \
      | awk -v s="$1" '$0 ~ s {print $2}' >> "$2" || true
    sleep 1
  done
}

bench() {  # $1=service $2=port $3=TARGET
  local svc="$1" base="http://localhost:$2" target="$3"
  echo ">> [$target] clean start"
  docker compose down -v >/dev/null 2>&1 || true
  # BENCH_SKIP_BUILD=1: use the existing hermit-bench:local image (e.g. one built
  # from a clean `git archive HEAD` while the working tree carries in-flight edits).
  if [ "${BENCH_SKIP_BUILD:-0}" = "1" ]; then
    docker compose up -d "$svc"
  else
    docker compose up -d --build "$svc"
  fi

  local cold; cold=$(coldstart "$base"); echo "   cold-start: ${cold}s"
  echo "$cold" > "results/raw/$target-cold.txt"

  sample_rss "$svc" "results/raw/$target-rss.txt" & local rss_pid=$!

  TARGET="$target" BASE_URL="$base" k6 run scenario.js

  # Stop RSS sampling BEFORE the transcode phase: its ffmpeg child peaks ~1 GB on a
  # 4K encode and would drown the server-footprint number on both sides identically.
  kill "$rss_pid" 2>/dev/null || true

  [ "${RUN_TRANSCODE:-0}" = "1" ] && TARGET="$target" BASE_URL="$base" k6 run transcode.js || true
  count_items "$base" > "results/raw/$target-count.txt" 2>/dev/null || echo "?" > "results/raw/$target-count.txt"
  docker compose stop "$svc" >/dev/null 2>&1 || true
}

if [ "${BENCH_ONLY:-}" != "jellyfin" ]; then bench hermit   18096 hermit;   fi
if [ "${BENCH_ONLY:-}" != "hermit" ];   then bench jellyfin 18097 jellyfin; fi
docker compose down -v >/dev/null 2>&1 || true

echo ">> rendering report"
VERSION=$(git -C .. describe --tags --always 2>/dev/null || echo dev)
DATE=$(date -u +%Y-%m-%dT%H:%MZ)
HOST="$(nproc) cores / $(free -h 2>/dev/null | awk '/Mem:/{print $2}' || echo '?') RAM, capped at ${BENCH_CPUS} cpus / ${BENCH_MEM}"
export VERSION DATE HOST JELLYFIN_IMAGE EXPECTED_ITEMS BENCH_VUS BENCH_DURATION BENCH_CPUS BENCH_MEM

peak() { awk '{gsub(/MiB|GiB/,"",$1); v=$1; if($0 ~ /GiB/) v*=1024; if(v>m)m=v} END{printf "%.0f", m}' "$1" 2>/dev/null || echo "?"; }
H_RSS=$(peak results/raw/hermit-rss.txt); J_RSS=$(peak results/raw/jellyfin-rss.txt)
H_COLD=$(cat results/raw/hermit-cold.txt); J_COLD=$(cat results/raw/jellyfin-cold.txt)
H_N=$(cat results/raw/hermit-count.txt); J_N=$(cat results/raw/jellyfin-count.txt)
WARN=""
[ "$H_N" != "$J_N" ] && WARN="> ⚠️ Servers scanned different item counts (Hermit ${H_N} vs Jellyfin ${J_N}) — naming/resolver divergence on real files. Latency numbers then compare slightly different workloads; investigate before publishing."

# jq builds the endpoint comparison table from the two summaries.
TABLE=$(jq -rn --slurpfile h results/raw/hermit-summary.json --slurpfile j results/raw/jellyfin-summary.json '
  ($h[0].endpoints) as $H | ($j[0].endpoints) as $J |
  ($H|keys_unsorted[]) as $k |
  (if $H[$k].p50 and $J[$k].p50 then "\((($J[$k].p50)/($H[$k].p50)*100|round)/100)x" else "n/a" end) as $spd |
  "| `\($k)` | \($H[$k].p50) / \($H[$k].p95) / \($H[$k].p99) | \($J[$k].p50) / \($J[$k].p95) / \($J[$k].p99) | \($H[$k].rps) vs \($J[$k].rps) | \($H[$k].okPct)% / \($J[$k].okPct)% | \($spd) |"
')

OUT="results/${VERSION}.md"
{
cat <<EOF
# Hermit vs Jellyfin — benchmark

- **Hermit:** \`${VERSION}\`  **Jellyfin:** \`${JELLYFIN_IMAGE}\`
- **When:** ${DATE}
- **Host:** ${HOST}
- **Library:** ${H_N} items (Hermit) / ${J_N} (Jellyfin) · **Load:** ${BENCH_VUS} VUs × ${BENCH_DURATION}/endpoint
- Method & caveats: see [README](../README.md).

${WARN}

## Latency (ms, p50 / p95 / p99) and throughput

| Endpoint | Hermit | Jellyfin | RPS (H vs J) | 200-rate (H / J) | p50 speedup |
|---|---|---|---|---|---|
${TABLE}

> "speedup" = Jellyfin p50 ÷ Hermit p50 (>1 means Hermit is faster). Latency is recorded for
> 200 responses only; a 200-rate below 100% means that endpoint partly errored on that server —
> treat its row as a parity bug to chase, not a performance signal.

## Footprint

| Metric | Hermit | Jellyfin |
|---|---|---|
| Cold start (container → first 200) | ${H_COLD}s | ${J_COLD}s |
| Peak RSS (scan + load, incl. ffprobe children) | ${H_RSS} MiB | ${J_RSS} MiB |
| Items scanned | ${H_N} | ${J_N} |
EOF
if [ "${RUN_TRANSCODE:-0}" = "1" ] && [ -f results/raw/hermit-transcode.json ]; then
  fmt_ttfs() { jq -r --arg m "$2" '.[$m] | if . then "\(.med) ms (\(.min)–\(.max), \(.runs) runs)" else "N/A" end' "$1"; }
  printf '| HLS play-start, stream-copy remux (median TTFS) | %s | %s |\n' \
    "$(fmt_ttfs results/raw/hermit-transcode.json copy)" "$(fmt_ttfs results/raw/jellyfin-transcode.json copy)"
  printf '| HLS play-start, forced 4K HEVC→H.264 encode (median TTFS) | %s | %s |\n' \
    "$(fmt_ttfs results/raw/hermit-transcode.json encode)" "$(fmt_ttfs results/raw/jellyfin-transcode.json encode)"
fi
} > "$OUT"

cp "$OUT" results/latest.md
echo ">> wrote $OUT (and results/latest.md)"
