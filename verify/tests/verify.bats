#!/usr/bin/env bats
# Each @test runs in its own subshell, so a test naming its own SCRATCH folder is meant to be
# local to it.
# shellcheck disable=SC2030,SC2031
# Tests for the scan-behaviour check (verify/lib.sh, verify/scan-behaviour.sh). Nothing here
# needs a Ferrofin: lib.sh is driven with canned /metrics text and throwaway files; the script's
# refusals run against a static file server (python3 -m http.server) standing in for the API,
# every one before the script writes or asks anything; and whole runs go against
# tests/fake_server.py, which plays back a scenario of what a well-behaved server's scans do.

setup() {
  VERIFY="$BATS_TEST_DIRNAME/.."
  # shellcheck source=verify/lib.sh
  . "$VERIFY/lib.sh"
  TMP="$(realpath "$BATS_TEST_TMPDIR")/verify"
  mkdir -p "$TMP"
  cd "$TMP" || exit 1
  export FERROFIN_TOKEN=s3cret-verify-token
  # The scratch folder the whole runs use; a TV test names it as the scratch series.
  SCRATCH=lib/scratch
}

teardown() {
  if [ -n "${SERVER_PID:-}" ]; then kill "$SERVER_PID" 2>/dev/null || true; fi
}

# A scrape in the exposition format the server renders.
metrics() {
  cat > "$1" <<'EOM'
# HELP ferrofin_library_scans_total Library scan passes.
# TYPE ferrofin_library_scans_total counter
ferrofin_library_scans_total{otel_scope_name="ferrofin",result="completed",trigger="api"} 3
ferrofin_library_scans_total{otel_scope_name="ferrofin",result="failed",trigger="api"} 1
ferrofin_library_scans_total{otel_scope_name="ferrofin",result="completed",trigger="webhook"} 2
ferrofin_library_scan_items_total{otel_scope_name="ferrofin",outcome="created",trigger="api"} 5
ferrofin_library_scan_items_total{otel_scope_name="ferrofin",outcome="unchanged",trigger="api"} 20497
ferrofin_library_scan_in_progress{otel_scope_name="ferrofin"} 0
ferrofin_library_scans_total_bogus{trigger="api"} 100
EOM
}

# --- /metrics ------------------------------------------------------------------------------

@test "metric_sum: sums the series carrying every label, and nothing else" {
  metrics m.txt
  [ "$(verify_metric_sum m.txt ferrofin_library_scans_total)" = 6 ]
  [ "$(verify_metric_sum m.txt ferrofin_library_scans_total trigger=api)" = 4 ]
  [ "$(verify_metric_sum m.txt ferrofin_library_scans_total trigger=api result=completed)" = 3 ]
  [ "$(verify_metric_sum m.txt ferrofin_library_scan_items_total outcome=unchanged)" = 20497 ]
  [ "$(verify_metric_sum m.txt ferrofin_library_scan_in_progress)" = 0 ]
}

@test "metric_sum: an absent series or label value is zero" {
  metrics m.txt
  [ "$(verify_metric_sum m.txt ferrofin_media_probe_total)" = 0 ]
  [ "$(verify_metric_sum m.txt ferrofin_library_scans_total trigger=watcher)" = 0 ]
}

@test "delta: how much a series grew between two scrapes" {
  metrics before.txt
  sed 's/trigger="api"} 3/trigger="api"} 5/' before.txt > after.txt
  [ "$(verify_delta before.txt after.txt ferrofin_library_scans_total trigger=api result=completed)" = 2 ]
  [ "$(verify_delta before.txt after.txt ferrofin_library_scans_total trigger=webhook)" = 0 ]
}

@test "metrics_enabled: a 404 points at FERROFIN_ENABLE_METRICS" {
  : > m.txt
  run verify_metrics_enabled m.txt 404
  [[ "$output" == *"/metrics is disabled"* ]]
  [[ "$output" == *"FERROFIN_ENABLE_METRICS=true"* ]]
}

@test "metrics_enabled: another status, and a scrape without the scan metrics, are named" {
  echo 'http_requests_received_total{code="200"} 1' > m.txt
  run verify_metrics_enabled m.txt 500
  [ "$output" = "/metrics answered HTTP 500" ]
  run verify_metrics_enabled m.txt 200
  [[ "$output" == *"no ferrofin_library_scans_total"* ]]
  metrics m.txt
  run verify_metrics_enabled m.txt 200
  [ -z "$output" ]
}

