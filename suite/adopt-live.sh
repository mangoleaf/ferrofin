#!/usr/bin/env bash
# Real-world drop-in adoption test — boot Ferrofin on a COPY of a real, live
# Jellyfin config directory and compare what it serves against what Jellyfin
# 10.11.8 serves from the very same files.
#
# `roundtrip.sh` proves adoption against a *synthetic* library a fresh Jellyfin
# container just scanned. This one proves it against a database with years of
# history in it: provider-keyed user data, an AggregateFolder/CollectionFolder
# hierarchy, plugins, multiple users, non-ASCII people names. Those are exactly
# the shapes the synthetic fixture does not have.
#
#   1. snapshot the real config dir (read-only source, never written to)
#   2. inject a throwaway API key into the COPY (auth without knowing a password)
#   3. boot Ferrofin on it — adoption must trigger, nothing may be lost
#   4. assert what a client would see (views, browse, latest, genres, user data)
#   5. --verify-jellyfin: boot real Jellyfin 10.11.8 on the same directory and
#      diff the same answers — Jellyfin is the oracle, not a hand-written expectation
#
# Media is NOT required (and the real paths usually do not exist on a dev box):
# this exercises the database/metadata path, not playback. Images are skipped
# unless --with-metadata is given, because Jellyfin stores image paths as the
# absolute container path (/config/metadata/...), which only resolves when the
# data dir is mounted where it was.
#
# Usage:
#   suite/adopt-live.sh [--src DIR] [--work DIR] [--port N] [--verify-jellyfin] [--keep]
set -uo pipefail
cd "$(dirname "$0")/.."

SRC=${ADOPT_SRC:-/mnt/nvme0/k3s/jellyfin-config}
WORK=${ADOPT_WORK:-}
PORT=18120
JF_PORT=18121
VERIFY_JF=0
KEEP=0
WITH_METADATA=0
FERROFIN_BIN=${FERROFIN_BIN:-./target/debug/ferrofin-server}
JELLYFIN_IMAGE=${JELLYFIN_IMAGE:-jellyfin/jellyfin:10.11.8}
TOKEN=adopttesttoken0000000000000000ff

while [ $# -gt 0 ]; do
  case $1 in
    --src) SRC=$2; shift 2;;
    --work) WORK=$2; shift 2;;
    --port) PORT=$2; JF_PORT=$((PORT+1)); shift 2;;
    --verify-jellyfin) VERIFY_JF=1; shift;;
    --with-metadata) WITH_METADATA=1; shift;;
    --keep) KEEP=1; shift;;
    *) echo "unknown arg: $1"; exit 2;;
  esac
done
# Default under target/ (gitignored, on the repo's disk): the snapshot is a
# few hundred MB and /tmp is a size-capped tmpfs on most boxes.
OWN_WORK=0
[ -n "$WORK" ] || { WORK=./target/adopt-live; OWN_WORK=1; }
mkdir -p "$WORK"
[ -x "$FERROFIN_BIN" ] || { echo "ferrofin binary missing — cargo build -p ferrofin-server"; exit 1; }
[ -f "$SRC/data/jellyfin.db" ] || { echo "no jellyfin.db under $SRC"; exit 1; }

DIR=$WORK/config           # the adopted data dir (a copy — the source is never touched)
DB=$DIR/data/jellyfin.db
LOG=$WORK/ferrofin.log
FAILED=0
SRV_PID=

