#!/usr/bin/env bash
# The comparison run (PLAN_BENCHMARK_V3 §5). For each server, alone on its cores:
#   provision (fresh copy of the test data) → drain → counts → shape → cold start
#   → unloaded / loaded / stress windows (+ memory sampler) → steady window → TTFS.
# Every phase writes its file the moment it ends and fails on its own: a broken phase
# is reported and the next one runs; report.py renders whatever exists.
#
#   run.sh [--testdata DIR] [--servers jellyfin,jellyfin12,ferrofin]
#          [--only counts|shape|coldstart|unloaded|loaded|stress|ttfs] [--rate N] [--out DIR]
#
# Requires: docker, k6, python3, jq; images jellyfin/jellyfin:10.11.8,
# jellyfin/jellyfin:12.0-rc7 and ferrofin:bench (docker build -t ferrofin:bench .).
set -euo pipefail
CALLER=$PWD
cd "$(dirname "$0")/.."

# ── tunables (§2) ──────────────────────────────────────────────────────────
WARMUP_S=${WARMUP_S:-30}        # discarded seconds at the same rate before each window
WINDOW_S=${WINDOW_S:-120}       # measured seconds per load level
SETTLE_S=${SETTLE_S:-30}        # idle after scheduled tasks report idle, before any window
STEADY_S=${STEADY_S:-60}        # idle after load; steady memory = median over this window
MEM_SAMPLE_MS=${MEM_SAMPLE_MS:-100}
POLL_MS=${POLL_MS:-10}          # cold-start readiness poll
RESTARTS=${RESTARTS:-5}
TTFS_REPS=${TTFS_REPS:-5}
CORE_IDLE_MIN=${CORE_IDLE_MIN:-0.90}  # the server/client cores must be this idle before a run starts
RATE_UNLOADED=${RATE_UNLOADED:-1}   # screens per second
RATE_LOADED=${RATE_LOADED:-5}
#: The third level exists to push the servers past a comfortable browse: 25 screens/s
#: is ~120 API requests/s plus ~245 poster fetches/s. It is a fixed rate like the others
#: rather than a ramp — finding one server's knee is a different question from comparing
#: two servers doing the same work.
#:
#: Why 25 and not more: measured mean container CPU over the loaded window, as a share of
#: the 8-core cpuset, was 1.03 cores for Jellyfin 12.0-rc7, 0.45 for 10.11.8 and 0.19 for
#: Ferrofin. Scaled to 25 screens/s that is roughly 65 %, 28 % and 12 % of the box — so
#: this is close to the highest fixed rate at which the SLOWEST server is still under
#: saturation, which is what keeps the comparison fair. Raising it further stresses only
#: Ferrofin (it would need ~150-200 screens/s to bend) while the oracle is already past
#: its knee, and past that point the two columns are no longer measuring the same thing.
#: Note k6's dropped-iteration flag guards the CLIENT, not the server, and at this rate
#: it will not fire — it is not the safety net for choosing this number.
RATE_STRESS=${RATE_STRESS:-25}
export SERVER_CPUS=${SERVER_CPUS:-8-15}    # cpuset for the server under test
export CLIENT_CPUS=${CLIENT_CPUS:-16-19}   # cpuset for k6 / the python clients / the sampler
MEMORY=${MEMORY:-8g}                # cgroup limit, swap disabled (part of the memory number's definition)

TESTDATA=$PWD/bench/testdata; SERVERS=jellyfin,jellyfin12,ferrofin; ONLY=""; OUT=""
while [ $# -gt 0 ]; do case "$1" in
  --testdata) TESTDATA=$2; shift 2;; --servers) SERVERS=$2; shift 2;; --only) ONLY=$2; shift 2;;
  # --rate sets the LOADED level only; RATE_UNLOADED and RATE_STRESS are env vars.
  --rate) RATE_LOADED=$2; shift 2;;
  --out) OUT=$2; shift 2;;
  *) echo "unknown $1" >&2; exit 2;;
