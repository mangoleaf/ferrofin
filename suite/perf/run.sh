#!/usr/bin/env bash
# One command to (re)generate the Ferrofin-vs-Jellyfin benchmark. Run this every release.
#   ./run.sh
# Requires: docker, docker compose, jq, python3 on the host, plus vegeta (pinned in
# ./mise.toml — `mise install` here). ffmpeg only for gen-fixtures.sh.
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

# B1 (stale-binary guard): bake the host tree's identity into the image build and
# verify it back from the running server before any measurement. GIT_DESCRIBE
# flows: here → compose build arg → Dockerfile ENV → build.rs → binary →
# GET /health/live `build`. A poisoned build cache that serves an old binary
# then reports an old identity, and the run aborts instead of measuring it.
GIT_DESCRIBE=$(git -C ../.. describe --tags --always --dirty --abbrev=12 2>/dev/null || echo "")
export GIT_DESCRIBE
case "$GIT_DESCRIBE" in *-dirty) echo ">> NOTE: working tree is dirty — build identity is ${GIT_DESCRIBE}" ;; esac

# Assert the running Ferrofin is the binary we just asked for. `-dirty` is
# stripped from both sides: BENCH_SKIP_BUILD images come from a clean
# `git archive HEAD` while the host tree may carry in-flight edits.
verify_build() {  # $1=base url $2=target
  [ "$2" = "ferrofin" ] || return 0
  local reported expect
  reported=$(curl -sf "$1/health/live" | jq -r '.build // empty')
  echo "$reported" > "results/raw/$2-build.txt"
  expect="${GIT_DESCRIBE%-dirty}"
  if [ -z "$reported" ]; then
    echo "!! [$2] server did not report a build identity (/health/live) — refusing to measure an unverifiable binary" >&2
    exit 1
  fi
  if [ -n "$expect" ] && [ "${reported%-dirty}" != "$expect" ]; then
    echo "!! [$2] STALE BINARY: server reports build '${reported}' but the tree is '${GIT_DESCRIBE}'." >&2
    echo "!!   The image cache served an old binary (or BENCH_SKIP_BUILD points at an outdated image)." >&2
    echo "!!   Rebuild (docker compose build ferrofin) or prune the cache mounts, then re-run." >&2
    exit 1
  fi
  echo "   build verified: ${reported}"
}

