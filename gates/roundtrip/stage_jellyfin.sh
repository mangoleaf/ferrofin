#!/usr/bin/env bash
# The Jellyfin stage of a round-trip route, sourced by run.sh: boot the Jellyfin release under test on what
# the export wrote, read its log (checks_boot.py), refresh every playlist's metadata in full (api.py
# refresh-playlists), compare its answers with Ferrofin's oracle, probe the files
# it serves (probes.py), scan its libraries, and check the file and its library rows (checks_paths.py) once it
# has stopped. It uses run.sh's state: route, dst, first_start, the settings, FIXTURES, JELLYFIN_ID, MADE, and
# why[] through check and mark.
# shellcheck disable=SC2154  # run.sh sets what this reads

# jellyfin_stage: the Jellyfin checks of one route, each recorded.
jellyfin_stage() {
  local name cid=$dst/jellyfin.cid port url='' ready=1 log
  name=$(rt_name "$RUN_ID" "$route" jellyfin)
  if ! mkdir -p "$dst/jellyfin-cache"; then
    why+=("cannot create its cache directory"); mark Jellyfin ready; return
  fi
  MADE+=("$cid")
  if port=$(rt_start "$name" "$cid" "$JELLYFIN_ID" -v "$dst/jellyfin:/config" -v "$dst/jellyfin-cache:/cache" \
      "${MEDIA[@]}" 2>"$dst/jellyfin-start.err"); then
    url=http://127.0.0.1:$port
    if ! rt_wait_up "$name" "$port" "$READY_TIMEOUT" "$RT_JELLYFIN_START_FAILED"; then
      ready=''
      log=$(rt_docker logs "$name" 2>&1)
      if grep -qE -- "$RT_JELLYFIN_START_FAILED" <<<"$log"; then why+=("its start failed")
      else why+=("$(rt_not_up "$name" "$READY_TIMEOUT")"); fi
    fi
  else
    why+=("did not start: $(head -c 300 "$dst/jellyfin-start.err")"); ready=''
  fi
  [ -s "$dst/jellyfin-start.err" ] || rm -f -- "$dst/jellyfin-start.err"
  mark Jellyfin ready
  if [ -n "$ready" ]; then
    check reasons python3 "$HERE/api.py" settle --base-url "$url" --data-dir "$dst/jellyfin" --timeout "$SCAN_TIMEOUT"
    mark Jellyfin "its startup tasks finish"
  fi
  if [ -s "$cid" ]; then
    rt_docker logs "$name" > "$dst/jellyfin-boot.log" 2>&1
    check reasons python3 "$HERE/checks_boot.py" migrations "$dst/jellyfin-boot.log"
    mark Jellyfin "every migration it began finished"
    # what Jellyfin's own first start on this route's source ran, as build-fixtures.sh logged it, less what the
    # source's history (as Ferrofin stopped with it) records
    check reasons python3 "$HERE/checks_boot.py" ran "$dst/jellyfin-boot.log" "$FIXTURES/$first_start.server.log" \
      "$dst/ferrofin/data/jellyfin.db"
    mark Jellyfin "it ran the code migrations it runs on the source, and no schema migration"
    check reasons python3 "$HERE/checks_boot.py" errors "$dst/jellyfin-boot.log"
    mark Jellyfin "no ERR or FTL outside the allowlist"
  fi
  if [ -n "$ready" ]; then
    # Before the answers: a refresh that merged a stale playlist.xml back would show in them (a removed share).
    check reasons python3 "$HERE/api.py" refresh-playlists --base-url "$url" --data-dir "$dst/jellyfin" \
      --timeout "$SCAN_TIMEOUT"
    mark Jellyfin "every playlist's full metadata refresh completes"
    check reasons_within "$SCAN_TIMEOUT" "Jellyfin's answers" \
      python3 "$HERE/answers.py" --base-url "$url" --data-dir "$dst/jellyfin" --out "$dst/answers.txt"
    [ ! -f "$dst/answers.txt" ] || check rt_compare_answers "$dst/oracle.txt" "$dst/answers.txt"
    mark Jellyfin "answers equal Ferrofin's before the stop"
    check reasons_within "$SCAN_TIMEOUT" "the probes" \
      python3 "$HERE/probes.py" --base-url "$url" --data-dir "$dst/jellyfin" --before "$dst/ferrofin/data/jellyfin.db" \
      --out "$dst/probes.txt"
    mark Jellyfin "it serves every image, trickplay tile, subtitle and attachment its database lists"
    check reasons python3 "$HERE/api.py" scan --base-url "$url" --data-dir "$dst/jellyfin" --timeout "$SCAN_TIMEOUT"
    mark Jellyfin "its library scan completes"
  fi
  if [ -s "$cid" ]; then
    rt_stop "$name" "$STOP_TIMEOUT" >/dev/null
    rt_docker logs "$name" > "$dst/jellyfin.log" 2>&1
  fi
  rt_remove "$cid"
  check adoption_check_db "$dst/jellyfin/data/jellyfin.db"
  mark Jellyfin "integrity_check and foreign_key_check"
  if [ -n "$ready" ]; then
    check reasons python3 "$HERE/checks_paths.py" libraries --before "$dst/ferrofin/data/jellyfin.db" \
      --after "$dst/jellyfin/data/jellyfin.db"
    mark Jellyfin "every library keeps its folder row and DateCreated through its scan"
  fi
}
