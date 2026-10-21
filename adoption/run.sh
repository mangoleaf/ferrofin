#!/usr/bin/env bash
# adoption/run.sh --fixtures DIR [--image IMAGE] [--only NAME] [--user USERNAME] [--oracle 12.1|12.2]
#
# Adopts every Jellyfin release Ferrofin claims to support, 11 paths: 10.11.8, 10.11.9, 10.11.10,
# 10.11.11, 12.0, 12.1 via both upgrade routes, 12.2 via all three (from 10.11, 12.0, 12.1), and a
# 12.2 fresh install (not upgraded from 10.11), through one image, on a FRESH copy of each pristine
# fixture under DIR, and checks each one
# the same way:
#   1. the boot log names the expected generation, applies every migration, logs no ERROR;
#   2. smoke.sh answers match the oracle release's own answers on the same library
#      (DIR/oracle/smoke-jellyfin-<oracle>.txt; --oracle or ADOPTION_ORACLE, default 12.2),
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
# owner, duplicate paths) are adopted too, each shape held to the end state Jellyfin 12.2 gave it,
# and the table digests (check 7) checked beside them. --only NAME also selects a messy fixture,
# by its directory name or by its generation (10.11.11, 12.2.0).
# Last, the scale fixture (scale.py: the 10.11.11 library cloned up to 40,000 items) is adopted, after
# a check that the disk can hold it. Every cloned item must survive, and the run records what the
# adoption cost: migration time, peak memory, disk high-water (WAL and .pre-* snapshots included)
# and the time of the every-boot foreign_key_check, on its PASS line and in
# WORK/scale-measurements-<fixture>.json. Recorded, not gated. The boot may take
# ADOPTION_SCALE_TIMEOUT seconds (default 1800) to answer. --only selects it like a messy fixture.
# One PASS/FAIL line per fixture; exit status is non-zero if any failed. Each fixture
# includes two boots and a full scan. Fixtures are synthetic and NOT in the repository:
# build-fixtures.sh builds DIR (invented media under DIR/media, the only media mounted) — see
# adoption/README.md. DIR and the work directory must be on a local disk.
set -uo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=adoption/lib.sh
. "$HERE/lib.sh"
FIXTURES=${FERROFIN_ADOPTION_FIXTURES:-}; IMAGE=${IMAGE:-ferrofin:bench}; ONLY=; USER_NAME=${ADOPTION_USER:-}
ORACLE_VERSION=${ADOPTION_ORACLE:-12.2}
while [ $# -gt 0 ]; do case $1 in
  --fixtures) FIXTURES=$2; shift 2;; --image) IMAGE=$2; shift 2;; --only) ONLY=$2; shift 2;; --user) USER_NAME=$2; shift 2;;
  --oracle) ORACLE_VERSION=$2; shift 2;;
  -h|--help) sed -n 2,34p "$0"; exit 0;; *) echo "run: unknown argument $1" >&2; exit 2;; esac; done
