#!/usr/bin/env bash
# The live parity replay: send jellyfin-web-shaped requests to a Jellyfin and a Ferrofin that serve
# the same library, compare the answers, match the differences against the ledger, and write a
# report. See gates/parity/README.md.
#
#   run.sh --oracle IMAGE --ferrofin IMAGE [--testdata DIR] [--requests FILE] [--out DIR] [--gate]
#
#   --oracle     the Jellyfin image (the pinned release image in gates/bench/testdata/jellyfin-image.txt)
#   --ferrofin   the Ferrofin image under test
#   --testdata   the benchmark corpus (config/, media/, ids.json, jellyfin/): default gates/bench/testdata
#   --requests   a request file, or a directory of them (every *.jsonl in it); repeat it to send several
#                sets as one (the sweep and the edge cases: --requests gates/parity/requests/sweep
#                --requests gates/parity/requests/edge): default
#                gates/parity/requests/tracer.jsonl; the full sweep is gates/parity/requests/sweep. Its writes each run
#                on a fresh database copy: gates/parity/reset.sh restores both servers to the snapshot taken at boot
#   --out        the run directory: default gates/parity/runs/<name>; never reused
#   --gate       a release-gate run: a stale oracle pin, or an --oracle other than the pin, is refused
#                (an ad-hoc run only warns)
#
# Exit: 0 no new difference; 1 a new difference or a run failure (a server did not boot, the cap
# passed, a reset failed); 2 the arguments or inputs were refused before anything started.
#
# Settings (environment): PARITY_CAP_S (the whole run's limit, default 7200 = 2 hours),
# PARITY_REQUEST_TIMEOUT (seconds per request, 10 to 120, default 30), PARITY_CONCURRENCY (reads in
# flight per server, 1 to 8, default 4; writes always run one at a time), PARITY_ORACLE_PORT /
# PARITY_FERROFIN_PORT, PARITY_PIN (0: no cpusets and no idle-core check, for hosts without the
# benchmark's core layout and for the harness's own tests), SERVER_CPUS, CLIENT_CPUS, MEMORY,
# CORE_IDLE_MIN, READY_TIMEOUT_S, DRAIN_TIMEOUT_S, HTTP_TIMEOUT_S, CLEANUP_TIMEOUT_S.
#
# Only copies of the corpus are booted, its media is mounted read-only, and nothing touches a real
# library. Waits use readiness signals (/System/Ping, idle /ScheduledTasks), never fixed sleeps.
# Requires: docker, python3, jq, curl, timeout; taskset when pinning.
set -euo pipefail
CALLER=$PWD
cd "$(dirname "$0")/../.."
START_EPOCH=$(date +%s)
# shellcheck source=gates/parity/lib.sh
source gates/parity/lib.sh   # READY_TIMEOUT_S, HTTP_TIMEOUT_S, CLEANUP_TIMEOUT_S, KILL_GRACE_S, POLL_S, and the readiness check

PARITY_CAP_S=${PARITY_CAP_S:-7200}
PARITY_REQUEST_TIMEOUT=${PARITY_REQUEST_TIMEOUT:-30}
PARITY_CONCURRENCY=${PARITY_CONCURRENCY:-4}   # reads in flight per server; writes are serial (decision D4)
DRAIN_TIMEOUT_S=${DRAIN_TIMEOUT_S:-300}   # this and the settings below are gates/bench/run.sh's
CORE_IDLE_MIN=${CORE_IDLE_MIN:-0.90}   # the server and client cores must be this idle before a run starts
SERVER_CPUS=${SERVER_CPUS:-8-15}       # the benchmark's server cpuset: each server gets half of it
CLIENT_CPUS=${CLIENT_CPUS:-16-19}
MEMORY=${MEMORY:-8g}                   # per server, swap disabled
PIN=${PARITY_PIN:-1}
ORACLE_PORT=${PARITY_ORACLE_PORT:-18111}    # beside the benchmark's 18101 and 18103
FERROFIN_PORT=${PARITY_FERROFIN_PORT:-18113}

