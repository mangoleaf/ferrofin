#!/usr/bin/env bash
# gates/adoption/run.sh [--fixtures DIR] [--image IMAGE] [--only NAME] [--user USERNAME] [--oracle 12.1|12.2|12.4]
#
# Adopts every Jellyfin release Ferrofin claims to support, 17 paths: 10.11.8, 10.11.9, 10.11.10,
# 10.11.11, 12.0, 12.1 via both upgrade routes, 12.2 via all three (from 10.11, 12.0, 12.1), 12.3
# from 10.11 and from 12.2, 12.4 from 10.11 and from 12.3, and a fresh install (not upgraded from
# 10.11) of each of 12.2, 12.3 and 12.4, through one image, on a FRESH copy of each pristine
# fixture under DIR, and checks each one
# the same way:
#   1. the boot log names the expected generation, applies every migration, logs no ERROR;
#   2. smoke.sh answers match the oracle release's own answers on the same library
#      (DIR/oracle/smoke-jellyfin-<oracle>.txt; --oracle or ADOPTION_ORACLE, default 12.4, the
#      latest release; a fresh install answers DIR/oracle/smoke-jellyfin-<oracle>-fresh.txt, 12.4's
#      with --oracle 12.1, which has no fresh-install fixture),
#      ignoring lines that legitimately differ (server version, task/plugin lists, /Devices,
#      folder order, activity-log count, image byte size); a 10.11 source compares /Persons
#      after the scan in step 5 instead, when the whole smoke is compared again;
#   3. PRAGMA integrity_check / foreign_key_check are clean on the adopted file;
#   4. a second boot runs no repair and changes no answer;
#   5. populated metadata survives both boots and an explicit library scan (DB + HTTP).
#   6. watch history survives each stage for every user (DB + HTTP).
#   7. every row of the Jellyfin-owned tables no other check covers (table_digests.py) survives
#      each stage, bar the rewrites the migration chain makes by design.
# After those, the "messy" fixtures (messy.py: rows of a deleted user, an extra with a dangling
# owner, duplicate paths) are adopted too, each shape held to the end state Jellyfin 12.4 gave it,
# and the table digests (check 7) checked beside them. --only NAME also selects a messy fixture,
# by its directory name or by its generation (10.11.11, 12.2.0).
# Last, the scale fixture (scale.py: the 10.11.11 library cloned up to 40,000 items) is adopted, after
# a check that the disk can hold it. Every cloned item must survive, and the run records what the
# adoption cost: migration time, peak memory, disk high-water (WAL and .pre-* snapshots included)
# and the time of the every-boot foreign_key_check, on its PASS line and in
# WORK/scale-measurements-<fixture>.json. Recorded, not gated. The boot may take
# ADOPTION_SCALE_TIMEOUT seconds (default 1800) to answer. --only selects it like a messy fixture.
# Every check's verdict is recorded as it runs, and WORK/report.html (gates/adoption/adoption_report.py) is built from
# those however the run ends: per fixture and stage, every check, the table-digest differences, the
# messy shapes against Jellyfin 12.4 and the scale measurements. It opens from disk, and the shared
# viewer lists it (python3 gates/bench/report.py --serve). WORK/result.json beside it holds the same rows
# in every gate's envelope (gates/result.py): what the gates session page and the release summary read.
# One PASS/FAIL line per fixture; exit status is non-zero if any failed. Each fixture
# includes two boots and a full scan. Fixtures are synthetic and NOT in the repository:
# build-fixtures.sh builds DIR (invented media under DIR/media, the only media mounted) — see
# gates/adoption/README.md. DIR and the work directory must be on a local disk.
# Without --fixtures (or FERROFIN_ADOPTION_FIXTURES), DIR is the one build-fixtures.sh builds under
# $FERROFIN_GATES_DIR (fixtures/adoption-<key>). The work directory is ADOPTION_WORK_DIR, else
# $FERROFIN_GATES_DIR/work/adoption, else DIR/work/run (gates/lib.sh).
set -uo pipefail
ARGS=("$@")   # the worker gets them again (see below)
HERE=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=gates/adoption/lib.sh
. "$HERE/lib.sh"
FIXTURES=${FERROFIN_ADOPTION_FIXTURES:-}; IMAGE=${IMAGE:-ferrofin:bench}; ONLY=; USER_NAME=${ADOPTION_USER:-}
ORACLE_VERSION=${ADOPTION_ORACLE:-12.4}
while [ $# -gt 0 ]; do case $1 in
  --fixtures) FIXTURES=$2; shift 2;; --image) IMAGE=$2; shift 2;; --only) ONLY=$2; shift 2;; --user) USER_NAME=$2; shift 2;;
  --oracle) ORACLE_VERSION=$2; shift 2;;
  -h|--help) sed -n 2,45p "$0"; exit 0;; *) echo "run: unknown argument $1" >&2; exit 2;; esac; done
