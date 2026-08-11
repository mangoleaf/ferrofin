#!/usr/bin/env bash
# The drop-in round-trip test — the DB-adoption requirement's definition of done
# (brain/plans/PLAN_DB_DROPIN.md Workstream E; REQ_JELLYFIN_DB_DROPIN).
#
#   1. A real jellyfin/jellyfin:10.11.8 container creates a database: startup
#      wizard, two libraries over the synthetic fixture media, a scan, and
#      user data (played flags, a favorite, a playlist, a collection).
#   2. Hermit ADOPTS the extracted config dir in place (no export, no rescan),
#      serves it over HTTP, and MUTATES it: marks a movie played, favorites
#      another, appends to the Jellyfin-created playlist.
#   3. Jellyfin 10.11.8 boots on the same dir and must see everything —
#      login, browse, Hermit's played flag/favorite, and the playlist edit.
#
# Requirements: docker, curl, jq, sqlite3, a built hermit-server binary
# (cargo build -p hermit-server). The fixture media must exist
# (suite/perf/gen-fixtures.sh) — its HOST path is mounted at the SAME path in
# the containers so the .mblink targets and DB paths resolve on both sides.
#
# Ports: 18110 (fixture jellyfin), 18111 (hermit), 18112 (verify jellyfin).
set -euo pipefail
cd "$(dirname "$0")/.."

JELLYFIN_IMAGE="${JELLYFIN_IMAGE:-jellyfin/jellyfin:10.11.8}"
MEDIA_DIR="$(pwd)/suite/perf/fixtures/media"
WORK="${ROUNDTRIP_WORK:-$(mktemp -d)}"
HERMIT_BIN="${HERMIT_BIN:-./target/debug/hermit-server}"
USER_NAME=rtadmin
USER_PW=rtpass123
AUTH='Authorization: MediaBrowser Client="rt", Device="rt", DeviceId="rt-1", Version="1.0"'

[ -d "$MEDIA_DIR/movies" ] || { echo "fixture media missing — run suite/perf/gen-fixtures.sh"; exit 1; }
[ -x "$HERMIT_BIN" ] || { echo "hermit binary missing — cargo build -p hermit-server"; exit 1; }

fail() { echo "ROUNDTRIP FAIL: $*" >&2; exit 1; }
cleanup() {
  docker rm -f rt-fixture rt-verify >/dev/null 2>&1 || true
  pkill -f "hermit-server .*--port 18111" 2>/dev/null || true
}
trap cleanup EXIT

api() { # $1=base $2=path [$3=token]
  local hdr="$AUTH"
  [ -n "${3:-}" ] && hdr="${AUTH%\"}\", Token=\"$3\""
  curl -sf "$1$2" -H "$hdr"
}
post() { # $1=base $2=path $3=token [$4=json]
  local hdr="${AUTH%\"}\", Token=\"$3\""
  if [ -n "${4:-}" ]; then
    curl -sf -X POST "$1$2" -H "$hdr" -H 'Content-Type: application/json' -d "$4"
  else
    curl -sf -X POST "$1$2" -H "$hdr" -d ''
  fi
}
login() { # $1=base -> "token userid"
  local r
  r=$(curl -sf -X POST "$1/Users/AuthenticateByName" -H 'Content-Type: application/json' \
        -H "$AUTH" -d "{\"Username\":\"$USER_NAME\",\"Pw\":\"$USER_PW\"}") || return 1
  echo "$(jq -r .AccessToken <<<"$r") $(jq -r .User.Id <<<"$r")"
}
wait_200() { # $1=url
  for _ in $(seq 1 90); do curl -sf "$1" >/dev/null 2>&1 && return 0; sleep 2; done
  return 1
}
movie_id() { # $1=base $2=token $3=uid $4=name
  api "$1" "/Items?userId=$3&recursive=true&includeItemTypes=Movie&searchTerm=$(echo "$4" | sed 's/ /%20/g')&limit=1" "$2" \
    | jq -r '.Items[0].Id'
}

# ── 1. A real Jellyfin creates the database ────────────────────────────────
echo ">> [1/3] generating a real $JELLYFIN_IMAGE database"
docker rm -f rt-fixture >/dev/null 2>&1 || true
docker run -d --name rt-fixture -p 18110:8096 \
  -v "$MEDIA_DIR":"$MEDIA_DIR":ro "$JELLYFIN_IMAGE" >/dev/null
