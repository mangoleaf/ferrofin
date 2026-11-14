#!/usr/bin/env bash
# The Jellyfin stage of a round-trip route, sourced by run.sh: boot the Jellyfin release under test on a
# copy of what Ferrofin left, read its log (checks_boot.py), compare its answers with Ferrofin's oracle
# and check the file once it has stopped. It uses run.sh's state: route, dst, the settings, JELLYFIN_ID,
# MADE, and why[] through check and mark.
# shellcheck disable=SC2154  # run.sh sets what this reads

# jellyfin_stage: the Jellyfin checks of one route, each recorded.
jellyfin_stage() {
  local name cid=$dst/jellyfin.cid port url='' ready=1 log
  name=$(rt_name "$RUN_ID" "$route" jellyfin)
  if ! { cp -a "$dst/ferrofin" "$dst/jellyfin" && mkdir -p "$dst/jellyfin-cache"; }; then
    why+=("cannot copy Ferrofin's result"); mark Jellyfin ready; return
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
    check reasons python3 "$HERE/checks_boot.py" errors "$dst/jellyfin-boot.log"
    mark Jellyfin "no ERR or FTL outside the allowlist"
  fi
  if [ -n "$ready" ]; then
    check reasons_within "$SCAN_TIMEOUT" "Jellyfin's answers" \
      python3 "$HERE/answers.py" --base-url "$url" --data-dir "$dst/jellyfin" --out "$dst/answers.txt"
    [ ! -f "$dst/answers.txt" ] || check rt_compare_answers "$dst/oracle.txt" "$dst/answers.txt"
    mark Jellyfin "answers equal Ferrofin's before the stop"
  fi
  if [ -s "$cid" ]; then
    rt_stop "$name" "$STOP_TIMEOUT" >/dev/null
    rt_docker logs "$name" > "$dst/jellyfin.log" 2>&1
  fi
  rt_remove "$cid"
  check adoption_check_db "$dst/jellyfin/data/jellyfin.db"
  mark Jellyfin "integrity_check and foreign_key_check"
}