case $ORACLE_VERSION in 12.1|12.2|12.4) ;; *) echo "run: --oracle must be 12.1, 12.2 or 12.4, not '$ORACLE_VERSION'" >&2; exit 2;; esac
[ -n "$FIXTURES" ] || FIXTURES=$(adoption_fixtures_dir run) || exit 2
[ -d "$FIXTURES" ] || { echo "run: $FIXTURES missing — run gates/adoption/build-fixtures.sh first" >&2; exit 2; }
FIXTURES=$(cd "$FIXTURES" && pwd)
WORK=${ADOPTION_WORK_DIR:-}
if [ -z "$WORK" ] && [ -n "${FERROFIN_GATES_DIR:-}" ]; then WORK=$(gates_dir run work/adoption) || exit 2; fi
WORK=${WORK:-$FIXTURES/work/run}  # synthetic.py run owns work/<fixture>
for t in docker sqlite3 jq curl python3; do command -v $t >/dev/null || { echo "run: $t not installed" >&2; exit 2; }; done
python3 -c 'from PIL import Image' 2>/dev/null || { echo "run: Python Pillow is required for artwork checks" >&2; exit 2; }
python3 -c 'import icu' 2>/dev/null || { echo "run: Python PyICU (python3-icu) is required for the CleanValue check" >&2; exit 2; }
docker image inspect "$IMAGE" >/dev/null 2>&1 || { echo "run: image $IMAGE missing (docker build -t ferrofin:bench .)" >&2; exit 2; }
ORACLE=$FIXTURES/oracle/smoke-jellyfin-$ORACLE_VERSION.txt
# A first-run install answers the oracle release's answers on a first-run library; 12.1 has no
# fresh-install fixture, so with --oracle 12.1 the fresh rows keep the latest release's.
FRESH_VERSION=$ORACLE_VERSION; [ "$FRESH_VERSION" != 12.1 ] || FRESH_VERSION=12.4
FRESH_ORACLE=$FIXTURES/oracle/smoke-jellyfin-$FRESH_VERSION-fresh.txt
[ -f "$ORACLE" ] || { echo "run: $ORACLE missing — run gates/adoption/build-fixtures.sh first" >&2; exit 2; }
# the probes must run as the account the oracle ran as; the builder records it
[ -n "$USER_NAME" ] || [ ! -f "$FIXTURES/oracle/user.txt" ] || USER_NAME=$(cat "$FIXTURES/oracle/user.txt")
[ -d "$FIXTURES/media" ] || { echo "run: $FIXTURES/media missing — run gates/adoption/build-fixtures.sh first" >&2; exit 2; }
gates_require_local "$FIXTURES" && gates_require_local "$WORK" || exit 2; mkdir -p "$WORK"
MEDIA=(-v "$FIXTURES/media:/media:ro")
# The report: recorded as the run goes, built when it ends. Recording never changes the verdict.
REPORT_PY="$HERE/adoption_report.py"
record() { python3 "$REPORT_PY" "$@" >/dev/null 2>&1 || true; }
# record_detail <args...> <detail>: the last argument goes in on stdin, so a detail that starts with "-" or is huge survives.
record_detail() { local detail=${!#}; set -- "${@:1:$#-1}"; printf '%s' "$detail" | python3 "$REPORT_PY" "$@" --detail-stdin >/dev/null 2>&1 || true; }
# bash 5.2 loses a trapped signal that arrives while it is parsing a $( … ): it prints "trap: line 2:
# unexpected EOF while looking for matching `)'" and carries on, and this script parses them all the time, so
# a TERM could go unanswered until every readiness deadline had run out (gl-920; bash 5.3 is not affected, and
# a trap that only sets a flag fails the same way). So the run itself goes to a worker, in a session of its
# own and with no traps (TERM keeps its default action; INT and QUIT are ignored, as for any asynchronous
# command, and so they are here too when this shell was started in the background), and this shell only
# waits for it: a signal always finds it in `wait`. It then stops the worker's process group (the worker,
# its sampler, any docker or curl call: nothing in the run starts a group of its own) and builds the
# report. A SIGKILL cannot be passed on: kill this shell that way and the worker runs on.
if [ -z "${ADOPTION_RUN_WORKER:-}" ]; then
  command -v setsid >/dev/null || { echo "run: setsid (util-linux) is required" >&2; exit 2; }
  worker='' spawned=''
  # the build the report names; read before the traps below, so no $( … ) runs while they are set
  IMAGE_ID=$(docker image inspect -f '{{.Id}}' "$IMAGE" 2>/dev/null)
  IMAGE_COMMIT=$(docker image inspect -f '{{index .Config.Labels "org.opencontainers.image.revision"}}' "$IMAGE" 2>/dev/null)
  # shellcheck disable=SC2317,SC2329  # invoked by the EXIT trap (SC2317 is the code before shellcheck 0.11)
  finish() {
    trap '' INT TERM HUP QUIT   # a second signal must not cut the report short
    # a signal between the spawn and `worker=$!` finds the worker in $! alone
    local w=${worker:-}; [ -n "$w" ] || [ -z "$spawned" ] || w=${!:-}
    if [ -n "$w" ]; then kill -TERM -- "-$w" 2>/dev/null; wait "$w" 2>/dev/null; fi
    python3 "$REPORT_PY" build "$WORK" >/dev/null 2>&1 || true
    # The recorder's own messages go to /dev/null so they never change what a run prints; a missing page is said here.
    if [ -f "$WORK/report.html" ]; then [ ! -t 1 ] || echo "report: $WORK/report.html"; else echo "report: not written to $WORK/report.html" >&2; fi
  }
  trap finish EXIT
  trap 'exit 130' INT   # bash runs the EXIT trap only on an exit, not on a fatal signal: the report is built either way
  trap 'exit 143' TERM
  trap 'exit 129' HUP
  trap 'exit 131' QUIT   # the worker ignores it (an asynchronous command): Ctrl-\ stops the run here
  record start "$WORK" --title "gates/adoption/run.sh" --image "$IMAGE" --oracle "$ORACLE_VERSION" \
    --image-id "$IMAGE_ID" --commit "$IMAGE_COMMIT"
  # <&0: an asynchronous command's stdin would otherwise be /dev/null. Without job control the background
  # job leads no process group, so setsid makes the session in place and $! is its leader and group.
  spawned=1
  ADOPTION_RUN_WORKER=1 setsid "$BASH" "$0" "${ARGS[@]}" <&0 &
  worker=$!
  status=0; wait "$worker" || status=$?
  worker='' spawned=''
  exit "$status"
fi
sampler=
trap '[ -z "$sampler" ] || kill "$sampler" 2>/dev/null' EXIT
# fixture_line <kind> <verdict> <expected> <fixture> <detail>: the PASS/FAIL line, and the fixture's row in the report.
fixture_line() {
  printf '%-5s %-9s %-28s %s\n' "$2" "$3" "$4" "$5"
  record_detail row "$WORK" "$4" "$1" "$3" "$2" "$5"
}
# mark <stage> <check>: record the verdict of the check that just ran, from the reasons it added to why[].
mark_from=0
mark() {
  local detail='' verdict=PASS i
  for ((i = mark_from; i < ${#why[@]}; i++)); do detail+="${detail:+; }${why[i]}"; verdict=FAIL; done
  mark_from=${#why[@]}
  record_detail check "$WORK" "$src" "$1" "$2" "$verdict" "$detail"
}
# attach_tables <fixture directory copy> <fixture>: the table-digest differences a stage logged, for the report.
attach_tables() {
  local f stage
  for f in "$1".tables-*.log; do
    [ -f "$f" ] || continue
    ! head -n 1 "$f" | grep -q '^PASS ' || continue
    stage=${f#"$1".tables-}; stage=${stage%.log}
    record attach "$WORK" "$2" "tables:${stage//-/ }" "$f" --text
  done
}
# name|fixture directory|host port
# The generation is the id SET the gate matches, so 10.11.9 adopts as "10.11.8" (it adds no
# migration), 10.11.10 as "10.11.11" (both add the three NormalizedUsername ids), and 12.3 and 12.4
# as "12.2.0" (neither adds a migration). Every jellyfin-*-from-* row is an upgraded database;
# each jellyfin-*-fresh row is a database created by that release's first run (not upgraded from
# 10.11), so it carries the fresh-install EF history id set (105 ids, without the retired
# 10.11-era routines) and the fresh-install schema, yet still adopts as generation 12.2.0 and
# answers the oracle release's smoke on a first-run library (an oracle of its own, since a clean
# install is a distinct library from the seeded routes).
FIXTURE_TABLE=(
  "10.11.8|jellyfin-10.11.8|18099"
  "10.11.8|jellyfin-10.11.9|18089"
  "10.11.11|jellyfin-10.11.10|18088"
  "10.11.11|jellyfin-10.11.11|18098"
  "12.0.0|jellyfin-12.0|18097"
  "12.1.0|jellyfin-12.1-from-10|18093"
  "12.1.0|jellyfin-12.1-from-12|18092"
  "12.2.0|jellyfin-12.2-from-10|18091"
  "12.2.0|jellyfin-12.2-from-12|18090"
  "12.2.0|jellyfin-12.2-from-121|18083"
  "12.2.0|jellyfin-12.2-fresh|18082"
  "12.2.0|jellyfin-12.3-from-122|18078"
  "12.2.0|jellyfin-12.3-from-10|18077"
  "12.2.0|jellyfin-12.3-fresh|18076"
  "12.2.0|jellyfin-12.4-from-123|18075"
  "12.2.0|jellyfin-12.4-from-10|18074"
  "12.2.0|jellyfin-12.4-fresh|18073"
)
wait_ready() { local port=$1; for _ in $(seq 1 300); do curl -sf "http://127.0.0.1:$port/System/Info/Public" >/dev/null && return 0; sleep 2; done; return 1; }
# wait_up <container> <port> [seconds]: ready, or failed as soon as the container has exited (a server
# that refuses its database exits at once, and nothing is gained by waiting out the whole deadline),
# or after `seconds` (default 600, the deadline of wait_ready).
wait_up() {
  local name=$1 port=$2 deadline=$((SECONDS + ${3:-600}))
  while [ "$SECONDS" -lt "$deadline" ]; do
    curl -sf "http://127.0.0.1:$port/System/Info/Public" >/dev/null && return 0
    [ "$(docker inspect -f '{{.State.Running}}' "$name" 2>/dev/null)" = true ] || return 1
    sleep 2
  done
  return 1
}
# auth_for <jellyfin.db>: the Authorization header of the newest API key in that database, for settle.
auth_for() {
  local key
  key=$(sqlite3 -readonly "file:$1?mode=ro" 'SELECT AccessToken FROM ApiKeys ORDER BY DateCreated DESC LIMIT 1')
  printf 'Authorization: MediaBrowser Token="%s", Client="adoption", Device="adoption", DeviceId="adoption", Version="1"' "$key"
}
settle() { local port=$1 auth=$2 busy; for _ in $(seq 1 60); do busy=$(curl -sf "http://127.0.0.1:$port/ScheduledTasks" -H "$auth" 2>/dev/null | jq -r '[.[]|select(.State!="Idle")]|length' 2>/dev/null); [ "${busy:-1}" = 0 ] && break; sleep 2; done; sleep 5; }
# Capture status as well as output: a failed checker must never silently pass.
metadata_check() {
  local stage=$1 mode=$2 result log
  log=$dst.metadata-${stage// /-}.log
  if ! result=$(python3 "$HERE/metadata.py" "$mode" "$dst/data/jellyfin.db" "$dst.metadata.json" \
      --base-url "http://127.0.0.1:$port" --timeout "${ADOPTION_SCAN_TIMEOUT:-600}" 2>&1); then
    printf '%s\n' "${result:-check failed}" > "$log"
    why+=("metadata $stage: ${result:-check failed}")
  else
    printf 'PASS metadata %s\n' "$stage" > "$log"
  fi
  mark "$stage" "metadata (database and API)"
  log=$dst.watch-history-${stage// /-}.log
  if ! result=$(python3 "$HERE/watch_history.py" check "$dst/data/jellyfin.db" "$dst.watch-history.json" \
      --base-url "http://127.0.0.1:$port" 2>&1); then
    printf '%s\n' "${result:-check failed}" > "$log"
    why+=("watch history $stage: ${result:-check failed}")
  else
    printf 'PASS watch history %s\n' "$stage" > "$log"
  fi
  mark "$stage" "watch history"
  log=$dst.tables-${stage// /-}.log
  if ! result=$(python3 "$HERE/table_digests.py" check "$dst/data/jellyfin.db" "$dst.tables.json" 2>&1); then
    printf '%s\n' "${result:-check failed}" > "$log"
    why+=("tables $stage: $(wc -l <<<"${result:-check failed}") difference(s), first: $(head -n 1 <<<"${result:-check failed}"); all in $log")
  else
    printf 'PASS tables %s\n' "$stage" > "$log"
  fi
  mark "$stage" "table digests"
}
failed=0
for spec in "${FIXTURE_TABLE[@]}"; do
  IFS='|' read -r expected src port <<<"$spec"
  [ -z "$ONLY" ] || [ "$ONLY" = "$src" ] || [ "$ONLY" = "$expected" ] || continue
  [ -d "$FIXTURES/$src" ] || { fixture_line path SKIP "$expected" "$src" "fixture missing"; continue; }
  # A first-run install is a distinct, clean library (no aged seeds), so it answers the
  # fresh-install oracle (FRESH_ORACLE); every other row answers the shared ORACLE_VERSION
  # oracle. Either way it is Jellyfin's own answers on the row's library, compared against
  # Ferrofin's after adoption; `against` names that release in the checks and their reasons.
  case $src in
    jellyfin-*-fresh) oracle=$FRESH_ORACLE against=$FRESH_VERSION;;
    *) oracle=$ORACLE against=$ORACLE_VERSION;;
  esac
  [ -f "$oracle" ] || { fixture_line path SKIP "$expected" "$src" "oracle ${oracle##*/} missing — run build-fixtures.sh"; continue; }
  name=adopt-$src; dst=$WORK/$src
  record begin "$WORK" "$src" path "$expected"
  # A 10.11 source has no music-artist credits until its first 12.x or Ferrofin scan: 10.11
  # never made them, and 12.x makes them only on a metadata refresh (AudioFileProber,
  # AlbumMetadataService.SetPeople), as Ferrofin does. The oracle and the 12.x fixtures carry
  # their release's own scan (synthetic.py scans every fixture it derives), so a 10.11 path
  # compares /Persons, and the whole smoke once more, after the scan stage.
  case $expected in 10.11.*) after_scan='/Persons\?';; *) after_scan=;; esac
  docker rm -f "$name" >/dev/null 2>&1; rm -rf "$dst" "$dst-cache"; mkdir -p "$WORK"
  cp -a "$FIXTURES/$src" "$dst"; mkdir -p "$dst-cache"
  if ! baseline=$(python3 "$HERE/metadata.py" snapshot "$dst/data/jellyfin.db" "$dst.metadata.json" 2>&1); then
    fixture_line path FAIL "$expected" "$src" "metadata baseline: $baseline"
    failed=1
    continue
  fi
  if ! baseline=$(python3 "$HERE/watch_history.py" snapshot "$dst/data/jellyfin.db" "$dst.watch-history.json" 2>&1); then
    fixture_line path FAIL "$expected" "$src" "watch history baseline: $baseline"
    failed=1
    continue
  fi
  if ! baseline=$(python3 "$HERE/table_digests.py" snapshot "$dst/data/jellyfin.db" "$dst.tables.json" 2>&1); then
    fixture_line path FAIL "$expected" "$src" "table baseline: ${baseline:-check failed}"
    failed=1
    continue
  fi
  # Provider failure must not erase adopted metadata. An unreachable local
  # proxy keeps providers from refilling lost values or depending on live keys.
  # HTTP probes run from the host; cached artwork and media stay available.
  docker run -d --name "$name" --user "$(id -u):$(id -g)" -p "127.0.0.1:$port:8096" \
    -e FERROFIN_DATA_DIR=/config -e FERROFIN_CACHE_DIR=/cache \
    -e HTTP_PROXY=http://127.0.0.1:9 -e HTTPS_PROXY=http://127.0.0.1:9 \
    -e ALL_PROXY=http://127.0.0.1:9 -e NO_PROXY=127.0.0.1,localhost \
    -v "$dst:/config" -v "$dst-cache:/cache" "${MEDIA[@]}" "$IMAGE" >/dev/null
  why=(); mark_from=0
  wait_ready "$port" || why+=("never became ready")
  mark "after adoption" "ready"
  APIKEY=$(sqlite3 -readonly "file:$dst/data/jellyfin.db?mode=ro" 'SELECT AccessToken FROM ApiKeys ORDER BY DateCreated DESC LIMIT 1')
  AUTH="Authorization: MediaBrowser Token=\"$APIKEY\", Client=\"adoption\", Device=\"adoption\", DeviceId=\"adoption\", Version=\"1\""
  settle "$port" "$AUTH"
  docker logs "$name" > "$dst.boot.log" 2>&1
  mapfile -t -O "${#why[@]}" why < <(adoption_check_boot_log "$dst.boot.log" "$expected")
  mark "after adoption" "boot log (generation, migrations, no ERROR)"
  "$HERE/smoke.sh" "http://127.0.0.1:$port" "$dst" "$USER_NAME" > "$dst.smoke.txt" 2>&1
  mapfile -t -O "${#why[@]}" why < <(ORACLE_VERSION=$against adoption_compare_smoke "$oracle" "$dst.smoke.txt" "$after_scan")
  mark "after adoption" "answers match Jellyfin $against"
  mapfile -t -O "${#why[@]}" why < <(adoption_check_db "$dst/data/jellyfin.db")
  mark "after adoption" "integrity_check and foreign_key_check"
  metadata_check "after adoption" check
  docker restart "$name" >/dev/null; wait_ready "$port" || why+=("second boot never ready")
  mark "after restart" "second boot ready"
  settle "$port" "$AUTH"
  # From the daemon's own start time for the restarted container, so the log holds this boot
  # only; a log with no server start must fail, never pass as "no repairs".
  docker logs --since "$(docker inspect -f '{{.State.StartedAt}}' "$name")" "$name" > "$dst.boot2.log" 2>&1
  grep -q '"ferrofin-server starting"' "$dst.boot2.log" || why+=("second boot log has no server start")
  second=$(adoption_second_boot_repairs "$dst.boot2.log")
  [ -z "$second" ] || why+=("second boot repaired again: $second")
  mark "after restart" "second boot runs no repair"
  "$HERE/smoke.sh" "http://127.0.0.1:$port" "$dst" "$USER_NAME" > "$dst.smoke2.txt" 2>&1
  diff -q <(adoption_normalise "$dst.smoke.txt") <(adoption_normalise "$dst.smoke2.txt") >/dev/null || why+=("second boot answers differ")
  mark "after restart" "answers unchanged by the restart"
  metadata_check "after restart" check
  metadata_check "after scan" scan
  if [ -n "$after_scan" ]; then
    settle "$port" "$AUTH"
    "$HERE/smoke.sh" "http://127.0.0.1:$port" "$dst" "$USER_NAME" > "$dst.smoke-scan.txt" 2>&1
    mapfile -t -O "${#why[@]}" why < <(ORACLE_VERSION=$against adoption_compare_smoke "$oracle" "$dst.smoke-scan.txt" | sed 's/^/after scan: /')
    mark "after scan" "answers match Jellyfin $against"
  fi
  mapfile -t -O "${#why[@]}" why < <(adoption_check_db "$dst/data/jellyfin.db")
  mark "after scan" "integrity_check and foreign_key_check"
  verdict=PASS; [ "${#why[@]}" = 0 ] || verdict=FAIL
  docker logs "$name" > "$dst.server.log" 2>&1; docker rm -f "$name" >/dev/null
  attach_tables "$dst" "$src"
  fixture_line path "$verdict" "$expected" "$src" "$(IFS='; '; echo "${why[*]:-}")"
  if [ "$verdict" = PASS ]; then rm -rf "$dst" "$dst-cache" "$dst".*; else failed=1; fi
done
# The messy fixtures (messy.py): a clean fixture plus the rows of shapes long-lived databases
# carry and no API can produce (nine shapes: rows of a deleted user, an extra whose owner is gone,
# two rows for one path, odd id spellings, names differing in case, ...). The same image adopts a
# fresh copy of each, and every shape must end as Jellyfin 12.4 left it (DIR/<fixture>/messy-oracle.json)
# or as a divergence written down in messy.py. They
# answer no smoke oracle: their library is the clean one plus rows no endpoint shows, so only the
# boot, the file's integrity, a quiet second boot and the shapes are checked.
# The messy rows mount media-messy (a hard-linked copy of the shared tree plus a link per file a
# seeded item needs; messy.py), else the shared tree of a fixture directory built before it existed.
MESSY_MEDIA=("${MEDIA[@]}")
[ ! -d "$FIXTURES/media-messy" ] || MESSY_MEDIA=(-v "$FIXTURES/media-messy:/media:ro")
# expected generation|fixture directory|host port
MESSY_TABLE=(
  "10.11.11|jellyfin-10.11.11-messy|18081"
  "12.2.0|jellyfin-12.4-messy|18080"
)
for spec in "${MESSY_TABLE[@]}"; do
  IFS='|' read -r expected src port <<<"$spec"
  [ -z "$ONLY" ] || [ "$ONLY" = "$src" ] || [ "$ONLY" = "$expected" ] || continue
  [ -d "$FIXTURES/$src" ] || { fixture_line messy SKIP "$expected" "$src" "fixture missing"; continue; }
  name=adopt-$src; dst=$WORK/$src
  record begin "$WORK" "$src" messy "$expected"
  docker rm -f "$name" >/dev/null 2>&1; rm -rf "$dst" "$dst-cache" "$dst".tables-*.log; mkdir -p "$WORK"
  cp -a "$FIXTURES/$src" "$dst"; mkdir -p "$dst-cache"
  # The table digests (check 7) are taken before Ferrofin starts, from the clean fixture the messy
  # copy was made from: a row it loses beside the messy ones fails, while the messy rows themselves
  # (some of which an adoption rightly merges or removes, and the rows of the missing user, which
  # 0044 deletes) are not in the baseline.
  source=$(jq -r '.source // empty' "$FIXTURES/$src/messy.json" 2>/dev/null)
  tables_from=$dst/data/jellyfin.db
  if [ -n "$source" ] && [ -f "$FIXTURES/$source/data/jellyfin.db" ]; then
    cp "$FIXTURES/$source/data/jellyfin.db" "$dst.source.db"   # a copy: reading the fixture in place can leave -wal/-shm files in it
    tables_from=$dst.source.db
  fi
  if ! baseline=$(python3 "$HERE/table_digests.py" snapshot "$tables_from" "$dst.tables.json" 2>&1); then
    fixture_line messy FAIL "$expected" "$src" "table baseline: ${baseline:-check failed}"
    failed=1
    continue
  fi
  docker run -d --name "$name" --user "$(id -u):$(id -g)" -p "127.0.0.1:$port:8096" \
    -e FERROFIN_DATA_DIR=/config -e FERROFIN_CACHE_DIR=/cache \
    -e HTTP_PROXY=http://127.0.0.1:9 -e HTTPS_PROXY=http://127.0.0.1:9 \
    -e ALL_PROXY=http://127.0.0.1:9 -e NO_PROXY=127.0.0.1,localhost \
    -v "$dst:/config" -v "$dst-cache:/cache" "${MESSY_MEDIA[@]}" "$IMAGE" >/dev/null
  why=(); mark_from=0
  if wait_up "$name" "$port"; then
    settle "$port" "$(auth_for "$dst/data/jellyfin.db")"
  else
    why+=("never became ready")
  fi
  mark "after adoption" "ready"
  docker logs "$name" > "$dst.boot.log" 2>&1
  mapfile -t -O "${#why[@]}" why < <(adoption_check_boot_log "$dst.boot.log" "$expected")
  mark "after adoption" "boot log (generation, migrations, no ERROR)"
  mapfile -t -O "${#why[@]}" why < <(adoption_check_db "$dst/data/jellyfin.db")
  mark "after adoption" "integrity_check and foreign_key_check"
  mapfile -t -O "${#why[@]}" why < <(adoption_check_messy "$dst/data/jellyfin.db" "$dst")
  mark "after adoption" "messy shapes end as Jellyfin 12.4 left them"
  mapfile -t -O "${#why[@]}" why < <(adoption_check_tables "$dst/data/jellyfin.db" "$dst.tables.json" "$dst.tables-after-adoption.log" | sed 's/^/after adoption: /')
  mark "after adoption" "table digests"
  docker restart "$name" >/dev/null
  if wait_up "$name" "$port"; then settle "$port" "$(auth_for "$dst/data/jellyfin.db")"; else why+=("second boot never ready"); fi
  mark "after restart" "second boot ready"
  docker logs --since "$(docker inspect -f '{{.State.StartedAt}}' "$name")" "$name" > "$dst.boot2.log" 2>&1
  grep -q '"ferrofin-server starting"' "$dst.boot2.log" || why+=("second boot log has no server start")
  second=$(adoption_second_boot_repairs "$dst.boot2.log")
  [ -z "$second" ] || why+=("second boot repaired again: $second")
  mark "after restart" "second boot runs no repair"
  mapfile -t -O "${#why[@]}" why < <(adoption_check_db "$dst/data/jellyfin.db" | sed 's/^/after restart: /')
  mark "after restart" "integrity_check and foreign_key_check"
  mapfile -t -O "${#why[@]}" why < <(adoption_check_messy "$dst/data/jellyfin.db" "$dst" | sed 's/^/after restart: /')
  mark "after restart" "messy shapes end as Jellyfin 12.4 left them"
  mapfile -t -O "${#why[@]}" why < <(adoption_check_tables "$dst/data/jellyfin.db" "$dst.tables.json" "$dst.tables-after-restart.log" | sed 's/^/after restart: /')
  mark "after restart" "table digests"
  verdict=PASS; [ "${#why[@]}" = 0 ] || verdict=FAIL
  docker logs "$name" > "$dst.server.log" 2>&1; docker rm -f "$name" >/dev/null
  # The shapes' end states for the report (a checker that cannot run leaves the table out; the check above said why).
  if python3 "$HERE/messy.py" shapes "$dst/data/jellyfin.db" "$dst" > "$dst.shapes.json" 2>/dev/null; then
    record attach "$WORK" "$src" shapes "$dst.shapes.json"
  fi
  attach_tables "$dst" "$src"
  fixture_line messy "$verdict" "$expected" "$src" "$(IFS='; '; echo "${why[*]:-}")"
  if [ "$verdict" = PASS ]; then rm -rf "$dst" "$dst-cache" "$dst".*; else failed=1; fi
done
# The scale fixture (scale.py): the clean 10.11.11 library cloned up to 40,000 items by SQL. The same
# image adopts a fresh copy, and the run records what that cost (recorded, not gated). No scan
# runs and no HTTP probe is compared: the clones' files are hard links to their templates', so only
# the boot, the file's integrity, a quiet second boot, the table digests and the survival of every
# cloned item are checked. The disk it needs is checked first; a run that fills the disk takes the
# host with it. It mounts media-scale (scale.py: the shared tree plus a link at each clone's path,
# which the adoption's stale-file sweep looks for), else the shared tree of an older fixture directory.
SCALE_MEDIA=("${MEDIA[@]}")
[ ! -d "$FIXTURES/media-scale" ] || SCALE_MEDIA=(-v "$FIXTURES/media-scale:/media:ro")
# expected generation|fixture directory|host port
SCALE_TABLE=(
  "10.11.11|jellyfin-10.11.11-scale|18079"
)
# Seconds each boot of the 40,000-item library may take to answer: long enough to record a slow
# migration instead of failing it, short enough to catch a hang the same hour. A first boot that never
# answers costs two of them, since the row restarts the container and waits again.
SCALE_TIMEOUT=${ADOPTION_SCALE_TIMEOUT:-1800}
for spec in "${SCALE_TABLE[@]}"; do
  IFS='|' read -r expected src port <<<"$spec"
  [ -z "$ONLY" ] || [ "$ONLY" = "$src" ] || [ "$ONLY" = "$expected" ] || continue
  [ -d "$FIXTURES/$src" ] || { fixture_line scale SKIP "$expected" "$src" "fixture missing"; continue; }
  name=adopt-$src; dst=$WORK/$src
  record begin "$WORK" "$src" scale "$expected"
  # The previous run's copy and numbers go first: it frees the disk this run needs, and a stale
  # samples file must never be read as this run's.
  docker rm -f "$name" >/dev/null 2>&1
  rm -rf "$dst" "$dst-cache" "$dst".tables-*.log "$dst.samples.json" "$WORK/scale-measurements-$src.json"; mkdir -p "$WORK"
  if ! reason=$(python3 "$HERE/scale.py" need-disk "$FIXTURES/$src" "$WORK" 2>&1); then
    fixture_line scale FAIL "$expected" "$src" "${reason:-disk check failed}"
    failed=1
    continue
  fi
  cp -a "$FIXTURES/$src" "$dst"; mkdir -p "$dst-cache"
  if ! baseline=$(python3 "$HERE/table_digests.py" snapshot "$dst/data/jellyfin.db" "$dst.tables.json" 2>&1); then
    fixture_line scale FAIL "$expected" "$src" "table baseline: ${baseline:-check failed}"
    failed=1
    continue
  fi
  if ! counts=$(python3 "$HERE/scale.py" counts "$dst/data/jellyfin.db" "$dst.scale.json" 2>&1); then
    fixture_line scale FAIL "$expected" "$src" "scale baseline: ${counts:-check failed}"
    failed=1
    continue
  fi
  docker run -d --name "$name" --user "$(id -u):$(id -g)" -p "127.0.0.1:$port:8096" \
    -e FERROFIN_DATA_DIR=/config -e FERROFIN_CACHE_DIR=/cache \
    -e HTTP_PROXY=http://127.0.0.1:9 -e HTTPS_PROXY=http://127.0.0.1:9 \
    -e ALL_PROXY=http://127.0.0.1:9 -e NO_PROXY=127.0.0.1,localhost \
    -v "$dst:/config" -v "$dst-cache:/cache" "${SCALE_MEDIA[@]}" "$IMAGE" >/dev/null
  # Disk and memory are sampled from the start of the first boot until it has settled.
  python3 "$HERE/scale.py" sample "$name" "$dst" "$dst.samples.json" & sampler=$!
  why=(); mark_from=0
  if wait_up "$name" "$port" "$SCALE_TIMEOUT"; then
    settle "$port" "$(auth_for "$dst/data/jellyfin.db")"
  else
    why+=("never became ready")
  fi
  mark "after adoption" "ready"
  kill -TERM "$sampler" 2>/dev/null; wait "$sampler" 2>/dev/null; sampler=
  docker logs "$name" > "$dst.boot.log" 2>&1
  measured=$(python3 "$HERE/scale.py" record "$dst.boot.log" "$dst.samples.json" "$dst/data/jellyfin.db" \
             "$WORK/scale-measurements-$src.json" 2>&1); rc=$?
  summary=$(head -n 1 <<<"$measured")
  case $rc in
    0) ;;
    1) mapfile -t -O "${#why[@]}" why < <(tail -n +2 <<<"$measured");;
    *) why+=("scale: ${measured:-recording failed}"); summary=;;
  esac
  mark "after adoption" "measurements recorded"
  mapfile -t -O "${#why[@]}" why < <(adoption_check_boot_log "$dst.boot.log" "$expected")
  mark "after adoption" "boot log (generation, migrations, no ERROR)"
  mapfile -t -O "${#why[@]}" why < <(adoption_check_db "$dst/data/jellyfin.db")
  mark "after adoption" "integrity_check and foreign_key_check"
  mapfile -t -O "${#why[@]}" why < <(adoption_check_tables "$dst/data/jellyfin.db" "$dst.tables.json" "$dst.tables-after-adoption.log" | sed 's/^/after adoption: /')
  mark "after adoption" "table digests"
  mapfile -t -O "${#why[@]}" why < <(adoption_check_scale "$dst/data/jellyfin.db" "$dst.scale.json" | sed 's/^/after adoption: /')
  mark "after adoption" "every cloned item survives"
  docker restart "$name" >/dev/null
  if wait_up "$name" "$port" "$SCALE_TIMEOUT"; then settle "$port" "$(auth_for "$dst/data/jellyfin.db")"; else why+=("second boot never ready"); fi
  mark "after restart" "second boot ready"
  docker logs --since "$(docker inspect -f '{{.State.StartedAt}}' "$name")" "$name" > "$dst.boot2.log" 2>&1
  grep -q '"ferrofin-server starting"' "$dst.boot2.log" || why+=("second boot log has no server start")
  second=$(adoption_second_boot_repairs "$dst.boot2.log")
  [ -z "$second" ] || why+=("second boot repaired again: $second")
  mark "after restart" "second boot runs no repair"
  mapfile -t -O "${#why[@]}" why < <(adoption_check_db "$dst/data/jellyfin.db" | sed 's/^/after restart: /')
  mark "after restart" "integrity_check and foreign_key_check"
  mapfile -t -O "${#why[@]}" why < <(adoption_check_tables "$dst/data/jellyfin.db" "$dst.tables.json" "$dst.tables-after-restart.log" | sed 's/^/after restart: /')
  mark "after restart" "table digests"
  mapfile -t -O "${#why[@]}" why < <(adoption_check_scale "$dst/data/jellyfin.db" "$dst.scale.json" | sed 's/^/after restart: /')
  mark "after restart" "every cloned item survives"
  verdict=PASS; [ "${#why[@]}" = 0 ] || verdict=FAIL
  docker logs "$name" > "$dst.server.log" 2>&1; docker rm -f "$name" >/dev/null
  detail=$(IFS='; '; echo "${why[*]:-}")
  [ -z "$summary" ] || detail="${detail:+$detail; }$summary"
  [ ! -f "$WORK/scale-measurements-$src.json" ] || record attach "$WORK" "$src" scale "$WORK/scale-measurements-$src.json"
  attach_tables "$dst" "$src"
  fixture_line scale "$verdict" "$expected" "$src" "$detail"
  if [ "$verdict" = PASS ]; then rm -rf "$dst" "$dst-cache" "$dst".*; else failed=1; fi
done
exit $failed