ORACLE_IMAGE=""; FERROFIN_IMAGE=""; GATE=false
TESTDATA=$PWD/gates/bench/testdata; REQUESTS=(); OUT=""
die() { echo "parity: $*" >&2; exit 2; }
while [ $# -gt 0 ]; do
  case "$1" in --oracle|--ferrofin|--testdata|--requests|--out) [ $# -ge 2 ] || die "$1 needs a value";; esac
  case "$1" in
    --oracle) ORACLE_IMAGE=$2; shift 2;;
    --ferrofin) FERROFIN_IMAGE=$2; shift 2;;
    --testdata) TESTDATA=$2; shift 2;;
    --requests) REQUESTS+=("$2"); shift 2;;
    --out) OUT=$2; shift 2;;
    --gate) GATE=true; shift;;
    *) die "unknown $1";;
  esac
done
abs() { case "$1" in /*) echo "$1";; *) echo "$CALLER/$1";; esac; }   # user paths are relative to the caller's shell
if [ -z "$ORACLE_IMAGE" ] || [ -z "$FERROFIN_IMAGE" ]; then die "--oracle IMAGE and --ferrofin IMAGE are required"; fi
for setting in PARITY_CAP_S PARITY_REQUEST_TIMEOUT PARITY_CONCURRENCY READY_TIMEOUT_S DRAIN_TIMEOUT_S HTTP_TIMEOUT_S CLEANUP_TIMEOUT_S ORACLE_PORT FERROFIN_PORT; do
  [[ "${!setting}" =~ ^[1-9][0-9]*$ ]] || die "$setting must be a positive integer"
done
(( PARITY_REQUEST_TIMEOUT >= 10 && PARITY_REQUEST_TIMEOUT <= 120 )) || die "PARITY_REQUEST_TIMEOUT is 10 to 120 seconds"   # diff.py holds the same range
(( PARITY_CONCURRENCY <= 8 )) || die "PARITY_CONCURRENCY is 1 to 8"   # diff.py holds the same range
(( PARITY_CAP_S <= 7200 )) || die "PARITY_CAP_S is at most 7200: a run is capped at two hours"
(( ORACLE_PORT <= 65535 && FERROFIN_PORT <= 65535 && ORACLE_PORT != FERROFIN_PORT )) || die "the two ports are different and at most 65535"
[[ "$PIN" =~ ^[01]$ ]] || die "PARITY_PIN is 0 or 1"
! $GATE || [ "$PIN" = 1 ] || die "a release-gate run is pinned: PARITY_PIN=0 is for ad-hoc runs and the harness's own tests"
TESTDATA=$(realpath -m "$(abs "$TESTDATA")")
(( ${#REQUESTS[@]} )) || REQUESTS=("$PWD/gates/parity/requests/tracer.jsonl")
request_args=()   # --requests once per set, in the order given: they are sent as one set
for i in "${!REQUESTS[@]}"; do REQUESTS[i]=$(realpath -m "$(abs "${REQUESTS[i]}")"); request_args+=(--requests "${REQUESTS[i]}"); done
export READY_TIMEOUT_S HTTP_TIMEOUT_S CLEANUP_TIMEOUT_S   # gates/parity/reset.sh waits as the boot does
DEADLINE=$((START_EPOCH + PARITY_CAP_S))

SHA=$(git rev-parse --short HEAD 2>/dev/null || echo nogit)
RUNS_DIR=$PWD/gates/parity/runs
run_name() {
  local base n=1 candidate
  base="$(date +%Y%m%d-%H%M)-$SHA"
  [ -z "$(git status --porcelain 2>/dev/null)" ] || base="$base-dirty"
  candidate=$base
  while [ -e "$RUNS_DIR/$candidate" ]; do n=$((n + 1)); candidate="$base-run$n"; done
  echo "$candidate"
}
OUT=$(realpath -m "$(abs "${OUT:-$RUNS_DIR/$(run_name)}")")
# Every run copies and boots the corpus: that load belongs on a local disk, never a network share
# (the same judgement as gates/adoption/lib.sh adoption_require_local, by the nearest existing parent).
require_local() {
  local dir=$1 types fs
  until [ -e "$dir" ]; do dir=$(dirname "$dir"); done
  types=$(findmnt -n -o FSTYPE -T "$dir") || die "refusing $1: cannot tell its filesystem"
  for fs in $types; do
    case $fs in nfs*|cifs|smb*|9p|ceph|afs|glusterfs|davfs|fuse.sshfs|fuse.rclone|fuse.glusterfs|fuse.s3fs|fuse.davfs2)
      die "refusing $1: it is on a network filesystem ($fs); use a local disk";; esac
  done
}
command -v findmnt >/dev/null || die "findmnt not installed"
require_local "$TESTDATA"; require_local "$OUT"

# ── preflight: nothing is booted, so a refusal leaves no run directory ─────────────────────────
for t in docker python3 jq curl timeout; do command -v "$t" >/dev/null || die "$t not installed"; done
[ "$PIN" = 0 ] || command -v taskset >/dev/null || die "taskset not installed (PARITY_PIN=0 runs without it)"
[ ! -e "$OUT" ] || die "$OUT exists: a run directory is never reused"
IDS=$TESTDATA/ids.json
[ -f "$IDS" ] || die "no $IDS: build the test data first (gates/bench/testdata/build.sh)"
jq -e '.user and .user_name and .password' "$IDS" >/dev/null || die "$IDS lacks the browse user (user, user_name, password)"
python3 gates/parity/diff.py check "${request_args[@]}" --ids "$IDS" >/dev/null || exit 2   # its message is on stderr
for d in config media jellyfin/config; do [ -d "$TESTDATA/$d" ] || die "no $TESTDATA/$d: build and prepare the test data (gates/bench/testdata/build.sh --prepare-jellyfin)"; done
PREP=$TESTDATA/jellyfin/preparation.json
[ -f "$PREP" ] || die "prepare Jellyfin first: gates/bench/testdata/build.sh --prepare-jellyfin '$TESTDATA'"
for image in "$ORACLE_IMAGE" "$FERROFIN_IMAGE"; do docker_cmd image inspect "$image" >/dev/null 2>&1 || die "image $image missing"; done
expected_image=$(docker_cmd image inspect -f '{{.Id}}' "$ORACLE_IMAGE")
expected_source=$(sha256sum "$TESTDATA/config/data/jellyfin.db" | cut -d' ' -f1)
expected_ids=$(sha256sum "$IDS" | cut -d' ' -f1)
jq -e --arg image "$expected_image" --arg source "$expected_source" --arg ids "$expected_ids" \
  '.image_id == $image and .source_db_sha256 == $source and .ids_sha256 == $ids' "$PREP" >/dev/null \
  || die "the Jellyfin preparation does not match this oracle image and source fixture"
# The oracle is the Jellyfin release named by UPSTREAM_TAG, pinned by digest (gates/bench/oracle.py): a
# release-gate run refuses a stale pin, an ad-hoc run warns.
oracle_args=(check); ! $GATE || oracle_args+=(--gate)
ORACLE_PIN=$(python3 gates/bench/oracle.py "${oracle_args[@]}") || die "the oracle pin was refused (see above)"
if [ "$ORACLE_IMAGE" != "$(jq -r .image <<<"$ORACLE_PIN")" ]; then
  $GATE && die "--oracle $ORACLE_IMAGE is not the pinned oracle $(jq -r .image <<<"$ORACLE_PIN")"
  echo "parity: warning: --oracle $ORACLE_IMAGE is not the pinned oracle $(jq -r .image <<<"$ORACLE_PIN")" >&2
fi
[ -z "$(docker_cmd ps -aq --filter name='^parity-')" ] || die "a parity-* container exists (docker rm -f it first)"
if [ "$PIN" = 1 ]; then
  [[ "$SERVER_CPUS" =~ ^([0-9]+)-([0-9]+)$ ]] || die "SERVER_CPUS is one range such as 8-15"
  lo=${BASH_REMATCH[1]}; hi=${BASH_REMATCH[2]}
  (( (hi - lo + 1) % 2 == 0 && hi > lo )) || die "SERVER_CPUS needs an even number of cpus to give each server half"
  half=$(( (hi - lo + 1) / 2 ))
  ORACLE_CPUS=$lo-$((lo + half - 1)); FERROFIN_CPUS=$((lo + half))-$hi
  topology=$(python3 gates/bench/mem_sample.py --topology "$SERVER_CPUS" "$CLIENT_CPUS") || die "invalid CPU allocation"
  idle=$(python3 gates/bench/mem_sample.py --check "$(jq -r '.checked_cpus|map(tostring)|join(",")' <<<"$topology")")
  awk -v i="$idle" -v m="$CORE_IDLE_MIN" -v c="$SERVER_CPUS,$CLIENT_CPUS" 'BEGIN { if (i < m) { printf "cores %s are only %.0f%% idle (need %.0f%%): something else is using them\n", c, i*100, m*100; exit 1 } }' || exit 2
fi

# ── the run ───────────────────────────────────────────────────────────────────────────────────
FAIL_REASON=""; CAP_EXCEEDED=false
# shellcheck disable=SC2317,SC2329  # invoked by the EXIT trap (SC2317 is the code before shellcheck 0.11)
cleanup() {   # the EXIT trap: stops what runs, removes the containers, and makes sure a report exists
  local rc=$? name cleanup_rc=0 give_up
  set +e
  trap - EXIT
  trap '' INT TERM HUP   # a second signal must not abort the teardown
  # Stop whatever is still running first: a boot in progress, the comparison.
  for job in $(jobs -p); do kill "$job" 2>/dev/null; done
  wait 2>/dev/null
  # A boot's `timeout docker run` leads its own process group and outlives its killed subshell (a signal
  # to the script's group kills the subshell first, so the create is orphaned). Find it by its command
  # line and stop it before the removals below: a container must not appear after them.
  give_up=$((SECONDS + CLEANUP_TIMEOUT_S))
  # A reset in flight leads a session of its own (diff.py stops it on a signal; this is the backstop for a
  # diff.py killed outright): stop its whole session, the docker calls it is waiting on included.
  for pid in $(pgrep -f -- "gates/parity/reset.sh $OUT\$"); do pkill -KILL -s "$pid" 2>/dev/null; done
  for name in oracle ferrofin; do
    pkill -TERM -f -- "--name parity-$name " 2>/dev/null
    while pgrep -f -- "--name parity-$name " >/dev/null 2>&1 && (( SECONDS < give_up )); do sleep 0.1; done   # a short poll, bounded by give_up
  done
  for name in oracle ferrofin; do
    if [ -s "$OUT/$name/container.cid" ] || [ -d "$OUT/$name" ]; then
      timeout --kill-after="$KILL_GRACE_S" "$CLEANUP_TIMEOUT_S" docker logs "parity-$name" > "$OUT/$name/server.log" 2>&1
      timeout --kill-after="$KILL_GRACE_S" "$CLEANUP_TIMEOUT_S" docker rm -f "parity-$name" >/dev/null 2>&1 || {
        # docker rm of a container that never started is not a failure of the run
        timeout --kill-after="$KILL_GRACE_S" "$CLEANUP_TIMEOUT_S" docker inspect "parity-$name" >/dev/null 2>&1 && cleanup_rc=1
      }
    fi
  done
  # A run that ends any way at all leaves a report.
  if [ ! -f "$OUT/report.html" ]; then
    flag=(); ! $CAP_EXCEEDED || flag=(--cap-exceeded)
    python3 gates/parity/diff.py failure --out "$OUT" --reason "${FAIL_REASON:-interrupted or aborted (exit $rc)}" "${flag[@]}" >/dev/null 2>&1
    (( rc )) || rc=1
  fi
  # The documented exits are 0, 1 and (signals) 129, 130 and 143; a set -e abort must not leak a command's own status.
  case "$rc" in 0|1|129|130|143) ;; *) rc=1;; esac
  (( cleanup_rc )) && { echo "parity: a container could not be removed" >&2; (( rc )) || rc=1; }
  exit "$rc"
}
mkdir -p "$OUT"
trap cleanup EXIT   # from here on, however the run ends it leaves a report
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP   # a closed terminal or a dropped ssh session
DIRTY=$([ -z "$(git status --porcelain 2>/dev/null)" ] && echo false || echo true)
requests_digest() {   # one digest over every request set in the order given (a directory: its *.jsonl in name order)
  local set
  for set in "${REQUESTS[@]}"; do
    if [ -d "$set" ]; then find "$set" -maxdepth 1 -name '*.jsonl' -print0 | LC_ALL=C sort -z | xargs -0 cat; else cat "$set"; fi
  done | sha256sum | cut -d' ' -f1
}
jq -n --arg name "$(basename "$OUT")" --arg sha "$SHA" --argjson dirty "$DIRTY" --argjson cap "$PARITY_CAP_S" --argjson timeout "$PARITY_REQUEST_TIMEOUT" \
  --argjson started "$START_EPOCH" --argjson concurrency "$PARITY_CONCURRENCY" --argjson oracle "$ORACLE_PIN" --arg oracle_image "$ORACLE_IMAGE" --arg oracle_id "$expected_image" \
  --arg ferrofin_image "$FERROFIN_IMAGE" --arg ferrofin_id "$(docker_cmd image inspect -f '{{.Id}}' "$FERROFIN_IMAGE")" \
  --arg host "$(uname -srm)" --arg cpu "$(grep -m1 'model name' /proc/cpuinfo 2>/dev/null | cut -d: -f2 | xargs || true)" \
  --arg ids_sha "$expected_ids" --arg requests_sha "$(requests_digest)" --argjson gate "$GATE" --arg pin "$PIN" \
  --arg server_cpus "${ORACLE_CPUS:-} ${FERROFIN_CPUS:-}" \
  '{name:$name, sha:$sha, dirty:$dirty, started:$started, cap_s:$cap, request_timeout_s:$timeout, concurrency:$concurrency, gate:$gate, pinned:($pin == "1"),
    oracle:($oracle + {image:$oracle_image, image_id:$oracle_id}), ferrofin:{image:$ferrofin_image, image_id:$ferrofin_id},
    host:$host, cpu:$cpu, ids_sha256:$ids_sha, requests_sha256:$requests_sha, server_cpus:$server_cpus}' > "$OUT/run.json"
echo "parity: oracle $ORACLE_IMAGE, ferrofin $FERROFIN_IMAGE; cap ${PARITY_CAP_S}s; output $OUT"

auth() { echo "MediaBrowser Client=\"parity\", Device=\"parity\", DeviceId=\"parity-run\", Version=\"1\", Token=\"$1\""; }
reason() { echo "$*" > "$D/failed"; echo "parity: $NAME: $*" >&2; }   # inside a server's boot subshell
api() { curl -sf --connect-timeout "$HTTP_TIMEOUT_S" --max-time "$HTTP_TIMEOUT_S" -H "Authorization: $(auth "$TOKEN")" "$URL$1"; }
api_post() { curl -sf --connect-timeout "$HTTP_TIMEOUT_S" --max-time "$HTTP_TIMEOUT_S" -X POST -H "Authorization: $(auth "$TOKEN")" -H 'Content-Type: application/json' --data "${2:-}" "$URL$1" >/dev/null; }
drain() {   # every scheduled task idle
  local give_up=$((SECONDS + DRAIN_TIMEOUT_S)) busy
  while (( SECONDS < give_up )); do
    busy=$(api /ScheduledTasks | jq -er 'if type != "array" then error("invalid tasks") else [.[] | select(.State != "Idle") | .Name] | join(",") end') || return 1
    [ -z "$busy" ] && return 0
    sleep "$POLL_S"
  done
  return 1
}
isolate() {   # no plugins, no automatic tasks: the answers come from the core servers, and nothing rewrites the copy
  # A plugin that cannot be uninstalled (an in-tree one: TMDb, OMDb, …) stays as it is on both servers: Jellyfin
  # cannot disable it, and its state and settings pages are answers to compare. (gates/bench/run.sh disables every
  # Ferrofin plugin, for load; parity treats both servers alike.)
  local id version
  api /Plugins > "$D/plugins-before.json" && jq -e 'type == "array"' "$D/plugins-before.json" >/dev/null || return 1
  while read -r id version; do api_post "/Plugins/$id/$version/Disable" || return 1
  done < <(jq -r '.[] | select(.CanUninstall != false) | "\(.Id) \(.Version)"' "$D/plugins-before.json")
  api /ScheduledTasks > "$D/tasks-before.json" && jq -e 'type == "array" and length > 0' "$D/tasks-before.json" >/dev/null || return 1
  while read -r id; do api_post "/ScheduledTasks/$id/Triggers" '[]' || return 1; done < <(jq -r '.[].Id' "$D/tasks-before.json")
}
require() {   # require FILE PATH MESSAGE FILTER [jq args]: fetch PATH into FILE and demand the jq FILTER holds of it
  local file=$1 path=$2 message=$3 filter=$4
  shift 4
  if ! api "$path" > "$file" || ! jq -e "$@" "$filter" "$file" >/dev/null; then reason "$message"; return 1; fi
}
verify_isolation() {
  require "$D/plugins.json" /Plugins "plugins not disabled" \
    'type == "array" and all(.[]; .Status == "Disabled" or .CanUninstall == false)' || return 1
  require "$D/tasks.json" /ScheduledTasks "automatic task triggers remain enabled" \
    'type == "array" and length > 0 and all(.[]; .Triggers == [])' || return 1
  require "$D/libraries.json" /Library/VirtualFolders "library monitoring, automatic refresh or fetchers not disabled" \
    'type == "array" and length > 0 and all(.[]; .LibraryOptions | .EnableRealtimeMonitor == false and .AutomaticRefreshIntervalDays == 0 and (.TypeOptions | length > 0 and all(.[]; .MetadataFetchers == [] and .ImageFetchers == [])))'
}
boot_server() {   # boot_server NAME IMAGE CONFIG_SOURCE PORT CPUS: runs in a background subshell, never exits the run
  NAME=$1; local image=$2 source=$3 port=$4 cpus=$5 started=$SECONDS
  D=$OUT/$NAME; URL=http://127.0.0.1:$port; TOKEN=""
  local cfg=$D/config cache=$D/cache env=() cpuset=()
  mkdir -p "$D" "$cache"
  echo "$URL" > "$D/url"   # for gates/parity/reset.sh
  cp -r --reflink=auto "$source" "$cfg" || { reason "cannot copy the configuration"; return 1; }
  # The parity answers are the core servers': no WASM plugin may ride along in the copy (as gates/bench/run.sh).
  if [ -n "$(find "$cfg" -name '*.wasm' -print -quit 2>/dev/null)" ]; then reason "a WASM plugin is present in the configuration copy"; return 1; fi
  # Change only the disposable copy, before either server starts its filesystem watcher (as gates/bench/run.sh,
  # whose isolate/verify_isolation/drain this section mirrors: change them together; gl-754 tracks sharing them).
  python3 - "$cfg" <<'PY_ISOLATION' || { reason "no library options in the configuration copy"; return 1; }
import pathlib, sys, xml.etree.ElementTree as ET
files = list(pathlib.Path(sys.argv[1]).glob("root/default/*/options.xml"))
if not files:
    raise SystemExit("no library options found in the configuration")
for path in files:
    tree = ET.parse(path)
    root = tree.getroot()
    node = root.find("EnableRealtimeMonitor")
    if node is None:
        node = ET.SubElement(root, "EnableRealtimeMonitor")
    node.text = "false"
    tree.write(path, encoding="utf-8", xml_declaration=True)
PY_ISOLATION
  # Ferrofin's compiled-in extensions (Intro Skipper, Merge Versions, …) are plugins Jellyfin does not have: off.
  [ "$NAME" != ferrofin ] || env=(-e FERROFIN_DATA_DIR=/config -e FERROFIN_CACHE_DIR=/cache -e FERROFIN_DISABLE_EXTENSIONS=1)
  [ "$PIN" = 0 ] || cpuset=(--cpuset-cpus "$cpus")
  timeout --kill-after="$KILL_GRACE_S" "$READY_TIMEOUT_S" docker run -d --cidfile "$D/container.cid" --name "parity-$NAME" --hostname parity-server --user "$(id -u):$(id -g)" "${cpuset[@]}" \
    --memory "$MEMORY" --memory-swap "$MEMORY" -p "127.0.0.1:$port:8096" "${env[@]}" \
    -v "$cfg:/config" -v "$cache:/cache" -v "$TESTDATA/media:/media:ro" "$image" >/dev/null || { reason "docker could not start $image"; return 1; }
  wait_ready || { reason "did not answer /System/Ping within ${READY_TIMEOUT_S}s"; return 1; }
  api_version=$(curl -sf --max-time "$HTTP_TIMEOUT_S" "$URL/System/Info/Public" | jq -r .Version) || { reason "cannot read its version"; return 1; }
  if [ "$NAME" = oracle ]; then
    [ "$api_version" = "$(jq -r '.validation.system_info.Version' "$PREP")" ] || { reason "expected Jellyfin $(jq -r '.validation.system_info.Version' "$PREP"), got $api_version"; return 1; }
    python3 gates/bench/oracle.py matches "$api_version" 2> "$D/oracle-matches.err" || { reason "$(cat "$D/oracle-matches.err")"; return 1; }
  fi
  TOKEN=$(python3 gates/parity/diff.py login --url "$URL" --ids "$IDS" --timeout "$HTTP_TIMEOUT_S") || { reason "cannot sign in as the browse user"; return 1; }
  isolate || { reason "could not disable plugins and automatic tasks"; return 1; }
  # Jellyfin unloads disabled plugin assemblies only on restart. While the server is stopped, its isolated
  # configuration is the snapshot every write starts from: gates/parity/reset.sh restores it before each one.
  stop_container "parity-$NAME" || { reason "isolation restart failed (stop)"; return 1; }
  mkdir -p "$D/pristine"
  if ! cp -r --reflink=auto "$cfg" "$D/pristine/config" || ! cp -r --reflink=auto "$cache" "$D/pristine/cache"; then
    reason "cannot snapshot the isolated configuration"; return 1
  fi
  docker_cmd start "parity-$NAME" >/dev/null || { reason "isolation restart failed (start)"; return 1; }
  wait_ready || { reason "did not come back after the isolation restart"; return 1; }
  TOKEN=$(python3 gates/parity/diff.py login --url "$URL" --ids "$IDS" --timeout "$HTTP_TIMEOUT_S") || { reason "cannot sign in after the restart"; return 1; }
  verify_isolation || return 1
  drain || { reason "scheduled tasks did not go idle within ${DRAIN_TIMEOUT_S}s"; return 1; }
  jq -n --argjson boot_s "$((SECONDS - started))" --arg version "$api_version" '{boot_s:$boot_s, version:$version}' > "$D/boot.json"
}


