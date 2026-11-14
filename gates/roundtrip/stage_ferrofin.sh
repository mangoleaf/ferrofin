#!/usr/bin/env bash
# The Ferrofin stage of a round-trip route, sourced by run.sh: adopt a fresh copy of the fixture, scan it,
# run the usage stage (usage.py, which captures Ferrofin's answers as the route's oracle) and stop. It uses
# run.sh's state: route, fixture, generation, dst, the settings, MADE, and why[] through check and mark.
# shellcheck disable=SC2154  # run.sh sets what this reads

# ferrofin_stage: status 0 when the copy is ready for Jellyfin; 1 (with the reasons in why[]) when not.
ferrofin_stage() {
  local name cid=$dst/ferrofin.cid port url='' failed
  name=$(rt_name "$RUN_ID" "$route" ferrofin)
  if ! { cp -a "$FIXTURES/$fixture" "$dst/ferrofin" && mkdir -p "$dst/ferrofin-cache"; }; then
    why+=("cannot copy $fixture"); mark Ferrofin ready; return 1
  fi
  MADE+=("$cid")
  if port=$(rt_start "$name" "$cid" "$TAG" -e FERROFIN_DATA_DIR=/config -e FERROFIN_CACHE_DIR=/cache \
      -v "$dst/ferrofin:/config" -v "$dst/ferrofin-cache:/cache" "${MEDIA[@]}" 2>"$dst/ferrofin-start.err"); then
    url=http://127.0.0.1:$port
    rt_wait_up "$name" "$port" "$READY_TIMEOUT" || why+=("$(rt_not_up "$name" "$READY_TIMEOUT")")
  else
    why+=("did not start: $(head -c 300 "$dst/ferrofin-start.err")")
  fi
  [ -s "$dst/ferrofin-start.err" ] || rm -f -- "$dst/ferrofin-start.err"
  mark Ferrofin ready
  if [ "${#why[@]}" = 0 ]; then
    check reasons python3 "$HERE/api.py" scan --base-url "$url" --data-dir "$dst/ferrofin" --timeout "$SCAN_TIMEOUT"
    mark Ferrofin "the library scan completes"
    rt_docker logs "$name" > "$dst/ferrofin-boot.log" 2>&1
    check adoption_check_boot_log "$dst/ferrofin-boot.log" "$generation"
    mark Ferrofin "boot log (generation, migrations, no ERROR)"
    # its actions, two scans each followed by a settle (each wait within SCAN_TIMEOUT) and the probes: five times that
    check reasons_within $((5 * SCAN_TIMEOUT)) "the usage stage" env ROUNDTRIP_SCAN_TIMEOUT="$SCAN_TIMEOUT" \
      python3 "$HERE/usage.py" --base-url "$url" --data-dir "$dst/ferrofin" --oracle-out "$dst/oracle.txt"
    [ -s "$dst/oracle.txt" ] || [ "$mark_from" != "${#why[@]}" ] || why+=("the usage stage wrote no answers")
    mark Ferrofin "usage stage, then Ferrofin's answers"
  fi
  failed=${#why[@]}
  if [ "$failed" = 0 ]; then
    check rt_stop "$name" "$STOP_TIMEOUT"   # judged only for a server that ran: one that never answered says nothing
    mark Ferrofin "clean stop"
  else
    rt_stop "$name" "$STOP_TIMEOUT" >/dev/null
  fi
  [ ! -s "$cid" ] || rt_docker logs "$name" > "$dst/ferrofin.log" 2>&1
  rt_remove "$cid"
  # a server that stopped badly may have left its database mid-write: Jellyfin is not booted on that
  [ "${#why[@]}" = 0 ]
}