# --- helpers -------------------------------------------------------------------------------

@test "inside: strictly below a library location, never the location or a lookalike" {
  verify_inside /media/movies/zz-verify /media/tv /media/movies
  verify_inside /media/movies/zz-verify/ /media/movies/
  run verify_inside /media/movies /media/movies
  [ "$status" -eq 1 ]
  run verify_inside /media/movies2/zz /media/movies
  [ "$status" -eq 1 ]
  run verify_inside /elsewhere/zz /media/movies
  [ "$status" -eq 1 ]
}

@test "inside: a '.' or '..' segment is refused, whatever it resolves to" {
  run verify_inside /media/movies/../etc /media/movies
  [ "$status" -eq 1 ]
  run verify_inside /media/movies/zz/.. /media/movies
  [ "$status" -eq 1 ]
  run verify_inside /media/movies/./zz /media/movies
  [ "$status" -eq 1 ]
  verify_inside /media/movies/zz..verify /media/movies
}

@test "nfo: every scratch NFO carries what the enriched gates read; a trailer when asked" {
  run verify_nfo movie "Ferrofin Verify A" "A plot." abc
  [[ "$output" == *"<movie>"*"<title>Ferrofin Verify A</title>"*"<plot>A plot.</plot>"* ]]
  [[ "$output" == *"<rating>6.5</rating>"*"<criticrating>70</criticrating>"*"<mpaa>PG</mpaa>"* ]]
  [[ "$output" == *"<trailer>plugin://plugin.video.youtube/play/?video_id=abc</trailer>"*"</movie>" ]]
  run verify_nfo episodedetails "Ferrofin Verify C" "C plot."
  [[ "$output" == *"<rating>6.5</rating>"*"<criticrating>70</criticrating>"* ]]
  [[ "$output" != *"<trailer>"* ]]
  [[ "$output" == *"</episodedetails>" ]]
}