mkdir -p results/raw fixtures/empty fixtures/media/movies fixtures/media/tv
# BENCH_ONLY=ferrofin|jellyfin re-runs one leg, keeping the other's raw results.
# ctx files are exempt from the wipe: they are provisioning STATE, not results
# — deleting one while its volume survives (BENCH_KEEP_DATA) made ready_ctx
# re-provision an already-provisioned server, duplicating every library
# (observed live: the item count doubled). bench() removes them explicitly
# whenever it actually wipes the volume.
if [ -z "${BENCH_ONLY:-}" ]; then find results/raw -name '*.json' ! -name '*-ctx.json' -delete 2>/dev/null || true; fi

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
  # BENCH_KEEP_DATA=1 (publish runs ≥2): reuse the scanned volume from the
  # previous run instead of wiping + rescanning identical media — the DB state
  # is identical by construction (write rows are state-preserving), and only
  # measurement noise needs to be independent across runs, not the scan.
  # compare.py's ready_ctx revalidates the saved token either way.
  if [ "${BENCH_KEEP_DATA:-0}" = "1" ]; then
    echo ">> [$target] start (keeping scanned volume from previous run)"
  else
    echo ">> [$target] clean start"
    docker compose down -v >/dev/null 2>&1 || true
    rm -f "results/raw/$target-ctx.json"   # fresh DB ⇒ saved tokens/ids are dead
  fi
  # BENCH_SKIP_BUILD=1: use the existing ferrofin-bench:local image (e.g. one built
  # from a clean `git archive HEAD` while the working tree carries in-flight edits).
  if [ "${BENCH_SKIP_BUILD:-0}" = "1" ]; then
    docker compose up -d "$svc"
  else
    docker compose up -d --build "$svc"
  fi

  local cold; cold=$(coldstart "$base"); echo "   cold-start: ${cold}s"
  echo "$cold" > "results/raw/$target-cold.txt"
  verify_build "$base" "$target"

  local ctok cuid

  : > "results/raw/$target-rss.txt"   # sampler appends; clear stale samples from prior runs
  sample_rss "$svc" "results/raw/$target-rss.txt" & local rss_pid=$!

  # compare.py exits non-zero when a leg couldn't hold its open-loop rate — that
  # invalidates the whole leg. The `if !` guard (not bare set -e) ensures the
  # background RSS sampler is reaped before we bail.
  if ! python3 compare.py --target "$target" --base "$base" 2>&1 | tee "results/raw/$target-bench.log"; then
    kill "$rss_pid" 2>/dev/null || true
    echo "!! [$target] bench leg failed — no record will be merged" >&2
    exit 3
  fi

  # Reuse the token compare.py minted during bring-up — provisioned (Jellyfin's bench
  # user is created there via the startup wizard) and unthrottled: the login-storm leg
  # hammers /Users/AuthenticateByName and a fresh post-load login 500s on Jellyfin.
  # compare.py persists it in results/raw/<target>-ctx.json (the old k6 setup() could
  # only smuggle it out via a console-log grep).
  ctok=$(jq -r '.token // empty' "results/raw/$target-ctx.json" 2>/dev/null)
  cuid=$(jq -r '.userId // empty' "results/raw/$target-ctx.json" 2>/dev/null)

  # Stop RSS sampling BEFORE the transcode phase: its ffmpeg child peaks ~1 GB on a
  # 4K encode and would drown the server-footprint number on both sides identically.
  kill "$rss_pid" 2>/dev/null || true

  [ "${RUN_TRANSCODE:-0}" = "1" ] && python3 ttfs.py --target "$target" --base "$base" || true
  suite_count_items "$base" "$ctok" "$cuid" > "results/raw/$target-count.txt" 2>/dev/null || echo "?" > "results/raw/$target-count.txt"
  # Perf-side body fingerprint, BOTH servers — merge.py compares Ferrofin's shape
  # against Jellyfin's from this same leg (same fresh-scan DB state), flagging
  # "fast because the body went hollow/differently-shaped" at bench time.
  # (Comparing against the parity pass false-flagged ~25 ops: parity's write
  # journeys leave play-state fields the fresh perf scan legitimately lacks.)
  mkdir -p ../results/raw
  python3 ../fingerprint.py capture "$base" "../results/raw/perf-fingerprints-$target.json" "$ctok" "$cuid" || true

  # H2: cold-request leg, LAST (it restarts the server, destroying warm state
  # everything above depends on). The server is restarted before EACH sentinel:
  # hitting one endpoint warms shared state (DB pool, page cache, JIT) for the
  # next, so per-endpoint cold needs a fresh process each time. Cold and warm
  # are published side by side, labeled — never blended.
  if [ -n "${BENCH_COLD_ENDPOINTS:-}" ]; then
    rm -f "results/raw/$target-cold-requests.json"
    for name in $BENCH_COLD_ENDPOINTS; do
      docker compose restart "$svc" >/dev/null 2>&1
      for _ in $(seq 1 240); do curl -sf "$base/System/Info/Public" >/dev/null 2>&1 && break; sleep 0.5; done
      # A failed probe leaves its endpoint missing — merge.py's manifest check
      # fails the run rather than shipping a record with a silent cold hole.
      python3 cold_probe.py --target "$target" --base "$base" --endpoint "$name" || true
    done
  fi

  # Login storm LAST — Jellyfin's brute-force limiter can lock the bench user
  # after the storm, poisoning anything measured later (it broke every cold
  # probe when the storm ran mid-leg). Nothing measures after this.
  if ! python3 login_storm.py --target "$target" --base "$base"; then
    echo "!! [$target] login storm could not hold its rate — leg fails" >&2
    docker compose stop "$svc" >/dev/null 2>&1 || true
    exit 3
  fi
  docker compose stop "$svc" >/dev/null 2>&1 || true
}

# F1 (fairness): the two legs are sequential on one host, so slow drift
# (thermal, background load) biases whichever side runs second. Single runs
# can't fix that; the publish loop alternates BENCH_LEG_ORDER per run so the
# drift cancels across the N aggregated runs instead of accumulating.
# Ports come from the same env the compose file maps (hardcoding them here
# while compose honored the override was an inconsistency — and running in an
# isolated COMPOSE_PROJECT_NAME + port pair is how a run survives another
# checkout's `docker compose down` on the shared default project).
FPORT="${FERROFIN_HOST_PORT:-18096}"
JPORT="${JELLYFIN_HOST_PORT:-18097}"
if [ "${BENCH_LEG_ORDER:-fj}" = "jf" ]; then
  if [ "${BENCH_ONLY:-}" != "ferrofin" ];   then bench jellyfin "$JPORT" jellyfin; fi
  if [ "${BENCH_ONLY:-}" != "jellyfin" ]; then bench ferrofin   "$FPORT" ferrofin;   fi
else
  if [ "${BENCH_ONLY:-}" != "jellyfin" ]; then bench ferrofin   "$FPORT" ferrofin;   fi
  if [ "${BENCH_ONLY:-}" != "ferrofin" ];   then bench jellyfin "$JPORT" jellyfin; fi
fi
if [ "${BENCH_KEEP_DATA:-0}" = "1" ]; then
  docker compose stop >/dev/null 2>&1 || true   # volumes live on for the next run
