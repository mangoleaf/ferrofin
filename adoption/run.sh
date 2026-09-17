#!/usr/bin/env bash
# adoption/run.sh --fixtures DIR [--image IMAGE] [--only NAME] [--user USERNAME]
#
# Adopts every Jellyfin release Ferrofin claims to support (10.11.8, 10.11.9, 10.11.10, 10.11.11,
# 12.0, 12.1 via both upgrade routes), through one image, on a FRESH
# copy of each pristine fixture under DIR, and checks each one the same way:
#   1. the boot log names the expected generation, applies every migration, logs no ERROR;
#   2. smoke.sh answers match Jellyfin 12.1's own answers on the same library
#      (DIR/oracle/smoke-jellyfin-12.1.txt), ignoring lines that legitimately differ
#      (server version, task/plugin lists, /Devices, folder order, activity-log count,
#      image byte size);
#   3. PRAGMA integrity_check / foreign_key_check are clean on the adopted file;
#   4. a second boot runs no repair and changes no answer.
# One PASS/FAIL line per fixture; exit status is non-zero if any failed. Roughly two minutes
# per fixture. The fixtures are NOT in the repository — see adoption/README.md for what DIR
# must contain and how build-fixtures.sh derives everything from one 10.11.8 snapshot.
set -uo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=adoption/lib.sh
. "$HERE/lib.sh"
FIXTURES=${FERROFIN_ADOPTION_FIXTURES:-}; IMAGE=${IMAGE:-ferrofin:bench}; ONLY=; USER_NAME=${ADOPTION_USER:-}
while [ $# -gt 0 ]; do case $1 in
  --fixtures) FIXTURES=$2; shift 2;; --image) IMAGE=$2; shift 2;; --only) ONLY=$2; shift 2;; --user) USER_NAME=$2; shift 2;;
  -h|--help) sed -n 2,16p "$0"; exit 0;; *) echo "run: unknown argument $1" >&2; exit 2;; esac; done
