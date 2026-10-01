#!/usr/bin/env bash
# verify/scan-behaviour.sh [options] BASE_URL LIBRARY_ID SCRATCH_DIR [SERVER_SCRATCH_DIR]
#
# Checks, against any running Ferrofin, that library scans reprocess only what changed: a
# first scan creates the new items, an unchanged rescan probes, asks and writes nothing, a
# touched file or a newer NFO refreshes that item alone, webhook reports add and remove exactly
# one item, editor changes and locks survive rescans and the dashboard's "Search for missing
# metadata" / "Replace all metadata", and a save of an episode or a locked item keeps its
# stored fields. It reads /metrics deltas (the library-scan metrics) and /Items answers, prints
# one PASS/FAIL/SKIP line per row, and exits 0 when nothing failed, 1 if a row failed, 2 when it
# refuses to run, 3 when nothing could be verified, 130 on SIGINT and 143 on SIGTERM.
#
#   FERROFIN_TOKEN      (environment) an administrator's access token. It is read from the
#                       environment, which only this user can see (/proc/PID/environ), never
#                       from the command line, which every user can (ps, /proc/PID/cmdline);
#                       curl gets it from a private header file.
#   BASE_URL            the server, e.g. http://127.0.0.1:8096
#   LIBRARY_ID          the library's id (GET /Library/VirtualFolders, ItemId): movies or tvshows
#   SCRATCH_DIR         an EMPTY folder inside that library, as this machine sees it, best
#                       directly under the library root (never inside a real item's folder).
#                       In a tvshows library it becomes the scratch series: it must be
#                       directly under the root and named "Ferrofin Verify …", and an empty
#                       series with its name stays in the library after the run.
#   SERVER_SCRATCH_DIR  the same folder as the server sees it, when that differs (a container
#                       or sandbox mounting the library elsewhere); default SCRATCH_DIR
#   --clip FILE         a small real video to copy as the scratch media; default: generated
#                       with ffmpeg. Without either, the rows are skipped (they need media the
#                       server can probe).
#   --timeout SECONDS   how long one scan may take (default 300)
#   --nfo-wait SECONDS  how long row 4 waits after the scratch items were saved before it
#                       rewrites an NFO (default 62: upstream re-reads an NFO only once it is
#                       more than a minute newer than the item's last save). Raise it when the
#                       server's clock and the file server's differ (NFS, a remote pod).
#
# Needs curl and jq. The server must run with /metrics on (FERROFIN_ENABLE_METRICS=true), and
# the library with real-time monitoring off (the watcher would process the scratch changes
# before the scans the rows time). The script writes nothing but its own scratch files under
# SCRATCH_DIR, edits the metadata of its own scratch items only, and deletes every file it
# created on exit, failure or interrupt included (anything else left there is listed, not
# removed), then asks the server to rescan the library so the scratch items are pruned. The
# /metrics counts it asserts are server-wide: run it while no other scan runs on the server,
# or a row fails that did nothing wrong.
set -uo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=verify/lib.sh
. "$HERE/lib.sh"

# usage: the header above, without its comment markers.
usage() { awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"; }
CLIP=; SCAN_TIMEOUT=300; NFO_WAIT=62; ARGS=()
while [ $# -gt 0 ]; do
  case $1 in
    --clip|--timeout|--nfo-wait)
      if [ $# -lt 2 ]; then echo "scan-behaviour: $1 needs a value" >&2; exit 2; fi
      case $1 in --clip) CLIP=$2 ;; --timeout) SCAN_TIMEOUT=$2 ;; *) NFO_WAIT=$2 ;; esac
      shift 2 ;;
    -h|--help) usage; exit 0 ;;
    --*) echo "scan-behaviour: unknown option $1" >&2; exit 2 ;;
    *) ARGS+=("$1"); shift ;;
  esac