case $ORACLE_VERSION in 12.1|12.2) ;; *) echo "run: --oracle must be 12.1 or 12.2, not '$ORACLE_VERSION'" >&2; exit 2;; esac
[ -n "$FIXTURES" ] || { echo "run: --fixtures DIR (or FERROFIN_ADOPTION_FIXTURES) is required" >&2; exit 2; }
FIXTURES=$(cd "$FIXTURES" && pwd)
WORK=${ADOPTION_WORK_DIR:-$FIXTURES/work/run}  # synthetic.py run owns work/<fixture>
for t in docker sqlite3 jq curl python3; do command -v $t >/dev/null || { echo "run: $t not installed" >&2; exit 2; }; done
python3 -c 'from PIL import Image' 2>/dev/null || { echo "run: Python Pillow is required for artwork checks" >&2; exit 2; }
python3 -c 'import icu' 2>/dev/null || { echo "run: Python PyICU (python3-icu) is required for the CleanValue check" >&2; exit 2; }
docker image inspect "$IMAGE" >/dev/null 2>&1 || { echo "run: image $IMAGE missing (docker build -t ferrofin:bench .)" >&2; exit 2; }
ORACLE=$FIXTURES/oracle/smoke-jellyfin-$ORACLE_VERSION.txt
[ -f "$ORACLE" ] || { echo "run: $ORACLE missing — run adoption/build-fixtures.sh first" >&2; exit 2; }
# the probes must run as the account the oracle ran as; the builder records it
[ -n "$USER_NAME" ] || [ ! -f "$FIXTURES/oracle/user.txt" ] || USER_NAME=$(cat "$FIXTURES/oracle/user.txt")
[ -d "$FIXTURES/media" ] || { echo "run: $FIXTURES/media missing — run adoption/build-fixtures.sh first" >&2; exit 2; }
adoption_require_local "$FIXTURES" && adoption_require_local "$WORK" || exit 2; mkdir -p "$WORK"
MEDIA=(-v "$FIXTURES/media:/media:ro")
# name|fixture directory|host port
# The generation is the id SET the gate matches, so 10.11.9 adopts as "10.11.8" (it adds no
# migration) and 10.11.10 as "10.11.11" (both add the three NormalizedUsername ids). Every
# jellyfin-*-from-* row is an upgraded database; jellyfin-12.2-fresh is a database created by a
# Jellyfin 12.2 first run (not upgraded from 10.11), so it carries the fresh-install EF history
# id set (105 ids, without the retired 10.11-era routines) and the fresh-install schema, yet
# still adopts as generation 12.2.0 and answers Jellyfin 12.2's own smoke on that first-run
# library (its own oracle, since a clean install is a distinct library from the seeded routes).
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
  log=$dst.watch-history-${stage// /-}.log
  if ! result=$(python3 "$HERE/watch_history.py" check "$dst/data/jellyfin.db" "$dst.watch-history.json" \
      --base-url "http://127.0.0.1:$port" 2>&1); then
    printf '%s\n' "${result:-check failed}" > "$log"
    why+=("watch history $stage: ${result:-check failed}")
  else
    printf 'PASS watch history %s\n' "$stage" > "$log"
  fi
  log=$dst.tables-${stage// /-}.log
  if ! result=$(python3 "$HERE/table_digests.py" check "$dst/data/jellyfin.db" "$dst.tables.json" 2>&1); then
    printf '%s\n' "${result:-check failed}" > "$log"
    why+=("tables $stage: $(wc -l <<<"${result:-check failed}") difference(s), first: $(head -n 1 <<<"${result:-check failed}"); all in $log")
  else
    printf 'PASS tables %s\n' "$stage" > "$log"
  fi
}
failed=0
for spec in "${FIXTURE_TABLE[@]}"; do
  IFS='|' read -r expected src port <<<"$spec"
  [ -z "$ONLY" ] || [ "$ONLY" = "$src" ] || [ "$ONLY" = "$expected" ] || continue
  [ -d "$FIXTURES/$src" ] || { printf '%-5s %-9s %-28s %s\n' SKIP "$expected" "$src" "fixture missing"; continue; }
  # A first-run install is a distinct, clean library (no aged seeds), so it always answers its
  # own Jellyfin 12.2 oracle (it is a 12.2 database; --oracle does not apply to it). Every other
  # row answers the shared ORACLE_VERSION oracle. Either way it is Jellyfin's own answers on the
  # row's library, compared against Ferrofin's after adoption.
  case $src in jellyfin-12.2-fresh) oracle=$FIXTURES/oracle/smoke-jellyfin-12.2-fresh.txt;; *) oracle=$ORACLE;; esac
  [ -f "$oracle" ] || { printf '%-5s %-9s %-28s %s\n' SKIP "$expected" "$src" "oracle ${oracle##*/} missing — run build-fixtures.sh"; continue; }
  name=adopt-$src; dst=$WORK/$src
  # A 10.11 source has no music-artist credits until its first 12.x or Ferrofin scan: 10.11
  # never made them, and 12.x makes them only on a metadata refresh (AudioFileProber,
  # AlbumMetadataService.SetPeople), as Ferrofin does. The oracle and the 12.x fixtures carry
  # their release's own scan (synthetic.py scans every fixture it derives), so a 10.11 path
  # compares /Persons, and the whole smoke once more, after the scan stage.
  case $expected in 10.11.*) after_scan='/Persons\?';; *) after_scan=;; esac
  docker rm -f "$name" >/dev/null 2>&1; rm -rf "$dst" "$dst-cache"; mkdir -p "$WORK"
  cp -a "$FIXTURES/$src" "$dst"; mkdir -p "$dst-cache"
  if ! baseline=$(python3 "$HERE/metadata.py" snapshot "$dst/data/jellyfin.db" "$dst.metadata.json" 2>&1); then
    printf '%-5s %-9s %-28s %s\n' FAIL "$expected" "$src" "metadata baseline: $baseline"
    failed=1
    continue
  fi
  if ! baseline=$(python3 "$HERE/watch_history.py" snapshot "$dst/data/jellyfin.db" "$dst.watch-history.json" 2>&1); then
    printf '%-5s %-9s %-28s %s\n' FAIL "$expected" "$src" "watch history baseline: $baseline"
    failed=1
    continue
  fi
  if ! baseline=$(python3 "$HERE/table_digests.py" snapshot "$dst/data/jellyfin.db" "$dst.tables.json" 2>&1); then
    printf '%-5s %-9s %-28s %s\n' FAIL "$expected" "$src" "table baseline: ${baseline:-check failed}"
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
  why=()
  wait_ready "$port" || why+=("never became ready")
  APIKEY=$(sqlite3 -readonly "file:$dst/data/jellyfin.db?mode=ro" 'SELECT AccessToken FROM ApiKeys ORDER BY DateCreated DESC LIMIT 1')
  AUTH="Authorization: MediaBrowser Token=\"$APIKEY\", Client=\"adoption\", Device=\"adoption\", DeviceId=\"adoption\", Version=\"1\""
  settle "$port" "$AUTH"
  docker logs "$name" > "$dst.boot.log" 2>&1
  mapfile -t -O "${#why[@]}" why < <(adoption_check_boot_log "$dst.boot.log" "$expected")
  "$HERE/smoke.sh" "http://127.0.0.1:$port" "$dst" "$USER_NAME" > "$dst.smoke.txt" 2>&1
  mapfile -t -O "${#why[@]}" why < <(adoption_compare_smoke "$oracle" "$dst.smoke.txt" "$after_scan")
  mapfile -t -O "${#why[@]}" why < <(adoption_check_db "$dst/data/jellyfin.db")
  metadata_check "after adoption" check
  docker restart "$name" >/dev/null; wait_ready "$port" || why+=("second boot never ready")
  settle "$port" "$AUTH"
  # From the daemon's own start time for the restarted container, so the log holds this boot
  # only; a log with no server start must fail, never pass as "no repairs".
  docker logs --since "$(docker inspect -f '{{.State.StartedAt}}' "$name")" "$name" > "$dst.boot2.log" 2>&1
  grep -q '"ferrofin-server starting"' "$dst.boot2.log" || why+=("second boot log has no server start")
  second=$(adoption_second_boot_repairs "$dst.boot2.log")
  [ -z "$second" ] || why+=("second boot repaired again: $second")
  "$HERE/smoke.sh" "http://127.0.0.1:$port" "$dst" "$USER_NAME" > "$dst.smoke2.txt" 2>&1
  diff -q <(adoption_normalise "$dst.smoke.txt") <(adoption_normalise "$dst.smoke2.txt") >/dev/null || why+=("second boot answers differ")
  metadata_check "after restart" check
  metadata_check "after scan" scan
  if [ -n "$after_scan" ]; then
    settle "$port" "$AUTH"
    "$HERE/smoke.sh" "http://127.0.0.1:$port" "$dst" "$USER_NAME" > "$dst.smoke-scan.txt" 2>&1
    mapfile -t -O "${#why[@]}" why < <(adoption_compare_smoke "$oracle" "$dst.smoke-scan.txt" | sed 's/^/after scan: /')
  fi
  mapfile -t -O "${#why[@]}" why < <(adoption_check_db "$dst/data/jellyfin.db")
  verdict=PASS; [ "${#why[@]}" = 0 ] || verdict=FAIL
  docker logs "$name" > "$dst.server.log" 2>&1; docker rm -f "$name" >/dev/null
  printf '%-5s %-9s %-28s %s\n' "$verdict" "$expected" "$src" "$(IFS='; '; echo "${why[*]:-}")"
  if [ "$verdict" = PASS ]; then rm -rf "$dst" "$dst-cache" "$dst".*; else failed=1; fi
