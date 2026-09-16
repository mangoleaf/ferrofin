#!/usr/bin/env bash
# The comparison run (PLAN_BENCHMARK_V3 §5). For each server, alone on its cores:
#   provision (fresh copy of the test data) → drain → counts → shape → cold start
#   → unloaded / loaded / stress windows (+ memory sampler) → steady window → TTFS.
# Every phase writes its file the moment it ends and fails on its own: a broken phase
# is reported and the next one runs; report.py renders whatever exists.
#
#   run.sh [--testdata DIR] [--servers jellyfin,jellyfin12,ferrofin]
#          [--only counts,shape,coldstart,unloaded,loaded,stress,ttfs] [--rate N] [--out DIR]
#
# Requires: docker, k6, python3, jq; images jellyfin/jellyfin:10.11.8,
# the pinned Jellyfin 12.0.0 image and ferrofin:bench (docker build -t ferrofin:bench .).
set -euo pipefail
CALLER=$PWD
cd "$(dirname "$0")/.."

# ── tunables (§2) ──────────────────────────────────────────────────────────
WARMUP_S=${WARMUP_S:-30}        # discarded seconds at the same rate before each window
WINDOW_S=${WINDOW_S:-120}       # measured seconds per load level
SETTLE_S=${SETTLE_S:-30}        # idle after scheduled tasks report idle, before any window
STEADY_S=${STEADY_S:-60}        # idle after load; steady memory = median over this window
# Shared bounded picks; the discarded warm-up uses a separate seed.
SLOTS=${SLOTS:-600}
SEED=${SEED:-0}
SHAPE_VUS=10
WARMUP_SEED=$((SEED + 900000))
MEM_SAMPLE_MS=${MEM_SAMPLE_MS:-100}
POLL_MS=${POLL_MS:-10}          # cold-start readiness poll
RESTARTS=${RESTARTS:-5}
TTFS_REPS=${TTFS_REPS:-5}
export READY_TIMEOUT_S=${READY_TIMEOUT_S:-300}
export DRAIN_TIMEOUT_S=${DRAIN_TIMEOUT_S:-300}
export HTTP_TIMEOUT_S=${HTTP_TIMEOUT_S:-10}
export STREAM_HTTP_TIMEOUT_S=${STREAM_HTTP_TIMEOUT_S:-120}
export PHASE_TIMEOUT_S=${PHASE_TIMEOUT_S:-600}
export CLEANUP_TIMEOUT_S=${CLEANUP_TIMEOUT_S:-30}
SAMPLE_BRACKET_INTERVALS=${SAMPLE_BRACKET_INTERVALS:-2}
SAMPLE_GAP_INTERVALS=${SAMPLE_GAP_INTERVALS:-5}
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
  --testdata) TESTDATA=$2; shift 2;; --servers) SERVERS=$2; shift 2;;
  --only) ONLY=${2:-}; [ -n "$ONLY" ] || { echo "--only requires a phase or comma list" >&2; exit 2; }; shift 2;;
  # --rate sets the LOADED level only; RATE_UNLOADED and RATE_STRESS are env vars.
  --rate) RATE_LOADED=$2; shift 2;;
  --out) OUT=$2; shift 2;;
  *) echo "unknown $1" >&2; exit 2;;
esac; done
[[ "$SLOTS" =~ ^[1-9][0-9]*$ ]] && (( SLOTS % 10 == 0 )) || { echo "SLOTS must be a positive multiple of ten" >&2; exit 2; }
for tunable in READY_TIMEOUT_S DRAIN_TIMEOUT_S HTTP_TIMEOUT_S STREAM_HTTP_TIMEOUT_S PHASE_TIMEOUT_S CLEANUP_TIMEOUT_S MEM_SAMPLE_MS SAMPLE_BRACKET_INTERVALS SAMPLE_GAP_INTERVALS RESTARTS TTFS_REPS; do
  [[ "${!tunable}" =~ ^[1-9][0-9]*$ ]] || { echo "$tunable must be a positive integer" >&2; exit 2; }
done
# Reject bad selections before any fixture access or Docker command.
for selection in "$ONLY" "$SERVERS"; do
  [[ "$selection" != *[[:space:]]* ]] || { echo "selections must be comma-separated without whitespace" >&2; exit 2; }
  case "$selection" in ,*|*,|*,,*) echo "invalid empty selection in $selection" >&2; exit 2;; esac