done
if [ ${#ARGS[@]} -lt 3 ] || [ ${#ARGS[@]} -gt 4 ]; then usage >&2; exit 2; fi
BASE=${ARGS[0]%/}; LIB=${ARGS[1]}; LOCAL=${ARGS[2]%/}; SERVER=${ARGS[3]:-${ARGS[2]}}
SERVER=${SERVER%/}
for value in "$SCAN_TIMEOUT" "$NFO_WAIT"; do
  case $value in ''|*[!0-9]*) echo "scan-behaviour: --timeout and --nfo-wait take whole seconds" >&2; exit 2 ;; esac
done
# Decimal, whatever the leading zeros (bash arithmetic would read 08 as a bad octal).
SCAN_TIMEOUT=$((10#$SCAN_TIMEOUT)); NFO_WAIT=$((10#$NFO_WAIT))
if [ -z "${FERROFIN_TOKEN:-}" ]; then
  echo "scan-behaviour: set FERROFIN_TOKEN to an administrator's access token (it is never taken from the command line)" >&2
  exit 2
fi
for tool in curl jq; do
  command -v "$tool" >/dev/null || { echo "scan-behaviour: $tool is not installed" >&2; exit 2; }
done

# How often the script polls /metrics while a scan runs, and how long one request may take.
POLL=0.5
REQUEST_TIMEOUT=60

WORK=$(mktemp -d) || exit 2
# CREATED_*: what the cleanup deletes. SCRATCH_CHECKED: the scratch folder was found empty, so
# anything in it at exit came from this run (worth listing); before that, it may be any folder
# at all. PRUNE: scratch items exist on the server, so the cleanup rescans the library.
CREATED_FILES=(); CREATED_DIRS=(); SCRATCH_CHECKED=0; PRUNE=0
# The token travels in a header file (curl -H @file), never on a command line other users can
# read; the directory is mktemp's, private to this user.
printf 'Authorization: MediaBrowser Client="ferrofin-verify", Device="verify", DeviceId="ferrofin-verify", Version="1", Token="%s"\n' \
  "$FERROFIN_TOKEN" >"$WORK/auth"

refuse() { echo "scan-behaviour: refusing to run: $*" >&2; exit 2; }

# api <method> <path> [json-body]: the answer's body on stdout; non-zero on a non-2xx answer.
api() {
  local method=$1 path=$2 body=${3-} status
  local args=(-sS -o "$WORK/response" -w '%{http_code}' -X "$method" -H "@$WORK/auth"
    --max-time "$REQUEST_TIMEOUT")
  [ -z "$body" ] || args+=(-H 'Content-Type: application/json' --data-binary "$body")
  status=$(curl "${args[@]}" "$BASE$path") || { echo "request failed: $method $path" >&2; return 1; }
  case $status in
    2??) cat "$WORK/response" ;;
    *) echo "HTTP $status from $method $path: $(head -c 300 "$WORK/response")" >&2; return 1 ;;
  esac
}

# scrape <file>: /metrics into <file>; prints the HTTP status (000 when there was no answer).
scrape() {
  local status
  status=$(curl -sS -o "$1" -w '%{http_code}' --max-time "$REQUEST_TIMEOUT" "$BASE/metrics") || true
  echo "${status:-000}"
}

# pause <seconds>: a sleep a signal interrupts at once (a trap runs only after a foreground
# command ends; `wait` ends when the trapped signal arrives).
pause() {
  sleep "$1" &
  wait $!
}

# remove_work: deletes the private temporary directory — the token's header file first. Only
# the files the script itself put there (no recursion), and only in the directory mktemp made.
# shellcheck disable=SC2317,SC2329 # invoked by the cleanup and its traps
remove_work() {
  [ -n "$WORK" ] && [ -d "$WORK" ] || return 0
  rm -f -- "$WORK/auth"
  rm -f -- "$WORK"/*
  rmdir -- "$WORK" 2>/dev/null
}

# shellcheck disable=SC2317,SC2329 # invoked by the EXIT trap
cleanup() {
  local status=$? i before
  trap - EXIT
  # A second Ctrl-C (or a TERM) during the cleanup's own waits still removes the token.
  trap 'remove_work; exit 130' INT
  trap 'remove_work; exit 143' TERM
  for (( i = ${#CREATED_FILES[@]} - 1; i >= 0; i-- )); do rm -f -- "${CREATED_FILES[i]}"; done
  for (( i = ${#CREATED_DIRS[@]} - 1; i >= 0; i-- )); do rmdir -- "${CREATED_DIRS[i]}" 2>/dev/null; done
  if [ ${#CREATED_FILES[@]} -gt 0 ]; then echo "scratch files removed from $LOCAL"; fi
  if [ "$SCRATCH_CHECKED" = 1 ] && [ -d "$LOCAL" ] && [ -n "$(ls -A -- "$LOCAL")" ]; then
    echo "left in $LOCAL (the script did not create these, so it did not delete them):"
    find "$LOCAL" -mindepth 1 | sed 's/^/  /'
  fi
  if [ "$PRUNE" = 1 ]; then
    before=$WORK/prune-before.txt
    if [ "$(scrape "$before")" = 200 ] && api POST "/Items/$LIB/Refresh?MetadataRefreshMode=Default&ImageRefreshMode=Default" >/dev/null \
      && wait_scans "$before" "$WORK/prune-after.txt" api 1 "$SCAN_TIMEOUT"; then
      echo "the library rescan pruned $(verify_delta "$before" "$WORK/prune-after.txt" ferrofin_library_scan_items_total trigger=api outcome=removed) scratch item(s)"
    else
      echo "could not rescan the library: its scratch items go at the next scan" >&2
    fi
    if [ "$KIND" = tvshows ]; then
      echo "the scratch folder is the scratch series, so an empty series '$(basename "$SERVER")' stays in"
      echo "the library: to remove it, delete the empty folder $LOCAL, then rescan the library"
    fi
  fi
  remove_work
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# wait_scans <before> <after> <trigger> <count> <seconds>: waits until <count> more <trigger>
# scans have ended and none runs, leaving the last scrape in <after>.
wait_scans() {
  local before=$1 after=$2 trigger=$3 count=$4 limit=$5 start=$SECONDS
  while :; do
    [ "$(scrape "$after")" = 200 ] || { echo "/metrics stopped answering" >&2; return 1; }
    if [ "$(verify_delta "$before" "$after" ferrofin_library_scans_total "trigger=$trigger")" -ge "$count" ] &&
      [ "$(verify_metric_sum "$after" ferrofin_library_scan_in_progress)" -eq 0 ]; then
      return 0
    fi
    if [ $((SECONDS - start)) -ge "$limit" ]; then
      echo "no $trigger scan finished within ${limit}s" >&2
      return 1
    fi
    pause "$POLL"
  done
}

# ---- preflight ------------------------------------------------------------------------------
status=$(scrape "$WORK/metrics.txt")
reason=$(verify_metrics_enabled "$WORK/metrics.txt" "$status")
[ -z "$reason" ] || refuse "$reason"
me=$(api GET /Users/Me) || refuse "the token was not accepted by $BASE"
[ "$(jq -r '.Policy.IsAdministrator' <<<"$me")" = true ] || refuse "the token is not an administrator's"
folders=$(api GET /Library/VirtualFolders) || refuse "cannot list the libraries"
library=$(jq -c --arg id "$LIB" '[.[] | select((.ItemId // "" | ascii_downcase | gsub("-"; "")) == ($id | ascii_downcase | gsub("-"; "")))][0] // empty' <<<"$folders")
[ -n "$library" ] || refuse "no library with id $LIB (GET /Library/VirtualFolders lists ItemId)"
LIB_NAME=$(jq -r '.Name' <<<"$library")
KIND=$(jq -r '.CollectionType // ""' <<<"$library")
case $KIND in
  movies|tvshows) ;;
  *) refuse "library '$LIB_NAME' is a '$KIND' library; movies and tvshows are supported" ;;
esac
[ "$(jq -r '.LibraryOptions.EnableRealtimeMonitor // false' <<<"$library")" != true ] ||
  refuse "library '$LIB_NAME' has real-time monitoring on: the watcher would process the scratch changes itself. Turn it off for the run (Dashboard > Libraries > $LIB_NAME)"
mapfile -t LOCATIONS < <(jq -r '.Locations[]' <<<"$library")
verify_inside "$SERVER" "${LOCATIONS[@]}" ||
  refuse "$SERVER is not inside library '$LIB_NAME' (${LOCATIONS[*]}), or it has a '.' or '..' segment"
if [ ! -d "$LOCAL" ] || [ ! -w "$LOCAL" ]; then refuse "$LOCAL is not a writable directory"; fi
# The folder the script deletes from is exactly the one it was given: no symlink or '..' on
# the way to it that could point the cleanup somewhere else.
resolved=$(realpath -e -- "$LOCAL") || refuse "$LOCAL cannot be resolved"
[ "$resolved" = "$LOCAL" ] || [ "$resolved" = "$(pwd -P)/$LOCAL" ] ||
  refuse "$LOCAL resolves to $resolved: give the scratch folder without symlinks or '..'"
[ -z "$(ls -A "$LOCAL")" ] || refuse "$LOCAL is not empty: the script deletes only what it creates, so it starts from an empty folder"
SCRATCH_CHECKED=1
if [ "$KIND" = tvshows ]; then
  for location in "${LOCATIONS[@]}"; do
    [ "$(dirname "$SERVER")" != "${location%/}" ] || TV_OK=1
  done
  [ "${TV_OK:-0}" = 1 ] || refuse "in a tvshows library the scratch folder must sit directly under the library root (it is the scratch series)"
  case $(basename "$SERVER") in "$VERIFY_PREFIX"*) ;; *)
    refuse "in a tvshows library the scratch folder is the scratch series: name it '$VERIFY_PREFIX …' (e.g. '$VERIFY_PREFIX Show'), so the script can tell it from a real series"
  esac
fi
# Whether the library's remote metadata fetchers are on for the scratch items' kinds (a movie;
# a series, its season and its episodes — the gate is per kind): "on" when any of them has a
# fetcher ticked, and a row that runs the providers must see requests; "off" when every one of
# them saved an empty choice, and every row must see none; "unknown" otherwise, and neither is
# checked — a kind with no saved choice follows the server-wide one. OMDb uses
# a shared key by default, so its checkbox is sufficient too.
case $KIND in movies) KINDS='["Movie"]' ;; *) KINDS='["Series","Season","Episode"]' ;; esac
REMOTE=$(jq -r --argjson kinds "$KINDS" '
  [ $kinds[] as $k | [.LibraryOptions.TypeOptions[]? | select(.Type == $k)][0]
    | if . == null then "unsaved"
      elif (.MetadataFetchers | length) > 0 then "on"
      else "off" end ]
  | if any(. == "on") then "on" elif all(. == "off") then "off" else "unknown" end' <<<"$library")
MONITOR_DELAY=$(api GET /System/Configuration | jq -r '.LibraryMonitorDelay // 0') || MONITOR_DELAY=0

if [ -z "$CLIP" ] && command -v ffmpeg >/dev/null; then
  CLIP=$WORK/clip.mkv
  ffmpeg -nostdin -v error -f lavfi -i testsrc=duration=1:size=64x48:rate=5 \
    -f lavfi -i sine=frequency=440:duration=1 -c:v mpeg4 -c:a aac -shortest -y "$CLIP" || CLIP=
fi

echo "Ferrofin scan-behaviour check: $BASE, library '$LIB_NAME' ($KIND), scratch $SERVER"
echo "remote metadata fetchers for $(jq -r 'join("/")' <<<"$KINDS"): $REMOTE; LibraryMonitorDelay ${MONITOR_DELAY}s"
if [ "$KIND" = tvshows ] && [ "$REMOTE" != off ]; then
  echo "(TV: row 1's provider requests are not checked; the scratch series has no provider ids,"
  echo " so every other row expects 0)"
fi
if [ -z "$CLIP" ] || [ ! -s "$CLIP" ]; then
  echo "SKIP: no scratch media — no --clip, and ffmpeg is missing or could not make a clip. Every"
  echo "      row needs a file the server can probe (a file it cannot probe is re-probed on every"
  echo "      scan)."
  exit 3
fi

# ---- scratch layout -------------------------------------------------------------------------
# A, B, C and D are the scratch items: A for the metadata rows, B for the lock rows, C for the
# NFO row (and row 13 in a TV library), D for the webhook rows.
if [ "$KIND" = movies ]; then
  NFO_KIND=movie; NEW_ITEMS=3; PARENT_UPDATES=0
  rel() { printf 'Ferrofin Verify %s (%s)/Ferrofin Verify %s (%s).%s' "$1" "$2" "$1" "$2" "$3"; }
  A=$(rel Alpha 2001 mkv); B=$(rel Beta 2002 mkv); C=$(rel Gamma 2003 mkv); D=$(rel Delta 2004 mkv)
  POSTER="$(dirname "$B")/poster.png"
else
  # The series, its season and three episodes; a new or removed episode moves the season
  # folder's mtime, so the season goes through the refresh decision with it.
  NFO_KIND=episodedetails; NEW_ITEMS=5; PARENT_UPDATES=1
  rel() { printf 'Season 01/Ferrofin Verify Show - S01E0%s.%s' "$1" "$2"; }
  A=$(rel 1 mkv); B=$(rel 2 mkv); C=$(rel 3 mkv); D=$(rel 4 mkv)
  POSTER="${B%.mkv}-thumb.png"
fi
title() { case $1 in "$A") echo "$VERIFY_PREFIX A" ;; "$B") echo "$VERIFY_PREFIX B" ;; "$C") echo "$VERIFY_PREFIX C" ;; *) echo "$VERIFY_PREFIX D" ;; esac; }

# new_dir <relative-dir>: creates it (and its parents) under the scratch folder, remembering
# every directory it made for the cleanup.
new_dir() {
  local rel=$1 parts path=$LOCAL
  IFS=/ read -ra parts <<<"$rel"
  for part in "${parts[@]}"; do
    path=$path/$part
    if [ ! -d "$path" ]; then mkdir -- "$path" && CREATED_DIRS+=("$path"); fi
  done
}
# new_file <relative-path> <source-file>: copies the source there, remembered for the cleanup,
# with an mtime an hour back (so a later touch is a change).
new_file() {
  local rel=$1 src=$2
  [ "$(dirname "$rel")" = . ] || new_dir "$(dirname "$rel")"
  CREATED_FILES+=("$LOCAL/$rel")
  cp -- "$src" "$LOCAL/$rel" && touch -m -d '1 hour ago' -- "$LOCAL/$rel"
}
# new_video <relative-path> [trailer]: the clip and its NFO.
new_video() {
  new_file "$1" "$CLIP"
  verify_nfo "$NFO_KIND" "$(title "$1")" "Written by the verify script." "${2:-}" >"$WORK/nfo"
  new_file "${1%.mkv}.nfo" "$WORK/nfo"
}

# series_id: the scratch series (a TV library's scratch folder), empty when it has no item.
series_id() {
  api GET "/Items?ParentId=$LIB&Recursive=true&IncludeItemTypes=Series&Fields=Path" |
    jq -r --arg p "$SERVER" '[.Items[] | select(.Path == $p)][0].Id // empty'
}
# item_json <relative-path>: the scratch item at that path, from /Items (empty when absent):
# a movie by its NFO title's prefix, an episode among the scratch series' items (an episode's
# sort name, which NameStartsWith matches, starts with its numbers).
item_json() {
  local query="ParentId=$LIB&Recursive=true&NameStartsWith=Ferrofin%20Verify&Fields=Path"
  [ "$KIND" != tvshows ] || query="ParentId=${SERIES_ID:-$LIB}&Recursive=true&Fields=Path"
  api GET "/Items?$query" | jq -c --arg p "$SERVER/$1" '[.Items[] | select(.Path == $p)][0] // empty'
}
item_id() { item_json "$1" | jq -r '.Id // empty'; }
# item <id>: GET /Items/{id}, the full DTO the metadata editor edits.
item() { api GET "/Items/$1"; }
# edit <id> <jq-filter>: the metadata editor's save — the DTO as GET returned it, filtered.
edit() {
  local dto
  dto=$(item "$1") && dto=$(jq -c "$2" <<<"$dto") && api POST "/Items/$1" "$dto" >/dev/null
}
refresh_library() { api POST "/Items/$LIB/Refresh?MetadataRefreshMode=Default&ImageRefreshMode=Default&ReplaceAllMetadata=false&ReplaceAllImages=false" >/dev/null; }
# refresh_item <id> <replace-all>: the dashboard's refresh dialog on one item, "Search for
# missing metadata" (false) or "Replace all metadata" (true).
refresh_item() {
  api POST "/Items/$1/Refresh?MetadataRefreshMode=FullRefresh&ImageRefreshMode=FullRefresh&ReplaceAllMetadata=$2&ReplaceAllImages=false&RegenerateTrickplay=false" >/dev/null
}
webhook() {
  api POST /Library/Media/Updated "$(jq -cn --arg p "$SERVER/$1" --arg t "$2" '{Updates: [{Path: $p, UpdateType: $t}]}')" >/dev/null
}

# A TV library's scratch folder becomes the scratch series, whose name and overview the
# script's tvshow.nfo sets: an existing series there is taken over only when it is a scratch
# series itself (the prefix, no provider ids), never a real one.
if [ "$KIND" = tvshows ]; then
  existing=$(series_id) || refuse "cannot look up the series at $SERVER"
  if [ -n "$existing" ]; then
    series=$(item "$existing") || refuse "cannot read the series at $SERVER"
    case $(jq -r '.Name // ""' <<<"$series") in "$VERIFY_PREFIX"*) ;; *)
      refuse "$SERVER is the series '$(jq -r '.Name' <<<"$series")': the scratch folder must not be a real series" ;;
    esac
    [ "$(jq -r '.ProviderIds // {} | length' <<<"$series")" = 0 ] ||
      refuse "$SERVER is a series with provider ids: the scratch folder must not be a real series"
  fi
fi

# ---- rows -----------------------------------------------------------------------------------
declare -A RESULT DETAIL SCENARIO
FAILS=(); ROW=; BEFORE=$WORK/before.txt; AFTER=$WORK/after.txt
row_start() { ROW=$1; SCENARIO[$1]=$2; FAILS=(); scrape "$BEFORE" >/dev/null; }
# observe <trigger>: the row's counts, from the scrapes around it.
observe() {
  local t=$1
  CREATED=$(verify_delta "$BEFORE" "$AFTER" ferrofin_library_scan_items_total "trigger=$t" outcome=created)
  UPDATED=$(verify_delta "$BEFORE" "$AFTER" ferrofin_library_scan_items_total "trigger=$t" outcome=updated)
  UNCHANGED=$(verify_delta "$BEFORE" "$AFTER" ferrofin_library_scan_items_total "trigger=$t" outcome=unchanged)
  REMOVED=$(verify_delta "$BEFORE" "$AFTER" ferrofin_library_scan_items_total "trigger=$t" outcome=removed)
  PROBES=$(verify_delta "$BEFORE" "$AFTER" ferrofin_media_probe_total result=ok)
  PROBE_FAILED=$(verify_delta "$BEFORE" "$AFTER" ferrofin_media_probe_total result=failed)
  PROVIDERS=$(verify_delta "$BEFORE" "$AFTER" ferrofin_metadata_provider_requests_total)
  COMPLETED=$(verify_delta "$BEFORE" "$AFTER" ferrofin_library_scans_total "trigger=$t" result=completed)
  verify_expect FAILS "completed $t scans" "$COMPLETED" ge 1
  verify_expect FAILS "failed probes" "$PROBE_FAILED" eq 0
}
counts() { echo "created=$CREATED updated=$UPDATED unchanged=$UNCHANGED removed=$REMOVED probes=$PROBES provider_requests=$PROVIDERS"; }
# expect_providers: a row that runs the remote providers asks them when they are on, and
# never when they are off. In a TV library it asks nothing either way: an episode's (and a
# season's) remote lookup starts from its series' provider ids, and row 1 stops the run unless
# the scratch series has none.
expect_providers() {
  if [ "$KIND" = tvshows ]; then
    verify_expect FAILS "provider requests" "$PROVIDERS" eq 0
    return
  fi
  case $REMOTE in
    on) verify_expect FAILS "provider requests" "$PROVIDERS" ge 1 ;;
    off) verify_expect FAILS "provider requests" "$PROVIDERS" eq 0 ;;
  esac
}
# expect_first_providers: row 1. In a TV library with the fetchers on, the new (or changed)
# scratch series searches for its match: how many requests that takes is the provider's
# business, so they are not counted.
expect_first_providers() {
  if [ "$KIND" = tvshows ]; then
    [ "$REMOTE" != off ] || verify_expect FAILS "provider requests" "$PROVIDERS" eq 0
    return
  fi
  expect_providers
}
# scan <trigger>: waits for the row's scan and observes it; a scan that never ends stops the
# run (every later row builds on this one's state).
scan() {
  local limit=$SCAN_TIMEOUT
  [ "$1" != webhook ] || limit=$((SCAN_TIMEOUT + MONITOR_DELAY))
  if ! wait_scans "$BEFORE" "$AFTER" "$1" 1 "$limit"; then
    FAILS+=("the $1 scan did not finish")
    row_end "stopping: the rows after this one build on its state"
    finish
  fi
  observe "$1"
}
row_end() {
  local result=PASS detail=$1
  if [ ${#FAILS[@]} -gt 0 ]; then
    result=FAIL
    detail="$(IFS=';'; echo "${FAILS[*]}") | $detail"
  fi
  RESULT[$ROW]=$result; DETAIL[$ROW]=$detail
  verify_row_line "$ROW" "$result" "${SCENARIO[$ROW]}" "$detail"
}
row_skip() { SCENARIO[$1]=$2; RESULT[$1]=SKIP; DETAIL[$1]=$3; verify_row_line "$1" SKIP "$2" "$3"; }
finish() {
  local row pass=0 fail=0 skip=0
  echo
  verify_row_line row result scenario observed
  for row in $(printf '%s\n' "${!RESULT[@]}" | sort -n); do
    verify_row_line "$row" "${RESULT[$row]}" "${SCENARIO[$row]}" "${DETAIL[$row]}"
    case ${RESULT[$row]} in PASS) pass=$((pass + 1)) ;; FAIL) fail=$((fail + 1)) ;; *) skip=$((skip + 1)) ;; esac
  done
  echo "summary: $pass PASS, $fail FAIL, $skip SKIP"
  [ "$fail" -eq 0 ] && exit 0
  exit 1
}

verify_row_line row result scenario observed

# Row 0 (not a check): absorb whatever changed in the library before the run.
scrape "$BEFORE" >/dev/null
if ! refresh_library || ! wait_scans "$BEFORE" "$AFTER" api 1 "$SCAN_TIMEOUT"; then
  refuse "the library refresh before the rows did not finish"
fi
observe api
verify_row_line 0 - "library refresh before the rows (not checked)" "$(counts)"

# 1: the first scan of new items creates them and asks the providers.
row_start 1 "first scan of new items"
# A TV library's scratch folder is a series: one a scan has already seen (as an empty series)
# is not created again, and its changed folder puts it through the refresh decision.
PARENT_BEFORE=0
if [ "$KIND" = tvshows ] && [ -n "$(series_id)" ]; then NEW_ITEMS=4; PARENT_BEFORE=1; fi
if [ "$KIND" = tvshows ]; then
  verify_nfo tvshow "$VERIFY_PREFIX Show" "Written by the verify script." ferrofinverify >"$WORK/nfo"
  new_file tvshow.nfo "$WORK/nfo"
  new_video "$A"; new_video "$B" ferrofinverifyb; new_video "$C"
else
  new_video "$A" ferrofinverifya; new_video "$B" ferrofinverifyb; new_video "$C" ferrofinverifyc
fi
PRUNE=1
refresh_library || FAILS+=("the refresh was refused")
scan api
SAVED_AT=$SECONDS
verify_expect FAILS created "$CREATED" eq "$NEW_ITEMS"
verify_expect FAILS updated "$UPDATED" eq "$PARENT_BEFORE"
[ "$KIND" != tvshows ] || SERIES_ID=$(series_id)
verify_expect FAILS removed "$REMOVED" eq 0
verify_expect FAILS probes "$PROBES" eq 3
expect_first_providers
ID_A=$(item_id "$A"); ID_B=$(item_id "$B"); ID_C=$(item_id "$C")
if [ -z "$ID_A" ] || [ -z "$ID_B" ] || [ -z "$ID_C" ]; then FAILS+=("the scratch items are not all in /Items"); fi
if [ "$KIND" = tvshows ]; then
  # Row 1's fuzzy name search may have pinned a real show on the scratch series: then its
  # episodes would ask that show's providers, and no later row could expect 0.
  if [ -z "$SERIES_ID" ]; then
    FAILS+=("the scratch series is not in /Items")
  else
    series_ids=$(item "$SERIES_ID" | jq -c '.ProviderIds // {}')
    [ "$series_ids" = '{}' ] ||
      FAILS+=("the scratch series matched a real show $series_ids: rename the scratch folder and delete that series")
  fi
fi
row_end "$(counts)"
[ "${RESULT[1]}" = PASS ] || finish

# 2: an unchanged rescan does nothing.
row_start 2 "rescan, nothing changed"
refresh_library; scan api
verify_expect FAILS created "$CREATED" eq 0
verify_expect FAILS updated "$UPDATED" eq 0
verify_expect FAILS removed "$REMOVED" eq 0
verify_expect FAILS probes "$PROBES" eq 0
verify_expect FAILS "provider requests" "$PROVIDERS" eq 0
row_end "$(counts)"

# 3: a touched file is probed, fetched and saved, alone.
row_start 3 "touch one file's mtime, rescan"
touch -m -- "$LOCAL/$A"
refresh_library; scan api
verify_expect FAILS updated "$UPDATED" eq 1
verify_expect FAILS created "$CREATED" eq 0
verify_expect FAILS removed "$REMOVED" eq 0
verify_expect FAILS probes "$PROBES" eq 1
expect_providers
row_end "$(counts)"

# 5: a webhook reports one new file (rows 5 and 6 run before 4, whose NFO rule needs a minute).
row_start 5 "webhook: add one file"
new_video "$D"
webhook "$D" Created; scan webhook
verify_expect FAILS created "$CREATED" eq 1
verify_expect FAILS updated "$UPDATED" le "$PARENT_UPDATES"
verify_expect FAILS removed "$REMOVED" eq 0
verify_expect FAILS probes "$PROBES" eq 1
expect_providers
[ -n "$(item_id "$D")" ] || FAILS+=("the new item is not in /Items")
row_end "$(counts)"

# 6: the webhook reports it deleted.
row_start 6 "webhook: delete one file"
rm -f -- "$LOCAL/$D" "$LOCAL/${D%.mkv}.nfo"
[ "$KIND" = tvshows ] || rmdir -- "$LOCAL/$(dirname "$D")"
webhook "$D" Deleted; scan webhook
verify_expect FAILS removed "$REMOVED" eq 1
verify_expect FAILS created "$CREATED" eq 0
verify_expect FAILS updated "$UPDATED" le "$PARENT_UPDATES"
verify_expect FAILS probes "$PROBES" eq 0
verify_expect FAILS "provider requests" "$PROVIDERS" eq 0
[ -z "$(item_id "$D")" ] || FAILS+=("the deleted item is still in /Items")
row_end "$(counts)"

# 4: an NFO written more than a minute after the item's last save re-reads its local metadata.
row_start 4 "NFO newer than DateLastSaved + 1 min"
wait_for=$((NFO_WAIT - (SECONDS - SAVED_AT)))
if [ "$wait_for" -gt 0 ]; then echo "      (row 4 waits ${wait_for}s for the NFO rule's minute)"; pause "$wait_for"; fi
# Overwritten in place (its folder's mtime stays), with the rewrite's own mtime: now. A movie
# keeps its trailer, so no backfill asks about it later.
trailer=; [ "$KIND" = tvshows ] || trailer=ferrofinverifyc
verify_nfo "$NFO_KIND" "$(title "$C")" "Rewritten by the verify script." "$trailer" >"$LOCAL/${C%.mkv}.nfo"
refresh_library; scan api
verify_expect FAILS updated "$UPDATED" eq 1
verify_expect FAILS created "$CREATED" eq 0
verify_expect FAILS probes "$PROBES" eq 0
verify_expect FAILS "provider requests" "$PROVIDERS" eq 0
verify_expect_str FAILS Overview "$(item "$ID_C" | jq -r '.Overview // ""')" "Rewritten by the verify script."
row_end "$(counts)"

# 7: an editor save without LockData survives a rescan.
row_start 7 "edit Overview (no LockData), rescan"
# The cleared field is the official rating: no "already enriched" gate reads it, so clearing
# it cannot make the next scan ask the providers again (clearing a rating could, with OMDb on).
edit "$ID_A" '.Overview = "Edited by the verify script." | .OfficialRating = null' || FAILS+=("the edit was refused")
refresh_library; scan api
verify_expect FAILS created "$CREATED" eq 0
verify_expect FAILS updated "$UPDATED" eq 0
verify_expect FAILS probes "$PROBES" eq 0
verify_expect FAILS "provider requests" "$PROVIDERS" eq 0
dto=$(item "$ID_A")
verify_expect_str FAILS Overview "$(jq -r '.Overview // ""' <<<"$dto")" "Edited by the verify script."
verify_expect_str FAILS LockData "$(jq -r '.LockData' <<<"$dto")" false
verify_expect_str FAILS OfficialRating "$(jq -r '.OfficialRating' <<<"$dto")" null
row_end "$(counts)"

# 8: "Search for missing metadata" fills the cleared official rating, keeps the edited overview.
row_start 8 "\"Search for missing metadata\" on the item"
refresh_item "$ID_A" false; scan api
verify_expect FAILS updated "$UPDATED" eq 1
verify_expect FAILS probes "$PROBES" eq 1
expect_providers
dto=$(item "$ID_A")
verify_expect_str FAILS Overview "$(jq -r '.Overview // ""' <<<"$dto")" "Edited by the verify script."
[ "$(jq -r '.OfficialRating' <<<"$dto")" != null ] || FAILS+=("the cleared official rating was not filled")
row_end "$(counts) OfficialRating=$(jq -r '.OfficialRating' <<<"$dto")"

# 9: "Replace all metadata" replaces the unlocked overview.
row_start 9 "\"Replace all metadata\", Overview unlocked"
refresh_item "$ID_A" true; scan api
verify_expect FAILS updated "$UPDATED" eq 1
verify_expect FAILS probes "$PROBES" eq 1
expect_providers
overview=$(item "$ID_A" | jq -r '.Overview // ""')
[ "$overview" != "Edited by the verify script." ] || FAILS+=("the edited overview was not replaced")
row_end "$(counts) Overview='$overview'"

# 10: a locked field survives "Replace all metadata"; the unlocked rating is replaced.
row_start 10 "lock Overview, \"Replace all metadata\""
edit "$ID_A" '.Overview = "Locked by the verify script." | .CommunityRating = 1 | .LockedFields = ["Overview"]' ||
  FAILS+=("the edit was refused")
refresh_item "$ID_A" true; scan api
verify_expect FAILS updated "$UPDATED" eq 1
expect_providers
dto=$(item "$ID_A")
verify_expect_str FAILS Overview "$(jq -r '.Overview // ""' <<<"$dto")" "Locked by the verify script."
verify_expect_str FAILS LockedFields "$(jq -c '.LockedFields' <<<"$dto")" '["Overview"]'
[ "$(jq -r '.CommunityRating' <<<"$dto")" != 1 ] || FAILS+=("the unlocked rating was not replaced")
row_end "$(counts) rating=$(jq -r '.CommunityRating' <<<"$dto")"

# 11: LockData refuses the providers; a new local poster is still discovered. The item's file
# is touched too: unlocked, that alone makes a scan ask its remote providers (row 3, when the
# library's fetchers are on), so with them on it is the lock that keeps this rescan quiet.
row_start 11 "LockData=true, add a poster, touch, rescan"
edit "$ID_B" '.LockData = true' || FAILS+=("the lock was refused")
[ "$(item "$ID_B" | jq -r '.ImageTags.Primary // ""')" = "" ] || FAILS+=("the item had a poster already")
base64 -d <<<"$VERIFY_POSTER_PNG" >"$WORK/poster.png"
new_file "$POSTER" "$WORK/poster.png"
touch -m -- "$LOCAL/$B"
refresh_library; scan api
# The item, and in a TV library its season: the new file moved the season folder's mtime.
verify_expect FAILS updated "$UPDATED" eq $((1 + PARENT_UPDATES))
verify_expect FAILS created "$CREATED" eq 0
# A locked item's file is still probed: the probe is not a remote provider.
verify_expect FAILS probes "$PROBES" eq 1
verify_expect FAILS "provider requests" "$PROVIDERS" eq 0
dto=$(item "$ID_B")
[ -n "$(jq -r '.ImageTags.Primary // ""' <<<"$dto")" ] || FAILS+=("the poster was not discovered")
verify_expect_str FAILS LockData "$(jq -r '.LockData' <<<"$dto")" true
row_end "$(counts)"

# 12: a locked item a rescan saves keeps its stored fields (Data: its trailers).
row_start 12 "locked item saved by a rescan"
kept='{Name, Overview, RemoteTrailers, LockData, CommunityRating}'
before_fields=$(item "$ID_B" | jq -cS "$kept")
# Two minutes ahead: row 11 touched it moments ago, within the scan's 1 s tolerance of now.
touch -m -d '2 minutes' -- "$LOCAL/$B"
refresh_library; scan api
verify_expect FAILS updated "$UPDATED" eq 1
verify_expect FAILS probes "$PROBES" eq 1
verify_expect FAILS "provider requests" "$PROVIDERS" eq 0
verify_expect_str FAILS "stored fields" "$(item "$ID_B" | jq -cS "$kept")" "$before_fields"
row_end "$(counts) trailers=$(jq -c '.RemoteTrailers | length' <<<"$before_fields")"

# 13: an episode's stored date and year survive a quiet rescan and a save without its providers.
# Only fields its NFO does not carry: the sidecar's re-probe makes the server run every local
# reader too (upstream runs them all when any provider's change monitor fires), so a field the
# NFO holds — its ratings — is legitimately the NFO's again after that save.
if [ "$KIND" != tvshows ]; then
  row_skip 13 "episode date/year survive rescans" "needs a tvshows library (a scratch series folder)"
else
  row_start 13 "episode date/year survive rescans"
  edit "$ID_C" '.PremiereDate = "2010-01-05T00:00:00.0000000Z" | .ProductionYear = 2010' ||
    FAILS+=("the edit was refused")
  fields='{PremiereDate, ProductionYear}'
  before_fields=$(item "$ID_C" | jq -cS "$fields")
  refresh_library; scan api
  verify_expect FAILS created "$CREATED" eq 0
  verify_expect FAILS updated "$UPDATED" eq 0
  verify_expect FAILS probes "$PROBES" eq 0
  verify_expect FAILS "provider requests" "$PROVIDERS" eq 0
  verify_expect_str FAILS "after a rescan" "$(item "$ID_C" | jq -cS "$fields")" "$before_fields"
  quiet=$(counts)
  # A new sidecar re-probes the episode and saves it without asking its providers.
  printf '1\n00:00:01,000 --> 00:00:02,000\nverify\n' >"$WORK/srt"
  new_file "${C%.mkv}.en.srt" "$WORK/srt"
  scrape "$BEFORE" >/dev/null
  refresh_library; scan api
  verify_expect FAILS "probes after a sidecar" "$PROBES" eq 1
  verify_expect FAILS "updated after a sidecar" "$UPDATED" eq $((1 + PARENT_UPDATES))
  verify_expect FAILS "provider requests after a sidecar" "$PROVIDERS" eq 0
  verify_expect_str FAILS "after a save" "$(item "$ID_C" | jq -cS "$fields")" "$before_fields"
  row_end "quiet: $quiet; sidecar: $(counts)"
fi

finish