[ -n "$FIXTURES" ] || { echo "run: --fixtures DIR (or FERROFIN_ADOPTION_FIXTURES) is required" >&2; exit 2; }
FIXTURES=$(cd "$FIXTURES" && pwd)
for t in docker sqlite3 jq curl; do command -v $t >/dev/null || { echo "run: $t not installed" >&2; exit 2; }; done
docker image inspect "$IMAGE" >/dev/null 2>&1 || { echo "run: image $IMAGE missing (docker build -t ferrofin:bench .)" >&2; exit 2; }
ORACLE=$FIXTURES/oracle/smoke-jellyfin-12.1.txt
[ -f "$ORACLE" ] || { echo "run: $ORACLE missing — run adoption/build-fixtures.sh first" >&2; exit 2; }
# the probes must run as the account the oracle ran as; the builder records it
[ -n "$USER_NAME" ] || [ ! -f "$FIXTURES/oracle/user.txt" ] || USER_NAME=$(cat "$FIXTURES/oracle/user.txt")
MEDIA=()
# shellcheck disable=SC1091 # user-supplied, outside the repository
[ ! -f "$FIXTURES/media-mounts.sh" ] || . "$FIXTURES/media-mounts.sh"
# name|fixture directory|host port
# The generation is the id SET the gate matches, so 10.11.9 adopts as "10.11.8" (it adds no
# migration) and 10.11.10 as "10.11.11" (both add the three NormalizedUsername ids).
FIXTURE_TABLE=(
  "10.11.8|jellyfin-10.11.8|18099"
  "10.11.8|jellyfin-10.11.9|18089"
  "10.11.11|jellyfin-10.11.10|18088"
  "10.11.11|jellyfin-10.11.11|18098"
  "12.0.0|jellyfin-12.0|18097"
  "12.1.0|jellyfin-12.1-from-10|18093"
  "12.1.0|jellyfin-12.1-from-12|18092"
)
wait_ready() { local port=$1; for _ in $(seq 1 300); do curl -sf "http://127.0.0.1:$port/System/Info/Public" >/dev/null && return 0; sleep 2; done; return 1; }
settle() { local port=$1 auth=$2 busy; for _ in $(seq 1 60); do busy=$(curl -sf "http://127.0.0.1:$port/ScheduledTasks" -H "$auth" 2>/dev/null | jq -r '[.[]|select(.State!="Idle")]|length' 2>/dev/null); [ "${busy:-1}" = 0 ] && break; sleep 2; done; sleep 5; }
failed=0
for spec in "${FIXTURE_TABLE[@]}"; do
  IFS='|' read -r expected src port <<<"$spec"
  [ -z "$ONLY" ] || [ "$ONLY" = "$src" ] || [ "$ONLY" = "$expected" ] || continue
  [ -d "$FIXTURES/$src" ] || { printf '%-5s %-9s %-28s %s\n' SKIP "$expected" "$src" "fixture missing"; continue; }
  name=adopt-$src; dst=$FIXTURES/work/$src
  docker rm -f "$name" >/dev/null 2>&1; rm -rf "$dst" "$dst-cache"; mkdir -p "$FIXTURES/work"
  cp -a "$FIXTURES/$src" "$dst"; mkdir -p "$dst-cache"
  docker run -d --name "$name" --user "$(id -u):$(id -g)" -p "127.0.0.1:$port:8096" \
    -e FERROFIN_DATA_DIR=/config -e FERROFIN_CACHE_DIR=/cache \
    -v "$dst:/config" -v "$dst-cache:/cache" "${MEDIA[@]}" "$IMAGE" >/dev/null
  why=()
  wait_ready "$port" || why+=("never became ready")
  APIKEY=$(sqlite3 -readonly "file:$dst/data/jellyfin.db?mode=ro" 'SELECT AccessToken FROM ApiKeys ORDER BY DateCreated DESC LIMIT 1')
  AUTH="Authorization: MediaBrowser Token=\"$APIKEY\", Client=\"adoption\", Device=\"adoption\", DeviceId=\"adoption\", Version=\"1\""
  settle "$port" "$AUTH"
  docker logs "$name" > "$dst.boot.log" 2>&1
  mapfile -t -O "${#why[@]}" why < <(adoption_check_boot_log "$dst.boot.log" "$expected")
  "$HERE/smoke.sh" "http://127.0.0.1:$port" "$dst" "$USER_NAME" > "$dst.smoke.txt" 2>&1
  mapfile -t -O "${#why[@]}" why < <(adoption_compare_smoke "$ORACLE" "$dst.smoke.txt")
  mapfile -t -O "${#why[@]}" why < <(adoption_check_db "$dst/data/jellyfin.db")
  docker restart "$name" >/dev/null; wait_ready "$port" || why+=("second boot never ready")
  settle "$port" "$AUTH"
  docker logs --since "$(date -u -d '-90 seconds' +%Y-%m-%dT%H:%M:%S)" "$name" > "$dst.boot2.log" 2>&1
  second=$(adoption_second_boot_repairs "$dst.boot2.log")
  [ -z "$second" ] || why+=("second boot repaired again: $second")
  "$HERE/smoke.sh" "http://127.0.0.1:$port" "$dst" "$USER_NAME" > "$dst.smoke2.txt" 2>&1
  diff -q <(adoption_normalise "$dst.smoke.txt") <(adoption_normalise "$dst.smoke2.txt") >/dev/null || why+=("second boot answers differ")
  verdict=PASS; [ "${#why[@]}" = 0 ] || verdict=FAIL
  docker logs "$name" > "$dst.server.log" 2>&1; docker rm -f "$name" >/dev/null
  printf '%-5s %-9s %-28s %s\n' "$verdict" "$expected" "$src" "$(IFS='; '; echo "${why[*]:-}")"
  if [ "$verdict" = PASS ]; then rm -rf "$dst" "$dst-cache" "$dst".*; else failed=1; fi
done
exit $failed
