#!/usr/bin/env bash
# The Ferrofin stage of a round-trip route, sourced by run.sh: adopt a fresh copy of the fixture, scan it,
# set up Live TV on the tuner and guide livetv.py writes into the route (mounted at /livetv, as for Jellyfin),
# run the usage stage (usage.py, which captures Ferrofin's answers as the route's oracle) and stop. The scale
# route only adopts it, waits for its startup tasks and stops: its 40,000 clones' files are hard links to their
# templates' (scale.py), so a scan re-identifies and re-extracts every clone, which measures the fixture, not the
# way back (the adoption gate scans it neither). The native route has no fixture: its source is Ferrofin's first
# start on an empty directory, seeded as each fixture's Jellyfin is and given the traps of native.py, with
# native.py's decoy written once it stops; its database is Ferrofin's own ferrofin.db, which data/jellyfin.db links
# to for the checks, written for the layout every other copy has. It uses run.sh's state: route, fixture,
# generation, kind, config_at and SERVER_DIRS (the directories it runs in), MEDIA, dst, the settings, MADE, and
# why[] through check and mark.
# shellcheck disable=SC2154  # run.sh sets what this reads

# ferrofin_stage: status 0 when the copy is ready for the export; 1 (with the reasons in why[]) when not.
ferrofin_stage() {
  local name cid=$dst/ferrofin.cid port url='' failed
  name=$(rt_name "$RUN_ID" "$route" ferrofin)
  if [ "$kind" = native ]; then
    mkdir -p "$dst/ferrofin/data" "$dst/ferrofin-cache" || { why+=("cannot create its empty directory"); mark Ferrofin ready; return 1; }
  elif ! { cp -a "$FIXTURES/$fixture" "$dst/ferrofin" && mkdir -p "$dst/ferrofin-cache"; }; then
    why+=("cannot copy $fixture"); mark Ferrofin ready; return 1
  fi
  if ! python3 "$HERE/livetv.py" fixture "$dst/livetv"; then
    why+=("cannot write the Live TV fixture"); mark Ferrofin ready; return 1
  fi
  # a configuration directory of its own: the copy's config/ moves there, as a package migration moves it
  if [ "$config_at" != - ] && ! mv "$dst/ferrofin/config" "$dst/ferrofin-config"; then
    why+=("$fixture has no config/ to run at $config_at"); mark Ferrofin ready; return 1
  fi
  MADE+=("$cid")
  if port=$(rt_start "$name" "$cid" "$TAG" "${SERVER_DIRS[@]}" -e FERROFIN_CACHE_DIR=/cache \
      -v "$dst/ferrofin-cache:/cache" -v "$dst/livetv:/livetv:ro" "${MEDIA[@]}" 2>"$dst/ferrofin-start.err"); then
    url=http://127.0.0.1:$port
    rt_wait_up "$name" "$port" "$READY_TIMEOUT" || why+=("$(rt_not_up "$name" "$READY_TIMEOUT")")
  else
    # the start error's first 300 bytes: docker's reason, short enough for the route's line
    why+=("did not start: $(head -c 300 "$dst/ferrofin-start.err")")
  fi
  [ -s "$dst/ferrofin-start.err" ] || rm -f -- "$dst/ferrofin-start.err"
  mark Ferrofin ready
  if [ "${#why[@]}" = 0 ] && [ "$kind" = native ]; then
    # its seeding scans twice and waits for the keyframe task: three scans' time
    check reasons_within $((3 * SCAN_TIMEOUT)) "the seeding" python3 "$HERE/native.py" seed --base-url "$url"
    [ -f "$dst/ferrofin/ferrofin.db" ] || why+=("it created no ferrofin.db")
    [ "$mark_from" != "${#why[@]}" ] || ln -s ../ferrofin.db "$dst/ferrofin/data/jellyfin.db" ||
      why+=("cannot link data/jellyfin.db to its ferrofin.db")
    mark Ferrofin "its first start, seeded as the fixtures' Jellyfin are, with a port, throttling and a cast receiver of its own"
  fi
  if [ "${#why[@]}" = 0 ] && [ "$kind" = scale ]; then
    check reasons python3 "$HERE/api.py" settle --base-url "$url" --data-dir "$dst/ferrofin" --timeout "$SCAN_TIMEOUT"
    mark Ferrofin "its startup tasks finish"
    rt_docker logs "$name" > "$dst/ferrofin-boot.log" 2>&1
    check adoption_check_boot_log "$dst/ferrofin-boot.log" "$generation"
    mark Ferrofin "boot log (generation, migrations, no ERROR)"
  elif [ "${#why[@]}" = 0 ]; then
    check reasons python3 "$HERE/api.py" scan --base-url "$url" --data-dir "$dst/ferrofin" --timeout "$SCAN_TIMEOUT"
    mark Ferrofin "the library scan completes"
    rt_docker logs "$name" > "$dst/ferrofin-boot.log" 2>&1
    check adoption_check_boot_log "$dst/ferrofin-boot.log" "${generation#-}"   # "-": the native route adopts none
    mark Ferrofin "boot log (generation, migrations, no ERROR)"
    # before the usage stage, so its answers (the oracle) hold the channels too
    # twice the script's own bound, so its own reason comes first
    check reasons_within $((2 * SCAN_TIMEOUT)) "the Live TV setup" python3 "$HERE/livetv.py" setup --base-url "$url" \
      --data-dir "$dst/ferrofin" --out "$dst/livetv-ferrofin.json" --timeout "$SCAN_TIMEOUT"
    mark Ferrofin "Live TV: a tuner, a guide, a timer, a series timer and a favourite channel"
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
  if [ "${#why[@]}" = 0 ] && [ "$kind" = native ]; then
    check reasons python3 "$HERE/native.py" decoy --data-dir "$dst/ferrofin"
    mark Ferrofin "a legacy library.db.old beside its database, which only Jellyfin's code migrations would read"
  fi
  # a server that stopped badly may have left its database mid-write: Jellyfin is not booted on that
  [ "${#why[@]}" = 0 ]
}