done
# The messy fixtures (messy.py): a clean fixture plus the rows of shapes long-lived databases
# carry and no API can produce (nine shapes: rows of a deleted user, an extra whose owner is gone,
# two rows for one path, odd id spellings, names differing in case, ...). The same image adopts a fresh copy of each, and every shape must end as Jellyfin 12.2
# left it (DIR/<fixture>/messy-oracle.json) or as a divergence written down in messy.py. They
# answer no smoke oracle: their library is the clean one plus rows no endpoint shows, so only the
# boot, the file's integrity, a quiet second boot and the shapes are checked.
# The messy rows mount media-messy (a hard-linked copy of the shared tree plus a link per file a
# seeded item needs; messy.py), else the shared tree of a fixture directory built before it existed.
MESSY_MEDIA=("${MEDIA[@]}")
[ ! -d "$FIXTURES/media-messy" ] || MESSY_MEDIA=(-v "$FIXTURES/media-messy:/media:ro")
# expected generation|fixture directory|host port
MESSY_TABLE=(
  "10.11.11|jellyfin-10.11.11-messy|18081"
  "12.2.0|jellyfin-12.2-messy|18080"
)
for spec in "${MESSY_TABLE[@]}"; do
  IFS='|' read -r expected src port <<<"$spec"
  [ -z "$ONLY" ] || [ "$ONLY" = "$src" ] || [ "$ONLY" = "$expected" ] || continue
  [ -d "$FIXTURES/$src" ] || { printf '%-5s %-9s %-28s %s\n' SKIP "$expected" "$src" "fixture missing"; continue; }
  name=adopt-$src; dst=$WORK/$src
  docker rm -f "$name" >/dev/null 2>&1; rm -rf "$dst" "$dst-cache" "$dst".tables-*.log; mkdir -p "$WORK"
  cp -a "$FIXTURES/$src" "$dst"; mkdir -p "$dst-cache"
  # The table digests (check 7) are taken before Ferrofin starts, from the clean fixture the messy
  # copy was made from: a row it loses beside the messy ones fails, while the messy rows themselves
  # (some of which an adoption rightly merges or removes) are not in the baseline.
  source=$(jq -r '.source // empty' "$FIXTURES/$src/messy.json" 2>/dev/null)
  tables_from=$dst/data/jellyfin.db
  if [ -n "$source" ] && [ -f "$FIXTURES/$source/data/jellyfin.db" ]; then
    cp "$FIXTURES/$source/data/jellyfin.db" "$dst.source.db"   # a copy: reading the fixture in place can leave -wal/-shm files in it
    tables_from=$dst.source.db
  fi
  if ! baseline=$(python3 "$HERE/table_digests.py" snapshot "$tables_from" "$dst.tables.json" 2>&1); then
    printf '%-5s %-9s %-28s %s\n' FAIL "$expected" "$src" "table baseline: ${baseline:-check failed}"
    failed=1
    continue
  fi
  docker run -d --name "$name" --user "$(id -u):$(id -g)" -p "127.0.0.1:$port:8096" \
    -e FERROFIN_DATA_DIR=/config -e FERROFIN_CACHE_DIR=/cache \
    -e HTTP_PROXY=http://127.0.0.1:9 -e HTTPS_PROXY=http://127.0.0.1:9 \
    -e ALL_PROXY=http://127.0.0.1:9 -e NO_PROXY=127.0.0.1,localhost \
    -v "$dst:/config" -v "$dst-cache:/cache" "${MESSY_MEDIA[@]}" "$IMAGE" >/dev/null
  why=()
  if wait_up "$name" "$port"; then
    settle "$port" "$(auth_for "$dst/data/jellyfin.db")"
  else
    why+=("never became ready")
  fi
  docker logs "$name" > "$dst.boot.log" 2>&1
  mapfile -t -O "${#why[@]}" why < <(adoption_check_boot_log "$dst.boot.log" "$expected")
  mapfile -t -O "${#why[@]}" why < <(adoption_check_db "$dst/data/jellyfin.db")
  mapfile -t -O "${#why[@]}" why < <(adoption_check_messy "$dst/data/jellyfin.db" "$dst")
  mapfile -t -O "${#why[@]}" why < <(adoption_check_tables "$dst/data/jellyfin.db" "$dst.tables.json" "$dst.tables-after-adoption.log" | sed 's/^/after adoption: /')
  docker restart "$name" >/dev/null
  if wait_up "$name" "$port"; then settle "$port" "$(auth_for "$dst/data/jellyfin.db")"; else why+=("second boot never ready"); fi
  docker logs --since "$(docker inspect -f '{{.State.StartedAt}}' "$name")" "$name" > "$dst.boot2.log" 2>&1
  grep -q '"ferrofin-server starting"' "$dst.boot2.log" || why+=("second boot log has no server start")
  second=$(adoption_second_boot_repairs "$dst.boot2.log")
  [ -z "$second" ] || why+=("second boot repaired again: $second")
  mapfile -t -O "${#why[@]}" why < <(adoption_check_db "$dst/data/jellyfin.db" | sed 's/^/after restart: /')
  mapfile -t -O "${#why[@]}" why < <(adoption_check_messy "$dst/data/jellyfin.db" "$dst" | sed 's/^/after restart: /')
  mapfile -t -O "${#why[@]}" why < <(adoption_check_tables "$dst/data/jellyfin.db" "$dst.tables.json" "$dst.tables-after-restart.log" | sed 's/^/after restart: /')
  verdict=PASS; [ "${#why[@]}" = 0 ] || verdict=FAIL
  docker logs "$name" > "$dst.server.log" 2>&1; docker rm -f "$name" >/dev/null
  printf '%-5s %-9s %-28s %s\n' "$verdict" "$expected" "$src" "$(IFS='; '; echo "${why[*]:-}")"
  if [ "$verdict" = PASS ]; then rm -rf "$dst" "$dst-cache" "$dst".*; else failed=1; fi