@test "expect: failures are collected with what was wanted" {
  local fails=()
  verify_expect fails created 3 eq 3
  verify_expect fails probes 0 le 1
  verify_expect fails requests 2 ge 1
  [ ${#fails[@]} -eq 0 ]
  verify_expect fails created 2 eq 3
  verify_expect fails updated 2 le 1
  verify_expect fails requests 0 ge 1
  verify_expect_str fails Overview "old" "new"
  [ "${fails[0]}" = "created 2 (want = 3)" ]
  [ "${fails[1]}" = "updated 2 (want <= 1)" ]
  [ "${fails[2]}" = "requests 0 (want >= 1)" ]
  [ "${fails[3]}" = "Overview 'old' (want 'new')" ]
}

@test "row_line: row, result, scenario and detail in columns" {
  run verify_row_line 3 PASS "touch one file's mtime, rescan" "updated=1"
  [ "$output" = "   3  PASS  touch one file's mtime, rescan                  updated=1" ]
}

# --- the script's refusals ----------------------------------------------------------------

# serve: a static file server over $TMP/www standing in for the API; sets BASE.
serve() {
  mkdir -p www/Users www/Library www/System
  python3 -u -m http.server --bind 127.0.0.1 0 --directory www > server.log 2>&1 &
  SERVER_PID=$!
  for _ in $(seq 1 100); do
    BASE=$(grep -o 'http://127.0.0.1:[0-9]*' server.log | head -1)
    [ -z "$BASE" ] || return 0
    sleep 0.1
  done
  return 1
}

# api_files <collection-type> <realtime> <location>: the answers the preflight reads.
api_files() {
  metrics www/metrics
  echo '{"Name":"admin","Policy":{"IsAdministrator":true}}' > www/Users/Me
  echo '{"LibraryMonitorDelay":1}' > www/System/Configuration
  cat > www/Library/VirtualFolders <<EOJ
[{"Name":"Lib","ItemId":"f137a2dd21bbc1b99aa5c0f6bf02a805","CollectionType":"$1","Locations":["$3"],
  "LibraryOptions":{"EnableRealtimeMonitor":$2,"TypeOptions":[]}}]
EOJ
}

@test "script: missing arguments, an unknown option and a bad number are usage errors" {
  run "$VERIFY/scan-behaviour.sh"
  [ "$status" -eq 2 ]
  [[ "$output" == *"BASE_URL LIBRARY_ID SCRATCH_DIR"* ]]
  run "$VERIFY/scan-behaviour.sh" --nope a b c
  [ "$status" -eq 2 ]
  run "$VERIFY/scan-behaviour.sh" --timeout soon a b c
  [ "$status" -eq 2 ]
  run "$VERIFY/scan-behaviour.sh" --nfo-wait -1 a b c
  [ "$status" -eq 2 ]
}

@test "script: an option with no value is a usage error, not a hang" {
  for option in --clip --timeout --nfo-wait; do
    run timeout 10 "$VERIFY/scan-behaviour.sh" a b c "$option"
    [ "$status" -eq 2 ]
    [[ "$output" == *"$option needs a value"* ]]
  done
}

@test "script: the token comes from FERROFIN_TOKEN, never the command line" {
  run env -u FERROFIN_TOKEN "$VERIFY/scan-behaviour.sh" http://127.0.0.1:9 lib "$TMP"
  [ "$status" -eq 2 ]
  [[ "$output" == *"set FERROFIN_TOKEN"* ]]
  # The old positional token is now one argument too many.
  run "$VERIFY/scan-behaviour.sh" http://127.0.0.1:9 token lib "$TMP" "$TMP" extra
  [ "$status" -eq 2 ]
}

@test "script: refuses when /metrics is disabled, pointing at FERROFIN_ENABLE_METRICS" {
  serve
  mkdir scratch
  run "$VERIFY/scan-behaviour.sh" "$BASE" f137a2dd21bbc1b99aa5c0f6bf02a805 "$TMP/scratch"
  [ "$status" -eq 2 ]
  [[ "$output" == *"/metrics is disabled"*"FERROFIN_ENABLE_METRICS=true"* ]]
}

@test "script: refuses a token the server rejects, and one that is not an administrator's" {
  serve
  metrics www/metrics
  mkdir scratch
  run "$VERIFY/scan-behaviour.sh" "$BASE" f137a2dd21bbc1b99aa5c0f6bf02a805 "$TMP/scratch"
  [ "$status" -eq 2 ]
  [[ "$output" == *"the token was not accepted"* ]]
  api_files movies false "$TMP/lib"
  echo '{"Name":"viewer","Policy":{"IsAdministrator":false}}' > www/Users/Me
  run "$VERIFY/scan-behaviour.sh" "$BASE" f137a2dd21bbc1b99aa5c0f6bf02a805 "$TMP/scratch"
  [ "$status" -eq 2 ]
  [[ "$output" == *"not an administrator's"* ]]
}

@test "script: refuses an unknown library, a music library and real-time monitoring" {
  serve
  mkdir -p lib/scratch
  api_files movies false "$TMP/lib"
  run "$VERIFY/scan-behaviour.sh" "$BASE" 00000000000000000000000000000000 "$TMP/lib/scratch"
  [ "$status" -eq 2 ]
  [[ "$output" == *"no library with id"* ]]
  api_files music false "$TMP/lib"
  run "$VERIFY/scan-behaviour.sh" "$BASE" f137a2dd-21bb-c1b9-9aa5-c0f6bf02a805 "$TMP/lib/scratch"
  [ "$status" -eq 2 ]
  [[ "$output" == *"movies and tvshows are supported"* ]]
  api_files movies true "$TMP/lib"
  run "$VERIFY/scan-behaviour.sh" "$BASE" f137a2dd21bbc1b99aa5c0f6bf02a805 "$TMP/lib/scratch"
  [ "$status" -eq 2 ]
  [[ "$output" == *"real-time monitoring on"* ]]
}

@test "script: a refusal lists nothing it found in the folder it was given" {
  # The library root given by mistake: refused (here, at once: no server), never listed.
  mkdir -p "lib/Real Movie (1999)"
  printf 'real media\n' > "lib/Real Movie (1999)/Real Movie (1999).mkv"
  run "$VERIFY/scan-behaviour.sh" http://127.0.0.1:9 lib "$TMP/lib"
  [ "$status" -eq 2 ]
  [[ "$output" == *"refusing to run"* ]]
  [[ "$output" != *"left in"* ]]
  [[ "$output" != *"Real Movie"* ]]
}

@test "script: refuses a scratch folder outside the library, missing or not empty" {
  serve
  mkdir -p lib/scratch elsewhere
  api_files movies false "$TMP/lib"
  run "$VERIFY/scan-behaviour.sh" "$BASE" f137a2dd21bbc1b99aa5c0f6bf02a805 "$TMP/elsewhere"
  [ "$status" -eq 2 ]
  [[ "$output" == *"is not inside library 'Lib'"* ]]
  run "$VERIFY/scan-behaviour.sh" "$BASE" f137a2dd21bbc1b99aa5c0f6bf02a805 "$TMP/lib/scratch/../../elsewhere"
  [ "$status" -eq 2 ]
  [[ "$output" == *"'.' or '..' segment"* ]]
  ln -s "$TMP/elsewhere" lib/link
  run "$VERIFY/scan-behaviour.sh" "$BASE" f137a2dd21bbc1b99aa5c0f6bf02a805 "$TMP/lib/link"
  [ "$status" -eq 2 ]
  [[ "$output" == *"resolves to $TMP/elsewhere"* ]]
  run "$VERIFY/scan-behaviour.sh" "$BASE" f137a2dd21bbc1b99aa5c0f6bf02a805 "$TMP/lib/missing"
  [ "$status" -eq 2 ]
  [[ "$output" == *"is not a writable directory"* ]]
  echo keep > lib/scratch/someone-elses-file
  run "$VERIFY/scan-behaviour.sh" "$BASE" f137a2dd21bbc1b99aa5c0f6bf02a805 "$TMP/lib/scratch"
  [ "$status" -eq 2 ]
  [[ "$output" == *"is not empty"* ]]
  [[ "$output" != *"left in"* ]]
  [ "$(cat lib/scratch/someone-elses-file)" = keep ]
}

@test "script: a TV scratch folder must be a series folder under the library root" {
  serve
  mkdir -p lib/deeper/scratch
  api_files tvshows false "$TMP/lib"
  run "$VERIFY/scan-behaviour.sh" "$BASE" f137a2dd21bbc1b99aa5c0f6bf02a805 "$TMP/lib/deeper/scratch"
  [ "$status" -eq 2 ]
  [[ "$output" == *"directly under the library root"* ]]
  mkdir -p lib/scratch
  run "$VERIFY/scan-behaviour.sh" "$BASE" f137a2dd21bbc1b99aa5c0f6bf02a805 "$TMP/lib/scratch"
  [ "$status" -eq 2 ]
  [[ "$output" == *"name it 'Ferrofin Verify …'"* ]]
}

@test "script: the server's view of the scratch folder is checked when it differs" {
  serve
  mkdir -p scratch
  api_files movies false /media/movies
  run "$VERIFY/scan-behaviour.sh" "$BASE" f137a2dd21bbc1b99aa5c0f6bf02a805 "$TMP/scratch" /media/shows/x
  [ "$status" -eq 2 ]
  [[ "$output" == *"/media/shows/x is not inside library 'Lib' (/media/movies)"* ]]
}

@test "script: without media it can probe it skips every row and says why" {
  serve
  mkdir -p lib/scratch
  : > empty.mkv
  api_files movies false "$TMP/lib"
  run "$VERIFY/scan-behaviour.sh" --clip "$TMP/empty.mkv" "$BASE" f137a2dd21bbc1b99aa5c0f6bf02a805 "$TMP/lib/scratch"
  [ "$status" -eq 3 ]
  [[ "$output" == *"SKIP: no scratch media"* ]]
  [ -z "$(ls -A lib/scratch)" ]
}

@test "script: refuses to take over a real series at a TV scratch folder" {
  SCRATCH="lib/Ferrofin Verify Show"
  mkdir -p "$SCRATCH"
  echo '[]' > scenario.json
  FAKE_KIND=tvshows FAKE_SERIES='{"Name":"Real Show","ProviderIds":{}}' fake
  run "$VERIFY/scan-behaviour.sh" --clip "$TMP/clip.mkv" "$BASE" f137a2dd21bbc1b99aa5c0f6bf02a805 "$TMP/$SCRATCH"
  [ "$status" -eq 2 ]
  [[ "$output" == *"is the series 'Real Show'"* ]]
  stop_fake
  FAKE_KIND=tvshows FAKE_SERIES='{"Name":"Ferrofin Verify Show","ProviderIds":{"Tmdb":"1"}}' fake
  run "$VERIFY/scan-behaviour.sh" --clip "$TMP/clip.mkv" "$BASE" f137a2dd21bbc1b99aa5c0f6bf02a805 "$TMP/$SCRATCH"
  [ "$status" -eq 2 ]
  [[ "$output" == *"a series with provider ids"* ]]
  [ -z "$(ls -A "$SCRATCH")" ]
}

# --- whole runs against the scripted fake server ---------------------------------------------

# fake: starts tests/fake_server.py over $TMP/$SCRATCH with scenario.json; sets BASE.
fake() {
  printf 'not really a video\n' > clip.mkv
  : > requests.log
  python3 -u "$VERIFY/tests/fake_server.py" "$TMP/$SCRATCH" "$TMP/lib" "$TMP/scenario.json" \
    "$FERROFIN_TOKEN" > fake.log 2>&1 &
  SERVER_PID=$!
  for _ in $(seq 1 100); do
    port=$(sed -n 's/^port //p' fake.log)
    if [ -n "$port" ]; then BASE=http://127.0.0.1:$port; return 0; fi
    sleep 0.1
  done
  return 1
}

stop_fake() {
  kill "$SERVER_PID" 2>/dev/null || true
  wait "$SERVER_PID" 2>/dev/null || true
  SERVER_PID=
}

# The scenario entries, in the order the script's scans run: 0 the refresh before the rows,
# 1-3 rows 1-3, 4-5 rows 5-6 (the webhooks), 6 row 4, 7-12 rows 7-12, 13-14 row 13's two scans.

# movies_scenario [jq-filter]: every scan of a movies-library run, as a well-behaved server
# with no remote fetcher answering does them (row 11's also writes a file into the scratch
# folder, as an NFO saver would); the filter edits it.
movies_scenario() {
  jq "${1:-.}" > scenario.json <<'EOJ'
[
  {"unchanged": 5},
  {"created": 3, "unchanged": 5, "probes": 3},
  {"unchanged": 8},
  {"updated": 1, "unchanged": 7, "probes": 1},
  {"created": 1, "probes": 1},
  {"removed": 1, "unchanged": 3},
  {"updated": 1, "unchanged": 7, "patch": {"Gamma": {"Overview": "Rewritten by the verify script."}}},
  {"unchanged": 8},
  {"updated": 1, "probes": 1, "patch": {"Alpha": {"OfficialRating": "PG"}}},
  {"updated": 1, "probes": 1, "patch": {"Alpha": {"Overview": "Written by the verify script."}}},
  {"updated": 1, "probes": 1, "patch": {"Alpha": {"CommunityRating": 6.5}}},
  {"updated": 1, "unchanged": 7, "probes": 1, "patch": {"Beta": {"ImageTags": {"Primary": "t"}}},
   "saver": "Ferrofin Verify Beta (2002)/movie.nfo"},
  {"updated": 1, "unchanged": 7, "probes": 1}
]
EOJ
}

# The remote requests the rows that run the providers make, when the library's fetchers are on
# (and no saver file, so one test can run twice over the same scratch folder).
WITH_REQUESTS='.[1].requests = 3 | .[3].requests = 1 | .[4].requests = 2 | .[8].requests = 1 | .[9].requests = 1 | .[10].requests = 1 | del(.[11].saver)'

# tv_scenario [jq-filter]: the same for a TV library: the scratch series, its season and its
# episodes, the season refreshing whenever an episode file comes or goes; no request at all,
# as the scratch series has no provider ids.
tv_scenario() {
  jq "${1:-.}" > scenario.json <<'EOJ'
[
  {},
  {"created": 5, "probes": 3},
  {"unchanged": 5},
  {"updated": 1, "unchanged": 4, "probes": 1},
  {"created": 1, "updated": 1, "probes": 1},
  {"removed": 1, "updated": 1},
  {"updated": 1, "unchanged": 4, "patch": {"S01E03": {"Overview": "Rewritten by the verify script."}}},
  {"unchanged": 5},
  {"updated": 1, "probes": 1, "patch": {"S01E01": {"OfficialRating": "PG"}}},
  {"updated": 1, "probes": 1, "patch": {"S01E01": {"Overview": "Written by the verify script."}}},
  {"updated": 1, "probes": 1, "patch": {"S01E01": {"CommunityRating": 6.5}}},
  {"updated": 2, "probes": 1, "patch": {"S01E02": {"ImageTags": {"Primary": "t"}}}},
  {"updated": 1, "probes": 1},
  {"unchanged": 5},
  {"updated": 2, "probes": 1}
]
EOJ
}

# A real item beside the scratch folder, which no run may touch.
real_item() {
  mkdir -p "lib/Real Movie (1999)" lib/scratch
  printf 'real media\n' > "lib/Real Movie (1999)/Real Movie (1999).mkv"
  touch -m -d '2020-01-01' "lib/Real Movie (1999)/Real Movie (1999).mkv"
}

real_item_untouched() {
  [ "$(cat "lib/Real Movie (1999)/Real Movie (1999).mkv")" = "real media" ]
  [ "$(stat -c %Y "lib/Real Movie (1999)/Real Movie (1999).mkv")" = "$(date -d 2020-01-01 +%s)" ]
}

# token_hidden <pid>: the script's command line (read while it runs) names the script and not
# the token, and no request so far carried the token in its URL.
token_hidden() {
  local cmdline=
  for _ in $(seq 1 100); do
    cmdline=$(tr '\0' ' ' < "/proc/$1/cmdline" 2>/dev/null) || break
    [[ "$cmdline" == *"scan-behaviour.sh"* ]] && break
    sleep 0.05
  done
  [[ "$cmdline" == *"scan-behaviour.sh"* && "$cmdline" != *"$FERROFIN_TOKEN"* ]]
  [ "$(grep -c -- "$FERROFIN_TOKEN" requests.log)" = 0 ]
}

# whole_run <args…>: runs the script against the fake as bats' `run` would (status, output),
# checking its command line while it runs and the request URLs after.
whole_run() {
  "$VERIFY/scan-behaviour.sh" --clip "$TMP/clip.mkv" --nfo-wait 0 "$@" > run.log 2>&1 &
  local pid=$!
  token_hidden "$pid"
  status=0
  wait "$pid" || status=$?
  output=$(cat run.log)
  [ "$(grep -c -- "$FERROFIN_TOKEN" requests.log)" = 0 ]
}

LIB_ID=f137a2dd21bbc1b99aa5c0f6bf02a805

@test "run: a well-behaved server passes every row, and only the script's files go" {
  real_item
  movies_scenario
  fake
  whole_run "$BASE" "$LIB_ID" "$TMP/lib/scratch"
  [ "$status" -eq 0 ]
  [[ "$output" == *"remote metadata fetchers for Movie: unknown"* ]]
  [[ "$output" == *"summary: 12 PASS, 0 FAIL, 1 SKIP"* ]]
  # The script's files are gone; the one the "server" wrote is listed, not deleted.
  [ "$(find lib/scratch -type f)" = "lib/scratch/Ferrofin Verify Beta (2002)/movie.nfo" ]
  [[ "$output" == *"left in $TMP/lib/scratch"*"Ferrofin Verify Beta (2002)/movie.nfo"* ]]
  real_item_untouched
}

@test "run: a row that fails exits 1, names what it wanted, and still cleans up" {
  real_item
  movies_scenario '.[2].updated = 1'
  fake
  whole_run "$BASE" "$LIB_ID" "$TMP/lib/scratch"
  [ "$status" -eq 1 ]
  [[ "$output" == *"   2  FAIL  rescan, nothing changed"*"updated 1 (want = 0)"* ]]
  [[ "$output" == *"summary: 11 PASS, 1 FAIL, 1 SKIP"* ]]
  [ "$(find lib/scratch -type f)" = "lib/scratch/Ferrofin Verify Beta (2002)/movie.nfo" ]
  real_item_untouched
}

@test "run: with the fetchers on, the rows that run the providers must see requests" {
  real_item
  movies_scenario "$WITH_REQUESTS"
  FAKE_FETCHERS=on fake
  whole_run "$BASE" "$LIB_ID" "$TMP/lib/scratch"
  [ "$status" -eq 0 ]
  [[ "$output" == *"remote metadata fetchers for Movie: on"* ]]
  [[ "$output" == *"summary: 12 PASS, 0 FAIL, 1 SKIP"* ]]
  [ -z "$(ls -A lib/scratch)" ]
  stop_fake
  # Row 3's touched movie asks nothing: with the fetchers on, that is a failure.
  movies_scenario "$WITH_REQUESTS | .[3].requests = 0"
  FAKE_FETCHERS=on fake
  whole_run "$BASE" "$LIB_ID" "$TMP/lib/scratch"
  [ "$status" -eq 1 ]
  [[ "$output" == *"   3  FAIL"*"provider requests 0 (want >= 1)"* ]]
  [ -z "$(ls -A lib/scratch)" ]
  real_item_untouched
}

@test "run: with the fetchers off, any request is a failure" {
  real_item
  movies_scenario 'del(.[11].saver)'
  FAKE_FETCHERS=off fake
  whole_run "$BASE" "$LIB_ID" "$TMP/lib/scratch"
  [ "$status" -eq 0 ]
  [[ "$output" == *"remote metadata fetchers for Movie: off"* ]]
  [[ "$output" == *"summary: 12 PASS, 0 FAIL, 1 SKIP"* ]]
  [ -z "$(ls -A lib/scratch)" ]
  stop_fake
  movies_scenario 'del(.[11].saver) | .[3].requests = 1'
  FAKE_FETCHERS=off fake
  whole_run "$BASE" "$LIB_ID" "$TMP/lib/scratch"
  [ "$status" -eq 1 ]
  [[ "$output" == *"   3  FAIL"*"provider requests 1 (want = 0)"* ]]
  [ -z "$(ls -A lib/scratch)" ]
  real_item_untouched
}

@test "run: numbers with leading zeros are decimal, not a bash octal error" {
  real_item
  movies_scenario
  fake
  whole_run --timeout 09 --nfo-wait 08 "$BASE" "$LIB_ID" "$TMP/lib/scratch"
  [ "$status" -eq 0 ]
  [[ "$output" != *"value too great for base"* ]]
  [[ "$output" == *"row 4 waits"* ]]
  [[ "$output" == *"summary: 12 PASS, 0 FAIL, 1 SKIP"* ]]
}

@test "run: OMDb alone is on with its shared key, and every scratch NFO holds what the enriched gates read" {
  # OMDb uses a shared key; ticking it must produce requests on forced passes.
  real_item
  movies_scenario "$WITH_REQUESTS | .[1].snapshot = \"$TMP/nfo-row1\" | .[6].snapshot = \"$TMP/nfo-row4\""
  FAKE_TYPE_OPTIONS='[{"Type":"Movie","MetadataFetchers":["The Open Movie Database"]}]' fake
  whole_run "$BASE" "$LIB_ID" "$TMP/lib/scratch"
  [ "$status" -eq 0 ]
  [[ "$output" == *"remote metadata fetchers for Movie: on"* ]]
  [[ "$output" == *"summary: 12 PASS, 0 FAIL, 1 SKIP"* ]]
  [ "$(find nfo-row1 -name '*.nfo' | wc -l)" -eq 3 ]
  for nfo in nfo-row1/*.nfo nfo-row4/*Gamma*.nfo; do
    for field in '<plot>' '<rating>6.5</rating>' '<criticrating>70</criticrating>' '<mpaa>PG</mpaa>' '<trailer>'; do
      grep -qF -- "$field" "$nfo"
    done
  done
}

@test "run: a TV library with the fetchers on checks every row but the first for no requests" {
  SCRATCH="lib/Ferrofin Verify Show"
  mkdir -p "$SCRATCH"
  # Row 1: the new scratch series searches for its match (not counted).
  tv_scenario '.[1].requests = 2'
  FAKE_KIND=tvshows FAKE_FETCHERS=on fake
  whole_run "$BASE" "$LIB_ID" "$TMP/$SCRATCH"
  [ "$status" -eq 0 ]
  [[ "$output" == *"remote metadata fetchers for Series/Season/Episode: on"* ]]
  [[ "$output" == *"row 1's provider requests are not checked; the scratch series has no provider ids,"*"so every other row expects 0"* ]]
  [[ "$output" == *"summary: 13 PASS, 0 FAIL, 0 SKIP"* ]]
  [[ "$output" == *"an empty series 'Ferrofin Verify Show' stays in"*"delete the empty folder"* ]]
  [ -z "$(ls -A "$SCRATCH")" ]
  stop_fake
  # A request for an episode, fetchers on or not, is a failure.
  tv_scenario '.[1].requests = 2 | .[3].requests = 1'
  FAKE_KIND=tvshows FAKE_FETCHERS=on fake
  whole_run "$BASE" "$LIB_ID" "$TMP/$SCRATCH"
  [ "$status" -eq 1 ]
  [[ "$output" == *"   3  FAIL"*"provider requests 1 (want = 0)"* ]]
  [ -z "$(ls -A "$SCRATCH")" ]
}

@test "run: a scratch series that matched a real show stops the run after row 1" {
  SCRATCH="lib/Ferrofin Verify Show"
  mkdir -p "$SCRATCH"
  tv_scenario '.[1].requests = 2 | .[1].patch = {"SERIES": {"ProviderIds": {"Tmdb": "1399"}}}'
  FAKE_KIND=tvshows FAKE_FETCHERS=on fake
  whole_run "$BASE" "$LIB_ID" "$TMP/$SCRATCH"
  [ "$status" -eq 1 ]
  [[ "$output" == *"   1  FAIL"*'the scratch series matched a real show {"Tmdb":"1399"}'* ]]
  [[ "$output" == *"summary: 0 PASS, 1 FAIL, 0 SKIP"* ]]
  [ -z "$(ls -A "$SCRATCH")" ]
}

@test "run: a TV library is on when any of its kinds is, not only Episode" {
  SCRATCH="lib/Ferrofin Verify Show"
  mkdir -p "$SCRATCH"
  # Series ticked, Episode not: the series' own search is a request row 1 must not refuse.
  tv_scenario '.[1].requests = 2'
  FAKE_KIND=tvshows FAKE_TYPE_OPTIONS='[{"Type":"Series","MetadataFetchers":["TheMovieDb"]},
    {"Type":"Season","MetadataFetchers":[]},{"Type":"Episode","MetadataFetchers":[]}]' fake
  whole_run "$BASE" "$LIB_ID" "$TMP/$SCRATCH"
  [ "$status" -eq 0 ]
  [[ "$output" == *"remote metadata fetchers for Series/Season/Episode: on"* ]]
  [[ "$output" == *"summary: 13 PASS, 0 FAIL, 0 SKIP"* ]]
  stop_fake
  # Every kind off: row 1's request is then a failure.
  tv_scenario '.[1].requests = 2'
  FAKE_KIND=tvshows FAKE_FETCHERS=off fake
  whole_run "$BASE" "$LIB_ID" "$TMP/$SCRATCH"
  [ "$status" -eq 1 ]
  [[ "$output" == *"remote metadata fetchers for Series/Season/Episode: off"* ]]
  [[ "$output" == *"   1  FAIL"*"provider requests 2 (want = 0)"* ]]
}

# interrupted_run <scenario-filter>: starts a movies run whose row 1 scan never finishes on its
# own (the script is waiting for it), with the script's temporary files under $TMP/work; sets
# pid once row 1's refresh has gone out.
interrupted_run() {
  real_item
  movies_scenario ".[1].hold = true${1:+ | $1}"
  fake
  mkdir -p work
  set -m  # a background job keeps SIGINT (a non-interactive shell would ignore it)
  TMPDIR=$TMP/work "$VERIFY/scan-behaviour.sh" --clip "$TMP/clip.mkv" --nfo-wait 0 \
    "$BASE" "$LIB_ID" "$TMP/lib/scratch" > run.log 2>&1 &
  pid=$!
  token_hidden "$pid"
  wait_for_refreshes 2
  [ -n "$(ls -A lib/scratch)" ]
}

# wait_for_refreshes <n>: until the fake has seen n library refreshes.
wait_for_refreshes() {
  for _ in $(seq 1 100); do
    [ "$(grep -c "POST /Items/$LIB_ID/Refresh" requests.log)" -ge "$1" ] && return 0
    sleep 0.1
  done
  return 1
}

@test "run: SIGINT mid-run exits 130 after the cleanup" {
  interrupted_run
  kill -INT "$pid"
  status=0
  wait "$pid" || status=$?
  set +m
  [ "$status" -eq 130 ]
  [ "$(grep -c -- "$FERROFIN_TOKEN" requests.log)" = 0 ]
  grep -q 'scratch files removed' run.log
  [ -z "$(ls -A lib/scratch)" ]
  [ -z "$(ls -A work)" ]
  real_item_untouched
}

@test "run: a second SIGINT during the cleanup's rescan still removes the token file" {
  # The cleanup's rescan (the next scan) never finishes either.
  interrupted_run '.[2].hold = true'
  kill -INT "$pid"
  wait_for_refreshes 3
  [ -n "$(ls -A work)" ]
  kill -INT "$pid"
  status=0
  wait "$pid" || status=$?
  set +m
  [ "$status" -eq 130 ]
  [ "$(grep -c -- "$FERROFIN_TOKEN" requests.log)" = 0 ]
  [ -z "$(ls -A lib/scratch)" ]
  [ -z "$(ls -A work)" ]
  real_item_untouched
}