else
  docker compose down -v >/dev/null 2>&1 || true
fi

echo ">> rendering report"
VERSION=$(git -C .. describe --tags --always 2>/dev/null || echo dev)
DATE=$(date -u +%Y-%m-%dT%H:%MZ)
HOST="$(nproc) cores / $(free -h 2>/dev/null | awk '/Mem:/{print $2}' || echo '?') RAM, capped at ${BENCH_CPUS} cpus / ${BENCH_MEM}"
export VERSION DATE HOST JELLYFIN_IMAGE EXPECTED_ITEMS BENCH_VUS BENCH_DURATION BENCH_CPUS BENCH_MEM

H_RSS=$(peak results/raw/ferrofin-rss.txt); J_RSS=$(peak results/raw/jellyfin-rss.txt)
H_COLD=$(cat results/raw/ferrofin-cold.txt); J_COLD=$(cat results/raw/jellyfin-cold.txt)
H_N=$(cat results/raw/ferrofin-count.txt); J_N=$(cat results/raw/jellyfin-count.txt)
WARN=""
[ "$H_N" != "$J_N" ] && WARN="> ⚠️ Servers scanned different item counts (Ferrofin ${H_N} vs Jellyfin ${J_N}) — naming/resolver divergence on real files. Latency numbers then compare slightly different workloads; investigate before publishing."

# jq builds the endpoint comparison table from the two summaries.
TABLE=$(jq -rn --slurpfile h results/raw/ferrofin-summary.json --slurpfile j results/raw/jellyfin-summary.json '
  ($h[0].endpoints) as $H | ($j[0].endpoints) as $J |
  ($H|keys_unsorted[]) as $k |
  (if $H[$k].p50 and $J[$k].p50 then "\((($J[$k].p50)/($H[$k].p50)*100|round)/100)x" else "n/a" end) as $spd |
  "| `\($k)` | \($H[$k].p50) / \($H[$k].p95) / \($H[$k].p99) | \($J[$k].p50) / \($J[$k].p95) / \($J[$k].p99) | \($H[$k].rps) vs \($J[$k].rps) | \($H[$k].okPct)% / \($J[$k].okPct)% | \($spd) |"
')

OUT="results/${VERSION}.md"
{
cat <<EOF
# Ferrofin vs Jellyfin — benchmark

- **Ferrofin:** \`${VERSION}\`  **Jellyfin:** \`${JELLYFIN_IMAGE}\`
- **When:** ${DATE}
- **Host:** ${HOST}
- **Library:** ${H_N} items (Ferrofin) / ${J_N} (Jellyfin) · **Load:** open-loop, per-endpoint arrival rates (rates.json or BENCH_RATE) × ${BENCH_DURATION_SECS:-30}s/endpoint
- Method & caveats: see [README](../README.md).

${WARN}

## Latency (ms, p50 / p95 / p99) and throughput

| Endpoint | Ferrofin | Jellyfin | RPS (H vs J) | 200-rate (H / J) | p50 speedup |
|---|---|---|---|---|---|
${TABLE}

> "speedup" = Jellyfin p50 ÷ Ferrofin p50 (>1 means Ferrofin is faster). Latency is recorded for
> expected-status responses only (200, or 204 for the playstate write); a rate below 100% means
> that endpoint partly errored on that server — treat its row as a parity bug to chase, not a
> performance signal.

## Footprint

| Metric | Ferrofin | Jellyfin |
|---|---|---|
| Cold start (container → first 200) | ${H_COLD}s | ${J_COLD}s |
| Peak anon memory (cache-excluded; scan + load, incl. ffprobe children) | ${H_RSS} MiB | ${J_RSS} MiB |
| Items scanned | ${H_N} | ${J_N} |
EOF
if [ "${RUN_TRANSCODE:-0}" = "1" ] && [ -f results/raw/ferrofin-transcode.json ]; then
  fmt_ttfs() { jq -r --arg m "$2" '.[$m] | if . then "\(.med) ms (\(.min)–\(.max), \(.runs) runs)" else "N/A" end' "$1"; }
  printf '| HLS play-start, stream-copy remux (median TTFS) | %s | %s |\n' \
    "$(fmt_ttfs results/raw/ferrofin-transcode.json copy)" "$(fmt_ttfs results/raw/jellyfin-transcode.json copy)"
  printf '| HLS play-start, forced 4K HEVC→H.264 encode (median TTFS) | %s | %s |\n' \
    "$(fmt_ttfs results/raw/ferrofin-transcode.json encode)" "$(fmt_ttfs results/raw/jellyfin-transcode.json encode)"
fi
} > "$OUT"

cp "$OUT" results/latest.md
echo ">> wrote $OUT (and results/latest.md)"