# Both servers boot at once, each on half of the server cpuset, each on its own copy of the SAME
# configuration: the one the pinned Jellyfin upgraded and scanned. (The benchmark boots Ferrofin on the
# 10.11.8 source instead; a replay on that would compare two different scans, not two servers.)
( boot_server oracle "$ORACLE_IMAGE" "$TESTDATA/jellyfin/config" "$ORACLE_PORT" "${ORACLE_CPUS:-}" ) & ORACLE_PID=$!
( boot_server ferrofin "$FERROFIN_IMAGE" "$TESTDATA/jellyfin/config" "$FERROFIN_PORT" "${FERROFIN_CPUS:-}" ) & FERROFIN_PID=$!
boot_failed=0
wait "$ORACLE_PID" || boot_failed=1
wait "$FERROFIN_PID" || boot_failed=1
if (( boot_failed )); then
  FAIL_REASON=$(for name in oracle ferrofin; do [ ! -s "$OUT/$name/failed" ] || echo "$name: $(cat "$OUT/$name/failed")"; done | paste -sd';' -)
  FAIL_REASON=${FAIL_REASON:-a server did not boot}
  exit 1
fi
jq --slurpfile o "$OUT/oracle/boot.json" --slurpfile f "$OUT/ferrofin/boot.json" \
  '.oracle.boot_s = $o[0].boot_s | .oracle.version = $o[0].version | .ferrofin.boot_s = $f[0].boot_s | .ferrofin.version = $f[0].version' \
  "$OUT/run.json" > "$OUT/run.json.tmp" && mv "$OUT/run.json.tmp" "$OUT/run.json"