JF=http://localhost:18110
wait_200 "$JF/System/Info/Public" || fail "fixture jellyfin never came up"
for _ in $(seq 1 60); do
  if curl -sf -X POST "$JF/Startup/Configuration" -H 'Content-Type: application/json' \
       -d '{"UICulture":"en-US","MetadataCountryCode":"US","PreferredMetadataLanguage":"en"}' >/dev/null; then
    curl -sf "$JF/Startup/User" >/dev/null || true
    curl -sf -X POST "$JF/Startup/User" -H 'Content-Type: application/json' \
      -d "{\"Name\":\"$USER_NAME\",\"Password\":\"$USER_PW\"}" >/dev/null
    curl -sf -X POST "$JF/Startup/Complete" >/dev/null && break
  fi
  sleep 2
done
read -r TOK UID_ <<<"$(login "$JF")" || fail "fixture login"
NOREMOTE='{"LibraryOptions":{"EnableRealtimeMonitor":false,"SaveLocalMetadata":false,"TypeOptions":[{"Type":"Movie","MetadataFetchers":[],"MetadataFetcherOrder":[],"ImageFetchers":[],"ImageFetcherOrder":[]},{"Type":"Series","MetadataFetchers":[],"MetadataFetcherOrder":[],"ImageFetchers":[],"ImageFetcherOrder":[]},{"Type":"Season","MetadataFetchers":[],"MetadataFetcherOrder":[],"ImageFetchers":[],"ImageFetcherOrder":[]},{"Type":"Episode","MetadataFetchers":[],"MetadataFetcherOrder":[],"ImageFetchers":[],"ImageFetcherOrder":[]}]}}'
enc_movies=$(python3 -c "import urllib.parse,sys;print(urllib.parse.quote('$MEDIA_DIR/movies',safe=''))")
enc_tv=$(python3 -c "import urllib.parse,sys;print(urllib.parse.quote('$MEDIA_DIR/tv',safe=''))")
post "$JF" "/Library/VirtualFolders?name=Movies&collectionType=movies&paths=$enc_movies&refreshLibrary=true" "$TOK" "$NOREMOTE" >/dev/null
post "$JF" "/Library/VirtualFolders?name=TV&collectionType=tvshows&paths=$enc_tv&refreshLibrary=true" "$TOK" "$NOREMOTE" >/dev/null
last=-1; stable=0
for _ in $(seq 1 480); do
  n=$(api "$JF" "/Items?userId=$UID_&recursive=true&limit=0" "$TOK" | jq -r .TotalRecordCount) || n=-1
  [ "$n" = "$last" ] && [ "$n" -gt 0 ] 2>/dev/null && stable=$((stable+1)) || stable=0
  [ "$stable" -ge 8 ] && break
  last="$n"; sleep 5
done
echo "   scanned: $last items"
M1=$(movie_id "$JF" "$TOK" "$UID_" "Movie 0001")
M2=$(movie_id "$JF" "$TOK" "$UID_" "Movie 0002")
post "$JF" "/UserPlayedItems/$M1?userId=$UID_" "$TOK" >/dev/null
post "$JF" "/UserFavoriteItems/$M1?userId=$UID_" "$TOK" >/dev/null
post "$JF" "/Playlists" "$TOK" "{\"Name\":\"RT Playlist\",\"Ids\":[\"$M1\",\"$M2\"],\"UserId\":\"$UID_\",\"MediaType\":\"Video\"}" >/dev/null
post "$JF" "/Collections?name=RT%20Collection&ids=$M1,$M2" "$TOK" >/dev/null
sleep 3
docker stop -t 30 rt-fixture >/dev/null
rm -rf "$WORK/config" && docker cp rt-fixture:/config "$WORK/config" >/dev/null
docker rm -f rt-fixture >/dev/null

# ── 2. Hermit adopts + mutates ─────────────────────────────────────────────
echo ">> [2/3] hermit adopts the database in place"
HERMIT_DATA_DIR="$WORK/config" HERMIT_ADMIN_USER=unused HERMIT_ADMIN_PASSWORD=unusedpw \
  "$HERMIT_BIN" --bind 127.0.0.1 --port 18111 > "$WORK/hermit.log" 2>&1 &