done
# The scale fixture (scale.py): the clean 10.11.11 library cloned up to 40,000 items by SQL. The same
# image adopts a fresh copy, and the run records what that cost (recorded, not gated). No scan
# runs and no HTTP probe is compared: the clones have no files, so only the boot, the file's
# integrity, a quiet second boot, the table digests and the survival of every cloned item are
# checked. The disk it needs is checked first; a run that fills the disk takes the host with it.
# expected generation|fixture directory|host port
SCALE_TABLE=(
  "10.11.11|jellyfin-10.11.11-scale|18079"
)
# Seconds each boot of the 40,000-item library may take to answer: long enough to record a slow
# migration instead of failing it, short enough to catch a hang the same hour. A first boot that never
# answers costs two of them, since the row restarts the container and waits again.
SCALE_TIMEOUT=${ADOPTION_SCALE_TIMEOUT:-1800}
sampler=
trap '[ -z "$sampler" ] || kill "$sampler" 2>/dev/null' EXIT
for spec in "${SCALE_TABLE[@]}"; do
  IFS='|' read -r expected src port <<<"$spec"
  [ -z "$ONLY" ] || [ "$ONLY" = "$src" ] || [ "$ONLY" = "$expected" ] || continue
  [ -d "$FIXTURES/$src" ] || { printf '%-5s %-9s %-28s %s\n' SKIP "$expected" "$src" "fixture missing"; continue; }
  name=adopt-$src; dst=$WORK/$src
  # The previous run's copy and numbers go first: it frees the disk this run needs, and a stale
  # samples file must never be read as this run's.
  docker rm -f "$name" >/dev/null 2>&1
  rm -rf "$dst" "$dst-cache" "$dst".tables-*.log "$dst.samples.json" "$WORK/scale-measurements-$src.json"; mkdir -p "$WORK"
  if ! reason=$(python3 "$HERE/scale.py" need-disk "$FIXTURES/$src" "$WORK" 2>&1); then
    printf '%-5s %-9s %-28s %s\n' FAIL "$expected" "$src" "${reason:-disk check failed}"
    failed=1
    continue
  fi
  cp -a "$FIXTURES/$src" "$dst"; mkdir -p "$dst-cache"
  if ! baseline=$(python3 "$HERE/table_digests.py" snapshot "$dst/data/jellyfin.db" "$dst.tables.json" 2>&1); then
    printf '%-5s %-9s %-28s %s\n' FAIL "$expected" "$src" "table baseline: ${baseline:-check failed}"
    failed=1
    continue
  fi
  if ! counts=$(python3 "$HERE/scale.py" counts "$dst/data/jellyfin.db" "$dst.scale.json" 2>&1); then
    printf '%-5s %-9s %-28s %s\n' FAIL "$expected" "$src" "scale baseline: ${counts:-check failed}"
    failed=1
    continue
  fi
  docker run -d --name "$name" --user "$(id -u):$(id -g)" -p "127.0.0.1:$port:8096" \
    -e FERROFIN_DATA_DIR=/config -e FERROFIN_CACHE_DIR=/cache \
    -e HTTP_PROXY=http://127.0.0.1:9 -e HTTPS_PROXY=http://127.0.0.1:9 \
    -e ALL_PROXY=http://127.0.0.1:9 -e NO_PROXY=127.0.0.1,localhost \
    -v "$dst:/config" -v "$dst-cache:/cache" "${MEDIA[@]}" "$IMAGE" >/dev/null
  # Disk and memory are sampled from the start of the first boot until it has settled.
  python3 "$HERE/scale.py" sample "$name" "$dst" "$dst.samples.json" & sampler=$!
  why=()
  if wait_up "$name" "$port" "$SCALE_TIMEOUT"; then
    settle "$port" "$(auth_for "$dst/data/jellyfin.db")"
  else
    why+=("never became ready")
  fi
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
  mapfile -t -O "${#why[@]}" why < <(adoption_check_boot_log "$dst.boot.log" "$expected")
  mapfile -t -O "${#why[@]}" why < <(adoption_check_db "$dst/data/jellyfin.db")
  mapfile -t -O "${#why[@]}" why < <(adoption_check_tables "$dst/data/jellyfin.db" "$dst.tables.json" "$dst.tables-after-adoption.log" | sed 's/^/after adoption: /')
  mapfile -t -O "${#why[@]}" why < <(adoption_check_scale "$dst/data/jellyfin.db" "$dst.scale.json" | sed 's/^/after adoption: /')
  docker restart "$name" >/dev/null
  if wait_up "$name" "$port" "$SCALE_TIMEOUT"; then settle "$port" "$(auth_for "$dst/data/jellyfin.db")"; else why+=("second boot never ready"); fi
  docker logs --since "$(docker inspect -f '{{.State.StartedAt}}' "$name")" "$name" > "$dst.boot2.log" 2>&1
  grep -q '"ferrofin-server starting"' "$dst.boot2.log" || why+=("second boot log has no server start")
  second=$(adoption_second_boot_repairs "$dst.boot2.log")
  [ -z "$second" ] || why+=("second boot repaired again: $second")
  mapfile -t -O "${#why[@]}" why < <(adoption_check_db "$dst/data/jellyfin.db" | sed 's/^/after restart: /')
  mapfile -t -O "${#why[@]}" why < <(adoption_check_tables "$dst/data/jellyfin.db" "$dst.tables.json" "$dst.tables-after-restart.log" | sed 's/^/after restart: /')
  mapfile -t -O "${#why[@]}" why < <(adoption_check_scale "$dst/data/jellyfin.db" "$dst.scale.json" | sed 's/^/after restart: /')
  verdict=PASS; [ "${#why[@]}" = 0 ] || verdict=FAIL
  docker logs "$name" > "$dst.server.log" 2>&1; docker rm -f "$name" >/dev/null
  detail=$(IFS='; '; echo "${why[*]:-}")
  [ -z "$summary" ] || detail="${detail:+$detail; }$summary"
  printf '%-5s %-9s %-28s %s\n' "$verdict" "$expected" "$src" "$detail"
  if [ "$verdict" = PASS ]; then rm -rf "$dst" "$dst-cache" "$dst".*; else failed=1; fi
done
exit $failed
