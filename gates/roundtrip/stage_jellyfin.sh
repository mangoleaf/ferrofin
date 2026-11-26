#!/usr/bin/env bash
# The Jellyfin stage of a round-trip route, sourced by run.sh: boot the Jellyfin release under test on what
# the export wrote, read its log (checks_boot.py), refresh every playlist's metadata in full (api.py
# refresh-playlists), compare its answers with Ferrofin's oracle, compare its Live TV with Ferrofin's before and
# after its first guide refresh (livetv.py), probe the files it serves (probes.py), scan its libraries, compare its
# answers again, stop it, and check its file and library rows (checks_paths.py); then start it a second time on the
# same file, which must run no migration, change no row the allowlist does not explain and give the same answers.
# Its database is copied at the end of each window (allowlist.py WINDOWS: its start, the gate's requests, its scan,
# the rest until its stop, its second start), and every row a window added, removed or changed must be explained by
# an allowlist entry (checks_db.py rows); its scan must keep every item and the counts by type (checks_db.py
# counts). On the scale route, where Ferrofin captured no answers, there is no playlist refresh, answer, Live TV
# check, probe or scan (stage_ferrofin.sh says why): its starts, logs, windows, stops and file are checked. It uses
# run.sh's state: route, kind, dst, first_start, the settings, FIXTURES, JELLYFIN_ID, MEDIA, MADE, and why[]
# through check and mark.
# shellcheck disable=SC2154  # run.sh sets what this reads

# jellyfin_start <stage> <role>: start Jellyfin on the export as the container <role> and wait until it answers and
# its startup tasks have finished, each a check of <stage>. Sets name and cid, and url once it answered.
jellyfin_start() {
  local stage=$1 role=$2 port log
  name=$(rt_name "$RUN_ID" "$route" "$role") cid=$dst/$role.cid url=''
  MADE+=("$cid")
  if port=$(rt_start "$name" "$cid" "$JELLYFIN_ID" -v "$dst/jellyfin:/config" -v "$dst/jellyfin-cache:/cache" \
      -v "$dst/livetv:/livetv:ro" "${MEDIA[@]}" 2>"$dst/$role-start.err"); then
    if rt_wait_up "$name" "$port" "$READY_TIMEOUT" "$RT_JELLYFIN_START_FAILED"; then
      url=http://127.0.0.1:$port
    else
      log=$(rt_docker logs "$name" 2>&1)
      if grep -qE -- "$RT_JELLYFIN_START_FAILED" <<<"$log"; then why+=("its start failed")
      else why+=("$(rt_not_up "$name" "$READY_TIMEOUT")"); fi
    fi
  else
    why+=("did not start: $(head -c 300 "$dst/$role-start.err")")
  fi
  [ -s "$dst/$role-start.err" ] || rm -f -- "$dst/$role-start.err"
  mark "$stage" ready
  [ -n "$url" ] || return 0
  check reasons python3 "$HERE/api.py" settle --base-url "$url" --data-dir "$dst/jellyfin" --timeout "$SCAN_TIMEOUT"
  mark "$stage" "its startup tasks finish"
}

# window <stage> <window> <before> <after> <check>: copy Jellyfin's database as it is now to the snapshot <after>,
# and check that every row the window changed since the snapshot <before> is explained (checks_db.py rows, which
# writes rows-<window>.txt and its .jsonl.gz).
window() {
  local out
  if out=$(python3 "$HERE/checks_db.py" snapshot --db "$dst/jellyfin/data/jellyfin.db" --to "$dst/db/$4.db" 2>&1); then
    check reasons_within "$SCAN_TIMEOUT" "the row diff" \
      python3 "$HERE/checks_db.py" rows --window "$2" --before "$dst/db/$3.db" --after "$dst/db/$4.db" --out "$dst/rows-$2.txt"
  else
    why+=("cannot copy its database at the end of the $2: $out")
  fi
  mark "$1" "$5"
}

# answers_equal <stage> <file> <check>: Jellyfin's answers now, written to <file>, equal Ferrofin's before the stop.
answers_equal() {
  check reasons_within "$SCAN_TIMEOUT" "Jellyfin's answers" \
    python3 "$HERE/answers.py" --base-url "$url" --data-dir "$dst/jellyfin" --out "$dst/$2"
  [ ! -f "$dst/$2" ] || check rt_compare_answers "$dst/oracle.txt" "$dst/$2"
  mark "$1" "$3"
}

# stopped <stage> <log>: stop the container, judged as a check of <stage> when it answered (one that never did says
# nothing by its exit), and keep its whole log in <log>.
stopped() {
  [ -s "$cid" ] || return 0
  if [ -n "$url" ]; then
    check rt_stop "$name" "$STOP_TIMEOUT"
    mark "$1" "it stops cleanly"
  else
    rt_stop "$name" "$STOP_TIMEOUT" >/dev/null
  fi
  rt_docker logs "$name" > "$dst/$2" 2>&1
}