esac; done
abs() { case "$1" in /*) echo "$1";; *) echo "$CALLER/$1";; esac; }   # user paths are relative to the caller's shell
TESTDATA=$(realpath -m "$(abs "$TESTDATA")")
SHA=$(git rev-parse --short HEAD 2>/dev/null || echo nogit)
OUT=$(realpath -m "$(abs "${OUT:-$PWD/bench/runs/$(date +%Y%m%d-%H%M)-$SHA}")")
for p in "$TESTDATA" "$OUT"; do case "$p" in /mnt/mangonas*|/mnt/nvme0/k3s*) echo "refusing to touch $p" >&2; exit 1;; esac; done

die() { echo "run: $*" >&2; exit 1; }
image_of() { case "$1" in jellyfin) echo jellyfin/jellyfin:10.11.8;; jellyfin12) echo jellyfin/jellyfin:12.0-rc7;; ferrofin) echo ferrofin:bench;; *) die "unknown server $1";; esac; }
port_of() { case "$1" in jellyfin) echo 18101;; jellyfin12) echo 18102;; ferrofin) echo 18103;; esac; }
want() { [ -z "$ONLY" ] || [ "$ONLY" = "$1" ]; }

# ── preflight ──────────────────────────────────────────────────────────────
[ -f "$TESTDATA/ids.json" ] || die "no $TESTDATA/ids.json — build the test data first (bench/testdata/build.sh)"
for t in docker k6 python3 jq taskset; do command -v $t >/dev/null || die "$t not installed"; done
for s in ${SERVERS//,/ }; do docker image inspect "$(image_of "$s")" >/dev/null 2>&1 || die "image $(image_of "$s") missing"; done
[ -z "$(docker ps -aq --filter name=^bench-)" ] || die "a bench-* container exists (docker rm -f it first)"
idle=$(python3 bench/mem_sample.py --check "$SERVER_CPUS,$CLIENT_CPUS")
awk -v i="$idle" -v m="$CORE_IDLE_MIN" -v c="$SERVER_CPUS,$CLIENT_CPUS" 'BEGIN { if (i < m) { printf "cores %s are only %.0f%% idle (need %.0f%%) — something else is using them\n", c, i*100, m*100; exit 1 } }' || exit 1
mkdir -p "$OUT"
IDS=$TESTDATA/ids.json; U=$(jq -r .user "$IDS"); TOK=$(jq -r .token "$IDS")
AUTH="Authorization: MediaBrowser Client=\"bench\", Device=\"bench\", DeviceId=\"bench-run\", Version=\"3\", Token=\"$TOK\""
jq -n --arg sha "$SHA" --arg host "$(uname -srm)" --arg cpu "$(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | xargs)" \
  --arg mem "$MEMORY" --arg cpus "$SERVER_CPUS" --arg k6 "$(k6 version | head -1)" --arg testdata "$(jq -c .counts "$IDS")" \
  --argjson rate_unloaded "$RATE_UNLOADED" --argjson rate_loaded "$RATE_LOADED" --argjson rate_stress "$RATE_STRESS" \
  --argjson window "$WINDOW_S" --argjson sample_ms "$MEM_SAMPLE_MS" \
  '{sha:$sha, host:$host, cpu:$cpu, memory_limit:$mem, server_cpus:$cpus, k6:$k6, testdata_counts:($testdata|fromjson),
    rate_unloaded:$rate_unloaded, rate_loaded:$rate_loaded, rate_stress:$rate_stress,
    window_s:$window, mem_sample_ms:$sample_ms, date: (now|todate)}' > "$OUT/run.json"

api() {  # GET with the bench token; retries through a starting server's 503s/refusals
  local _; for _ in $(seq 1 120); do curl -sf -H "$AUTH" "$URL$1" && return 0; sleep 1; done
  echo "run: $NAME: GET $1 kept failing" >&2; return 1
}
wait_ready() { local _; for _ in $(seq 1 600); do curl -sf "$URL/System/Info/Public" >/dev/null 2>&1 && return; sleep 1; done; echo "run: $NAME never became ready" >&2; return 1; }
drain() {  # every scheduled task idle, then settle (before EVERY window)
  local t0=$SECONDS busy
  while :; do
    busy=$(api "/ScheduledTasks" | jq -r '[.[] | select(.State != "Idle") | .Name] | join(",")') || return 1
    [ -z "$busy" ] && break
    echo "  drain: waiting for [$busy] ($((SECONDS - t0))s)"; sleep 5
  done
  sleep "$SETTLE_S"
}
count() { api "$1" | jq -r 'if type=="array" then length else (.TotalRecordCount // (.Items|length)) end'; }
disable_plugins() {  # core Ferrofin only: every compiled-in extension off, no WASM plugin loadable (owner rule)
  local wasm; wasm=$(find "$1" -name '*.wasm' 2>/dev/null | head -1)
  [ -z "$wasm" ] || die "$NAME: a WASM plugin is present in the config copy ($wasm) — the benchmark measures core Ferrofin only"
  local id ver
  while read -r id ver; do
    curl -sf -X POST -H "$AUTH" "$URL/Plugins/$id/$ver/Disable" >/dev/null || die "$NAME: could not disable plugin $id $ver"
  done < <(api "/Plugins" | jq -r '.[] | "\(.Id) \(.Version)"')
  api "/Plugins" > "$D/plugins.json"
  local on; on=$(jq -r '[.[] | select(.Status != "Disabled") | .Name] | join(", ")' "$D/plugins.json")
  [ -z "$on" ] || die "$NAME: plugins still enabled after disabling: $on"
  echo "  plugins: $(jq -r 'map(.Name) | join(", ")' "$D/plugins.json") — all disabled"
}
k6run() { taskset -c "$CLIENT_CPUS" k6 run --quiet -e URL="$URL" -e IDS="$IDS" "$@" bench/screens.js; }
phase() { echo "  -- $1"; if ! "phase_$1"; then echo "  ✗ phase $1 failed for $NAME (see $D/server.log)" >&2; fi; }

phase_counts() {
  local movies series episodes albums tracks artists persons genres studios resume nextup latest
  movies=$(count "/Items?userId=$U&recursive=true&includeItemTypes=Movie&limit=0") &&
  series=$(count "/Items?userId=$U&recursive=true&includeItemTypes=Series&limit=0") &&
  episodes=$(count "/Items?userId=$U&recursive=true&includeItemTypes=Episode&limit=0") &&
  albums=$(count "/Items?userId=$U&recursive=true&includeItemTypes=MusicAlbum&limit=0") &&
  tracks=$(count "/Items?userId=$U&recursive=true&includeItemTypes=Audio&limit=0") &&
  artists=$(count "/Artists?userId=$U&limit=0") && persons=$(count "/Persons?limit=0") &&
  genres=$(count "/Genres?userId=$U&limit=0") && studios=$(count "/Studios?userId=$U&limit=0") &&
  resume=$(count "/UserItems/Resume?userId=$U&limit=100") && nextup=$(count "/Shows/NextUp?userId=$U&limit=100") &&
  latest=$(count "/Items/Latest?userId=$U&limit=16") || return 1
  jq -n --arg movies "$movies" --arg series "$series" --arg episodes "$episodes" --arg albums "$albums" --arg tracks "$tracks" \
    --arg artists "$artists" --arg persons "$persons" --arg genres "$genres" --arg studios "$studios" \
    --arg resume "$resume" --arg nextup "$nextup" --arg latest "$latest" \
    '$ARGS.named | map_values(tonumber? // .)' > "$D/counts.json"
  echo "  counts: $(jq -c . "$D/counts.json")"
}
phase_shape() {
  taskset -c "$CLIENT_CPUS" k6 run --quiet --log-format json --console-output "$D/shape.log" \
    -e URL="$URL" -e IDS="$IDS" -e SHAPE=1 -e OUT="$D/shape-summary.json" bench/screens.js >/dev/null || return 1
  echo "  shape: $(wc -l < "$D/shape.log") responses"
}
phase_coldstart() {
  taskset -c "$CLIENT_CPUS" python3 bench/coldstart.py "$CONTAINER" "$URL" "$IDS" "$D/coldstart.json" "$RESTARTS" "$POLL_MS" | sed 's/^/  /' || return 1
  wait_ready && drain
}
phase_load() {  # every load level under one sampler (drained between), then the steady window
  taskset -c "$CLIENT_CPUS" python3 bench/mem_sample.py "$CONTAINER" "$D/mem.csv" "$MEM_SAMPLE_MS" "$SERVER_CPUS" &
  local sampler=$! w='{}' level rate seed t0 t1 rc=0
  for level in unloaded loaded stress; do
    want $level || continue
    case $level in
      unloaded) rate=$RATE_UNLOADED; seed=0;;
      loaded)   rate=$RATE_LOADED;   seed=500000;;
      stress)   rate=$RATE_STRESS;   seed=250000;;
    esac
    drain || rc=1
    echo "  $level: warm-up ${WARMUP_S}s @ $rate/s"
    k6run -e RATE="$rate" -e DURATION="${WARMUP_S}s" -e SEED=$((seed + 900000)) -e OUT="$D/k6-$level-warmup.json" >/dev/null || { echo "  warm-up failed" >&2; rc=1; }
    echo "  $level: window ${WINDOW_S}s @ $rate/s"
    t0=$(date +%s.%N)
    k6run -e RATE="$rate" -e DURATION="${WINDOW_S}s" -e SEED=$seed -e OUT="$D/k6-$level.json" | sed 's/^/    /' || { echo "  window failed" >&2; rc=1; }
    t1=$(date +%s.%N)
    w=$(jq -c --arg l "$level" --argjson t0 "$t0" --argjson t1 "$t1" '. + {($l): {start:$t0, end:$t1}}' <<<"$w")
  done
  # Drain before the steady window as well: it now follows the stress level, and an
  # in-flight transcode or a scheduled task started under that load would otherwise be
  # measured as idle memory.
  drain || rc=1
  echo "  steady: idle ${STEADY_S}s"
  t0=$(date +%s.%N); sleep "$STEADY_S"; t1=$(date +%s.%N)
  jq -c --argjson t0 "$t0" --argjson t1 "$t1" '. + {steady: {start:$t0, end:$t1}}' <<<"$w" > "$D/windows.json"
  kill "$sampler" 2>/dev/null || true; wait "$sampler" 2>/dev/null || true
  return $rc
}
phase_ttfs() {
  drain || return 1
  taskset -c "$CLIENT_CPUS" python3 bench/ttfs.py "$URL" "$IDS" "$D/ttfs.json" "$TTFS_REPS" | sed 's/^/  /' || return 1
}

run_server() {
  NAME=$1; URL=http://127.0.0.1:$(port_of "$1"); D=$OUT/$1; CONTAINER=bench-$1
  local cfg=$D/config cache=$D/cache
  echo "== $1 ($(image_of "$1")) → $D"
  [ -e "$cfg" ] && die "$cfg exists — a run dir is never reused"
  mkdir -p "$D"
  # provision: fresh copy of the test data; media read-only, no flag removes it
  cp -r --reflink=auto "$TESTDATA/config" "$cfg"; mkdir -p "$cache"
  local env=(); [ "$1" = ferrofin ] && env=(-e FERROFIN_DATA_DIR=/config -e FERROFIN_CACHE_DIR=/cache)
  # the server log is the one diagnostic a failed phase needs: always captured, on any exit
  trap 'docker logs "$CONTAINER" > "$D/server.log" 2>&1 || true; docker rm -f "$CONTAINER" >/dev/null 2>&1 || true' EXIT
  docker run -d --name "$CONTAINER" --user "$(id -u):$(id -g)" --cpuset-cpus "$SERVER_CPUS" \
    --memory "$MEMORY" --memory-swap "$MEMORY" \
    -p "127.0.0.1:$(port_of "$1"):8096" "${env[@]}" -v "$cfg:/config" -v "$cache:/cache" -v "$TESTDATA/media:/media:ro" \
    "$(image_of "$1")" >/dev/null
  wait_ready || die "$NAME did not start — see $D/server.log"
  [ "$1" = ferrofin ] && disable_plugins "$cfg"
  drain || die "$NAME: could not read /ScheduledTasks"
  docker inspect -f '{{.Config.Image}} {{.Image}}' "$CONTAINER" > "$D/image.txt"
  ! want counts || phase counts
  ! want shape || phase shape
  ! want coldstart || phase coldstart
  { ! want unloaded && ! want loaded && ! want stress; } || phase load
  ! want ttfs || phase ttfs
  docker logs "$CONTAINER" > "$D/server.log" 2>&1 || true
  docker rm -f "$CONTAINER" >/dev/null; trap - EXIT
}

for s in ${SERVERS//,/ }; do run_server "$s"; done
python3 bench/report.py "$OUT" > "$OUT/report.md"
echo "report: $OUT/report.md  —  compare in the browser: python3 bench/report.py --serve"