remaining=$((DEADLINE - $(date +%s)))
if (( remaining <= 0 )); then CAP_EXCEEDED=true; FAIL_REASON="cap exceeded: the servers took the whole ${PARITY_CAP_S}s to boot"; exit 1; fi
pin_prefix=(); [ "$PIN" = 0 ] || pin_prefix=(taskset -c "$CLIENT_CPUS")
# python stops itself at the deadline; the timeout is the backstop for anything that hangs it. It runs
# as a job we wait on, so a signal to this script is handled at once (timeout leads its own process
# group, which a signal to ours does not reach) and cleanup stops it.
rc=0
timeout --kill-after="${CLEANUP_TIMEOUT_S}s" "$((remaining + CLEANUP_TIMEOUT_S))s" "${pin_prefix[@]}" python3 gates/parity/diff.py run \
  --oracle-url "http://127.0.0.1:$ORACLE_PORT" --ferrofin-url "http://127.0.0.1:$FERROFIN_PORT" --ids "$IDS" "${request_args[@]}" \
  --out "$OUT" --request-timeout "$PARITY_REQUEST_TIMEOUT" --deadline "$DEADLINE" --concurrency "$PARITY_CONCURRENCY" \
  --reset-command "$(printf '%q ' bash gates/parity/reset.sh "$OUT")" 2> "$OUT/diff.err" &
wait "$!" || rc=$?
cat "$OUT/diff.err" >&2
# diff.py exits 0 (nothing new), 1 (new differences) or 2 (an input it could not use); anything else
# is the backstop timeout (124, 137) or a crash. Whatever it was, the run's exit is 0 or 1.
case "$rc" in
  0|1) ;;
  124|137) CAP_EXCEEDED=true; FAIL_REASON="cap exceeded: the comparison did not stop at the ${PARITY_CAP_S}s limit and was killed (exit $rc)"; rc=1;;
  *) FAIL_REASON="the comparison could not run (exit $rc): $(tail -n1 "$OUT/diff.err")"; rc=1;;
esac
if [ ! -f "$OUT/report.html" ]; then
  FAIL_REASON=${FAIL_REASON:-the comparison wrote no report: $(tail -n1 "$OUT/diff.err")}
  rc=1
fi
echo "parity: report $OUT/report.html  (compare runs: python3 gates/bench/report.py --serve --parity-dir gates/parity/runs)"
exit "$rc"