pass() { printf '  \033[32mok\033[0m   %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*"; FAILED=$((FAILED+1)); }
note() { printf '  --   %s\n' "$*"; }
eq()   { [ "$2" = "$3" ] && pass "$1 ($2)" || fail "$1: expected $2, got $3"; }
gt0()  { [ "${2:-0}" -gt 0 ] 2>/dev/null && pass "$1 ($2)" || fail "$1: expected > 0, got ${2:-}"; }
# The verify container writes as root, so a plain rm cannot clear a previous
# run's work dir — hand it to a throwaway container.
nuke_work() {
  [ -d "$WORK" ] || return 0
  docker run --rm -v "$(cd "$WORK" && pwd)":/w alpine sh -c 'rm -rf /w/..?* /w/.[!.]* /w/*' >/dev/null 2>&1
  rmdir "$WORK" 2>/dev/null
}
cleanup() {
  [ -n "$SRV_PID" ] && kill "$SRV_PID" 2>/dev/null
  docker rm -f ferrofin-adopt-jf >/dev/null 2>&1
  # Only ever delete a directory this script created (mktemp under /tmp); a
  # --work dir the caller named is theirs to keep. The jellyfin container writes
  # as root, so the copy is removed from inside a container.
  if [ "$KEEP" = 1 ] || [ "$OWN_WORK" = 0 ]; then
    echo "work dir kept: $WORK"
  else
    nuke_work
  fi
}
trap cleanup EXIT

api()  { curl -sf "http://127.0.0.1:$PORT$1" -H "X-Emby-Token: $TOKEN"; }
japi() { curl -sf "http://127.0.0.1:$JF_PORT$1" -H "X-Emby-Token: $TOKEN"; }
sq()   { sqlite3 "$DB" "$1"; }

echo ">> [1/5] snapshotting $SRC -> $DIR"
[ "$OWN_WORK" = 1 ] && nuke_work
mkdir -p "$DIR/data"
cp -a "$SRC/config" "$DIR/config"
cp -a "$SRC/root" "$DIR/root"
[ -d "$SRC/plugins" ] && cp -a "$SRC/plugins" "$DIR/plugins"
cp -a "$SRC/data/jellyfin.db" "$DIR/data/"
for f in jellyfin.db-wal jellyfin.db-shm device.txt collections playlists; do
  [ -e "$SRC/data/$f" ] && cp -a "$SRC/data/$f" "$DIR/data/"
done
[ "$WITH_METADATA" = 1 ] && cp -a "$SRC/metadata" "$DIR/metadata"
# A pinned snapshot source (suite/test_data) is chmod'd read-only, and `cp -a`
# preserves that — but this COPY must be writable: the checkpoint, the API-key
# inject, and Ferrofin's adoption all write to it.
chmod -R u+w "$DIR"
sq 'PRAGMA wal_checkpoint(TRUNCATE);' >/dev/null
[ "$(sq 'PRAGMA integrity_check;' | head -1)" = ok ] || { echo "the snapshot is not a consistent database (copied mid-write?)"; exit 1; }

# Baselines read straight out of the Jellyfin database, before Ferrofin touches it.
DB_MOVIES=$(sq "SELECT count(*) FROM BaseItems WHERE type LIKE '%Movies.Movie';")
DB_EPISODES=$(sq "SELECT count(*) FROM BaseItems WHERE type LIKE '%TV.Episode';")
DB_ITEMS=$(sq 'SELECT count(*) FROM BaseItems;')
DB_EF=$(sq 'SELECT count(*) FROM __EFMigrationsHistory;')
DB_USERS=$(sq 'SELECT count(*) FROM Users;')
WIZARD=$(grep -o '<IsStartupWizardCompleted>[a-z]*' "$DIR/config/system.xml" | cut -d'>' -f2)
# By-name rows, counted the way Jellyfin resolves them: a Genre/Studio item
# whose CleanName is one of the ItemValues of that kind (ItemValueId is a
# synthetic guid that matches no item, so it cannot be joined on). An exact
# expectation, because "> 0" once passed on an answer that also returned four
# episodes and a folder.
# `/Genres` excludes music items (those are the MusicGenres tab), so the
# expectation has to as well, or a genre carried only by music would make an
# exact check fail correct behaviour.
DB_GENRES=$(sq "SELECT count(*) FROM BaseItems b WHERE b.type='MediaBrowser.Controller.Entities.Genre'
                AND b.CleanName IN (
                    SELECT iv.CleanValue FROM ItemValues iv
                    JOIN ItemValuesMap m ON m.ItemValueId = iv.ItemValueId
                    JOIN BaseItems ci ON ci.Id = m.ItemId
                    WHERE iv.Type=2 AND ci.type NOT IN (
                        'MediaBrowser.Controller.Entities.Audio.Audio',
                        'MediaBrowser.Controller.Entities.MusicVideo',
                        'MediaBrowser.Controller.Entities.Audio.MusicAlbum',
                        'MediaBrowser.Controller.Entities.Audio.MusicArtist'));")
DB_STUDIOS=$(sq "SELECT count(*) FROM BaseItems b WHERE b.type='MediaBrowser.Controller.Entities.Studio'
                 AND b.CleanName IN (SELECT CleanValue FROM ItemValues WHERE Type=3);")
# A view (CollectionFolder) that actually has items under it, and its item count.
VIEW_ID=$(sq "SELECT lower(a.ParentItemId) FROM AncestorIds a JOIN BaseItems b ON b.Id=a.ParentItemId
              WHERE b.type LIKE '%CollectionFolder' GROUP BY 1 ORDER BY count(*) DESC LIMIT 1;")
VIEW_N=$(sq "SELECT count(*) FROM AncestorIds WHERE lower(ParentItemId)='$VIEW_ID';")
# An item whose watch state Jellyfin keyed by a PROVIDER id, not the item guid.
read -r UD_ITEM UD_PLAYS <<<"$(sq "SELECT lower(ItemId), PlayCount FROM UserData
     WHERE PlayCount > 0 AND CustomDataKey <> lower(ItemId) LIMIT 1;" | tr '|' ' ')"
sq "INSERT INTO ApiKeys (DateCreated,DateLastActivity,Name,AccessToken)
    VALUES (datetime('now'),datetime('now'),'adopt-live','$TOKEN');"
note "snapshot: $DB_ITEMS items ($DB_MOVIES movies, $DB_EPISODES episodes), $DB_USERS users, $DB_EF EF migrations"

echo ">> [2/5] booting ferrofin on the adopted directory"
FERROFIN_DATA_DIR=$DIR "$FERROFIN_BIN" --bind 127.0.0.1 --port "$PORT" >"$LOG" 2>&1 &
SRV_PID=$!
for _ in $(seq 1 120); do api /System/Info/Public >/dev/null 2>&1 && break; sleep 2; done
api /System/Info/Public >/dev/null || { echo "ferrofin never came up — see $LOG"; exit 1; }
# Ferrofin runs startup-triggered tasks (merge-versions among them) that mutate
# the library; probe only once they are idle, or the counts move under the test.
for _ in $(seq 1 90); do
  [ "$(api /ScheduledTasks | jq '[.[]|select(.State!="Idle")]|length')" = 0 ] && break
  sleep 2
done

echo ">> [3/5] adoption invariants"
grep -q 'adopted an existing Jellyfin' "$LOG" && pass "adoption triggered" || fail "adoption did not trigger"
[ -f "$DIR/data/jellyfin.db.pre-ferrofin" ] && pass "pre-ferrofin backup taken" || fail "no pre-ferrofin backup"
eq "EF migration history untouched" "$DB_EF" "$(sq 'SELECT count(*) FROM __EFMigrationsHistory;')"
gt0 "ferrofin migrations baselined" "$(sq 'SELECT count(*) FROM _sqlx_migrations;')"
eq "no items lost on adoption" "$DB_ITEMS" "$(sq 'SELECT count(*) FROM BaseItems;')"
ERRS=$(grep -c '"level":"ERROR"' "$LOG")
eq "no ERROR log lines" 0 "$ERRS"

echo ">> [4/5] what a client sees"
eq "startup wizard already completed" "$WIZARD" "$(api /System/Info/Public | jq -r '.StartupWizardCompleted')"
eq "the jellyfin users are there" "$DB_USERS" "$(api /Users | jq 'length')"
USER_ID=$(api /Users | jq -r '.[0].Id')
# The probe set: one line per "name<TAB>answer". Run against Ferrofin, and
# (with --verify-jellyfin) against real Jellyfin on the same files, so the
# expected values are Jellyfin's own answers rather than something hand-written.
get() { curl -sf "$1$2" -H "X-Emby-Token: $TOKEN" | jq -r "$3" 2>/dev/null; }
probes() {
  local b=$1 t
  for t in Movie Episode Series Audio Photo BoxSet Playlist; do
    printf 'items:%s\t%s\n' "$t" "$(get "$b" "/Items?userId=$USER_ID&recursive=true&includeItemTypes=$t&limit=0" .TotalRecordCount)"
  done
  printf 'items:all\t%s\n'        "$(get "$b" "/Items?userId=$USER_ID&recursive=true&limit=0" .TotalRecordCount)"
  printf 'views\t%s\n'            "$(get "$b" "/Users/$USER_ID/Views" '[.Items[].Name]|sort|join(",")')"
  printf 'browse:recursive\t%s\n' "$(get "$b" "/Items?userId=$USER_ID&parentId=$VIEW_ID&recursive=true&limit=0" .TotalRecordCount)"
  printf 'browse:children\t%s\n'  "$(get "$b" "/Items?userId=$USER_ID&parentId=$VIEW_ID&limit=1" .TotalRecordCount)"
  printf 'latest\t%s\n'           "$(get "$b" "/Users/$USER_ID/Items/Latest?limit=5" 'length')"
  printf 'genres\t%s\n'           "$(get "$b" "/Genres?userId=$USER_ID&limit=5" .TotalRecordCount)"
  printf 'genres:all\t%s\n'       "$(get "$b" "/Genres?userId=$USER_ID" '.Items|length')"
  printf 'persons\t%s\n'          "$(get "$b" "/Persons?userId=$USER_ID&limit=5" '.Items|length')"
  printf 'studios\t%s\n'          "$(get "$b" "/Studios?userId=$USER_ID&limit=5" '.Items|length')"
  printf 'studios:all\t%s\n'      "$(get "$b" "/Studios?userId=$USER_ID" '.Items|length')"
  printf 'resume\t%s\n'           "$(get "$b" "/Users/$USER_ID/Items/Resume?limit=5" '.Items|length')"
}
probes "http://127.0.0.1:$PORT" > "$WORK/ferrofin.probe"
# These cannot legitimately be empty on a real library, with or without the
# Jellyfin leg. The rest of the probe set is compare-only.
while IFS=$'\t' read -r name value; do
  case $name in
    genres:all) eq "every genre, and only genres" "$DB_GENRES" "$value";;
    studios:all) eq "every studio, and only studios" "$DB_STUDIOS" "$value";;
    items:Movie|items:Episode|items:all|browse:*|latest|genres|persons) gt0 "$name" "$value";;
    views) [ -n "$value" ] && pass "views ($value)" || fail "views: empty";;
  esac
