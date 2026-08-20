#!/usr/bin/env bash
# Give a running Ferrofin a tiny library, so the probe's content-dependent
# checks (folder expansion, SyncPlay queue verbs) actually run.
#
#   FERROFIN_BASE=http://127.0.0.1:18099 FERROFIN_USER=admin FERROFIN_PASS= \
#     suite/ws/seed_library.sh
#
# One movie plus one series of two episodes — enough to prove a container id
# expands to its children. Deliberately NOT the perf fixtures: those are sized
# for the benchmark and regenerating them would invalidate the baseline.
set -euo pipefail

BASE=${FERROFIN_BASE:-http://127.0.0.1:8096}
USER=${FERROFIN_USER:-admin}
PASS=${FERROFIN_PASS:-}
ROOT=${FERROFIN_PROBE_MEDIA:-/tmp/ferrofin-probe-media}

command -v ffmpeg >/dev/null || { echo "ffmpeg required to generate the fixture" >&2; exit 1; }

if [ ! -f "$ROOT/movies/Probe Movie (2020)/Probe Movie (2020).mkv" ]; then
  echo "generating fixture in $ROOT"
  rm -rf "$ROOT"
  mkdir -p "$ROOT/movies/Probe Movie (2020)" "$ROOT/tv/Probe Show/Season 01"
  master="$ROOT/.master.mkv"
  ffmpeg -y -loglevel error \
    -f lavfi -i testsrc=duration=1:size=320x240:rate=5 \
    -f lavfi -i sine=duration=1:frequency=440 \
    -c:v libx264 -pix_fmt yuv420p -c:a aac -shortest "$master"
  cp "$master" "$ROOT/movies/Probe Movie (2020)/Probe Movie (2020).mkv"
  cp "$master" "$ROOT/tv/Probe Show/Season 01/Probe Show S01E01.mkv"
  cp "$master" "$ROOT/tv/Probe Show/Season 01/Probe Show S01E02.mkv"
  rm -f "$master"
fi

auth='MediaBrowser Client="ProbeSeed", Device="Seed", DeviceId="probe-seed", Version="1"'
token=$(curl -sf -X POST "$BASE/Users/AuthenticateByName" \
  -H "Content-Type: application/json" -H "Authorization: $auth" \
  -d "{\"Username\":\"$USER\",\"Pw\":\"$PASS\"}" | python3 -c 'import json,sys;print(json.load(sys.stdin)["AccessToken"])')
auth="$auth, Token=\"$token\""

enc() { python3 -c 'import sys,urllib.parse;print(urllib.parse.quote(sys.argv[1],safe=""))' "$1"; }

# Adding a library that already exists is a 4xx — harmless on a re-run.
for spec in "Probe Movies:movies:$ROOT/movies" "Probe TV:tvshows:$ROOT/tv"; do
  name=${spec%%:*}; rest=${spec#*:}; type=${rest%%:*}; path=${rest#*:}
  curl -s -o /dev/null -X POST -H "Authorization: $auth" \
    "$BASE/Library/VirtualFolders?name=$(enc "$name")&collectionType=$type&paths=$(enc "$path")&refreshLibrary=true" || true
done

curl -s -o /dev/null -X POST -H "Authorization: $auth" "$BASE/Library/Refresh"

echo -n "waiting for the scan"
for _ in $(seq 1 60); do
  count=$(curl -sf -H "Authorization: $auth" \
    "$BASE/Items?Recursive=true&IncludeItemTypes=Movie,Episode&Limit=1&EnableTotalRecordCount=true" \
    | python3 -c 'import json,sys;print(json.load(sys.stdin).get("TotalRecordCount",0))' 2>/dev/null || echo 0)
  [ "$count" -ge 3 ] && { echo " done ($count items)"; exit 0; }
  echo -n "."
  sleep 2
done
echo " timed out" >&2
exit 1