done
[ -n "$SERVERS" ] || { echo "--servers cannot be empty" >&2; exit 2; }
for phase in ${ONLY//,/ }; do
  case "$phase" in counts|shape|coldstart|unloaded|loaded|stress|ttfs) ;; *) echo "unknown phase $phase" >&2; exit 2;; esac
done
for server in ${SERVERS//,/ }; do
  case "$server" in jellyfin|jellyfin12|ferrofin) ;; *) echo "unknown server $server" >&2; exit 2;; esac
done
abs() { case "$1" in /*) echo "$1";; *) echo "$CALLER/$1";; esac; }   # user paths are relative to the caller's shell
TESTDATA=$(realpath -m "$(abs "$TESTDATA")")
SHA=$(git rev-parse --short HEAD 2>/dev/null || echo nogit)

# A run is named for the code it measured, so a run dir can be traced back to a build:
#   on a tag            v0.42.1                 (and v0.42.1-run2, -run3 … for repeats)
#   ahead of a tag      v0.42.1-3-7e80268       (3 commits past the tag, at that sha)
#   dirty working tree  …-dirty                 the tree had uncommitted changes when the
#                                               run started, so the sha does not identify
#                                               it; image.txt and run.json.sha record what
#                                               actually ran
#   no tags / no git    20260903-1412-7e80268   the date form, as before
RUNS_DIR=$PWD/bench/runs
run_name() {
  local base n
  if base=$(git describe --tags --exact-match 2>/dev/null); then
    :
  elif base=$(git describe --tags --long 2>/dev/null); then
    # git prints tag-N-gSHA; the g is git's marker, not part of the sha.
    base=$(sed -E 's/-([0-9]+)-g([0-9a-f]+)$/-\1-\2/' <<<"$base")
  else
    base="$(date +%Y%m%d-%H%M)-$SHA"
  fi
  [ -z "$(git status --porcelain 2>/dev/null)" ] || base="$base-dirty"
  # Repeats are counted as -run2, -run3 …, so a rerun never lands in a populated dir
  # and the count cannot be misread as part of the version: a bare -2 reads as "two
  # commits on" exactly as much as "the second run", and a word cannot. No spaces or
  # glob characters, so the path needs no quoting on a command line. On a dirty tree
  # the counter only means "another run", not "the same code again" — two dirty trees
  # are not the same tree.
  n=1
  local candidate="$base"
  while [ -e "$RUNS_DIR/$candidate" ]; do
    n=$((n + 1)); candidate="$base-run$n"
  done
  echo "$candidate"
}
OUT=$(realpath -m "$(abs "${OUT:-$RUNS_DIR/$(run_name)}")")
for p in "$TESTDATA" "$OUT"; do case "$p" in /mnt/mangonas*|/mnt/nvme0/k3s*) echo "refusing to touch $p" >&2; exit 1;; esac; done

docker_cmd() { timeout --kill-after=2 "$HTTP_TIMEOUT_S" docker "$@"; }
die() { echo "run: $*" >&2; exit 1; }
image_of() { case "$1" in jellyfin) echo jellyfin/jellyfin:10.11.8;; jellyfin12) cat bench/testdata/jellyfin12-image.txt;; ferrofin) echo ferrofin:bench;; *) die "unknown server $1";; esac; }
port_of() { case "$1" in jellyfin) echo 18101;; jellyfin12) echo 18102;; ferrofin) echo 18103;; esac; }
want() { [ -z "$ONLY" ] || [[ ",$ONLY," == *",$1,"* ]]; }

# ── preflight ──────────────────────────────────────────────────────────────
[ ! -e "$OUT" ] || die "$OUT exists — a run directory is never reused"
[ -f "$TESTDATA/ids.json" ] || die "no $TESTDATA/ids.json — build the test data first (bench/testdata/build.sh)"
for t in docker k6 python3 jq taskset curl sha256sum timeout setsid; do command -v $t >/dev/null || die "$t not installed"; done
[ ! -z "$ONLY" ] && ! want ttfs || command -v ffprobe >/dev/null || die "ffprobe not installed"
jq -e '.pools | (.movies|length)>0 and (.series|length)>0 and (.terms|length)>0 and (.movieCount>0) and (.nextUpCutoff|type=="string")' "$TESTDATA/ids.json" >/dev/null || die "fixture pools missing; run bench/testdata/build.sh --export-pools"
for s in ${SERVERS//,/ }; do docker_cmd image inspect "$(image_of "$s")" >/dev/null 2>&1 || die "image $(image_of "$s") missing"; done
if [[ ",$SERVERS," == *,jellyfin12,* ]]; then
  prep=$TESTDATA/jellyfin12/preparation.json
  [ -f "$prep" ] || die "prepare Jellyfin 12.0.0 first: bench/testdata/build.sh --prepare-jellyfin12 '$TESTDATA'"
  expected_image=$(docker_cmd image inspect -f '{{.Id}}' "$(image_of jellyfin12)")
  expected_source=$(sha256sum "$TESTDATA/config/data/jellyfin.db" | cut -d' ' -f1)
  expected_ids=$(sha256sum "$TESTDATA/ids.json" | cut -d' ' -f1)
  jq -e --arg image "$expected_image" --arg source "$expected_source" --arg ids "$expected_ids" \
    '.image_id == $image and .source_db_sha256 == $source and .ids_sha256 == $ids and .validation.system_info.Version == "12.0.0"' \
    "$prep" >/dev/null || die "Jellyfin 12 preparation does not match this image/source fixture"
fi
echo "servers: $SERVERS; selected phases: ${ONLY:-all}; warm-up ${WARMUP_S}s; windows ${WINDOW_S}s"
[ -z "$(docker_cmd ps -aq --filter name=^bench-)" ] || die "a bench-* container exists (docker rm -f it first)"
topology=$(python3 bench/mem_sample.py --topology "$SERVER_CPUS" "$CLIENT_CPUS") || die "invalid CPU allocation"
checked_cpus=$(jq -r '.checked_cpus|map(tostring)|join(",")' <<<"$topology")
idle=$(python3 bench/mem_sample.py --check "$checked_cpus")
awk -v i="$idle" -v m="$CORE_IDLE_MIN" -v c="$SERVER_CPUS,$CLIENT_CPUS" 'BEGIN { if (i < m) { printf "cores %s are only %.0f%% idle (need %.0f%%) — something else is using them\n", c, i*100, m*100; exit 1 } }' || exit 1
mkdir -p "$OUT"
IDS=$TESTDATA/ids.json; U=$(jq -r .user "$IDS"); TOK=$(jq -r .token "$IDS")
AUTH="Authorization: MediaBrowser Client=\"bench\", Device=\"bench\", DeviceId=\"bench-run\", Version=\"3\", Token=\"$TOK\""
DIRTY=$([ -z "$(git status --porcelain 2>/dev/null)" ] && echo false || echo true)
jq -n --arg sha "$SHA" --arg name "$(basename "$OUT")" --argjson dirty "$DIRTY" --arg host "$(uname -srm)" --arg cpu "$(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | xargs)" \
  --arg mem "$MEMORY" --arg cpus "$SERVER_CPUS" --arg k6 "$(k6 version | head -1)" --arg testdata "$(jq -c .counts "$IDS")" \
  --argjson rate_unloaded "$RATE_UNLOADED" --argjson rate_loaded "$RATE_LOADED" --argjson rate_stress "$RATE_STRESS" \
  --argjson idle "$idle" --argjson idle_min "$CORE_IDLE_MIN" --argjson topology "$topology" --argjson bracket "$SAMPLE_BRACKET_INTERVALS" --argjson gap "$SAMPLE_GAP_INTERVALS" \
  --argjson ready "$READY_TIMEOUT_S" --argjson drain "$DRAIN_TIMEOUT_S" --argjson http "$HTTP_TIMEOUT_S" --argjson stream_http "$STREAM_HTTP_TIMEOUT_S" \
  --argjson phase "$PHASE_TIMEOUT_S" --argjson cleanup "$CLEANUP_TIMEOUT_S" --argjson restarts "$RESTARTS" --argjson ttfs_reps "$TTFS_REPS" \
  --argjson window "$WINDOW_S" --argjson sample_ms "$MEM_SAMPLE_MS" \
  --arg ids_sha "$(sha256sum "$IDS" | cut -d' ' -f1)" --arg script_sha "$(sha256sum bench/screens.js | cut -d' ' -f1)" \
  --argjson pools "$(jq -c .pools "$IDS")" --argjson warmup_s "$WARMUP_S" --argjson settle_s "$SETTLE_S" --argjson steady_s "$STEADY_S" --arg client_cpus "$CLIENT_CPUS" --argjson slots "$SLOTS" --argjson seed "$SEED" --argjson warmup_seed "$WARMUP_SEED" --argjson shape_vus "$SHAPE_VUS" \
  '{preflight_idle:$idle, core_idle_min:$idle_min, phase_schema:1, resource_schema:1, streaming_validation:1, topology:$topology, sample_bracket_intervals:$bracket, sample_gap_intervals:$gap, ready_timeout_s:$ready, drain_timeout_s:$drain, http_timeout_s:$http, stream_http_timeout_s:$stream_http, phase_timeout_s:$phase, cleanup_timeout_s:$cleanup, restarts:$restarts, ttfs_reps:$ttfs_reps, workload:4, warmup_s:$warmup_s, settle_s:$settle_s, steady_s:$steady_s, client_cpus:$client_cpus, ids_sha256:$ids_sha, screens_sha256:$script_sha, pools:$pools, slots:$slots, seed:$seed, warmup_seed:$warmup_seed, shape_vus:$shape_vus, sha:$sha, name:$name, dirty:$dirty, host:$host, cpu:$cpu, memory_limit:$mem, server_cpus:$cpus, k6:$k6, testdata_counts:($testdata|fromjson),
    rate_unloaded:$rate_unloaded, rate_loaded:$rate_loaded, rate_stress:$rate_stress,
    window_s:$window, mem_sample_ms:$sample_ms, date: (now|todate)}' > "$OUT/run.json"

api() { curl -sf --connect-timeout "$HTTP_TIMEOUT_S" --max-time "$HTTP_TIMEOUT_S" -H "$AUTH" "$URL$1"; }
wait_ready() {
  local deadline=$((SECONDS + READY_TIMEOUT_S))
  while (( SECONDS < deadline )); do
    curl -sf --connect-timeout "$HTTP_TIMEOUT_S" --max-time "$HTTP_TIMEOUT_S" "$URL/System/Info/Public" 2>/dev/null | jq -e '.Version | strings | length > 0' >/dev/null 2>&1 && return 0
    sleep 1
  done
  FAIL_REASON="readiness timed out"; return 1
}
drain() {
  local deadline=$((SECONDS + DRAIN_TIMEOUT_S)) busy
  while (( SECONDS < deadline )); do
    busy=$(api "/ScheduledTasks" | jq -er 'if type != "array" then error("invalid tasks") else [.[] | select(.State != "Idle") | .Name] | join(",") end') || { FAIL_REASON="cannot read scheduled tasks"; return 1; }
    if [ -z "$busy" ]; then sleep "$SETTLE_S"; return 0; fi
    echo "  drain: waiting for [$busy]"; sleep 1
  done
  FAIL_REASON="scheduled tasks did not drain before deadline"; return 1
}
count() { api "$1" | jq -r 'if type=="array" then length else (.TotalRecordCount // (.Items|length)) end'; }
disable_plugins() {  # core Ferrofin only: every compiled-in extension off, no WASM plugin loadable (owner rule)
  local wasm; wasm=$(find "$1" -name '*.wasm' 2>/dev/null | head -1)
  [ -z "$wasm" ] || die "$NAME: a WASM plugin is present in the config copy ($wasm) — the benchmark measures core Ferrofin only"
  local id ver
  while read -r id ver; do
    curl -sf --max-time "$HTTP_TIMEOUT_S" -X POST -H "$AUTH" "$URL/Plugins/$id/$ver/Disable" >/dev/null || die "$NAME: could not disable plugin $id $ver"
  done < <(api "/Plugins" | jq -r '.[] | "\(.Id) \(.Version)"')
  api "/Plugins" > "$D/plugins.json"
  local on; on=$(jq -r '[.[] | select(.Status != "Disabled") | .Name] | join(", ")' "$D/plugins.json")
  [ -z "$on" ] || die "$NAME: plugins still enabled after disabling: $on"
  echo "  plugins: $(jq -r 'map(.Name) | join(", ")' "$D/plugins.json") — all disabled"
}
# A client gets its own process group, so interruption/timeout also stops descendants.
client() {
  local rc=0 deadline
  setsid timeout --signal=TERM --kill-after="${CLEANUP_TIMEOUT_S}s" "${PHASE_TIMEOUT_S}s" "$@" &
  CLIENT_PID=$!
  wait "$CLIENT_PID" || rc=$?
  if (( rc == 124 )); then FAIL_REASON="client timed out after ${PHASE_TIMEOUT_S}s"; fi
  if kill -0 -- "-$CLIENT_PID" 2>/dev/null; then
    kill -TERM -- "-$CLIENT_PID" 2>/dev/null || true
    deadline=$((SECONDS + CLEANUP_TIMEOUT_S))
    while kill -0 -- "-$CLIENT_PID" 2>/dev/null && (( SECONDS < deadline )); do sleep 0.1; done
    kill -KILL -- "-$CLIENT_PID" 2>/dev/null || true
    FAIL_REASON="client left descendants after exit"; rc=1
  fi
  CLIENT_PID=""
  return "$rc"
}
k6run() {
  local arg output="" validation
  for arg in "$@"; do case "$arg" in OUT=*) output=${arg#OUT=};; esac; done
  client taskset -c "$CLIENT_CPUS" k6 run --quiet --log-format json -e SLOTS="$SLOTS" -e URL="$URL" -e IDS="$IDS" "$@" bench/screens.js || return $?
  validation=$(python3 bench/report.py --window-check "$output" 2>&1) || { FAIL_REASON="$validation"; echo "$validation" >&2; return 1; }
}
state() {
  local name=$1 status=$2 reason=${3:-}
  jq --arg n "$name" --arg s "$status" --arg r "$reason" --argjson t "$(date +%s.%N)" \
    '.[$n] = ((.[$n] // {}) + {status:$s,reason:$r} + (if $s=="running" then {start:$t} elif $s=="pending" then {} else {end:$t} end))' \
    "$D/phases.json" > "$D/phases.json.tmp" && mv "$D/phases.json.tmp" "$D/phases.json"
}
phase() {
  local name=$1 rc=0; shift
  [ $# -gt 0 ] || set -- "phase_$name"
  CURRENT_PHASE=$name; FAIL_REASON=""; state "$name" running
  echo "  -- $name"
  "$@" || rc=$?
  if (( rc )); then
    state "$name" failed "${FAIL_REASON:-command exited $rc}"
    FAILED=1
  else state "$name" completed; fi
  CURRENT_PHASE=""
  return "$rc"
}
stop_sampler() {
  local rc=0 deadline=$((SECONDS + CLEANUP_TIMEOUT_S))
  [ -n "$SAMPLER_PID" ] || return 1
  if kill -0 "$SAMPLER_PID" 2>/dev/null; then kill -TERM "$SAMPLER_PID"; else rc=1; fi
  while kill -0 "$SAMPLER_PID" 2>/dev/null && (( SECONDS < deadline )); do sleep 0.1; done
  if kill -0 "$SAMPLER_PID" 2>/dev/null; then kill -KILL "$SAMPLER_PID"; rc=1; fi
  wait "$SAMPLER_PID" || rc=1
  SAMPLER_PID=""
  return "$rc"
}
cleanup() {
  local rc=$? cid deadline cleanup_rc=0
  set +e
  trap - EXIT INT TERM
  [ -z "$CURRENT_PHASE" ] || state "$CURRENT_PHASE" failed "interrupted or aborted (exit $rc)"
  state cleanup running
  if [ -n "$CLIENT_PID" ]; then
    kill -TERM -- "-$CLIENT_PID" 2>/dev/null || true
    deadline=$((SECONDS + CLEANUP_TIMEOUT_S))
    while kill -0 -- "-$CLIENT_PID" 2>/dev/null && (( SECONDS < deadline )); do sleep 0.1; done
    if kill -0 -- "-$CLIENT_PID" 2>/dev/null; then kill -KILL -- "-$CLIENT_PID" 2>/dev/null; cleanup_rc=1; fi
    wait "$CLIENT_PID" 2>/dev/null || true
  fi
  if [ -n "$SAMPLER_PID" ]; then
    stop_sampler || rc=1
    state sampler failed "run interrupted before sampler validation"
  fi
  if [ -s "$D/container.cid" ]; then
    cid=$(cat "$D/container.cid")
    timeout --kill-after=2 "$CLEANUP_TIMEOUT_S" docker logs "$cid" > "$D/server.log" 2>&1 || true
    timeout --kill-after=2 "$CLEANUP_TIMEOUT_S" docker rm -f "$cid" >/dev/null 2>&1 || cleanup_rc=1
  fi
  if (( cleanup_rc )); then state cleanup failed "owned resource cleanup failed or required SIGKILL"; else state cleanup completed; fi
  exit "$((rc != 0 || FAILED != 0 || cleanup_rc != 0))"
}

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
  local validation
  client taskset -c "$CLIENT_CPUS" k6 run --quiet --log-format json --console-output "$D/shape.log" \
    -e URL="$URL" -e IDS="$IDS" -e SHAPE=1 -e SEED="$SEED" -e SLOTS="$SLOTS" -e OUT="$D/shape-summary.json" bench/screens.js >/dev/null || return 1
  echo "  shape: $(jq -r '.iterations|tostring' "$D/shape-summary.json") slots; $(jq -r '.elapsed_ms/1000' "$D/shape-summary.json") seconds"
  validation=$(python3 bench/report.py --shape-coverage "$D" 2>&1) || { FAIL_REASON="$validation"; echo "$validation" >&2; return 1; }
  echo "$validation"
}
phase_coldstart() {
  client taskset -c "$CLIENT_CPUS" python3 bench/coldstart.py "$CONTAINER" "$URL" "$IDS" "$D/coldstart.json" "$RESTARTS" "$POLL_MS" || return 1
  wait_ready && drain
}
phase_load() {
  local level rate t0 t1 sampler_ok=1 deadline
  state sampler running
  taskset -c "$CLIENT_CPUS" python3 bench/mem_sample.py "$CONTAINER" "$D/mem.csv" "$MEM_SAMPLE_MS" "$SERVER_CPUS" &
  SAMPLER_PID=$!
  deadline=$((SECONDS + HTTP_TIMEOUT_S))
  while [ ! -s "$D/mem.csv" ] && kill -0 "$SAMPLER_PID" 2>/dev/null && (( SECONDS < deadline )); do sleep 0.1; done
  [ -s "$D/mem.csv" ] || sampler_ok=0
  echo '{}' > "$D/windows.json"
  for level in unloaded loaded stress; do
    want "$level" || continue
    case "$level" in unloaded) rate=$RATE_UNLOADED;; loaded) rate=$RATE_LOADED;; stress) rate=$RATE_STRESS;; esac
    if ! phase "drain-$level" drain; then
      state "warmup-$level" skipped "drain failed"; state "$level" skipped "drain failed"; continue
    fi
    if ! phase "warmup-$level" k6run --console-output "$D/selections-$level-warmup.log" -e RATE="$rate" -e DURATION="${WARMUP_S}s" -e SEED="$WARMUP_SEED" -e OUT="$D/k6-$level-warmup.json"; then
      state "$level" skipped "warm-up failed"; continue
    fi
    t0=$(date +%s.%N)
    phase "$level" k6run --console-output "$D/selections-$level.log" -e RATE="$rate" -e DURATION="${WINDOW_S}s" -e SEED="$SEED" -e OUT="$D/k6-$level.json" || true
    t1=$(date +%s.%N)
    jq --arg l "$level" --argjson start "$t0" --argjson end "$t1" '. + {($l): {start:$start,end:$end}}' "$D/windows.json" > "$D/windows.tmp" && mv "$D/windows.tmp" "$D/windows.json"
  done
  if phase drain-steady drain; then
    t0=$(date +%s.%N); phase steady sleep "$STEADY_S" || true; t1=$(date +%s.%N)
    jq --argjson start "$t0" --argjson end "$t1" '. + {steady:{start:$start,end:$end}}' "$D/windows.json" > "$D/windows.tmp" && mv "$D/windows.tmp" "$D/windows.json"
  else state steady skipped "drain failed"; fi
  sleep "$(python3 -c 'import sys; print(2*float(sys.argv[1])/1000)' "$MEM_SAMPLE_MS")"
  stop_sampler || sampler_ok=0
  python3 bench/report.py --resource-check "$D" || sampler_ok=0
  if (( sampler_ok )); then state sampler completed; else state sampler failed "sampler exit or coverage failed"; FAILED=1; fi
}

phase_ttfs() {
  drain || return 1
  client taskset -c "$CLIENT_CPUS" python3 bench/ttfs.py "$URL" "$IDS" "$D/ttfs.json" "$TTFS_REPS" || return 1
}

run_server() {
  set -e
  NAME=$1; URL=http://127.0.0.1:$(port_of "$1"); D=$OUT/$1; CONTAINER=bench-$1
  local cfg=$D/config cache=$D/cache
  echo "== $1 ($(image_of "$1")) → $D"
  [ -e "$cfg" ] && die "$cfg exists — a run dir is never reused"
  mkdir -p "$D"
  FAILED=0; CLIENT_PID=""; SAMPLER_PID=""; CURRENT_PHASE=""
  echo '{}' > "$D/phases.json"
  state startup pending; state startup-drain pending; state cleanup pending
  for selected in counts shape coldstart ttfs; do ! want "$selected" || state "$selected" pending; done
  for selected in unloaded loaded stress; do
    if want "$selected"; then
      state "$selected" pending; state "warmup-$selected" pending; state "drain-$selected" pending
      state sampler pending; state steady pending; state drain-steady pending
    fi
  done
  trap cleanup EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM
  CURRENT_PHASE=startup; state startup running
  # provision: fresh copy of the test data; media read-only, no flag removes it
  local source=$TESTDATA/config
  if [ "$1" = jellyfin12 ]; then
    source=$TESTDATA/jellyfin12/config
    cp "$TESTDATA/jellyfin12/preparation.json" "$D/preparation.json"
  fi
  cp -r --reflink=auto "$source" "$cfg"; mkdir -p "$cache"
  local env=(); [ "$1" = ferrofin ] && env=(-e FERROFIN_DATA_DIR=/config -e FERROFIN_CACHE_DIR=/cache)
  # the server log is the one diagnostic a failed phase needs: always captured, on any exit
  timeout --kill-after=2 "$READY_TIMEOUT_S" docker run -d --cidfile "$D/container.cid" --name "$CONTAINER" --user "$(id -u):$(id -g)" --cpuset-cpus "$SERVER_CPUS" \
    --memory "$MEMORY" --memory-swap "$MEMORY" \
    -p "127.0.0.1:$(port_of "$1"):8096" "${env[@]}" -v "$cfg:/config" -v "$cache:/cache" -v "$TESTDATA/media:/media:ro" \
    "$(image_of "$1")" >/dev/null
  wait_ready || die "$NAME did not start — see $D/server.log"
  api "/System/Info/Public" > "$D/system-info.json"
  if [ "$1" = jellyfin12 ]; then
    [ "$(jq -r .Version "$D/system-info.json")" = 12.0.0 ] || die "expected Jellyfin 12.0.0"
  fi
  [ "$1" = ferrofin ] && disable_plugins "$cfg"
  state startup completed; CURRENT_PHASE=""
  phase startup-drain drain || exit 1
  docker_cmd inspect -f '{{.Config.Image}} {{.Image}}' "$CONTAINER" > "$D/image.txt"
  ! want counts || phase counts || true
  ! want shape || phase shape || true
  ! want coldstart || phase coldstart || true
  { ! want unloaded && ! want loaded && ! want stress; } || phase_load
  ! want ttfs || phase ttfs || true
  exit "$FAILED"
}

rc=0; SERVER_PID=""
interrupt_run() {
  trap - INT TERM
  if [ -n "$SERVER_PID" ]; then kill -TERM "$SERVER_PID" 2>/dev/null || true; wait "$SERVER_PID" || true; fi
  exit 130
}
trap interrupt_run INT TERM
for s in ${SERVERS//,/ }; do
  set +e
  run_server "$s" &
  SERVER_PID=$!
  wait "$SERVER_PID"
  status=$?
  SERVER_PID=""
  set -e
  (( status == 0 )) || rc=1
done
python3 bench/report.py "$OUT" > "$OUT/report.md" || rc=1
echo "report: $OUT/report.md  —  compare in the browser: python3 bench/report.py --serve"
exit "$rc"