HB=http://127.0.0.1:18111
wait_200 "$HB/System/Info/Public" || fail "hermit never came up (see $WORK/hermit.log)"
grep -q 'adopted an existing Jellyfin' "$WORK/hermit.log" || fail "adoption did not trigger"
[ -f "$WORK/config/data/jellyfin.db.pre-hermit" ] || fail "no pre-hermit backup"
read -r HTOK HUID <<<"$(login "$HB")" || fail "hermit login with the JELLYFIN-created account"
hm_movies=$(api "$HB" "/Items?userId=$HUID&recursive=true&includeItemTypes=Movie&limit=0" "$HTOK" | jq -r .TotalRecordCount)
[ "$hm_movies" -ge 100 ] || fail "hermit browse: expected the adopted movies, got $hm_movies"
HM3=$(movie_id "$HB" "$HTOK" "$HUID" "Movie 0003")
HM4=$(movie_id "$HB" "$HTOK" "$HUID" "Movie 0004")
post "$HB" "/UserPlayedItems/$HM3?userId=$HUID" "$HTOK" >/dev/null
post "$HB" "/UserFavoriteItems/$HM4?userId=$HUID" "$HTOK" >/dev/null
PL=$(api "$HB" "/Items?userId=$HUID&recursive=true&includeItemTypes=Playlist&limit=1" "$HTOK" | jq -r '.Items[0].Id')
post "$HB" "/Playlists/$PL/Items?ids=$HM3&userId=$HUID" "$HTOK" >/dev/null
pl_n=$(api "$HB" "/Playlists/$PL/Items?userId=$HUID" "$HTOK" | jq '.Items | length')
[ "$pl_n" = 3 ] || fail "hermit playlist edit: expected 3 items, got $pl_n"
pkill -f "hermit-server .*--port 18111"; sleep 2

# ── 3. Jellyfin boots on the result and must see everything ────────────────
echo ">> [3/3] jellyfin boots on the hermit-mutated database"
docker run -d --name rt-verify -p 18112:8096 \
  -v "$WORK/config":/config -v "$MEDIA_DIR":"$MEDIA_DIR":ro "$JELLYFIN_IMAGE" >/dev/null
VB=http://localhost:18112
wait_200 "$VB/System/Info/Public" || fail "verify jellyfin never came up"
read -r VTOK VUID <<<"$(login "$VB")" || fail "jellyfin login after hermit's tenure"
v_movies=$(api "$VB" "/Items?userId=$VUID&recursive=true&includeItemTypes=Movie&limit=0" "$VTOK" | jq -r .TotalRecordCount)
[ "$v_movies" = "$hm_movies" ] || fail "movie count changed across the round trip ($hm_movies -> $v_movies)"
played=$(api "$VB" "/Items?userId=$VUID&recursive=true&filters=IsPlayed&includeItemTypes=Movie&limit=10&sortBy=SortName" "$VTOK" | jq -r '[.Items[].Name] | join(",")')
grep -q 'Movie 0003' <<<"$played" || fail "hermit's played flag lost (played: $played)"
favs=$(api "$VB" "/Items?userId=$VUID&recursive=true&filters=IsFavorite&includeItemTypes=Movie&limit=10" "$VTOK" | jq -r '[.Items[].Name] | join(",")')
grep -q 'Movie 0004' <<<"$favs" || fail "hermit's favorite lost (favorites: $favs)"
VPL=$(api "$VB" "/Items?userId=$VUID&recursive=true&includeItemTypes=Playlist&limit=1" "$VTOK" | jq -r '.Items[0].Id')
vpl_items=$(api "$VB" "/Playlists/$VPL/Items?userId=$VUID" "$VTOK" | jq -r '[.Items[].Name] | join(",")')
grep -q 'Movie 0003' <<<"$vpl_items" || fail "hermit's playlist edit lost (items: $vpl_items)"
docker logs rt-verify 2>&1 | grep -qiE '\[ERR\]|fatal' && fail "jellyfin logged errors on the adopted-then-mutated database"

echo "ROUNDTRIP PASS: adopt -> serve -> mutate -> swap back, nothing lost"
# The verify container writes metadata as root; a plain rm -rf will hit
# permission-denied leftovers. Clean via a helper container:
echo "   work dir: $WORK — clean with:"
echo "   docker run --rm -v $WORK:/w alpine rm -rf /w/config && rm -rf $WORK"