# jellyfin_stage: the Jellyfin checks of one route, each recorded.
jellyfin_stage() {
  local name cid url full=1 last out
  [ "$kind" != scale ] || full=''   # nothing Ferrofin captured to compare, and no scan (stage_ferrofin.sh)
  if ! mkdir -p "$dst/jellyfin-cache" "$dst/db"; then
    why+=("cannot create its cache and snapshot directories"); mark Jellyfin ready; return
  fi
  if ! out=$(python3 "$HERE/checks_db.py" snapshot --db "$dst/jellyfin/data/jellyfin.db" --to "$dst/db/export.db" 2>&1); then
    why+=("cannot copy the export's database: $out"); mark Jellyfin ready; return
  fi
  jellyfin_start Jellyfin jellyfin
  if [ -s "$cid" ]; then
    rt_docker logs "$name" > "$dst/jellyfin-boot.log" 2>&1
    check reasons python3 "$HERE/checks_boot.py" migrations "$dst/jellyfin-boot.log"
    mark Jellyfin "every migration it began finished"
    # what Jellyfin's own first start on this route's source ran, as build-fixtures.sh logged it, less what the
    # source's history (as Ferrofin stopped with it) records and what the export records as applied (classify.rs)
    check reasons python3 "$HERE/checks_boot.py" ran "$dst/jellyfin-boot.log" "$FIXTURES/$first_start.server.log" \
      "$dst/ferrofin/data/jellyfin.db"
    mark Jellyfin "it ran the code migrations it runs on the source, and no schema migration"
    check reasons python3 "$HERE/checks_boot.py" errors "$dst/jellyfin-boot.log"
    mark Jellyfin "no ERR or FTL outside the allowlist"
  fi
  [ -z "$url" ] || window Jellyfin start export started "every row its start changed is explained"
  last=started
  if [ -n "$url" ] && [ -n "$full" ]; then
    # Before the answers: a refresh that merged a stale playlist.xml back would show in them (a removed share).
    check reasons python3 "$HERE/api.py" refresh-playlists --base-url "$url" --data-dir "$dst/jellyfin" \
      --timeout "$SCAN_TIMEOUT"
    mark Jellyfin "every playlist's full metadata refresh completes"
    answers_equal Jellyfin answers.txt "answers equal Ferrofin's before the stop"
    # twice the script's own bound, so its own reason comes first
    check reasons_within $((2 * SCAN_TIMEOUT)) "the Live TV check" python3 "$HERE/livetv.py" check --base-url "$url" \
      --data-dir "$dst/jellyfin" --ferrofin "$dst/livetv-ferrofin.json" --timeout "$SCAN_TIMEOUT"
    mark Jellyfin "Live TV: timers, a favourite channel and the guide equal Ferrofin's before and after its first guide refresh"
    check reasons_within "$SCAN_TIMEOUT" "the probes" \
      python3 "$HERE/probes.py" --base-url "$url" --data-dir "$dst/jellyfin" --before "$dst/ferrofin/data/jellyfin.db" \
      --out "$dst/probes.txt"
    mark Jellyfin "it serves every image, trickplay tile, subtitle and attachment its database lists"
    window Jellyfin capture started captured "every row the gate's requests changed is explained"
    check reasons python3 "$HERE/api.py" scan --base-url "$url" --data-dir "$dst/jellyfin" --timeout "$SCAN_TIMEOUT"
    [ "$mark_from" != "${#why[@]}" ] ||
      check reasons python3 "$HERE/api.py" settle --base-url "$url" --data-dir "$dst/jellyfin" --timeout "$SCAN_TIMEOUT"
    mark Jellyfin "its library scan completes, and its tasks after it"
    window Jellyfin scan captured scanned "every row its scan changed is explained"
    check reasons python3 "$HERE/checks_db.py" counts --before "$dst/db/captured.db" --after "$dst/db/scanned.db"
    mark Jellyfin "its scan kept every item, and the item counts by type"
    answers_equal Jellyfin answers-scanned.txt "answers after its scan equal Ferrofin's before the stop"
    last=scanned
  fi
  stopped Jellyfin jellyfin.log
  if [ -n "$url" ]; then
    # the log after the one its first checks read: the gate's requests, the scan and the stop
    check reasons python3 "$HERE/checks_boot.py" errors "$dst/jellyfin.log" "$dst/jellyfin-boot.log"
    mark Jellyfin "no ERR or FTL after its startup outside the allowlist"
  fi
  rt_remove "$cid"
  [ -z "$url" ] || window Jellyfin stop "$last" stopped "every row changed from then until its stop is explained"
  check adoption_check_db "$dst/jellyfin/data/jellyfin.db"
  mark Jellyfin "integrity_check and foreign_key_check"
  if [ -n "$url" ] && [ -n "$full" ]; then   # a Jellyfin that never answered scanned nothing
    check reasons python3 "$HERE/checks_paths.py" libraries --before "$dst/ferrofin/data/jellyfin.db" \
      --after "$dst/jellyfin/data/jellyfin.db"
    mark Jellyfin "every library keeps its folder row and DateCreated through its scan"
  fi
  [ -n "$url" ] || return 0   # a Jellyfin that never answered is not started again
  jellyfin_again
}

# jellyfin_again: Jellyfin's second start on the file its first left, each check recorded.
jellyfin_again() {
  local name cid url ran=''
  jellyfin_start "Jellyfin again" jellyfin-again
  [ ! -s "$cid" ] || ran=1
  if [ -s "$cid" ]; then
    rt_docker logs "$name" > "$dst/jellyfin-again-boot.log" 2>&1
    check reasons python3 "$HERE/checks_boot.py" quiet "$dst/jellyfin-again-boot.log"
    mark "Jellyfin again" "it ran no migration"
  fi
  if [ -n "$url" ]; then
    window "Jellyfin again" restart stopped restarted "every row its start changed is explained"
    [ -z "$full" ] || answers_equal "Jellyfin again" answers-again.txt "answers equal Ferrofin's before the stop"
  fi
  stopped "Jellyfin again" jellyfin-again.log
  if [ -n "$ran" ]; then   # an empty log fails here rather than passing unread
    check reasons python3 "$HERE/checks_boot.py" errors "$dst/jellyfin-again.log"
    mark "Jellyfin again" "no ERR or FTL outside the allowlist"
  fi
  rt_remove "$cid"
  check adoption_check_db "$dst/jellyfin/data/jellyfin.db"
  mark "Jellyfin again" "integrity_check and foreign_key_check"
}
