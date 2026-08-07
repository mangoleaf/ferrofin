#!/usr/bin/env bash
# One command to (re)generate the Hermit-vs-Jellyfin benchmark. Run this every release.
#   ./run.sh
# Requires: docker, docker compose, k6, jq on the host. ffmpeg only for gen-fixtures.sh.
set -euo pipefail
cd "$(dirname "$0")"

# --- pure helpers, kept above the source guard so run.bats can exercise them ---
# memory.stat (cgroup v2) on stdin -> anonymous memory in MiB, one number per line.
# `/^anon /` (trailing space) matches only `anon`, never `anon_thp`/`file` etc.
anon_mib() { awk '/^anon /{printf "%.1f\n", $2/1048576}'; }
# Peak (max) value from a file of plain numbers; "?" if the file is unreadable.
peak() { awk '{if($1+0>m)m=$1+0} END{printf "%.0f", m}' "$1" 2>/dev/null || echo "?"; }
# Tests source this file to get the helpers, then stop here before running a benchmark.
if [ -n "${BENCH_TEST_SOURCE:-}" ]; then return 0; fi

# shellcheck source=../lib.sh
source ../lib.sh
suite_load_env .env
suite_mint_device_id run

mkdir -p results/raw fixtures/empty fixtures/media/movies fixtures/media/tv
# BENCH_ONLY=hermit|jellyfin re-runs one leg, keeping the other's raw results.
if [ -z "${BENCH_ONLY:-}" ]; then rm -f results/raw/*.json; fi

# Library list + synthetic fixtures (shared bring-up — see suite/lib.sh).
suite_build_libraries
echo ">> libraries: $LIBRARIES"
suite_gen_fixtures

# Wait for container start -> first 200, return elapsed seconds (cold-start metric).
coldstart() {  # $1=base url
  local start now; start=$(date +%s.%N)
  for _ in $(seq 1 120); do
    curl -sf "$1/System/Info/Public" >/dev/null 2>&1 && { now=$(date +%s.%N); awk -v a="$start" -v b="$now" 'BEGIN{printf "%.1f", b-a}'; return; }
    sleep 0.5
  done
  echo "NaN"
}

# Sample the container's anonymous memory every 1s into a file (MiB), until told to stop.
# We read cgroup-v2 memory.stat `anon` (page-cache EXCLUDED) instead of `docker stats`
# MemUsage: MemUsage counts file-backed page cache, and ffprobe reading the whole media
# library during the scan drags GiBs of media into cache billed to the container — that
# measured the kernel's cache of your files, not the server's working set. `anon` covers
# all processes in the container cgroup (server threads + ffprobe children), cache-free.
sample_rss() {  # $1=service $2=outfile
  while :; do
    docker compose exec -T "$1" cat /sys/fs/cgroup/memory.stat 2>/dev/null \
      | anon_mib >> "$2" || true
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

  local ctok cuid

  : > "results/raw/$target-rss.txt"   # sampler appends; clear stale samples from prior runs
  sample_rss "$svc" "results/raw/$target-rss.txt" & local rss_pid=$!

  TARGET="$target" BASE_URL="$base" k6 run scenario.js 2>&1 | tee "results/raw/$target-k6.log"

  # Reuse the token scenario.js minted in setup() — provisioned (Jellyfin's bench user is
  # created there via the startup wizard) and unthrottled. Re-authenticating here instead
  # would 500 against Jellyfin: the auth_login scenario hammers /Users/AuthenticateByName
  # at ${BENCH_VUS} VUs (login drops to ~7% success) and a fresh post-load login fails.
  # setup() console.logs "CAPTURE_CREDS <token> <userId>"; empty ⇒ consumers self-auth.
  local creds; creds=$(grep -oE 'CAPTURE_CREDS [^ "]+ [^ "]+' "results/raw/$target-k6.log" | tail -1)
  ctok=$(awk '{print $2}' <<<"$creds"); cuid=$(awk '{print $3}' <<<"$creds")

  # Stop RSS sampling BEFORE the transcode phase: its ffmpeg child peaks ~1 GB on a
  # 4K encode and would drown the server-footprint number on both sides identically.
  kill "$rss_pid" 2>/dev/null || true

  [ "${RUN_TRANSCODE:-0}" = "1" ] && TARGET="$target" BASE_URL="$base" CAP_TOKEN="$ctok" CAP_UID="$cuid" k6 run transcode.js || true
  suite_count_items "$base" "$ctok" "$cuid" > "results/raw/$target-count.txt" 2>/dev/null || echo "?" > "results/raw/$target-count.txt"
  # Perf-side body fingerprint, BOTH servers — merge.py compares Hermit's shape
  # against Jellyfin's from this same leg (same fresh-scan DB state), flagging
  # "fast because the body went hollow/differently-shaped" at bench time.
  # (Comparing against the parity pass false-flagged ~25 ops: parity's write
  # journeys leave play-state fields the fresh perf scan legitimately lacks.)
  mkdir -p ../results/raw
  python3 ../fingerprint.py capture "$base" "../results/raw/perf-fingerprints-$target.json" "$ctok" "$cuid" || true
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
> expected-status responses only (200, or 204 for the playstate write); a rate below 100% means
> that endpoint partly errored on that server — treat its row as a parity bug to chase, not a
> performance signal.

## Footprint

| Metric | Hermit | Jellyfin |
|---|---|---|
| Cold start (container → first 200) | ${H_COLD}s | ${J_COLD}s |
| Peak anon memory (cache-excluded; scan + load, incl. ffprobe children) | ${H_RSS} MiB | ${J_RSS} MiB |
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