done < "$WORK/ferrofin.probe"
if [ -n "${UD_ITEM:-}" ]; then
  eq "existing watch history readable (provider-keyed)" "$UD_PLAYS" \
     "$(api "/Items/$UD_ITEM?userId=$USER_ID" | jq -r '.UserData.PlayCount')"
fi
# Ferrofin writes user data; Jellyfin must find it under the key IT uses.
FAV_ITEM=$(api "/Items?userId=$USER_ID&recursive=true&includeItemTypes=Movie&limit=1&sortBy=SortName" | jq -r '.Items[0].Id')
curl -sf -X POST "http://127.0.0.1:$PORT/UserFavoriteItems/$FAV_ITEM?userId=$USER_ID" -H "X-Emby-Token: $TOKEN" -d '' >/dev/null
FAV_NAME=$(api "/Items/$FAV_ITEM?userId=$USER_ID" | jq -r .Name)
eq "ferrofin's favorite reads back" true "$(api "/Items/$FAV_ITEM?userId=$USER_ID" | jq -r '.UserData.IsFavorite')"
JF_KEY=$(sq "SELECT CustomDataKey FROM UserData WHERE lower(ItemId)='$FAV_ITEM' AND CustomDataKey <> '$FAV_ITEM' LIMIT 1;")
if [ -n "$JF_KEY" ]; then
  eq "favorite written under jellyfin's user-data key" 1 \
     "$(sq "SELECT IsFavorite FROM UserData WHERE lower(ItemId)='$FAV_ITEM' AND CustomDataKey='$JF_KEY';")"
