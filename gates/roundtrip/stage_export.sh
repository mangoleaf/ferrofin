#!/usr/bin/env bash
# The export stage of a round-trip route, sourced by run.sh: `ferrofin-server export-jellyfin` from the image
# under test, run as an operator runs it once the server has stopped, writes the copy Jellyfin boots on, and
# its stored paths, its schema and its loss report are checked before Jellyfin opens it (checks_paths.py stored,
# checks_db.py export). It uses run.sh's state: route, ran_at, config_at,
# SERVER_DIRS, media, dst, the settings, TAG, FIXTURES, MADE, and why[] through check and mark.
# shellcheck disable=SC2154  # run.sh sets what this reads

# export_stage: status 0 when the export wrote the copy; 1 (with the reason in why[]) when not.
export_stage() {
  local name cid=$dst/export.cid rc=0 start=$SECONDS last
  name=$(rt_name "$RUN_ID" "$route" export)
  MADE+=("$cid")
  # The directories at the paths the server ran on them. The output's parent is a mount of its own, since the
  # export writes <out>.partial beside the output and renames it. A library's size bounds it, as a scan's.
  # Waited for in the background: bash runs a trap only once a foreground command ends, and a run stopped
  # meanwhile must stop at once (its cleanup removes the container).
  rt_run_once "$SCAN_TIMEOUT" "$name" "$cid" "$TAG" "${SERVER_DIRS[@]}" \
    -v "$dst:/route" -- export-jellyfin --out /route/jellyfin > "$dst/export.log" 2>&1 &
  wait "$!" || rc=$?
  last=$(grep -v '^[[:space:]]*$' "$dst/export.log" | tail -n 1 | sed 's/^[[:space:]]*//')
  # Past the deadline: timeout's 124, or its 137 once it killed a docker client still waiting on a container
  # that ignored the TERM (the export's process 1 has no handler for it).
  if [ "$rc" = 124 ] || { [ "$rc" = 137 ] && [ $((SECONDS - start)) -ge "$SCAN_TIMEOUT" ]; }; then
    why+=("the export had no result within $SCAN_TIMEOUT s")
  elif [ "$rc" != 0 ]; then
    why+=("the export failed with status $rc${last:+: $last}")
  fi
  rt_remove "$cid"
  mark Export "the export completes"
  if [ "${#why[@]}" = 0 ]; then
    # where the server ran and the copies that hold it: no exported path may lie under either, and a path the
    # source already stored without a file there is not the export's to resolve
    local copies=(--ran "$ran_at" "$dst/ferrofin")
    [ "$config_at" = - ] || copies+=(--ran "$config_at" "$dst/ferrofin-config")
    check reasons python3 "$HERE/checks_paths.py" stored --db "$dst/jellyfin/data/jellyfin.db" --out "$dst/jellyfin" \
      --media "$FIXTURES/$media" --before "$dst/ferrofin/data/jellyfin.db" "${copies[@]}"
    mark Export "every stored path is in the export, none under the directory Ferrofin ran at"
    check reasons python3 "$HERE/checks_db.py" export --db "$dst/jellyfin/data/jellyfin.db" \
      --source "$dst/ferrofin/data/jellyfin.db" --report "$dst/jellyfin/$RT_LOSS_REPORT" --schema "$RT_SCHEMA"
    mark Export "its database has Jellyfin's schema, and its loss report every table of Ferrofin's that held rows"
  fi
  [ "${#why[@]}" = 0 ]
}