fi

kill "$SRV_PID" 2>/dev/null; wait "$SRV_PID" 2>/dev/null; SRV_PID=

echo ">> [5/5] the database after ferrofin's tenure"
FK=$(sq 'PRAGMA foreign_key_check;' | head -3)
[ -z "$FK" ] && pass "foreign keys intact" || fail "foreign key violations: $FK"
# `ok` outright, with no special case. There used to be one here for
# `FerrofinIX_Peoples_LowerName_Cover`, whose `LOWER("Name")` key is ASCII-only
# in Ferrofin's bundled SQLite and Unicode-aware in an ICU-enabled sqlite3, so
# the two engines disagreed about 22 of 25,722 people. Migration 0022 replaced
# it with a `COLLATE NOCASE` key, which no build overrides — so a mismatch here
# is now a real finding again, not a known wart to be excused.
IC=$(sq 'PRAGMA integrity_check;')
if [ "$IC" = ok ]; then pass "integrity_check"
else fail "integrity_check: $(head -3 <<<"$IC")"; fi

if [ "$VERIFY_JF" = 1 ]; then
  echo ">> [+] jellyfin $JELLYFIN_IMAGE boots on the same directory"
  docker rm -f ferrofin-adopt-jf >/dev/null 2>&1
  docker run -d --name ferrofin-adopt-jf -p "$JF_PORT:8096" -v "$DIR":/config "$JELLYFIN_IMAGE" >/dev/null
  for _ in $(seq 1 120); do japi /System/Info/Public | jq -e .Version >/dev/null 2>&1 && break; sleep 2; done
  eq "jellyfin serves the adopted+mutated database" 10.11.8 "$(japi /System/Info/Public | jq -r .Version)"
  eq "jellyfin still sees ferrofin's favorite" "$FAV_NAME" \
     "$(japi "/Items?userId=$USER_ID&recursive=true&filters=IsFavorite&includeItemTypes=Movie" | jq -r '[.Items[].Name]|join(",")')"
  probes "http://127.0.0.1:$JF_PORT" > "$WORK/jellyfin.probe"
  while IFS=$'\t' read -r name value; do
    eq "vs jellyfin: $name" "$value" "$(awk -F'\t' -v n="$name" '$1==n{print $2}' "$WORK/ferrofin.probe")"
  done < "$WORK/jellyfin.probe"
  docker logs ferrofin-adopt-jf 2>&1 | grep -qiE '\[FTL\]|Unhandled exception' && fail "jellyfin logged a fatal error" || pass "jellyfin log clean"
fi

echo
if [ "$FAILED" = 0 ]; then echo "ADOPT-LIVE PASS"; else echo "ADOPT-LIVE: $FAILED check(s) failed  (log: $LOG)"; fi
exit $((FAILED > 0))
